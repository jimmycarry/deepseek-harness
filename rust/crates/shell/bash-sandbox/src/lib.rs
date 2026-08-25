//! Sandbox-consuming bash executor. Registers as `ctx.shell` in place of the
//! local executor. `danger-full-access` runs unconfined; every other mode
//! wraps `bash -c` through `ctx.sandbox.confine` and fails closed when no
//! backend is usable.

use async_trait::async_trait;
use dsh_bash_local::BashLocal;
use dsh_cordis::Context;
use dsh_sandbox::{SandboxError, SandboxExecutionPolicy, SandboxMode, SandboxRuntime};
use dsh_shell::{ShellError, ShellExecutor, ShellRuntime, ShellSpec};
use dsh_subprocess::{resolve, SpawnRequest, SubprocessRuntime};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-bash-sandbox"
}

/// Deployment policy read from `ctx.sandboxPolicy` at apply time.
#[derive(Debug, Clone)]
pub struct Config {
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    pub mode: String,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
}

/// Bash executor that confines through `ctx.sandbox` except under full access.
pub struct BashSandbox {
    local: BashLocal,
    subprocess: Arc<SubprocessRuntime>,
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
            local: BashLocal::new(Arc::clone(&subprocess)),
            subprocess,
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
}

#[async_trait]
impl ShellExecutor for BashSandbox {
    async fn run(&self, spec: ShellSpec) -> Result<String, ShellError> {
        let policy = spec
            .sandbox_policy
            .clone()
            .unwrap_or_else(|| self.policy());
        if policy.mode == SandboxMode::DangerFullAccess {
            return self.local.run(spec).await;
        }
        let confined = self
            .sandbox
            .confine(
                &["bash".into(), "-c".into(), spec.command.clone()],
                &policy,
            )
            .map_err(|error| match error {
                SandboxError::Unavailable { message, .. } => ShellError::Unavailable(message),
            })?;
        let program =
            confined.argv.first().cloned().ok_or_else(|| {
                ShellError::Failed("sandbox confine returned an empty argv".into())
            })?;
        let args = confined.argv[1..].to_vec();
        let spawn = resolve(SpawnRequest {
            program,
            args,
            cwd: spec.cwd,
            dsh_env: spec.dsh_env,
        });
        let output = self
            .subprocess
            .run(spawn)
            .await
            .map_err(|error| ShellError::Failed(error.to_string()))?;
        if output.status != 0 {
            return Err(ShellError::Failed(output.stderr));
        }
        Ok(output.stdout)
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

    #[tokio::test]
    async fn danger_full_access_runs_unconfined() {
        let (subprocess, sandbox, config) = runtime("danger-full-access");
        let bash = BashSandbox::new(subprocess, sandbox, config).unwrap();
        let out = bash
            .run(resolve_shell(ShellRequest {
                command: "echo hello".into(),
                cwd: None,
                dsh_env: None,
                sandbox_policy: None,
            }))
            .await
            .unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn confined_without_runner_fails_closed() {
        let (subprocess, sandbox, config) = runtime("workspace-write");
        let bash = BashSandbox::new(subprocess, sandbox, config).unwrap();
        let result = bash
            .run(resolve_shell(ShellRequest {
                command: "echo hello".into(),
                cwd: None,
                dsh_env: None,
                sandbox_policy: None,
            }))
            .await;
        match result {
            Ok(out) => assert_eq!(out.trim(), "hello"),
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
