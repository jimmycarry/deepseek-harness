//! Local PowerShell provider. Each command runs as
//! `pwsh -NoLogo -NoProfile -NonInteractive -Command <command>`.
//! The command string is one argv element: there is no bash-style quoting layer.

use async_trait::async_trait;
use dsh_bash_local::{
    result_from_exit, spawn_argv_with_env, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS,
};
use dsh_shell::{
    ShellChild, ShellError, ShellExecutor, ShellRequest, ShellRunResult, ShellSpec,
};
use dsh_subprocess::SubprocessRuntime;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-pwsh-local"
}

/// Model-friendly environment overrides. `TERM=dumb` is a POSIX concept and is
/// deliberately absent; `NO_COLOR` is honored by modern pwsh renderers.
pub const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

/// UTF-8 output pinning prepended to every command.
pub const ENCODING_PREAMBLE: &str =
    "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [System.Text.UTF8Encoding]::new($false); ";

/// Resolve the pwsh executable. An explicit `configured` path is trusted as-is.
/// On Windows, well-known install locations are probed; elsewhere the result is
/// the bare `pwsh` name for PATH resolution.
pub fn resolve_pwsh_path(configured: Option<&str>) -> String {
    if let Some(path) = configured.filter(|path| !path.is_empty()) {
        return path.to_string();
    }
    if cfg!(windows) {
        for candidate in candidate_pwsh_paths() {
            if candidate_exists(&candidate) {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "pwsh".into()
}

/// Well-known Windows PowerShell install locations plus PATH `pwsh.exe` entries.
pub fn candidate_pwsh_paths() -> Vec<PathBuf> {
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut candidates = vec![PathBuf::from(program_files)
        .join("PowerShell")
        .join("7")
        .join("pwsh.exe")];
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            candidates.push(entry.join("pwsh.exe"));
        }
    }
    candidates.push(
        PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    );
    candidates
}

fn candidate_exists(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.is_file() || meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Whether a `pwsh` (or configured) executable is on PATH.
pub fn pwsh_on_path() -> bool {
    let name = resolve_pwsh_path(None);
    if Path::new(&name).is_file() {
        return true;
    }
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                let candidate = dir.join(&name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// Local PowerShell executor over the same process handle as bash-local.
pub struct PwshLocal {
    /// Required by the TypeScript constructor (`inject: ['subprocess']`).
    #[allow(dead_code)]
    subprocess: Arc<SubprocessRuntime>,
    pwsh_path: String,
}

impl PwshLocal {
    /// Bind to the subprocess seam. `pwsh_path` is resolved from config.
    pub fn new(subprocess: Arc<SubprocessRuntime>, pwsh_path: Option<&str>) -> Self {
        Self {
            subprocess,
            pwsh_path: resolve_pwsh_path(pwsh_path),
        }
    }

    /// The pwsh executable every command runs through.
    pub fn pwsh_path(&self) -> &str {
        &self.pwsh_path
    }

    /// Exact argv a confining subclass wraps through `ctx.sandbox.confine`.
    pub fn argv(&self, spec: &ShellSpec) -> Vec<String> {
        vec![
            self.pwsh_path.clone(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("{}{}", ENCODING_PREAMBLE, spec.command),
        ]
    }

    /// Foreground run of an exact argv.
    pub async fn run_argv(
        &self,
        spec: ShellSpec,
        argv: &[String],
    ) -> Result<ShellRunResult, ShellError> {
        let timeout_ms = spec.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let child = spawn_argv_with_env(argv, &spec, ENV_OVERRIDES)?;
        let waiter = child.clone();
        let joined = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            tokio::task::spawn_blocking(move || waiter.wait()),
        )
        .await;
        match joined {
            Ok(Ok(result)) => {
                let exit = result?;
                Ok(result_from_exit(&child, exit, timeout_ms, false))
            }
            Ok(Err(panic)) => Err(ShellError::Failed(panic.to_string())),
            Err(_elapsed) => {
                child.cancel();
                let waiter = child.clone();
                let exit = tokio::task::spawn_blocking(move || waiter.wait())
                    .await
                    .map_err(|error| ShellError::Failed(error.to_string()))??;
                Ok(result_from_exit(&child, exit, timeout_ms, true))
            }
        }
    }

    /// Background start of an exact argv.
    pub fn start_argv(
        &self,
        spec: &ShellSpec,
        argv: &[String],
    ) -> Result<Box<dyn ShellChild>, ShellError> {
        Ok(Box::new(spawn_argv_with_env(argv, spec, ENV_OVERRIDES)?))
    }
}

#[async_trait]
impl ShellExecutor for PwshLocal {
    fn resolve(&self, request: ShellRequest) -> ShellSpec {
        let mut spec = dsh_shell::resolve(request);
        let timeout = spec
            .timeout_ms
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        spec.timeout_ms = Some(timeout.min(MAX_TIMEOUT_MS));
        spec
    }

    async fn run(&self, spec: ShellSpec) -> Result<ShellRunResult, ShellError> {
        let argv = self.argv(&spec);
        self.run_argv(spec, &argv).await
    }

    fn start(&self, spec: ShellSpec) -> Result<Box<dyn ShellChild>, ShellError> {
        let argv = self.argv(&spec);
        self.start_argv(&spec, &argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_subprocess_local::LocalSubprocess;

    fn pwsh() -> PwshLocal {
        PwshLocal::new(
            Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess))),
            None,
        )
    }

    #[test]
    fn resolve_without_config_is_bare_pwsh_on_unix() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(resolve_pwsh_path(None), "pwsh");
        assert_eq!(resolve_pwsh_path(Some("/opt/pwsh")), "/opt/pwsh");
    }

    #[test]
    fn argv_passes_the_command_as_one_element() {
        let argv = pwsh().argv(&ShellSpec {
            command: "Get-Date; Write-Output 'a b'".into(),
            ..ShellSpec::default()
        });
        assert_eq!(argv[1], "-NoLogo");
        assert_eq!(argv[2], "-NoProfile");
        assert_eq!(argv[3], "-NonInteractive");
        assert_eq!(argv[4], "-Command");
        assert!(argv[5].starts_with(ENCODING_PREAMBLE));
        assert!(argv[5].ends_with("Get-Date; Write-Output 'a b'"));
        assert!(!argv.iter().any(|part| part == "-c"));
    }

    #[test]
    fn env_overrides_omit_term() {
        assert!(!ENV_OVERRIDES.iter().any(|(key, _)| *key == "TERM"));
        assert!(ENV_OVERRIDES.contains(&("NO_COLOR", "1")));
    }

    #[tokio::test]
    async fn echo_runs_when_pwsh_is_installed() {
        if !pwsh_on_path() {
            return;
        }
        let result = pwsh()
            .run(dsh_shell::resolve(ShellRequest {
                command: "Write-Output 'hello'".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.text.contains("hello"), "{result:?}");
    }
}
