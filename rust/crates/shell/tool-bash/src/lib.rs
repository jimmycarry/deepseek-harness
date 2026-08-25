//! Model-facing bash tool. Depends on the shell Service Definition only;
//! background starts go through `ctx.jobs` when that registry is mounted.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::Context;
use dsh_jobs::{JobHooks, JobOutcome, JobRegistry, JobStart, JobStatus};
use dsh_sandbox::{
    approve_escalation, escalation_audit_reason, validate_escalation_args, EscalationIngredients,
    EscalationRequest, ESCALATION_TARGETS,
};
use dsh_sandbox_policy::{resolve_from_context, SandboxPolicyService};
use dsh_session::session_id;
use dsh_shell::{resolve, DSH_ENV_PREFIX, ShellRequest, ShellRuntime};
use dsh_shell_env::ShellEnvRegistry;
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome};
use dsh_user_approval::{ApprovalRequest, ApprovalService};
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

fn bash_description(background_enabled: bool, advertises_escalation: bool) -> String {
    let background = if background_enabled {
        "Set `run_in_background: true` for long-running commands: the call returns a job id immediately; read its output with `job_output` and stop it with `job_kill`."
    } else {
        "Background execution is not available; long-running commands must finish within the timeout."
    };
    let mut base = format!(
        "Execute a bash command (`bash -c`) and return its stdout/stderr. \
Each call runs in a fresh shell: no state (cwd, variables, functions) persists between calls — \
pass `workdir` instead of using `cd`. Non-zero exits are reported as `[exit code: N]`. \
Current harness environment facts are exposed through managed `${DSH_ENV_PREFIX}*` variables; inspect them when needed. \
Commands may run under a file sandbox; a blocked file operation is reported as `[sandbox: file access denied under <mode> mode]` — a policy denial, not a bug in the command; do not retry another way. \
Long output is truncated to its tail; the full output is saved to a file whose path is reported when available. \
{background}"
    );
    if advertises_escalation {
        base.push_str(
            " Attempting a command the sandbox may deny is safe and expected: run it and read the \
marker rather than assuming the denial. When a command is denied and a wider mode would let it \
succeed, escalate immediately in the same turn — the one sanctioned exception to a denial: retry \
the exact same command once with `sandbox_permissions` (the narrowest wider mode that suffices) \
plus a one-sentence `justification`. Do not detour through chat to ask permission first — the \
approval prompt raised by that retry is how the user consents. If the session states approval \
prompts are disabled, there is no exception: a denial is final — do not set `sandbox_permissions`. \
Never escalate speculatively: ground the request in a real denial — normally the one this command \
just hit; escalating up front is fine only when this session already denied the same access. \
A rejected escalation is final for that command — stop and explain, never work around \
it — but it does not forbid attempting or escalating other commands later.",
        );
    }
    base
}

/// Fail loud when a confining executor is mounted without `ctx.sandboxPolicy`.
///
/// # Errors
/// The TypeScript load-failure sentence.
pub fn require_confining_policy(ctx: &Context, shell: &ShellRuntime) -> Result<(), String> {
    if shell.sandbox_mode().is_some() && ctx.get::<SandboxPolicyService>().is_none() {
        return Err(
            "tool-bash: the mounted bash executor confines but ctx.sandboxPolicy is missing".into(),
        );
    }
    Ok(())
}

/// `bash` tool.
pub struct BashTool {
    shell: Arc<ShellRuntime>,
    jobs: Option<Arc<JobRegistry>>,
    enable_run_in_background: bool,
    shell_env: Option<Arc<ShellEnvRegistry>>,
    lookup: Option<Context>,
    description: String,
}

impl BashTool {
    /// Bind to `ctx.shell` without jobs (foreground only).
    pub fn new(shell: Arc<ShellRuntime>) -> Self {
        let description = bash_description(true, shell.sandbox_mode().is_some());
        Self {
            shell,
            jobs: None,
            enable_run_in_background: true,
            shell_env: None,
            lookup: None,
            description,
        }
    }

    /// Bind to shell and an optional job registry.
    pub fn with_jobs(
        shell: Arc<ShellRuntime>,
        jobs: Option<Arc<JobRegistry>>,
        enable_run_in_background: bool,
    ) -> Self {
        let description = bash_description(enable_run_in_background, shell.sandbox_mode().is_some());
        Self {
            shell,
            jobs,
            enable_run_in_background,
            shell_env: None,
            lookup: None,
            description,
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
        self.description =
            bash_description(self.enable_run_in_background, self.advertises_escalation());
        self
    }

    fn advertises_escalation(&self) -> bool {
        if let Some(ctx) = &self.lookup {
            if let Some(shell) = ctx.get::<ShellRuntime>() {
                return shell.sandbox_mode().is_some();
            }
        }
        self.shell.sandbox_mode().is_some()
    }

    async fn approve_if_requested(
        &self,
        call: &ToolCall,
        standing: Option<dsh_sandbox::SandboxExecutionPolicy>,
    ) -> Result<Option<dsh_sandbox::SandboxExecutionPolicy>, String> {
        let sandbox_permissions = call.args.get("sandbox_permissions").and_then(Value::as_str);
        let justification = call.args.get("justification").and_then(Value::as_str);
        validate_escalation_args(sandbox_permissions, justification)?;
        if sandbox_permissions.is_none() || justification.is_none() {
            return Ok(standing);
        }
        if !self.advertises_escalation() {
            return Err(
                "sandbox_permissions is not available in this composition (no sandboxing executor to escalate)"
                    .into(),
            );
        }
        let policy = standing.ok_or_else(|| {
            "tool-bash: the mounted bash executor confines but ctx.sandboxPolicy is missing"
                .to_string()
        })?;
        let Some(ctx) = &self.lookup else {
            return Err(format!(
                "sandbox escalation to \"{}\" requires approval, but no approval service is composed",
                sandbox_permissions.unwrap_or("")
            ));
        };
        let requested = sandbox_permissions.expect("paired").to_string();
        let justification = justification.expect("paired").to_string();
        let reason = escalation_audit_reason(&requested, &justification);
        let approver = ctx.get::<ApprovalService>();
        let agent = call.agent_id.as_deref().and_then(|id| {
            ctx.get::<AgentRegistry>()
                .and_then(|registry| registry.get(&session_id(id)))
        });
        let has_approver = approver.is_some();
        let has_agent = agent.is_some();
        let ctx = ctx.clone();
        let approved = approve_escalation(
            EscalationRequest {
                requested_mode: requested,
                justification,
                effective_mode: policy.mode,
                subject: "command".into(),
            },
            EscalationIngredients {
                has_approver,
                has_agent,
            },
            async move {
                let Some(approver) = approver else {
                    return Ok("unavailable".into());
                };
                let Some(agent) = agent else {
                    return Ok("unavailable".into());
                };
                approver
                    .request(
                        &ctx,
                        agent.session().as_ref(),
                        ApprovalRequest {
                            tool_name: "bash".into(),
                            call_id: None,
                            reason: Some(reason),
                        },
                    )
                    .map(|outcome| outcome.as_str().to_string())
            },
        )
        .await?;
        Ok(Some(dsh_sandbox::SandboxExecutionPolicy {
            mode: approved,
            workspace_root: policy.workspace_root,
        }))
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
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
        if self.advertises_escalation() {
            let enum_values: Vec<Value> = ESCALATION_TARGETS
                .iter()
                .map(|mode| Value::String(mode.as_str().to_string()))
                .collect();
            properties.insert(
                "sandbox_permissions".into(),
                json!({
                    "type": "string",
                    "enum": enum_values,
                    "description": "The wider sandbox mode this command needs. Only valid as a one-shot retry of a command the sandbox just denied; requires justification and user approval."
                }),
            );
            properties.insert(
                "justification".into(),
                json!({
                    "type": "string",
                    "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact command needs the wider access."
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
        let standing = self
            .lookup
            .as_ref()
            .and_then(|ctx| resolve_from_context(ctx, call.agent_id.as_deref()));
        let policy = match self.approve_if_requested(call, standing).await {
            Ok(policy) => policy,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
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
                sandbox_policy: policy,
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
    use dsh_sandbox::SandboxMode;
    use dsh_shell::{ShellError, ShellExecutor, ShellSpec};

    #[test]
    fn resolve_defaults_background_on() {
        assert!(Config::resolve(None).unwrap().enable_run_in_background);
    }

    struct RecordingBash {
        modes: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ShellExecutor for RecordingBash {
        async fn run(&self, spec: ShellSpec) -> Result<String, ShellError> {
            self.modes.lock().expect("modes").push(
                spec.sandbox_policy
                    .as_ref()
                    .map(|policy| policy.mode.as_str().to_string())
                    .unwrap_or_default(),
            );
            Ok(String::new())
        }
    }

    fn text(outcome: &ToolOutcome) -> String {
        match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn confining_without_policy_fails_loud() {
        let ctx = Context::new();
        let shell = ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))
        .with_sandbox_mode(SandboxMode::ReadOnly);
        let err = require_confining_policy(&ctx, &shell).unwrap_err();
        assert!(err.contains(
            "tool-bash: the mounted bash executor confines but ctx.sandboxPolicy is missing"
        ));
    }

    #[test]
    fn advertises_fields_and_description_under_a_confining_executor() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "read-only", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let shell = Arc::new(
            ShellRuntime::new(Arc::new(RecordingBash {
                modes: Mutex::new(Vec::new()),
            }))
            .with_sandbox_mode(SandboxMode::ReadOnly),
        );
        ctx.provide(Arc::clone(&shell)).unwrap();
        let tool = BashTool::new(shell).with_context(ctx);
        let parameters = tool.parameters();
        let props = parameters["properties"].as_object().unwrap();
        assert_eq!(
            props["sandbox_permissions"]["enum"],
            json!(["workspace-write", "danger-full-access"])
        );
        assert!(tool.description().contains("approval prompt"));
    }

    #[tokio::test]
    async fn pairing_and_unadvertised_fields_fail_closed() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "read-only", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let shell = Arc::new(
            ShellRuntime::new(Arc::new(RecordingBash {
                modes: Mutex::new(Vec::new()),
            }))
            .with_sandbox_mode(SandboxMode::ReadOnly),
        );
        ctx.provide(Arc::clone(&shell)).unwrap();
        let tool = BashTool::new(shell).with_context(ctx);
        let missing = tool
            .execute(json!({
                "command": "true",
                "sandbox_permissions": "workspace-write"
            }))
            .await
            .unwrap();
        assert!(text(&missing).contains("requires a justification"));

        let plain = BashTool::new(Arc::new(ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))));
        let unadvertised = plain
            .execute(json!({
                "command": "true",
                "sandbox_permissions": "workspace-write",
                "justification": "why"
            }))
            .await
            .unwrap();
        assert!(text(&unadvertised).contains("not available in this composition"));
    }

    #[tokio::test]
    async fn non_widening_never_runs() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "workspace-write", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        dsh_user_approval::install(&ctx, Some(&json!({ "policy": "ask" }))).unwrap();
        let backend = Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        });
        let shell = Arc::new(
            ShellRuntime::new(Arc::clone(&backend) as Arc<dyn ShellExecutor>)
                .with_sandbox_mode(SandboxMode::WorkspaceWrite),
        );
        ctx.provide(Arc::clone(&shell)).unwrap();
        let tool = BashTool::new(shell).with_context(ctx);
        let outcome = tool
            .execute(json!({
                "command": "true",
                "sandbox_permissions": "workspace-write",
                "justification": "why"
            }))
            .await
            .unwrap();
        assert!(text(&outcome).contains("not strictly wider"));
        assert!(backend.modes.lock().expect("modes").is_empty());
    }
}
