//! Process-confinement seam (`ctx.sandbox`).
//!
//! Path policy ([`SandboxPolicy`]) and process wrapping ([`ProcessConfiner`])
//! share this service. `confine` must return enforcing argv or fail closed;
//! it never passes the caller's argv through unconfined.

use dsh_cordis::Service;
use std::sync::Arc;
use thiserror::Error;

mod classify;
mod escalation;

pub use classify::{
    bwrap_runner_failure_rules, classify_denial, classify_runner_failure, is_usable_workdir,
    landlock_runner_failure_rules, matches_signature, seatbelt_runner_failure_rules,
    windows_acl_denial_signatures, windows_acl_runner_failure_rules, RunnerFailureMatch,
    LANDLOCK_LAUNCHER_BIN, LANDLOCK_LAUNCHER_FAILURE_EXIT, WINDOWS_ACL_RUNNER_FAILURE_EXIT,
};
pub use escalation::{
    approve_escalation, escalation_audit_reason, escalation_hint_marker, sandbox_denial_marker,
    validate_escalation_args, wider_modes, EscalationIngredients, EscalationRequest,
    ESCALATION_TARGETS,
};

/// File-effect mode for one execution. Matches the TypeScript `SandboxMode` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Required sinks only (for example `/dev/null`).
    ReadOnly,
    /// Workspace root plus a backend temp area may be written.
    WorkspaceWrite,
    /// No process confinement.
    DangerFullAccess,
}

impl SandboxMode {
    /// Parse a TypeScript mode string.
    pub fn parse(mode: &str) -> Option<Self> {
        match mode {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    /// TypeScript mode string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// Per-call file-effect policy carried into [`SandboxRuntime::confine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExecutionPolicy {
    /// File-effect mode.
    pub mode: SandboxMode,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
}

/// How completely the selected backend governs the promised file effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEnforcement {
    /// Every promised file effect is governed.
    Full,
    /// An older kernel ABI or backend cannot govern every promised effect.
    Partial,
}

/// Evidence that identifies a sandbox runner failing before it executes the
/// wrapped command. A consumer first applies [`Self::allowed_exit_codes`] when
/// present, removes [`Self::informational_lines`] by case-insensitive exact line
/// equality, then matches [`Self::fatal_signatures`] case-insensitively within
/// each remaining stderr line. Exit status alone never proves runner failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFailureRule {
    /// Nonzero process exit codes on which this rule may match; `None` permits any nonzero exit.
    pub allowed_exit_codes: Option<Vec<i32>>,
    /// Non-empty substrings identifying a fatal runner diagnostic on one stderr line.
    pub fatal_signatures: Vec<String>,
    /// Benign stderr lines excluded by exact full-line equality before fatal matching.
    pub informational_lines: Vec<String>,
}

/// Argv to spawn instead of the caller's own, plus enforcement completeness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedArgv {
    /// Runner, profile, separator, then the caller's argv.
    pub argv: Vec<String>,
    /// Enforcement completeness the selected backend achieves.
    pub enforcement: SandboxEnforcement,
    /// Case-insensitive stderr substrings this backend emits on a file-effect denial.
    pub denial_signatures: Vec<String>,
    /// Structured runner-failure evidence rules for this wrap.
    pub runner_failure_rules: Vec<RunnerFailureRule>,
}

/// Fail-closed code when a confined mode cannot be enforced.
pub const SANDBOX_UNAVAILABLE: &str = "SANDBOX_UNAVAILABLE";

/// Process-confinement failures.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// No usable backend for a confined mode. Never run the command unconfined.
    #[error("{message}")]
    Unavailable {
        /// Requested confined mode string.
        mode: String,
        /// Model-facing text, including the TypeScript `SANDBOX_UNAVAILABLE` sentence.
        message: String,
    },
}

impl SandboxError {
    /// TypeScript `SandboxUnavailableError` text for `mode`.
    pub fn unavailable(mode: &str, detail: Option<&str>) -> Self {
        let mut message = format!(
            "sandbox mode \"{mode}\" is requested but no sandbox backend is usable on this host; \
             refusing to run the command unconfined. Install bubblewrap or run a Landlock-enforcing \
             kernel (Linux), ensure sandbox-exec is usable (macOS), or ensure the ACL \
             restricted-token runner can start (Windows) — otherwise switch the consumer to \
             danger-full-access."
        );
        if let Some(detail) = detail {
            message.push_str(" Runner failure: ");
            message.push_str(detail);
        }
        Self::Unavailable {
            mode: mode.to_string(),
            message,
        }
    }
}

/// File-path policy for one confined execution.
pub trait SandboxPolicy: Send + Sync {
    /// Whether `path` may be read or written under this policy.
    fn allow_path(&self, path: &str) -> bool;
}

/// Process wrapper: return enforcing argv or fail closed.
pub trait ProcessConfiner: Send + Sync {
    /// Wrap `argv` under `policy`. The caller spawns the returned argv instead.
    fn confine(
        &self,
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<ConfinedArgv, SandboxError>;
}

/// `ctx.sandbox`.
pub struct SandboxRuntime {
    policy: Arc<dyn SandboxPolicy>,
    confiner: Option<Arc<dyn ProcessConfiner>>,
}

impl SandboxRuntime {
    /// Wrap a path policy. Process confinement stays fail-closed until a confiner is installed.
    pub fn new(policy: Arc<dyn SandboxPolicy>) -> Self {
        Self {
            policy,
            confiner: None,
        }
    }

    /// Attach a process confiner. Path policy is unchanged.
    pub fn with_confiner(mut self, confiner: Arc<dyn ProcessConfiner>) -> Self {
        self.confiner = Some(confiner);
        self
    }

    /// Borrow the active path policy.
    pub fn policy(&self) -> Arc<dyn SandboxPolicy> {
        Arc::clone(&self.policy)
    }

    /// Delegate a path check to the active policy.
    pub fn allow_path(&self, path: &str) -> bool {
        self.policy.allow_path(path)
    }

    /// Wrap `argv` under `policy`, or fail closed when no backend is usable.
    pub fn confine(
        &self,
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        match &self.confiner {
            Some(confiner) => confiner.confine(argv, policy),
            None => Err(SandboxError::unavailable(policy.mode.as_str(), None)),
        }
    }
}

impl Service for SandboxRuntime {
    const KEY: &'static str = "sandbox";
}

impl SandboxPolicy for SandboxRuntime {
    fn allow_path(&self, path: &str) -> bool {
        self.policy.allow_path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    struct AllowAll;

    impl SandboxPolicy for AllowAll {
        fn allow_path(&self, _: &str) -> bool {
            true
        }
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SandboxRuntime::new(Arc::new(AllowAll))))
            .unwrap();
        assert!(ctx.has_service("sandbox"));
        ctx.dispose();
        assert!(!ctx.has_service("sandbox"));
    }

    #[test]
    fn confine_without_confiner_fails_closed() {
        let runtime = SandboxRuntime::new(Arc::new(AllowAll));
        let err = runtime
            .confine(
                &["echo".into(), "hi".into()],
                &SandboxExecutionPolicy {
                    mode: SandboxMode::ReadOnly,
                    workspace_root: "/tmp".into(),
                },
            )
            .unwrap_err();
        match err {
            SandboxError::Unavailable { mode, message } => {
                assert_eq!(mode, "read-only");
                assert!(
                    message.contains(SANDBOX_UNAVAILABLE) || message.contains("refusing to run")
                );
                assert!(message.contains("danger-full-access"));
            }
        }
    }

    #[test]
    fn unavailable_appends_runner_failure_detail() {
        let err = SandboxError::unavailable(
            "read-only",
            Some("landlock-run: landlock is not enforced by this kernel"),
        );
        match err {
            SandboxError::Unavailable { mode, message } => {
                assert_eq!(mode, "read-only");
                assert!(message.contains("refusing to run the command unconfined"));
                assert!(message.contains(
                    "Runner failure: landlock-run: landlock is not enforced by this kernel"
                ));
            }
        }
    }
}
