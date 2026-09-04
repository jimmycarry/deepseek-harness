//! Local bash provider. Spawns `/bin/bash -lc` (or an explicit confined argv).

use async_trait::async_trait;
use dsh_shell::{
    CollectedOutput, ShellChild, ShellChildExit, ShellError, ShellExecutor, ShellRequest,
    ShellRunResult, ShellSandboxInfo, ShellSpec,
};
use dsh_subprocess::SubprocessRuntime;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default foreground timeout (TypeScript `bash-local` Config).
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Upper bound for per-call timeout overrides.
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// Model-friendly environment overrides (TypeScript `ENV_OVERRIDES`).
const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

/// Bash via `/bin/bash -lc`.
pub struct BashLocal {
    /// Required by the TypeScript constructor (`inject: ['subprocess']`).
    #[allow(dead_code)]
    subprocess: Arc<SubprocessRuntime>,
}

impl BashLocal {
    /// Bind to the subprocess seam. Foreground `run` still uses that seam for
    /// unconfined argv that callers spawn through [`Self::run_argv`].
    pub fn new(subprocess: Arc<SubprocessRuntime>) -> Self {
        Self { subprocess }
    }

    /// Run an explicit argv with this executor's timeout and environment.
    pub async fn run_argv(
        &self,
        spec: ShellSpec,
        argv: &[String],
    ) -> Result<ShellRunResult, ShellError> {
        let timeout_ms = spec.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let child = spawn_argv(argv, &spec)?;
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

    /// Start an explicit argv with no executor timeout.
    pub fn start_argv(
        &self,
        spec: &ShellSpec,
        argv: &[String],
    ) -> Result<Box<dyn ShellChild>, ShellError> {
        Ok(Box::new(spawn_argv(argv, spec)?))
    }
}

#[async_trait]
impl ShellExecutor for BashLocal {
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
        let argv = bash_login_argv(&spec.command);
        self.run_argv(spec, &argv).await
    }

    fn start(&self, spec: ShellSpec) -> Result<Box<dyn ShellChild>, ShellError> {
        let argv = bash_login_argv(&spec.command);
        self.start_argv(&spec, &argv)
    }
}

fn bash_login_argv(command: &str) -> Vec<String> {
    vec!["/bin/bash".into(), "-lc".into(), command.to_string()]
}

/// Project a settled local child into a foreground run result.
pub fn result_from_exit(
    child: &LocalShellChild,
    exit: ShellChildExit,
    timeout_ms: u64,
    timed_out: bool,
) -> ShellRunResult {
    ShellRunResult {
        exit_code: exit.exit_code,
        signal: exit.signal,
        timed_out,
        aborted: false,
        timeout_ms,
        stdout: CollectedOutput {
            text: child.collected_stdout(),
            truncated: false,
            spill_path: None,
        },
        stderr: CollectedOutput {
            text: child.collected_stderr(),
            truncated: false,
            spill_path: None,
        },
        sandbox: None,
    }
}

/// Spawn `argv` under `spec`'s cwd and the bash model-friendly environment.
pub fn spawn_argv(argv: &[String], spec: &ShellSpec) -> Result<LocalShellChild, ShellError> {
    spawn_argv_with_env(argv, spec, ENV_OVERRIDES)
}

/// Spawn `argv` with an explicit environment-override table (pwsh omits `TERM=dumb`).
pub fn spawn_argv_with_env(
    argv: &[String],
    spec: &ShellSpec,
    extra_env: &[(&str, &str)],
) -> Result<LocalShellChild, ShellError> {
    let program = argv
        .first()
        .ok_or_else(|| ShellError::Failed("sandbox confine returned an empty argv".into()))?
        .clone();
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    apply_env_overrides(&mut cmd, spec, extra_env);
    if let Some(extra) = &spec.extra_env {
        for (key, value) in extra {
            cmd.env(key, value);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|error| ShellError::from_spawn(program, error))?;
    if let Some(stdin_text) = &spec.stdin {
        if let Some(mut pipe) = child.stdin.take() {
            use std::io::Write;
            let _ = pipe.write_all(stdin_text.as_bytes());
        }
    }
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout = Arc::new(Mutex::new(String::new()));
    let stderr = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    if let Some(pipe) = stdout_pipe {
        readers.push(spawn_reader(pipe, Arc::clone(&stdout)));
    }
    if let Some(pipe) = stderr_pipe {
        readers.push(spawn_reader(pipe, Arc::clone(&stderr)));
    }
    Ok(LocalShellChild {
        inner: Arc::new(LocalShellChildInner {
            child: Mutex::new(Some(child)),
            readers: Mutex::new(readers),
            stdout,
            stderr,
            stdout_offset: Mutex::new(0),
            stderr_offset: Mutex::new(0),
            done: Mutex::new(None),
        }),
    })
}

fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    dest: Arc<Mutex<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => dest
                    .lock()
                    .expect("shell stream")
                    .push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => break,
            }
        }
    })
}

/// Apply model-friendly environment overrides plus an optional trusted overlay.
pub fn apply_env_overrides(command: &mut Command, spec: &ShellSpec, extra_env: &[(&str, &str)]) {
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let Some(overlay) = &spec.dsh_env else {
        return;
    };
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("DSH_"))
        .collect();
    for (key, value) in extra_env {
        env.insert((*key).into(), (*value).into());
    }
    env.extend(
        overlay
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    command.env_clear();
    command.envs(env);
}

/// Live local process handle. Clone shares the same process.
#[derive(Clone)]
pub struct LocalShellChild {
    inner: Arc<LocalShellChildInner>,
}

struct LocalShellChildInner {
    child: Mutex<Option<std::process::Child>>,
    readers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    stdout_offset: Mutex<usize>,
    stderr_offset: Mutex<usize>,
    done: Mutex<Option<Result<ShellChildExit, ShellError>>>,
}

impl LocalShellChild {
    /// Full collected stdout.
    pub fn collected_stdout(&self) -> String {
        self.inner.stdout.lock().expect("stdout").clone()
    }

    fn join_readers(&self) {
        for handle in self.inner.readers.lock().expect("readers").drain(..) {
            let _ = handle.join();
        }
    }
}

impl ShellChild for LocalShellChild {
    fn cancel(&self) {
        if let Some(child) = self.inner.child.lock().expect("child").as_mut() {
            let _ = child.kill();
        }
    }

    fn wait(&self) -> Result<ShellChildExit, ShellError> {
        loop {
            {
                let done = self.inner.done.lock().expect("done");
                if let Some(result) = done.as_ref() {
                    return result.clone();
                }
            }
            let status = {
                let mut slot = self.inner.child.lock().expect("child");
                match slot.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            let _ = slot.take();
                            Some(Ok(status))
                        }
                        Ok(None) => None,
                        Err(error) => Some(Err(ShellError::Failed(error.to_string()))),
                    },
                    None => {
                        drop(slot);
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
            };
            match status {
                None => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Some(Err(error)) => {
                    *self.inner.done.lock().expect("done") = Some(Err(error.clone()));
                    return Err(error);
                }
                Some(Ok(status)) => {
                    self.join_readers();
                    let exit = exit_from_status(status);
                    *self.inner.done.lock().expect("done") = Some(Ok(exit.clone()));
                    return Ok(exit);
                }
            }
        }
    }

    fn read_output(&self) -> String {
        let stdout = self.inner.stdout.lock().expect("stdout");
        let stderr = self.inner.stderr.lock().expect("stderr");
        let mut stdout_offset = self.inner.stdout_offset.lock().expect("stdout offset");
        let mut stderr_offset = self.inner.stderr_offset.lock().expect("stderr offset");
        let out = stdout.get(*stdout_offset..).unwrap_or("").to_string();
        let err = stderr.get(*stderr_offset..).unwrap_or("").to_string();
        *stdout_offset = stdout.len();
        *stderr_offset = stderr.len();
        drop(stdout);
        drop(stderr);
        let mut delta = out;
        if !err.is_empty() {
            if !delta.is_empty() && !delta.ends_with('\n') {
                delta.push('\n');
            }
            delta.push_str("[stderr]\n");
            delta.push_str(&err);
        }
        delta
    }

    fn collected_stderr(&self) -> String {
        self.inner.stderr.lock().expect("stderr").clone()
    }

    fn sandbox_info(&self) -> Option<ShellSandboxInfo> {
        None
    }
}

fn exit_from_status(status: ExitStatus) -> ShellChildExit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return ShellChildExit {
                exit_code: None,
                signal: Some(signal_name(sig)),
                killed: true,
            };
        }
    }
    ShellChildExit {
        exit_code: status.code(),
        signal: None,
        killed: false,
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_shell::ShellRequest;
    use dsh_subprocess_local::LocalSubprocess;

    fn bash() -> BashLocal {
        BashLocal::new(Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess))))
    }

    #[tokio::test]
    async fn echo_is_a_successful_result() {
        let result = bash()
            .run(dsh_shell::resolve(ShellRequest {
                command: "echo hello".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.text.trim(), "hello");
        assert!(result.stderr.text.is_empty());
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_not_errored() {
        let result = bash()
            .run(dsh_shell::resolve(ShellRequest {
                command: "false".into(),
                ..ShellRequest::default()
            }))
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(1));
        assert!(result.signal.is_none());
    }

    #[tokio::test]
    async fn timeout_kills_and_flags_timed_out() {
        let spec = ShellSpec {
            command: "sleep 5".into(),
            timeout_ms: Some(200),
            ..ShellSpec::default()
        };
        let result = bash().run(spec).await.unwrap();
        assert!(result.timed_out, "{result:?}");
        assert_eq!(result.timeout_ms, 200);
    }

    #[test]
    fn resolve_fills_the_default_timeout() {
        let spec = bash().resolve(ShellRequest {
            command: "true".into(),
            ..ShellRequest::default()
        });
        assert_eq!(spec.timeout_ms, Some(DEFAULT_TIMEOUT_MS));
    }
}
