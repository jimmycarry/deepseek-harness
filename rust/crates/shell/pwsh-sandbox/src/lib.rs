//! Sandbox-consuming PowerShell executor — the pwsh twin of `dsh-bash-sandbox`.
//! It wraps the exact local pwsh argv through `ctx.sandbox`. Foreground runner
//! failure throws `SANDBOX_UNAVAILABLE`; background processes stamp `runnerFailed`.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_pwsh_local::PwshLocal;
use dsh_sandbox::{SandboxExecutionPolicy, SandboxMode, SandboxRuntime};
use dsh_shell::{
    map_spawn, stamp_foreground, unavailable_error, ConfinedChild, ShellChild, ShellError,
    ShellExecutor, ShellRuntime, ShellRunResult, ShellSandboxInfo, ShellSpec,
};
use dsh_subprocess::SubprocessRuntime;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-pwsh-sandbox"
}

/// Deployment policy read from `ctx.sandboxPolicy` at apply time.
#[derive(Debug, Clone)]
pub struct Config {
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    pub mode: String,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
    /// Explicit pwsh executable; omitted uses PATH / well-known locations.
    pub pwsh_path: Option<String>,
}

/// PowerShell executor that confines through `ctx.sandbox` except under full access.
pub struct PwshSandbox {
    local: PwshLocal,
    sandbox: Arc<SandboxRuntime>,
    mode: SandboxMode,
    workspace_root: String,
}

impl PwshSandbox {
    /// Bind local pwsh, subprocess, and the process-confinement service.
    pub fn new(
        subprocess: Arc<SubprocessRuntime>,
        sandbox: Arc<SandboxRuntime>,
        config: Config,
    ) -> Result<Self, String> {
        let mode = SandboxMode::parse(&config.mode)
            .ok_or_else(|| format!("pwsh-sandbox: unknown sandbox mode `{}`", config.mode))?;
        if config.workspace_root.is_empty() {
            return Err("pwsh-sandbox: workspace_root must be a non-empty path".into());
        }
        Ok(Self {
            local: PwshLocal::new(subprocess, config.pwsh_path.as_deref()),
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
            .confine(&self.local.argv(spec), policy)
            .map_err(unavailable_error)
    }
}

#[async_trait]
impl ShellExecutor for PwshSandbox {
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
        let result = match self.local.run_argv(spec.clone(), &confined.argv).await {
            Ok(result) => result,
            Err(error) => return Err(map_spawn(error, &confined, &spec, policy.mode)),
        };
        stamp_foreground(result, &policy, &confined)
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
        let inner = match self.local.start_argv(&spec, &confined.argv) {
            Ok(inner) => inner,
            Err(error) => return Err(map_spawn(error, &confined, &spec, policy.mode)),
        };
        Ok(Box::new(ConfinedChild::new(inner, confined, policy.mode)))
    }
}

/// Provide `ctx.shell` as [`PwshSandbox`]. Requires `ctx.subprocess` and `ctx.sandbox`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<Arc<ShellRuntime>> {
    let subprocess = ctx.service::<SubprocessRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("pwsh-sandbox requires ctx.subprocess".into())
    })?;
    let sandbox = ctx.service::<SandboxRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("pwsh-sandbox requires ctx.sandbox".into())
    })?;
    let executor = PwshSandbox::new(subprocess, sandbox, config)
        .map_err(dsh_cordis::CordisError::Validation)?;
    let mode = executor.mode.clone();
    let runtime = Arc::new(ShellRuntime::new(Arc::new(executor)).with_sandbox_mode(mode));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sandbox::{ConfinedArgv, ProcessConfiner, SandboxEnforcement, SandboxError};
    use dsh_sandbox_local::CwdSandbox;
    use dsh_shell::resolve as resolve_shell;
    use dsh_shell::ShellRequest;
    use dsh_subprocess_local::LocalSubprocess;

    struct ScriptedConfiner {
        argv: Vec<String>,
    }

    impl ProcessConfiner for ScriptedConfiner {
        fn confine(
            &self,
            argv: &[String],
            _policy: &SandboxExecutionPolicy,
        ) -> Result<ConfinedArgv, SandboxError> {
            Ok(ConfinedArgv {
                argv: if self.argv.is_empty() {
                    argv.to_vec()
                } else {
                    self.argv.clone()
                },
                enforcement: SandboxEnforcement::Full,
                denial_signatures: vec!["permission denied".into()],
                runner_failure_rules: dsh_sandbox::landlock_runner_failure_rules(),
            })
        }
    }

    fn sandbox_with(confiner: ScriptedConfiner) -> PwshSandbox {
        let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
        let sandbox = Arc::new(
            SandboxRuntime::new(Arc::new(CwdSandbox::new("/tmp"))).with_confiner(Arc::new(confiner)),
        );
        PwshSandbox::new(
            subprocess,
            sandbox,
            Config {
                mode: "read-only".into(),
                workspace_root: "/tmp".into(),
                pwsh_path: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn confine_hands_the_provider_pwsh_argv_not_bash_c() {
        struct Recording;
        impl ProcessConfiner for Recording {
            fn confine(
                &self,
                argv: &[String],
                _policy: &SandboxExecutionPolicy,
            ) -> Result<ConfinedArgv, SandboxError> {
                assert_ne!(argv.get(1).map(String::as_str), Some("-c"));
                assert!(argv.iter().any(|part| part == "-Command"));
                assert!(argv.iter().any(|part| part == "-NoLogo"));
                Ok(ConfinedArgv {
                    argv: vec!["true".into()],
                    enforcement: SandboxEnforcement::Full,
                    denial_signatures: Vec::new(),
                    runner_failure_rules: Vec::new(),
                })
            }
        }
        let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
        let sandbox = Arc::new(
            SandboxRuntime::new(Arc::new(CwdSandbox::new("/tmp"))).with_confiner(Arc::new(Recording)),
        );
        let pwsh = PwshSandbox::new(
            subprocess,
            sandbox,
            Config {
                mode: "read-only".into(),
                workspace_root: "/tmp".into(),
                pwsh_path: None,
            },
        )
        .unwrap();
        let _ = pwsh.confine(
            &resolve_shell(ShellRequest {
                command: "Get-Date".into(),
                ..ShellRequest::default()
            }),
            &SandboxExecutionPolicy {
                mode: SandboxMode::ReadOnly,
                workspace_root: "/tmp".into(),
            },
        );
    }

    #[tokio::test]
    async fn foreground_runner_failure_is_unavailable() {
        let pwsh = sandbox_with(ScriptedConfiner {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "echo 'landlock-run: exec failed' >&2; exit 125".into(),
            ],
        });
        let err = pwsh
            .run(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap_err();
        match err {
            ShellError::Unavailable(message) => {
                assert!(message.contains("Runner failure: landlock-run: exec failed"));
            }
            other => panic!("expected Unavailable, got {other}"),
        }
    }

    #[test]
    fn unknown_mode_fails_at_construction() {
        let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
        let sandbox = Arc::new(SandboxRuntime::new(Arc::new(CwdSandbox::new("/tmp"))));
        match PwshSandbox::new(
            subprocess,
            sandbox,
            Config {
                mode: "nope".into(),
                workspace_root: "/tmp".into(),
                pwsh_path: None,
            },
        ) {
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
                pwsh_path: None,
            },
        ) {
            Ok(_) => panic!("expected missing subprocess"),
            Err(err) => assert!(err.to_string().contains("subprocess")),
        }
    }
}
