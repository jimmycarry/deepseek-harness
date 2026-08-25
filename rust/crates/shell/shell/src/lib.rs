//! Shell executor seam (`ctx.shell`).

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_sandbox::{SandboxExecutionPolicy, SandboxMode};
use std::collections::BTreeMap;
use thiserror::Error;

/// Prefix of every managed `DSH_*` environment key.
pub const DSH_ENV_PREFIX: &str = "DSH_";

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
}

/// Explicit resolve.
pub fn resolve(request: ShellRequest) -> ShellSpec {
    ShellSpec {
        command: request.command,
        cwd: request.cwd,
        dsh_env: request.dsh_env,
        sandbox_policy: request.sandbox_policy,
    }
}

/// Shell failures.
#[derive(Debug, Error)]
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
    /// Run one resolved spec.
    async fn run(&self, spec: ShellSpec) -> Result<String, ShellError>;
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

    /// Run a resolved spec.
    pub async fn run(&self, spec: ShellSpec) -> Result<String, ShellError> {
        self.backend.run(spec).await
    }
}

impl Service for ShellRuntime {
    const KEY: &'static str = "shell";
}
