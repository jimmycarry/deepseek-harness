//! Model-facing bash tool. Depends on the shell Service Definition only;
//! background starts go through `ctx.jobs` when that registry is mounted.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_jobs::{JobHooks, JobOutcome, JobRegistry, JobStart, JobStatus};
use dsh_sandbox_policy::resolve_from_context;
use dsh_shell::{resolve, ShellRequest, ShellRuntime};
use dsh_shell_env::ShellEnvRegistry;
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Deployment-varying background switch.
#[derive(Debug, Clone)]
pub struct Config {
    /// Expose `run_in_background` (default true); disabled calls are rejected.
    pub enable_run_in_background: bool,
}

impl Config {
    /// Validate raw cordis.yml config. Omission defaults to true.
    ///
    /// # Errors
    /// Non-boolean `enableRunInBackground`.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let enable = match config.and_then(|value| value.get("enableRunInBackground")) {
            None => true,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "tool-bash: enableRunInBackground must be a boolean".to_string())?,
        };
        Ok(Self {
            enable_run_in_background: enable,
        })
    }
}

/// `bash` tool.
pub struct BashTool {
    shell: Arc<ShellRuntime>,
    jobs: Option<Arc<JobRegistry>>,
    enable_run_in_background: bool,
    shell_env: Option<Arc<ShellEnvRegistry>>,
    lookup: Option<Context>,
}

impl BashTool {
    /// Bind to `ctx.shell` without jobs (foreground only).
    pub fn new(shell: Arc<ShellRuntime>) -> Self {
        Self {
            shell,
            jobs: None,
            enable_run_in_background: true,
            shell_env: None,
            lookup: None,
        }
    }

    /// Bind to shell and an optional job registry.
    pub fn with_jobs(
        shell: Arc<ShellRuntime>,
        jobs: Option<Arc<JobRegistry>>,
        enable_run_in_background: bool,
    ) -> Self {
        Self {
            shell,
            jobs,
            enable_run_in_background,
            shell_env: None,
            lookup: None,
        }
    }

    /// Collect trusted `DSH_*` facts for each model bash call.
    pub fn with_shell_env(mut self, shell_env: Arc<ShellEnvRegistry>) -> Self {
        self.shell_env = Some(shell_env);
        self
    }

    /// Bind the plugin context used to resolve per-call sandbox policy.
    pub fn with_context(mut self, ctx: Context) -> Self {
        self.lookup = Some(ctx);
        self
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        if self.enable_run_in_background {
            "Run a bash command in the workspace. Set `run_in_background: true` for long-running commands: the call returns a job id immediately; read its output with `job_output` and stop it with `job_kill`."
        } else {
            "Run a bash command in the workspace."
        }
    }

    fn parameters(&self) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert("command".into(), json!({ "type": "string" }));
        if self.enable_run_in_background {
            properties.insert(
                "run_in_background".into(),
                json!({
                    "type": "boolean",
                    "description": "Run in the background and return a job id immediately (collect with job_output, stop with job_kill). No timeout applies."
                }),
            );
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let command = call
            .args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("command required".into()))?;
        if call.args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            if !self.enable_run_in_background {
                return Ok(ToolOutcome::error(
                    "Error: run_in_background is disabled for this deployment (enableRunInBackground: false)",
                ));
            }
            let Some(jobs) = &self.jobs else {
                return Ok(ToolOutcome::error(
                    "Error: background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs",
                ));
            };
            let command = command.to_string();
            let owner = call.agent_id.clone();
            let dsh_env = collect_dsh_env(self.shell_env.as_deref(), call.agent_id.as_deref())?;
            match jobs.start(JobStart {
                kind: "bash".into(),
                label: command.clone(),
                output_limit_bytes: None,
                owner_session: owner,
                run: Box::new(move || spawn_bash(command, None, dsh_env)),
            }) {
                Ok(id) => Ok(ToolOutcome::text(format!("started background job {id}"))),
                Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
            }
        } else {
            let spec = resolve(ShellRequest {
                command: command.into(),
                cwd: None,
                dsh_env: collect_dsh_env(self.shell_env.as_deref(), call.agent_id.as_deref())?,
                sandbox_policy: self
                    .lookup
                    .as_ref()
                    .and_then(|ctx| resolve_from_context(ctx, call.agent_id.as_deref())),
            });
            match self.shell.run(spec).await {
                Ok(stdout) => Ok(ToolOutcome::text(stdout)),
                Err(error) => Ok(ToolOutcome::error(error.to_string())),
            }
        }
    }
}

fn collect_dsh_env(
    registry: Option<&ShellEnvRegistry>,
    session_id: Option<&str>,
) -> Result<Option<BTreeMap<String, String>>, ToolError> {
    let Some(registry) = registry else {
        return Ok(None);
    };
    registry
        .collect(session_id)
        .map(Some)
        .map_err(|error| ToolError::Body(error.to_string()))
}

fn apply_dsh_env(command: &mut Command, dsh_env: &Option<BTreeMap<String, String>>) {
    let Some(overlay) = dsh_env else {
        return;
    };
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("DSH_"))
        .collect();
    env.extend(overlay.iter().map(|(key, value)| (key.clone(), value.clone())));
    command.env_clear();
    command.envs(env);
}

fn spawn_bash(
    command: String,
    cwd: Option<String>,
    dsh_env: Option<BTreeMap<String, String>>,
) -> Result<JobHooks, String> {
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-lc")
        .arg(&command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    apply_dsh_env(&mut cmd, &dsh_env);
    let mut child = cmd.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let unread = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    let unread_out = Arc::clone(&unread);
    if let Some(mut pipe) = stdout {
        readers.push(std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            unread_out.lock().expect("bash out").push_str(&buf);
        }));
    }
    let unread_err = Arc::clone(&unread);
    if let Some(mut pipe) = stderr {
        readers.push(std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            if !buf.is_empty() {
                let mut guard = unread_err.lock().expect("bash err");
                if !guard.is_empty() && !guard.ends_with('\n') {
                    guard.push('\n');
                }
                guard.push_str(&buf);
            }
        }));
    }
    let child = Arc::new(Mutex::new(Some(child)));
    let wait_child = Arc::clone(&child);
    let cancel_child = Arc::clone(&child);
    let read_unread = Arc::clone(&unread);
    let readers = Arc::new(Mutex::new(readers));
    let wait_readers = Arc::clone(&readers);
    Ok(JobHooks {
        cancel: Arc::new(move |_| {
            if let Some(child) = cancel_child.lock().expect("bash child").as_mut() {
                let _ = child.kill();
            }
        }),
        wait_done: Arc::new(move || {
            let status = wait_child
                .lock()
                .expect("bash child")
                .take()
                .and_then(|mut child| child.wait().ok());
            for handle in wait_readers.lock().expect("bash readers").drain(..) {
                let _ = handle.join();
            }
            let killed = status
                .as_ref()
                .is_some_and(|status| status.code().is_none());
            let code = status.and_then(|status| status.code()).unwrap_or(0);
            if killed {
                JobOutcome {
                    status: JobStatus::Killed,
                    detail: Some("killed before exit".into()),
                    output: None,
                }
            } else {
                JobOutcome {
                    status: JobStatus::Completed,
                    detail: Some(format!("exit code: {code}")),
                    output: None,
                }
            }
        }),
        read_output: Some(Arc::new(move || {
            std::mem::take(&mut *read_unread.lock().expect("bash out"))
        })),
    })
}

pub fn name() -> &'static str {
    "dsh-tool-bash"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_background_on() {
        assert!(Config::resolve(None).unwrap().enable_run_in_background);
    }
}
