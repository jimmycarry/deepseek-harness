//! Shell executor seam (`ctx.shell`).

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_sandbox::{SandboxEnforcement, SandboxExecutionPolicy, SandboxMode};
use std::collections::BTreeMap;
use thiserror::Error;

/// Prefix of every managed `DSH_*` environment key.
pub const DSH_ENV_PREFIX: &str = "DSH_";

/// Collected stdout or stderr from one run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectedOutput {
    /// Captured text (possibly a truncated tail).
    pub text: String,
    /// Whether capture dropped bytes from memory.
    pub truncated: bool,
    /// Spill file holding the full stream, when available.
    pub spill_path: Option<String>,
}

/// Sandbox facts for one run, present iff a sandboxing executor handled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSandboxInfo {
    /// Mode the command actually ran under.
    pub mode: SandboxMode,
    /// Whether the sandbox denied a file operation.
    pub denied: bool,
    /// How completely the selected runner enforced the requested mode.
    pub enforcement: Option<SandboxEnforcement>,
    /// Whether the sandbox runner failed before the command could run.
    pub runner_failed: Option<bool>,
}

/// Outcome of one completed (or killed) foreground run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRunResult {
    /// Exit code; `None` when the process died from a signal.
    pub exit_code: Option<i32>,
    /// Terminating signal name (for example `SIGTERM`); `None` on a normal exit.
    pub signal: Option<String>,
    /// True when the executor's own timeout was the first cause to stop the command.
    pub timed_out: bool,
    /// True when a caller abort was the first cause to kill the command.
    pub aborted: bool,
    /// Effective timeout applied to this run, in milliseconds.
    pub timeout_ms: u64,
    /// Captured stdout.
    pub stdout: CollectedOutput,
    /// Captured stderr.
    pub stderr: CollectedOutput,
    /// Sandbox execution facts, absent for an unsandboxed executor.
    pub sandbox: Option<ShellSandboxInfo>,
}

impl ShellRunResult {
    /// Successful empty-stderr run used by in-process test doubles.
    pub fn from_stdout(text: impl Into<String>) -> Self {
        Self {
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            aborted: false,
            timeout_ms: 120_000,
            stdout: CollectedOutput {
                text: text.into(),
                truncated: false,
                spill_path: None,
            },
            stderr: CollectedOutput::default(),
            sandbox: None,
        }
    }
}

/// Settled background-process exit facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellChildExit {
    /// Exit code; `None` when killed by a signal.
    pub exit_code: Option<i32>,
    /// Terminating signal name, when signal-killed.
    pub signal: Option<String>,
    /// True when the process was signal-killed (including an explicit cancel).
    pub killed: bool,
}

/// Background process handle returned by [`ShellExecutor::start`].
pub trait ShellChild: Send + Sync {
    /// Request termination. Idempotent.
    fn cancel(&self);
    /// Block until the process exits. Idempotent.
    ///
    /// # Errors
    /// Infrastructure wait failures, not a nonzero command exit.
    fn wait(&self) -> Result<ShellChildExit, ShellError>;
    /// Consume output produced since the previous read.
    fn read_output(&self) -> String;
    /// Full collected stderr, used by sandboxing executors to classify denials.
    fn collected_stderr(&self) -> String;
    /// Sandbox facts, stamped once a confined process settles.
    fn sandbox_info(&self) -> Option<ShellSandboxInfo>;
}

/// Resolved shell spec.
#[derive(Debug, Clone, Default)]
pub struct ShellSpec {
    /// Command string.
    pub command: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Trusted managed environment overlay. `None` means inherit ambient `DSH_*`.
    pub dsh_env: Option<BTreeMap<String, String>>,
    /// Per-call file-effect policy; sandboxing executors default it.
    pub sandbox_policy: Option<SandboxExecutionPolicy>,
    /// Foreground timeout in milliseconds. [`ShellExecutor::start`] ignores it.
    pub timeout_ms: Option<u64>,
}

/// Shell request before resolve.
#[derive(Debug, Clone, Default)]
pub struct ShellRequest {
    /// Command string.
    pub command: String,
    /// Optional cwd.
    pub cwd: Option<String>,
    /// Trusted managed environment overlay.
    pub dsh_env: Option<BTreeMap<String, String>>,
    /// Per-call file-effect policy; sandboxing executors default it.
    pub sandbox_policy: Option<SandboxExecutionPolicy>,
    /// Optional foreground timeout in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// Copy request fields onto a spec. Executors may fill remaining defaults.
pub fn resolve(request: ShellRequest) -> ShellSpec {
    ShellSpec {
        command: request.command,
        cwd: request.cwd,
        dsh_env: request.dsh_env,
        sandbox_policy: request.sandbox_policy,
        timeout_ms: request.timeout_ms,
    }
}

/// Shell failures.
#[derive(Debug, Error, Clone)]
pub enum ShellError {
    /// Backend failure.
    #[error("{0}")]
    Failed(String),
    /// A confined mode was requested and no sandbox backend is usable.
    #[error("{0}")]
    Unavailable(String),
}

/// Provider interface.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Apply implementation-owned defaults before [`run`] / [`start`].
    fn resolve(&self, request: ShellRequest) -> ShellSpec {
        resolve(request)
    }

    /// Run a resolved spec. Nonzero exits, timeouts, and kills resolve as a result.
    async fn run(&self, spec: ShellSpec) -> Result<ShellRunResult, ShellError>;

    /// Start a background process. No timeout applies.
    ///
    /// # Errors
    /// Spawn failure, or an executor that does not support background start.
    fn start(&self, spec: ShellSpec) -> Result<Box<dyn ShellChild>, ShellError> {
        let _ = spec;
        Err(ShellError::Failed(
            "background start is not supported by this executor".into(),
        ))
    }
}

/// `ctx.shell`.
pub struct ShellRuntime {
    backend: std::sync::Arc<dyn ShellExecutor>,
    sandbox_mode: Option<SandboxMode>,
}

impl ShellRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn ShellExecutor>) -> Self {
        Self {
            backend,
            sandbox_mode: None,
        }
    }

    /// Record the executor's standing sandbox mode.
    pub fn with_sandbox_mode(mut self, mode: SandboxMode) -> Self {
        self.sandbox_mode = Some(mode);
        self
    }

    /// Standing sandbox mode when the executor confines.
    pub fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.sandbox_mode.clone()
    }

    /// Apply the backend's resolve step.
    pub fn resolve(&self, request: ShellRequest) -> ShellSpec {
        self.backend.resolve(request)
    }

    /// Run a resolved spec.
    pub async fn run(&self, spec: ShellSpec) -> Result<ShellRunResult, ShellError> {
        self.backend.run(spec).await
    }

    /// Start a background process.
    ///
    /// # Errors
    /// Spawn failure, or an executor that does not support background start.
    pub fn start(&self, spec: ShellSpec) -> Result<Box<dyn ShellChild>, ShellError> {
        self.backend.start(spec)
    }
}

impl Service for ShellRuntime {
    const KEY: &'static str = "shell";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_copies_timeout_without_defaulting() {
        let spec = resolve(ShellRequest {
            command: "true".into(),
            timeout_ms: Some(50),
            ..ShellRequest::default()
        });
        assert_eq!(spec.timeout_ms, Some(50));
        let unspecified = resolve(ShellRequest {
            command: "true".into(),
            ..ShellRequest::default()
        });
        assert!(unspecified.timeout_ms.is_none());
    }

    #[test]
    fn from_stdout_is_a_successful_empty_stderr_run() {
        let result = ShellRunResult::from_stdout("hello\n");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.text, "hello\n");
        assert!(result.stderr.text.is_empty());
        assert!(!result.timed_out);
    }
}
