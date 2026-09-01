//! Local cwd-rooted sandbox backend, plus Linux process wrapping (bwrap, then landlock-run).

use dsh_cordis::Context;
use dsh_sandbox::{
    bwrap_runner_failure_rules, landlock_runner_failure_rules, windows_acl_denial_signatures,
    windows_acl_runner_failure_rules, ConfinedArgv, ProcessConfiner, SandboxEnforcement,
    SandboxError, SandboxExecutionPolicy, SandboxMode, SandboxPolicy, SandboxRuntime,
};
use dsh_sandbox_windows_acl::{
    windows_acl_runner_argv, WindowsAclRunnerError, WindowsAclRunnerInvocation,
    WindowsAclRunnerRequest,
};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-sandbox-local"
}

/// Lexically normalize `.` / `..` without touching the filesystem.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Whether `resolved` is `/etc` or a descendant — host-secret escape.
fn is_etc_escape(resolved: &Path) -> bool {
    resolved.starts_with(Path::new("/etc"))
}

/// Deny paths that escape `root` via `..` or land under `/etc`.
pub fn allow_path(root: &str, path: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let root_norm = normalize_lexical(Path::new(root));
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_norm.join(candidate)
    };
    let resolved = normalize_lexical(&joined);
    if is_etc_escape(&resolved) {
        return false;
    }
    resolved == root_norm || resolved.starts_with(&root_norm)
}

/// Workspace-rooted policy used by the local sandbox provider.
pub struct CwdSandbox {
    root: String,
}

impl CwdSandbox {
    /// Bind the policy to an explicit workspace root.
    pub fn new(root: impl Into<String>) -> Self {
        let root = root.into();
        if root.is_empty() {
            panic!("CwdSandbox: root must be a non-empty path");
        }
        Self { root }
    }

    /// Configured workspace root.
    pub fn root(&self) -> &str {
        &self.root
    }
}

impl SandboxPolicy for CwdSandbox {
    fn allow_path(&self, path: &str) -> bool {
        allow_path(&self.root, path)
    }
}

/// bwrap profile arguments for one file-effect policy (before `--` and the command).
pub fn bwrap_profile_args(policy: &SandboxExecutionPolicy) -> Vec<String> {
    let mut args = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--unshare-pid".into(),
        "--proc".into(),
        "/proc".into(),
        "--die-with-parent".into(),
    ];
    if policy.mode == SandboxMode::WorkspaceWrite {
        args.extend([
            "--tmpfs".into(),
            "/tmp".into(),
            "--bind".into(),
            policy.workspace_root.clone(),
            policy.workspace_root.clone(),
        ]);
    }
    args
}

/// landlock-run grant arguments for one file-effect policy (before `--` and the command).
pub fn landlock_grant_args(policy: &SandboxExecutionPolicy) -> Vec<String> {
    let mut args = vec!["--ro".into(), "/".into(), "--rw".into(), "/dev/null".into()];
    if policy.mode == SandboxMode::WorkspaceWrite {
        args.extend([
            "--rw".into(),
            "/tmp".into(),
            "--rw".into(),
            policy.workspace_root.clone(),
        ]);
    }
    args
}

/// Selected Linux runner. Probed once per process.
#[derive(Clone)]
enum LinuxRunner {
    Bwrap,
    Landlock { path: PathBuf },
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn probe_bwrap() -> bool {
    let profile = bwrap_profile_args(&SandboxExecutionPolicy {
        mode: SandboxMode::ReadOnly,
        workspace_root: "/".into(),
    });
    Command::new("bwrap")
        .args(&profile)
        .arg("--")
        .arg("true")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn probe_landlock(path: &Path) -> bool {
    Command::new(path)
        .arg("--probe")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn select_linux_runner() -> Option<LinuxRunner> {
    static SELECTED: OnceLock<Option<LinuxRunner>> = OnceLock::new();
    SELECTED
        .get_or_init(|| {
            if find_on_path("bwrap").is_some() && probe_bwrap() {
                return Some(LinuxRunner::Bwrap);
            }
            let path = find_on_path("landlock-run")?;
            if probe_landlock(&path) {
                Some(LinuxRunner::Landlock { path })
            } else {
                None
            }
        })
        .clone()
}

/// Prefix `argv` with the TypeScript windows-acl Node runner.
///
/// Enforcement stays in that runner (`CreateRestrictedToken` via koffi).
/// This helper only builds the seam `ConfinedArgv` (partial enforcement,
/// the windows-acl denial dialect, exit 127 plus `windows-acl-run: `).
///
/// # Errors
/// [`WindowsAclRunnerError`] when the request violates the runner argv contract.
pub fn confine_windows_acl_node(
    invocation: &WindowsAclRunnerInvocation,
    request: &WindowsAclRunnerRequest,
) -> Result<ConfinedArgv, WindowsAclRunnerError> {
    Ok(ConfinedArgv {
        argv: windows_acl_runner_argv(invocation, request)?,
        enforcement: SandboxEnforcement::Partial,
        denial_signatures: windows_acl_denial_signatures(),
        runner_failure_rules: windows_acl_runner_failure_rules(),
    })
}

/// Linux process confiner: bwrap, then landlock-run, else fail closed.
pub struct LocalConfiner;

impl ProcessConfiner for LocalConfiner {
    fn confine(
        &self,
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        if matches!(policy.mode, SandboxMode::DangerFullAccess) {
            return Ok(ConfinedArgv {
                argv: argv.to_vec(),
                enforcement: SandboxEnforcement::Full,
                denial_signatures: Vec::new(),
                runner_failure_rules: Vec::new(),
            });
        }
        match select_linux_runner() {
            Some(LinuxRunner::Bwrap) => {
                let mut wrapped = vec!["bwrap".into()];
                wrapped.extend(bwrap_profile_args(policy));
                wrapped.push("--".into());
                wrapped.extend(argv.iter().cloned());
                Ok(ConfinedArgv {
                    argv: wrapped,
                    enforcement: SandboxEnforcement::Full,
                    denial_signatures: vec!["read-only file system".into()],
                    runner_failure_rules: bwrap_runner_failure_rules(),
                })
            }
            Some(LinuxRunner::Landlock { path }) => {
                let mut wrapped = vec![path.display().to_string()];
                wrapped.extend(landlock_grant_args(policy));
                wrapped.push("--".into());
                wrapped.extend(argv.iter().cloned());
                Ok(ConfinedArgv {
                    argv: wrapped,
                    enforcement: SandboxEnforcement::Partial,
                    denial_signatures: vec!["permission denied".into()],
                    runner_failure_rules: landlock_runner_failure_rules(),
                })
            }
            None => Err(SandboxError::unavailable(policy.mode.as_str(), None)),
        }
    }
}

/// Provide `ctx.sandbox` as a cwd-rooted [`CwdSandbox`] plus a Linux process confiner.
pub fn install(ctx: &Context, root: impl Into<String>) -> dsh_cordis::Result<Arc<SandboxRuntime>> {
    let runtime = Arc::new(
        SandboxRuntime::new(Arc::new(CwdSandbox::new(root))).with_confiner(Arc::new(LocalConfiner)),
    );
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn denies_absolute_escape() {
        assert!(!allow_path("/tmp/workspace", "/etc/passwd"));
    }

    #[test]
    fn denies_parent_escape() {
        assert!(!allow_path("/tmp/workspace", "../etc/passwd"));
        assert!(!allow_path("/tmp/workspace", "foo/../../etc/passwd"));
        assert!(!allow_path(
            "/tmp/workspace",
            "/tmp/workspace/../etc/passwd"
        ));
    }

    #[test]
    fn allows_relative_child() {
        assert!(allow_path("/tmp/workspace", "ok.txt"));
        assert!(allow_path("/tmp/workspace", "foo/../ok.txt"));
    }

    #[test]
    fn cwd_sandbox_matches_allow_path() {
        let sandbox = CwdSandbox::new("/tmp/workspace");
        assert!(sandbox.allow_path("ok.txt"));
        assert!(!sandbox.allow_path("/etc/passwd"));
        assert!(!sandbox.allow_path(".."));
    }

    #[test]
    fn install_provides_sandbox() {
        let ctx = Context::new();
        install(&ctx, "/tmp/workspace").unwrap();
        assert!(ctx.has_service("sandbox"));
        let runtime = ctx.service::<SandboxRuntime>().unwrap();
        assert!(runtime.allow_path("ok.txt"));
        assert!(!runtime.allow_path("/etc/passwd"));
        ctx.dispose();
        assert!(!ctx.has_service("sandbox"));
    }

    #[test]
    fn seam_is_present() {
        assert_eq!(name(), "dsh-sandbox-local");
    }

    #[test]
    fn bwrap_profile_read_only_has_no_bind() {
        let args = bwrap_profile_args(&SandboxExecutionPolicy {
            mode: SandboxMode::ReadOnly,
            workspace_root: "/tmp/ws".into(),
        });
        assert!(!args.iter().any(|part| part == "--bind"));
        assert!(args.windows(2).any(|pair| pair == ["--ro-bind", "/"]));
    }

    #[test]
    fn bwrap_profile_workspace_write_binds_root() {
        let args = bwrap_profile_args(&SandboxExecutionPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: "/tmp/ws".into(),
        });
        assert!(args
            .windows(3)
            .any(|pair| pair == ["--bind", "/tmp/ws", "/tmp/ws"]));
        assert!(args.windows(2).any(|pair| pair == ["--tmpfs", "/tmp"]));
    }

    #[test]
    fn landlock_grants_workspace_write() {
        let args = landlock_grant_args(&SandboxExecutionPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: "/tmp/ws".into(),
        });
        assert_eq!(
            args,
            [
                "--ro",
                "/",
                "--rw",
                "/dev/null",
                "--rw",
                "/tmp",
                "--rw",
                "/tmp/ws"
            ]
        );
    }

    #[test]
    fn danger_full_access_reports_empty_denial_signatures() {
        let confined = LocalConfiner
            .confine(
                &["true".into()],
                &SandboxExecutionPolicy {
                    mode: SandboxMode::DangerFullAccess,
                    workspace_root: "/tmp".into(),
                },
            )
            .unwrap();
        assert!(confined.denial_signatures.is_empty());
        assert!(confined.runner_failure_rules.is_empty());
        assert_eq!(confined.argv, ["true"]);
    }

    #[test]
    fn confine_reports_backend_denial_signatures() {
        let Some(runner) = select_linux_runner() else {
            return;
        };
        let confined = LocalConfiner
            .confine(
                &["true".into()],
                &SandboxExecutionPolicy {
                    mode: SandboxMode::ReadOnly,
                    workspace_root: "/tmp".into(),
                },
            )
            .unwrap();
        match runner {
            LinuxRunner::Bwrap => {
                assert_eq!(confined.denial_signatures, ["read-only file system"]);
                assert_eq!(confined.enforcement, SandboxEnforcement::Full);
                assert_eq!(
                    confined.runner_failure_rules,
                    dsh_sandbox::bwrap_runner_failure_rules()
                );
            }
            LinuxRunner::Landlock { .. } => {
                assert_eq!(confined.denial_signatures, ["permission denied"]);
                assert_eq!(confined.enforcement, SandboxEnforcement::Partial);
                assert_eq!(
                    confined.runner_failure_rules,
                    dsh_sandbox::landlock_runner_failure_rules()
                );
            }
        }
    }

    #[test]
    fn windows_acl_node_delegation_is_partial_with_the_typescript_dialect() {
        let workspace = r"C:\Users\agent\repo";
        let temp = r"C:\Users\agent\AppData\Local\Temp\dsh-abc123";
        let confined = confine_windows_acl_node(
            &dsh_sandbox_windows_acl::WindowsAclRunnerInvocation {
                node: std::path::PathBuf::from("node"),
                entry: dsh_sandbox_windows_acl::WindowsAclRunnerEntry::BuiltJs(
                    std::path::PathBuf::from("runner.js"),
                ),
            },
            &dsh_sandbox_windows_acl::WindowsAclRunnerRequest {
                workspace: workspace.into(),
                temp: temp.into(),
                mode: dsh_sandbox_windows_acl::WindowsAclRunnerMode::WorkspaceWrite,
                write_sid: Some(dsh_sandbox_windows_acl::workspace_write_sid(workspace)),
                temp_write_sid: Some(dsh_sandbox_windows_acl::temp_write_sid(temp)),
                command: vec!["pwsh.exe".into(), "-Command".into(), "echo hi".into()],
            },
        )
        .unwrap();
        assert_eq!(confined.enforcement, SandboxEnforcement::Partial);
        assert_eq!(
            confined.denial_signatures,
            dsh_sandbox::windows_acl_denial_signatures()
        );
        assert_eq!(
            confined.runner_failure_rules,
            dsh_sandbox::windows_acl_runner_failure_rules()
        );
        assert_eq!(confined.argv[0], "node");
        assert_eq!(confined.argv[1], "runner.js");
        assert!(confined
            .argv
            .windows(2)
            .any(|pair| pair == ["--mode", "workspace-write"]));
        assert_eq!(confined.argv.last().map(String::as_str), Some("echo hi"));
    }

    #[test]
    fn confine_without_runner_fails_closed() {
        if select_linux_runner().is_some() {
            return;
        }
        let err = LocalConfiner
            .confine(
                &["true".into()],
                &SandboxExecutionPolicy {
                    mode: SandboxMode::ReadOnly,
                    workspace_root: "/tmp".into(),
                },
            )
            .unwrap_err();
        match err {
            SandboxError::Unavailable { mode, message } => {
                assert_eq!(mode, "read-only");
                assert!(message.contains("refusing to run the command unconfined"));
            }
        }
    }
}
