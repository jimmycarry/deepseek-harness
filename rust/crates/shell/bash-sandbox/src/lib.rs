//! Sandbox-consuming bash executor. Registers as `ctx.shell` in place of the
//! local executor. `danger-full-access` runs unconfined; every other mode
//! wraps `bash -c` through `ctx.sandbox.confine` and fails closed when no
//! backend is usable. Foreground runner failure throws `SANDBOX_UNAVAILABLE`;
//! background processes stamp `runnerFailed`.

use async_trait::async_trait;
use dsh_bash_local::BashLocal;
use dsh_cordis::Context;
use dsh_sandbox::{SandboxExecutionPolicy, SandboxMode, SandboxRuntime};
use dsh_shell::{
    map_spawn, stamp_foreground, unavailable_error, ConfinedChild, ShellChild, ShellError,
    ShellExecutor, ShellRuntime, ShellRunResult, ShellSandboxInfo, ShellSpec,
};

use dsh_subprocess::SubprocessRuntime;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-bash-sandbox"
}

pub use dsh_sandbox::{classify_denial, classify_runner_failure, matches_signature};

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
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<dsh_sandbox::ConfinedArgv, ShellError> {
        self.sandbox.confine(argv, policy).map_err(unavailable_error)
    }

    fn bash_argv(spec: &ShellSpec) -> Vec<String> {
        vec!["bash".into(), "-c".into(), spec.command.clone()]
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
        let confined = self.confine(&Self::bash_argv(&spec), &policy)?;
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
        let confined = self.confine(&Self::bash_argv(&spec), &policy)?;
        let inner = match self.local.start_argv(&spec, &confined.argv) {
            Ok(inner) => inner,
            Err(error) => return Err(map_spawn(error, &confined, &spec, policy.mode)),
        };
        Ok(Box::new(ConfinedChild::new(inner, confined, policy.mode)))
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
    use dsh_sandbox::{
        ConfinedArgv, ProcessConfiner, RunnerFailureRule, SandboxEnforcement, SandboxError,
    };
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

    struct ScriptedConfiner {
        argv: Vec<String>,
        denial_signatures: Vec<String>,
        runner_failure_rules: Vec<RunnerFailureRule>,
    }

    impl ProcessConfiner for ScriptedConfiner {
        fn confine(
            &self,
            _argv: &[String],
            _policy: &SandboxExecutionPolicy,
        ) -> Result<ConfinedArgv, SandboxError> {
            Ok(ConfinedArgv {
                argv: self.argv.clone(),
                enforcement: SandboxEnforcement::Full,
                denial_signatures: self.denial_signatures.clone(),
                runner_failure_rules: self.runner_failure_rules.clone(),
            })
        }
    }

    fn scripted_bash(
        argv: Vec<String>,
        denial: Vec<String>,
        rules: Vec<RunnerFailureRule>,
    ) -> BashSandbox {
        let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
        let sandbox = Arc::new(
            SandboxRuntime::new(Arc::new(CwdSandbox::new("/tmp"))).with_confiner(Arc::new(
                ScriptedConfiner {
                    argv,
                    denial_signatures: denial,
                    runner_failure_rules: rules,
                },
            )),
        );
        BashSandbox::new(
            subprocess,
            sandbox,
            Config {
                mode: "read-only".into(),
                workspace_root: "/tmp".into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn matches_signature_requires_nonzero_and_dialect() {
        assert!(!matches_signature(Some(0), "read-only file system", &[
            "read-only file system".into()
        ]));
        assert!(matches_signature(Some(1), "touch: Read-only file system", &[
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

    #[tokio::test]
    async fn foreground_runner_failure_is_unavailable() {
        let bash = scripted_bash(
            vec![
                "sh".into(),
                "-c".into(),
                "echo 'landlock-run: exec failed' >&2; exit 125".into(),
            ],
            vec!["permission denied".into()],
            dsh_sandbox::landlock_runner_failure_rules(),
        );
        let err = bash
            .run(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap_err();
        match err {
            ShellError::Unavailable(message) => {
                assert!(message.contains("refusing to run the command unconfined"));
                assert!(message.contains("Runner failure: landlock-run: exec failed"));
            }
            other => panic!("expected Unavailable, got {other}"),
        }
    }

    #[tokio::test]
    async fn landlock_notice_alone_is_not_runner_failure() {
        let bash = scripted_bash(
            vec![
                "sh".into(),
                "-c".into(),
                "echo 'landlock-run: partial enforcement (older Landlock ABI)' >&2; exit 125".into(),
            ],
            vec!["permission denied".into()],
            dsh_sandbox::landlock_runner_failure_rules(),
        );
        let out = bash
            .run(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(125));
        let facts = out.sandbox.expect("facts");
        assert!(!facts.denied);
        assert!(facts.runner_failed.is_none());
    }

    #[tokio::test]
    async fn background_runner_failure_stamps_runner_failed() {
        let bash = scripted_bash(
            vec![
                "sh".into(),
                "-c".into(),
                "echo 'bwrap: cannot mount tmpfs' >&2; exit 1".into(),
            ],
            vec!["read-only file system".into()],
            dsh_sandbox::bwrap_runner_failure_rules(),
        );
        let child = bash
            .start(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .unwrap();
        let _ = child.wait().unwrap();
        let facts = child.sandbox_info().expect("background stamps");
        assert_eq!(facts.runner_failed, Some(true));
        assert!(!facts.denied);
    }

    #[tokio::test]
    async fn missing_runner_with_usable_cwd_is_unavailable() {
        let bash = scripted_bash(
            vec!["/no-such-dsh-sandbox-runner".into(), "true".into()],
            vec!["read-only file system".into()],
            dsh_sandbox::bwrap_runner_failure_rules(),
        );
        let err = bash
            .run(resolve_shell(ShellRequest {
                command: "true".into(),
                cwd: Some("/tmp".into()),
                ..ShellRequest::default()
            }))
            .await
            .unwrap_err();
        match err {
            ShellError::Unavailable(message) => {
                assert!(message.contains("Runner failure:"));
            }
            other => panic!("expected Unavailable, got {other}"),
        }
    }

    #[tokio::test]
    async fn runner_failure_wins_over_denial_signatures() {
        let bash = scripted_bash(
            vec![
                "sh".into(),
                "-c".into(),
                "echo \"bwrap: cannot mount tmpfs\" >&2; echo \"touch: Read-only file system\" >&2; exit 1".into(),
            ],
            vec!["read-only file system".into()],
            dsh_sandbox::bwrap_runner_failure_rules(),
        );
        let err = bash
            .run(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap_err();
        match err {
            ShellError::Unavailable(message) => {
                assert!(message.contains("Runner failure: bwrap: cannot mount tmpfs"));
            }
            other => panic!("expected Unavailable, got {other}"),
        }
        let child = bash
            .start(resolve_shell(ShellRequest {
                command: "true".into(),
                ..ShellRequest::default()
            }))
            .unwrap();
        let _ = child.wait().unwrap();
        let facts = child.sandbox_info().expect("background stamps");
        assert_eq!(facts.runner_failed, Some(true));
        assert!(!facts.denied);
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
