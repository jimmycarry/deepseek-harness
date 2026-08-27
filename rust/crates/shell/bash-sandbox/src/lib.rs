//! Sandbox-consuming bash executor. Registers as `ctx.shell` in place of the
//! local executor. `danger-full-access` runs unconfined; every other mode
//! wraps `bash -c` through `ctx.sandbox.confine` and fails closed when no
//! backend is usable.

use async_trait::async_trait;
use dsh_bash_local::BashLocal;
use dsh_cordis::Context;
use dsh_sandbox::{
    SandboxEnforcement, SandboxError, SandboxExecutionPolicy, SandboxMode, SandboxRuntime,
};
use dsh_shell::{
    ShellChild, ShellChildExit, ShellError, ShellExecutor, ShellRuntime, ShellRunResult,
    ShellSandboxInfo, ShellSpec,
};
use dsh_subprocess::SubprocessRuntime;
use std::sync::{Arc, Mutex};

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-bash-sandbox"
}

/// Match a non-zero exit against case-insensitive stderr signatures.
pub fn matches_signature(
    exit_code: Option<i32>,
    stderr: &str,
    signatures: &[String],
) -> bool {
    let Some(code) = exit_code else {
        return false;
    };
    if code == 0 {
        return false;
    }
    let lowered = stderr.to_ascii_lowercase();
    signatures
        .iter()
        .any(|signature| lowered.contains(&signature.to_ascii_lowercase()))
}

/// Deployment policy read from `ctx.sandboxPolicy` at apply time.
#[derive(Debug, Clone)]
pub struct Config {
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    pub mode: String,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
}

struct ProcessFacts {
    mode: SandboxMode,
    enforcement: SandboxEnforcement,
    denial_signatures: Vec<String>,
}

struct ConfinedChild {
    inner: Box<dyn ShellChild>,
    facts: ProcessFacts,
    sandbox: Mutex<Option<ShellSandboxInfo>>,
}

impl ConfinedChild {
    fn stamp(&self, exit: &ShellChildExit) {
        let mut slot = self.sandbox.lock().expect("sandbox facts");
        if slot.is_some() {
            return;
        }
        let denied = matches_signature(
            exit.exit_code,
            &self.inner.collected_stderr(),
            &self.facts.denial_signatures,
        );
        *slot = Some(ShellSandboxInfo {
            mode: self.facts.mode,
            denied,
            enforcement: Some(self.facts.enforcement),
            runner_failed: None,
        });
    }
}

impl ShellChild for ConfinedChild {
    fn cancel(&self) {
        self.inner.cancel();
    }

    fn wait(&self) -> Result<ShellChildExit, ShellError> {
        let exit = self.inner.wait()?;
        self.stamp(&exit);
        Ok(exit)
    }

    fn read_output(&self) -> String {
        self.inner.read_output()
    }

    fn collected_stderr(&self) -> String {
        self.inner.collected_stderr()
    }

    fn sandbox_info(&self) -> Option<ShellSandboxInfo> {
        self.sandbox.lock().expect("sandbox facts").clone()
    }
}

/// Bash executor that confines through `ctx.sandbox` except under full access.
pub struct BashSandbox {
    local: BashLocal,
    sandbox: Arc<SandboxRuntime>,
    mode: SandboxMode,
    workspace_root: String,
}

impl BashSandbox {
    /// Bind local bash, subprocess, and the process-confinement service.
    pub fn new(
        subprocess: Arc<SubprocessRuntime>,
        sandbox: Arc<SandboxRuntime>,
        config: Config,
    ) -> Result<Self, String> {
        let mode = SandboxMode::parse(&config.mode)
            .ok_or_else(|| format!("bash-sandbox: unknown sandbox mode `{}`", config.mode))?;
        if config.workspace_root.is_empty() {
            return Err("bash-sandbox: workspace_root must be a non-empty path".into());
        }
        Ok(Self {
            local: BashLocal::new(subprocess),
            sandbox,
            mode,
            workspace_root: config.workspace_root,
        })
    }

    fn policy(&self) -> SandboxExecutionPolicy {
        SandboxExecutionPolicy {
            mode: self.mode.clone(),
            workspace_root: self.workspace_root.clone(),
        }
    }

    fn confine(
        &self,
        spec: &ShellSpec,
        policy: &SandboxExecutionPolicy,
    ) -> Result<dsh_sandbox::ConfinedArgv, ShellError> {
        self.sandbox
            .confine(
                &["bash".into(), "-c".into(), spec.command.clone()],
                policy,
            )
            .map_err(|error| match error {
                SandboxError::Unavailable { message, .. } => ShellError::Unavailable(message),
            })
    }
}

#[async_trait]
impl ShellExecutor for BashSandbox {
    fn resolve(&self, request: dsh_shell::ShellRequest) -> ShellSpec {
        let mut spec = self.local.resolve(request);
        if spec.sandbox_policy.is_none() {
            spec.sandbox_policy = Some(self.policy());
        }
        spec
    }

    async fn run(&self, spec: ShellSpec) -> Result<ShellRunResult, ShellError> {
        let policy = spec
            .sandbox_policy
            .clone()
            .unwrap_or_else(|| self.policy());
        if policy.mode == SandboxMode::DangerFullAccess {
            let mut result = self.local.run(spec).await?;
            result.sandbox = Some(ShellSandboxInfo {
                mode: SandboxMode::DangerFullAccess,
                denied: false,
                enforcement: None,
                runner_failed: None,
            });
            return Ok(result);
        }
        let confined = self.confine(&spec, &policy)?;
        let mut result = self.local.run_argv(spec, &confined.argv).await?;
        let denied = matches_signature(
            result.exit_code,
            &result.stderr.text,
            &confined.denial_signatures,
        );
        result.sandbox = Some(ShellSandboxInfo {
            mode: policy.mode,
            denied,
            enforcement: Some(confined.enforcement),
            runner_failed: None,
        });
        Ok(result)
    }

    fn start(&self, spec: ShellSpec) -> Result<Box<dyn ShellChild>, ShellError> {
        let policy = spec
            .sandbox_policy
            .clone()
            .unwrap_or_else(|| self.policy());
        if policy.mode == SandboxMode::DangerFullAccess {
            return self.local.start(spec);
        }
        let confined = self.confine(&spec, &policy)?;
        let inner = self.local.start_argv(&spec, &confined.argv)?;
        Ok(Box::new(ConfinedChild {
            inner,
            facts: ProcessFacts {
                mode: policy.mode,
                enforcement: confined.enforcement,
                denial_signatures: confined.denial_signatures,
            },
            sandbox: Mutex::new(None),
        }))
    }
}

/// Provide `ctx.shell` as [`BashSandbox`]. Requires `ctx.subprocess` and `ctx.sandbox`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<Arc<ShellRuntime>> {
    let subprocess = ctx.service::<SubprocessRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("bash-sandbox requires ctx.subprocess".into())
    })?;
    let sandbox = ctx.service::<SandboxRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("bash-sandbox requires ctx.sandbox".into())
    })?;
    let executor = BashSandbox::new(subprocess, sandbox, config)
        .map_err(dsh_cordis::CordisError::Validation)?;
    let mode = executor.mode.clone();
    let runtime = Arc::new(ShellRuntime::new(Arc::new(executor)).with_sandbox_mode(mode));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sandbox_local::CwdSandbox;
    use dsh_shell::resolve as resolve_shell;
    use dsh_shell::ShellRequest;
    use dsh_subprocess_local::LocalSubprocess;

    fn runtime(mode: &str) -> (Arc<SubprocessRuntime>, Arc<SandboxRuntime>, Config) {
        let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
        let sandbox = Arc::new(
            SandboxRuntime::new(Arc::new(CwdSandbox::new("/tmp")))
                .with_confiner(Arc::new(dsh_sandbox_local::LocalConfiner)),
        );
        (
            subprocess,
            sandbox,
            Config {
                mode: mode.into(),
                workspace_root: "/tmp".into(),
            },
        )
    }

    #[test]
    fn matches_signature_requires_nonzero_and_dialect() {
        assert!(!matches_signature(Some(0), "read-only file system", &[
            "read-only file system".into()
        ]));
        assert!(!matches_signature(None, "read-only file system", &[
            "read-only file system".into()
        ]));
        assert!(matches_signature(Some(1), "touch: Read-only file system", &[
            "read-only file system".into()
        ]));
        assert!(!matches_signature(Some(1), "permission denied", &[
            "read-only file system".into()
        ]));
    }

    #[tokio::test]
    async fn danger_full_access_runs_unconfined() {
        let (subprocess, sandbox, config) = runtime("danger-full-access");
        let bash = BashSandbox::new(subprocess, sandbox, config).unwrap();
        let out = bash
            .run(resolve_shell(ShellRequest {
                command: "echo hello".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(out.stdout.text.trim(), "hello");
        let sandbox = out.sandbox.expect("full-access stamps sandbox");
        assert_eq!(sandbox.mode, SandboxMode::DangerFullAccess);
        assert!(!sandbox.denied);
        assert!(sandbox.enforcement.is_none());
    }

    #[tokio::test]
    async fn confined_without_runner_fails_closed() {
        let (subprocess, sandbox, config) = runtime("workspace-write");
        let bash = BashSandbox::new(subprocess, sandbox, config).unwrap();
        let result = bash
            .run(resolve_shell(ShellRequest {
                command: "echo hello".into(),
                ..ShellRequest::default()
            }))
            .await;
        match result {
            Ok(out) => assert_eq!(out.stdout.text.trim(), "hello"),
            Err(ShellError::Unavailable(message)) => {
                assert!(message.contains("workspace-write"));
                assert!(message.contains("refusing to run the command unconfined"));
            }
            Err(other) => panic!("unexpected confined result: {other}"),
        }
    }

    #[test]
    fn unknown_mode_fails_at_construction() {
        let (subprocess, sandbox, mut config) = runtime("danger-full-access");
        config.mode = "nope".into();
        match BashSandbox::new(subprocess, sandbox, config) {
            Ok(_) => panic!("expected unknown-mode failure"),
            Err(err) => assert!(err.contains("unknown sandbox mode")),
        }
    }

    #[test]
    fn install_requires_subprocess() {
        let ctx = Context::new();
        match install(
            &ctx,
            Config {
                mode: "danger-full-access".into(),
                workspace_root: "/tmp".into(),
            },
        ) {
            Ok(_) => panic!("expected missing subprocess"),
            Err(err) => assert!(err.to_string().contains("subprocess")),
        }
    }
}
