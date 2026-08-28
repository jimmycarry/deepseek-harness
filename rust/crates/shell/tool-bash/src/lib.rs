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
use dsh_shell::{
    parse_exit_status, render_process_read, render_shell_result, shell_tool_output_schema,
    DSH_ENV_PREFIX, ShellChild, ShellChildExit, ShellRequest, ShellRunResult, ShellRuntime,
    ShellToolOutput,
};
use dsh_shell_env::ShellEnvRegistry;
use dsh_tools::{
    GenericCallView, GenericResultView, TerminalCallView, TerminalResultView, Tool, ToolCall,
    ToolCallKind, ToolCallView, ToolError, ToolOutcome, ToolResultView,
};
use dsh_user_approval::{ApprovalRequest, ApprovalService};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

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
    require_confining_policy_named(
        ctx,
        shell,
        "tool-bash: the mounted bash executor confines but ctx.sandboxPolicy is missing",
    )
}

/// Fail loud when a confining executor is mounted without `ctx.sandboxPolicy`.
///
/// # Errors
/// The TypeScript load-failure sentence for this tool package.
pub fn require_confining_policy_named(
    ctx: &Context,
    shell: &ShellRuntime,
    sentence: &str,
) -> Result<(), String> {
    if shell.sandbox_mode().is_some() && ctx.get::<SandboxPolicyService>().is_none() {
        return Err(sentence.into());
    }
    Ok(())
}

/// Names and schema copy that distinguish bash from its pwsh twin.
#[derive(Debug, Clone, Copy)]
pub struct ShellToolDialect {
    /// Advertised tool name (`bash` / `pwsh`).
    pub name: &'static str,
    /// `ctx.jobs` kind (`bash` / `pwsh`).
    pub job_kind: &'static str,
    /// Load-failure sentence when a confining executor lacks `ctx.sandboxPolicy`.
    pub missing_policy: &'static str,
    /// `command` parameter description.
    pub command_param: &'static str,
    /// Examples inside the `description` parameter prose.
    pub description_examples: &'static str,
    /// Tool description builder.
    pub describe: fn(bool, bool) -> String,
}

/// Bash dialect.
pub const BASH_DIALECT: ShellToolDialect = ShellToolDialect {
    name: "bash",
    job_kind: "bash",
    missing_policy: "tool-bash: the mounted bash executor confines but ctx.sandboxPolicy is missing",
    command_param: "The bash command to execute.",
    description_examples: "Examples: \"ls\" → \"List files in current directory\"; \"git status\" → \"Show working tree status\"; \"npm install\" → \"Install package dependencies\".",
    describe: bash_description,
};

/// Shape one finished run into the text the model sees.
pub fn render_result(result: &ShellRunResult, advertises_escalation: bool) -> String {
    render_shell_result(result, advertises_escalation)
}

fn present_bash_call(args: &Value) -> Option<ToolCallView> {
    let command = args.get("command")?.as_str()?;
    let description = args.get("description")?.as_str()?;
    if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return Some(ToolCallView::Generic(GenericCallView {
            title: command.to_string(),
            kind: Some(ToolCallKind::Execute),
            raw_input: Some(Value::String(command.to_string())),
            content: Some(vec![dsh_llm::ContentBlock::text(description)]),
        }));
    }
    Some(ToolCallView::Terminal(TerminalCallView {
        title: command.to_string(),
        description: Some(description.to_string()),
        cwd: args
            .get("workdir")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn present_bash_result(args: &Value, outcome: &ToolOutcome) -> Option<ToolResultView> {
    if args.get("command").and_then(Value::as_str).is_none()
        || args.get("description").and_then(Value::as_str).is_none()
    {
        return None;
    }
    if outcome.content.len() != 1 {
        return None;
    }
    let dsh_llm::ContentBlock::Text { text } = &outcome.content[0] else {
        return None;
    };
    let is_background =
        args.get("run_in_background").and_then(Value::as_bool) == Some(true);
    if is_background || outcome.is_error {
        let trimmed = text.trim_end_matches('\n');
        return Some(ToolResultView::Generic(GenericResultView {
            title: None,
            content: Some(vec![dsh_llm::ContentBlock::text(format!(
                "```console\n{trimmed}\n```"
            ))]),
        }));
    }
    let parsed = parse_exit_status(text);
    Some(ToolResultView::Terminal(TerminalResultView {
        title: None,
        output: Some(parsed.body),
        exit_code: parsed.exit_code,
        signal: parsed.signal,
    }))
}

fn process_outcome(exit: &ShellChildExit) -> JobOutcome {
    if exit.killed {
        JobOutcome {
            status: JobStatus::Killed,
            detail: Some(
                exit.signal
                    .as_ref()
                    .map(|signal| format!("signal: {signal}"))
                    .unwrap_or_else(|| "killed before exit".into()),
            ),
            output: None,
        }
    } else {
        JobOutcome {
            status: JobStatus::Completed,
            detail: Some(format!("exit code: {}", exit.exit_code.unwrap_or(0))),
            output: None,
        }
    }
}

fn job_hooks_from_child(child: Box<dyn ShellChild>, advertises_escalation: bool) -> JobHooks {
    let child: Arc<dyn ShellChild> = Arc::from(child);
    let cancel_child = Arc::clone(&child);
    let wait_child = Arc::clone(&child);
    let read_child = Arc::clone(&child);
    JobHooks {
        cancel: Arc::new(move |_| cancel_child.cancel()),
        wait_done: Arc::new(move || match wait_child.wait() {
            Ok(exit) => process_outcome(&exit),
            Err(error) => JobOutcome {
                status: JobStatus::Failed,
                detail: Some(error.to_string()),
                output: None,
            },
        }),
        read_output: Some(Arc::new(move || {
            render_process_read(
                &read_child.read_output(),
                read_child.sandbox_info().as_ref(),
                advertises_escalation,
            )
        })),
    }
}

fn canonical_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

fn resolve_workdir(model_workdir: Option<&str>, session_cwd: Option<String>) -> Option<String> {
    match model_workdir {
        None => session_cwd,
        Some(model) => {
            if Path::new(model).is_absolute() {
                Some(model.to_string())
            } else if let Some(base) = session_cwd {
                Some(Path::new(&base).join(model).to_string_lossy().into_owned())
            } else {
                Some(model.to_string())
            }
        }
    }
}

fn validate_bash_args(
    args: &Value,
) -> Result<(String, Option<u64>, Option<String>), String> {
    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    if command.trim().is_empty() {
        return Err("invalid command: expected a non-empty string".into());
    }
    let description = args.get("description").and_then(Value::as_str).unwrap_or("");
    if description.trim().is_empty() {
        return Err("invalid description: expected a non-empty string".into());
    }
    let timeout_ms = if let Some(value) = args.get("timeoutMs") {
        match value.as_f64() {
            Some(number) if number.is_finite() && number > 0.0 => Some(number as u64),
            _ => {
                return Err(format!(
                    "invalid timeoutMs: expected a positive number, got {value}"
                ))
            }
        }
    } else {
        None
    };
    let workdir = args
        .get("workdir")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((command.to_string(), timeout_ms, workdir))
}

/// `bash` (or pwsh-twin) tool.
pub struct BashTool {
    dialect: ShellToolDialect,
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
        Self::with_jobs(shell, None, true)
    }

    /// Bind to shell and an optional job registry.
    pub fn with_jobs(
        shell: Arc<ShellRuntime>,
        jobs: Option<Arc<JobRegistry>>,
        enable_run_in_background: bool,
    ) -> Self {
        Self::with_dialect(shell, jobs, enable_run_in_background, BASH_DIALECT)
    }

    /// Bind a bash/pwsh dialect to shell and an optional job registry.
    pub fn with_dialect(
        shell: Arc<ShellRuntime>,
        jobs: Option<Arc<JobRegistry>>,
        enable_run_in_background: bool,
        dialect: ShellToolDialect,
    ) -> Self {
        let description = (dialect.describe)(enable_run_in_background, shell.sandbox_mode().is_some());
        Self {
            dialect,
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
            (self.dialect.describe)(self.enable_run_in_background, self.advertises_escalation());
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

    fn session_cwd(&self, agent_id: Option<&str>) -> Option<String> {
        let ctx = self.lookup.as_ref()?;
        let id = agent_id?;
        ctx.get::<AgentRegistry>()?
            .get(&session_id(id))?
            .session()
            .header()
            .cwd
            .clone()
            .map(|cwd| canonical_path(&cwd))
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
        let policy = standing.ok_or_else(|| self.dialect.missing_policy.to_string())?;
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
        let call_id = call.call_id.clone();
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
                            tool_name: self.dialect.name.into(),
                            call_id,
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
        self.dialect.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "command".into(),
            json!({
                "type": "string",
                "description": self.dialect.command_param
            }),
        );
        properties.insert(
            "description".into(),
            json!({
                "type": "string",
                "description": format!(
                    "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI). {}",
                    self.dialect.description_examples
                )
            }),
        );
        properties.insert(
            "timeoutMs".into(),
            json!({
                "type": "number",
                "description": "Timeout in milliseconds. The executor applies its configured default and cap, and kills the command on expiry."
            }),
        );
        properties.insert(
            "workdir".into(),
            json!({
                "type": "string",
                "description": "Working directory for this command. Defaults to the session workspace; a relative path is resolved against it."
            }),
        );
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
            "required": ["command", "description"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(shell_tool_output_schema())
    }

    fn present_call(&self, args: &Value) -> Option<ToolCallView> {
        present_bash_call(args)
    }

    fn present_result(&self, args: &Value, outcome: &ToolOutcome) -> Option<ToolResultView> {
        present_bash_result(args, outcome)
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall::new(self.name(), args)).await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let (command, timeout_ms, workdir) = match validate_bash_args(&call.args) {
            Ok(parsed) => parsed,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let standing = self
            .lookup
            .as_ref()
            .and_then(|ctx| resolve_from_context(ctx, call.agent_id.as_deref()));
        let policy = match self.approve_if_requested(call, standing.clone()).await {
            Ok(policy) => policy,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let session_cwd = standing
            .as_ref()
            .map(|policy| policy.workspace_root.clone())
            .or_else(|| self.session_cwd(call.agent_id.as_deref()));
        let cwd = resolve_workdir(workdir.as_deref(), session_cwd);
        let dsh_env = collect_dsh_env(self.shell_env.as_deref(), call.agent_id.as_deref())?;
        let request = ShellRequest {
            command: command.clone(),
            cwd,
            dsh_env,
            sandbox_policy: policy,
            timeout_ms,
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
            let shell = Arc::clone(&self.shell);
            let spec = shell.resolve(request);
            let advertises = self.advertises_escalation();
            match jobs.start(JobStart {
                kind: self.dialect.job_kind.into(),
                label: command,
                output_limit_bytes: None,
                owner_session: call.agent_id.clone(),
                run: Box::new(move || {
                    let child = shell.start(spec).map_err(|error| error.to_string())?;
                    Ok(job_hooks_from_child(child, advertises))
                }),
            }) {
                Ok(id) => {
                    let output = ShellToolOutput::Background { job_id: id };
                    Ok(ToolOutcome::text(output.render(false)))
                }
                Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
            }
        } else {
            let spec = self.shell.resolve(request);
            match self.shell.run(spec).await {
                Ok(result) => {
                    let output = ShellToolOutput::Foreground { result };
                    Ok(ToolOutcome::text(output.render(self.advertises_escalation())))
                }
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

pub fn name() -> &'static str {
    "dsh-tool-bash"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sandbox::{SandboxEnforcement, SandboxMode};
    use dsh_shell::{ShellError, ShellExecutor, ShellSandboxInfo, ShellSpec};
    use std::sync::Mutex;

    #[test]
    fn resolve_defaults_background_on() {
        assert!(Config::resolve(None).unwrap().enable_run_in_background);
    }

    struct RecordingBash {
        modes: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ShellExecutor for RecordingBash {
        async fn run(&self, spec: ShellSpec) -> Result<ShellRunResult, ShellError> {
            self.modes.lock().expect("modes").push(
                spec.sandbox_policy
                    .as_ref()
                    .map(|policy| policy.mode.as_str().to_string())
                    .unwrap_or_default(),
            );
            Ok(ShellRunResult::from_stdout(""))
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
        assert_eq!(
            parameters["required"],
            json!(["command", "description"])
        );
        assert!(props.contains_key("timeoutMs"));
        assert!(props.contains_key("workdir"));
        assert_eq!(
            props["description"]["description"],
            "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI). Examples: \"ls\" → \"List files in current directory\"; \"git status\" → \"Show working tree status\"; \"npm install\" → \"Install package dependencies\"."
        );
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
                "description": "no-op",
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
                "description": "no-op",
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
                "description": "no-op",
                "sandbox_permissions": "workspace-write",
                "justification": "why"
            }))
            .await
            .unwrap();
        assert!(text(&outcome).contains("not strictly wider"));
        assert!(backend.modes.lock().expect("modes").is_empty());
    }

    #[tokio::test]
    async fn validate_bash_args_matches_typescript_sentences() {
        let tool = BashTool::new(Arc::new(ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))));
        assert_eq!(
            text(
                &tool
                    .execute(json!({ "command": "   ", "description": "ok" }))
                    .await
                    .unwrap()
            ),
            "invalid command: expected a non-empty string"
        );
        assert_eq!(
            text(
                &tool
                    .execute(json!({ "command": "true" }))
                    .await
                    .unwrap()
            ),
            "invalid description: expected a non-empty string"
        );
        assert_eq!(
            text(
                &tool
                    .execute(json!({
                        "command": "true",
                        "description": "ok",
                        "timeoutMs": 0
                    }))
                    .await
                    .unwrap()
            ),
            "invalid timeoutMs: expected a positive number, got 0"
        );
    }

    #[test]
    fn render_result_appends_denial_and_exit_markers() {
        let mut result = ShellRunResult::from_stdout("denied\n");
        result.exit_code = Some(1);
        result.stderr.text = "touch: Read-only file system\n".into();
        result.sandbox = Some(ShellSandboxInfo {
            mode: SandboxMode::ReadOnly,
            denied: true,
            enforcement: Some(SandboxEnforcement::Full),
            runner_failed: None,
        });
        let text = render_result(&result, true);
        assert!(text.contains("[stderr]"));
        assert!(text.contains("[sandbox: file access denied under read-only mode]"));
        assert!(text.contains("sandbox_permissions"));
        assert!(text.contains("[exit code: 1]"));
    }

    #[test]
    fn output_schema_is_the_shared_shell_oneof() {
        let tool = BashTool::new(Arc::new(ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))));
        let schema = tool.output_schema().expect("bash declares output schema");
        assert_eq!(schema["oneOf"][0]["properties"]["kind"]["const"], "background");
        assert_eq!(schema["oneOf"][1]["properties"]["kind"]["const"], "foreground");
    }

    #[test]
    fn present_call_is_a_terminal_card_and_background_is_generic() {
        let tool = BashTool::new(Arc::new(ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))));
        let fg = tool
            .present_call(&json!({
                "command": "ls -la src",
                "description": "List files in src"
            }))
            .expect("foreground call");
        match fg {
            ToolCallView::Terminal(view) => {
                assert_eq!(view.title, "ls -la src");
                assert_eq!(view.description.as_deref(), Some("List files in src"));
                assert!(view.cwd.is_none());
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        let with_cwd = tool
            .present_call(&json!({
                "command": "pwd",
                "description": "Print dir",
                "workdir": "/tmp/x"
            }))
            .expect("cwd");
        match with_cwd {
            ToolCallView::Terminal(view) => assert_eq!(view.cwd.as_deref(), Some("/tmp/x")),
            other => panic!("expected terminal, got {other:?}"),
        }
        let bg = tool
            .present_call(&json!({
                "command": "sleep 100",
                "description": "wait",
                "run_in_background": true
            }))
            .expect("background call");
        match bg {
            ToolCallView::Generic(view) => {
                assert_eq!(view.title, "sleep 100");
                assert_eq!(view.kind, Some(ToolCallKind::Execute));
            }
            other => panic!("expected generic, got {other:?}"),
        }
        assert!(tool.present_call(&json!({ "command": "ls" })).is_none());
    }

    #[test]
    fn present_result_parses_exit_and_keeps_timeout_in_body() {
        let tool = BashTool::new(Arc::new(ShellRuntime::new(Arc::new(RecordingBash {
            modes: Mutex::new(Vec::new()),
        }))));
        let args = json!({ "command": "x", "description": "x" });
        match tool
            .present_result(
                &args,
                &ToolOutcome::text("hi\n\n"),
            )
            .expect("zero")
        {
            ToolResultView::Terminal(view) => {
                assert_eq!(view.output.as_deref(), Some("hi\n\n"));
                assert_eq!(view.exit_code, Some(0));
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        match tool
            .present_result(
                &args,
                &ToolOutcome::text("oops\n[exit code: 3]"),
            )
            .expect("nonzero")
        {
            ToolResultView::Terminal(view) => {
                assert_eq!(view.output.as_deref(), Some("oops"));
                assert_eq!(view.exit_code, Some(3));
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        match tool
            .present_result(
                &args,
                &ToolOutcome::text("slow\n[timed out after 100ms]\n[exit code: 143]"),
            )
            .expect("timeout")
        {
            ToolResultView::Terminal(view) => {
                assert_eq!(
                    view.output.as_deref(),
                    Some("slow\n[timed out after 100ms]")
                );
                assert_eq!(view.exit_code, Some(143));
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        match tool
            .present_result(
                &args,
                &ToolOutcome::error("invalid command: expected a non-empty string"),
            )
            .expect("error")
        {
            ToolResultView::Generic(_) => {}
            other => panic!("expected generic, got {other:?}"),
        }
        match tool
            .present_result(
                &json!({
                    "command": "sleep 100",
                    "description": "wait",
                    "run_in_background": true
                }),
                &ToolOutcome::text("started background job abc"),
            )
            .expect("background ack")
        {
            ToolResultView::Generic(view) => {
                let text = match &view.content.as_ref().unwrap()[0] {
                    dsh_llm::ContentBlock::Text { text } => text.clone(),
                    _ => String::new(),
                };
                assert_eq!(text, "```console\nstarted background job abc\n```");
            }
            other => panic!("expected generic, got {other:?}"),
        }
        match tool
            .present_result(&args, &ToolOutcome::text("[exit code: 5]"))
            .expect("marker-like body")
        {
            ToolResultView::Terminal(view) => {
                assert_eq!(view.output.as_deref(), Some("[exit code: 5]"));
                assert_eq!(view.exit_code, Some(0));
            }
            other => panic!("expected terminal, got {other:?}"),
        }
        assert!(tool
            .present_result(&args, &ToolOutcome {
                content: vec![],
                is_error: false,
            })
            .is_none());
    }
}
