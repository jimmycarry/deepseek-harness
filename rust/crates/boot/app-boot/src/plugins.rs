//! Apply functions for shipped plugin names.
//!
//! Rows that already have a Rust crate call that crate. Remaining names
//! mount as no-ops so the headless tree can load; their behavior is filled
//! in later without changing the composition identity.

use dsh_agent::{AgentDefaultModel, AgentRegistry};
use dsh_agent_loop::AgentLoop;
use dsh_bash_local::BashLocal;
use dsh_commands::CommandRegistry;
use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_fs::FsRuntime;
use dsh_llm::{LlmAdapter, LlmError, LlmRequest, LlmRuntime, StreamChunk};
use dsh_llm_replay::ReplayAdapter;
use dsh_sandbox::SandboxRuntime;
use dsh_session::SessionStore;
use dsh_shell::ShellRuntime;
use dsh_subprocess::SubprocessRuntime;
use dsh_subprocess_local::LocalSubprocess;
use dsh_system_prompt::SystemPrompt;
use dsh_tool_bash::BashTool;
use dsh_tool_fs::{ReadFileTool, WriteFileTool};
use dsh_tools::ToolRuntime;
use futures::stream::BoxStream;
use serde_json::Value;
use std::sync::Arc;

/// Dispatch one composed row.
pub fn apply_named(name: &str, ctx: &Context, config: Option<Value>) -> Result<()> {
    match name {
        "@deepseek-ai/cordis-plugin-timer" => provide_marker::<Timer>(ctx),
        "@deepseek-ai/cordis-plugin-hmr" => Ok(()),
        "@deepseek-ai/dsh-llm" => apply_llm(ctx),
        "@deepseek-ai/dsh-session" => ctx.provide(Arc::new(SessionStore::new())),
        "@deepseek-ai/dsh-agent" => ctx.provide(Arc::new(AgentRegistry::new())),
        "@deepseek-ai/dsh-agent-default-model" => apply_default_model(ctx, config),
        "@deepseek-ai/dsh-jobs-local" => {
            dsh_jobs_local::install(ctx, config.as_ref())?;
            Ok(())
        }
        "@deepseek-ai/dsh-settings-file" => provide_marker::<SettingsRuntime>(ctx),
        "@deepseek-ai/dsh-credentials-local" => {
            dsh_credentials_local::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-session-title" => apply_session_title(ctx, config),
        "@deepseek-ai/dsh-session-title-first-prompt-llm" => apply_session_title_llm(ctx, config),
        "@deepseek-ai/dsh-session-persistence-jsonl" => apply_persistence(ctx, config),
        "@deepseek-ai/dsh-session-persistence-sqlite" => apply_persistence_sqlite(ctx, config),
        "@deepseek-ai/dsh-attachment-local" => apply_attachment(ctx, config),
        "@deepseek-ai/dsh-session-projection" => {
            dsh_session_projection::SessionProjectionRegistry::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-session-checkpoint-policy" => dsh_session_checkpoint_policy::install(ctx),
        "@deepseek-ai/dsh-llm-retry" => dsh_llm_retry::install(ctx, config.as_ref()),
        "@deepseek-ai/dsh-agent-instructions" => apply_agent_instructions(ctx, config),
        "@deepseek-ai/dsh-session-query-sqlite" => apply_session_query(ctx, config),
        "@deepseek-ai/dsh-spill-local" => apply_spill_local(ctx, config),
        "@deepseek-ai/dsh-spill-policy" => apply_spill_policy(ctx, config),
        "@deepseek-ai/dsh-subprocess-local" => apply_subprocess(ctx),
        "@deepseek-ai/dsh-sandbox-local" => apply_sandbox(ctx, config),
        "@deepseek-ai/dsh-sandbox-policy" => apply_sandbox_policy(ctx, config),
        "@deepseek-ai/dsh-bash-sandbox" => apply_bash_sandbox(ctx),
        "@deepseek-ai/dsh-user-approval" => apply_approval(ctx, config),
        "@deepseek-ai/dsh-permission-presets" => apply_permission(ctx, config),
        "@deepseek-ai/dsh-shell-env" => apply_shell(ctx),
        "@deepseek-ai/dsh-tool-bash" => apply_tool_bash(ctx, config),
        "@deepseek-ai/dsh-tool-jobs" => apply_tool_jobs(ctx, config),
        "@deepseek-ai/dsh-tool-fs" => apply_tool_fs(ctx),
        "@deepseek-ai/dsh-tool-fs-search" => apply_tool_fs_search(ctx, config),
        "@deepseek-ai/dsh-tool-str-replace-editor" => apply_tool_str_replace_editor(ctx, config),
        "@deepseek-ai/dsh-tools" => {
            ensure_tools(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-system-prompt" => apply_system_prompt(ctx, config),
        "@deepseek-ai/dsh-agent-loop" => {
            AgentLoop::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-commands" => ctx.provide(Arc::new(CommandRegistry::new())),
        "@deepseek-ai/dsh-fs-sandbox" => {
            dsh_fs_local::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-llm-deepseek" => apply_llm_deepseek(ctx),
        "@deepseek-ai/dsh-llm-replay" => apply_llm_replay(ctx, config),
        "@deepseek-ai/dsh-goal" => apply_goal(ctx, config),
        "@deepseek-ai/dsh-goal-round-driver" => dsh_goal_round_driver::install(ctx),
        "@deepseek-ai/dsh-command-goal" => dsh_command_goal::install(ctx),
        "@deepseek-ai/dsh-tool-goal" => apply_tool_goal(ctx, config),
        "@deepseek-ai/dsh-subagent" => {
            dsh_subagent::SubagentRuntime::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-subagent-spawn-in-process" => apply_subagent_provider(ctx, config, false),
        "@deepseek-ai/dsh-subagent-fork-in-process" => apply_subagent_provider(ctx, config, true),
        "@deepseek-ai/dsh-tool-subagent" => apply_tool_subagent(ctx, config),
        "@deepseek-ai/dsh-tool-subagent-control" => apply_tool_subagent_control(ctx),
        "@deepseek-ai/dsh-tool-subagent-control/list-agents" => {
            apply_tool_subagent_list_agents(ctx)
        }
        "@deepseek-ai/dsh-tool-subagent-report" => apply_tool_subagent_report(ctx, config),
        "@deepseek-ai/dsh-workflow-worker-thread" => apply_workflow(ctx, config),
        "@deepseek-ai/dsh-tool-workflow" => apply_tool_workflow(ctx, config),
        "@deepseek-ai/dsh-tool-ralph" => apply_tool_ralph(ctx, config),
        "@deepseek-ai/dsh-repeat-tool-reminder" => apply_repeat_tool_reminder(ctx, config),
        "@deepseek-ai/dsh-tool-todo" => apply_tool_todo(ctx, config),
        "@deepseek-ai/dsh-tool-call-timeout-policy" => dsh_timeout_policy::install(ctx),
        "@deepseek-ai/dsh-compaction-tool-result-pruner" => apply_tool_result_pruner(ctx, config),
        "@deepseek-ai/dsh-plan-mode" => apply_plan_mode(ctx, config),
        "@deepseek-ai/dsh-user-questions" => {
            dsh_user_questions::UserQuestionsService::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-skill" => apply_skill(ctx),
        "@deepseek-ai/dsh-skill-filesystem" => apply_skill_filesystem(ctx, config),
        "@deepseek-ai/dsh-tool-skill" => apply_tool_skill(ctx, config),
        "@deepseek-ai/dsh-token-meter" => apply_token_meter(ctx, config),
        "@deepseek-ai/dsh-compaction-basic" => apply_compaction_basic(ctx, config),
        "@deepseek-ai/dsh-command-compact" => apply_command_compact(ctx),
        "@deepseek-ai/dsh-web" => apply_web(ctx, config),
        "@deepseek-ai/dsh-web-search-deepseek" => apply_web_search_deepseek(ctx, config),
        "@deepseek-ai/dsh-tool-web" => apply_tool_web(ctx, config),
        "@deepseek-ai/dsh-code-runtime-worker-thread" => provide_marker::<CodeRuntime>(ctx),
        "@deepseek-ai/dsh-headless/startup" => dsh_bundle_headless::apply_startup(ctx, config),
        "@deepseek-ai/dsh-headless" => dsh_bundle_headless::apply_runner(ctx, config),
        "@deepseek-ai/dsh-acp" => dsh_acp::AcpServer::install(ctx),
        "@deepseek-ai/dsh-sdk-jsonrpc-server" => {
            dsh_sdk_server::HarnessSdkJsonRpcServer::install(ctx)
        }
        _ => Ok(()),
    }
}

struct Timer;
impl Service for Timer {
    const KEY: &'static str = "timer";
}

struct SettingsRuntime;
impl Service for SettingsRuntime {
    const KEY: &'static str = "settings";
}

struct CodeRuntime;
impl Service for CodeRuntime {
    const KEY: &'static str = "codeRuntime";
}

/// `ctx.sandboxPolicy`.
pub struct SandboxPolicyService {
    /// Permission mode (`workspace-write`, `read-only`, `danger-full-access`).
    pub mode: String,
    /// Workspace root used in the workspace-write snapshot sentence.
    pub workspace_root: String,
}

impl Service for SandboxPolicyService {
    const KEY: &'static str = "sandboxPolicy";
}

/// `ctx.approval`.
pub struct ApprovalService {
    /// `ask` or `never`.
    pub policy: String,
}

impl Service for ApprovalService {
    const KEY: &'static str = "approval";
}

/// `ctx.permission`.
pub struct PermissionService {
    /// Active preset name.
    pub preset: String,
}
impl Service for PermissionService {
    const KEY: &'static str = "permission";
}

struct UnsetAdapter;

#[async_trait::async_trait]
impl LlmAdapter for UnsetAdapter {
    async fn stream(
        &self,
        _request: LlmRequest,
    ) -> std::result::Result<BoxStream<'static, StreamChunk>, LlmError> {
        Err(LlmError::Failure(dsh_llm::LlmFailure {
            message: "no LLM adapter is mounted".into(),
            code: "MISSING_ADAPTER".into(),
            status: None,
        }))
    }
}

fn provide_marker<S: Service + Default + 'static>(ctx: &Context) -> Result<()> {
    ctx.provide(Arc::new(S::default()))
}

impl Default for Timer {
    fn default() -> Self {
        Self
    }
}
impl Default for SettingsRuntime {
    fn default() -> Self {
        Self
    }
}
impl Default for CodeRuntime {
    fn default() -> Self {
        Self
    }
}

fn apply_llm(ctx: &Context) -> Result<()> {
    if ctx.has_service(LlmRuntime::KEY) {
        return Ok(());
    }
    ctx.provide(Arc::new(LlmRuntime::new(Arc::new(UnsetAdapter))))
}

fn apply_default_model(ctx: &Context, config: Option<Value>) -> Result<()> {
    let provider = config
        .as_ref()
        .and_then(|value| value.get("provider"))
        .and_then(Value::as_str)
        .ok_or_else(|| CordisError::Validation("agent-default-model requires provider".into()))?;
    let model = config
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .ok_or_else(|| CordisError::Validation("agent-default-model requires model".into()))?;
    ctx.provide(Arc::new(AgentDefaultModel::new(provider, model)))
}

fn apply_persistence_sqlite(ctx: &Context, config: Option<Value>) -> Result<()> {
    let path = config
        .as_ref()
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CordisError::Validation("session-persistence-sqlite requires path".into())
        })?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|error| CordisError::plugin(error.to_string()))?;
    }
    dsh_session_persistence_sqlite::install(ctx, path)?;
    Ok(())
}

fn apply_attachment(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved =
        dsh_attachment_local::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_attachment_local::install(ctx, resolved)?;
    Ok(())
}

fn apply_session_query(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service(dsh_session::SessionStore::KEY) {
        ctx.provide(Arc::new(dsh_session::SessionStore::new()))?;
    }
    let resolved = dsh_session_query_sqlite::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_session_query_sqlite::install(ctx, resolved)?;
    Ok(())
}

fn apply_spill_local(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved =
        dsh_spill_local::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_spill_local::install(ctx, resolved)?;
    Ok(())
}

fn apply_spill_policy(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved =
        dsh_spill_policy::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_spill_policy::install(ctx, resolved)
}

fn apply_session_title(ctx: &Context, config: Option<Value>) -> Result<()> {
    fn positive(config: Option<&Value>, key: &str) -> Result<usize> {
        config
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .ok_or_else(|| {
                CordisError::Validation(format!("session-title requires positive {key}"))
            })
    }
    let resolved = dsh_session_title::SessionTitleConfig {
        fallback_max_words: positive(config.as_ref(), "fallbackMaxWords")?,
        fallback_max_bytes: positive(config.as_ref(), "fallbackMaxBytes")?,
        max_title_bytes: positive(config.as_ref(), "maxTitleBytes")?,
    };
    dsh_session_title::SessionTitleService::install(ctx, resolved)?;
    Ok(())
}

fn apply_session_title_llm(ctx: &Context, config: Option<Value>) -> Result<()> {
    fn positive(config: Option<&Value>, key: &str) -> Result<u64> {
        config
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                CordisError::Validation(format!("session-title-llm requires positive {key}"))
            })
    }
    let optional = |key: &str| {
        config
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let resolved = dsh_session_title_first_prompt_llm::SessionTitleLlmConfig {
        target_words: positive(config.as_ref(), "targetWords")? as u32,
        target_cjk_characters: positive(config.as_ref(), "targetCjkCharacters")? as u32,
        max_input_bytes: positive(config.as_ref(), "maxInputBytes")? as usize,
        max_output_tokens: positive(config.as_ref(), "maxOutputTokens")? as u32,
        timeout_ms: positive(config.as_ref(), "timeoutMs")?,
        provider: optional("provider"),
        model: optional("model"),
    };
    dsh_session_title_first_prompt_llm::install(ctx, resolved)
}

fn apply_persistence(ctx: &Context, config: Option<Value>) -> Result<()> {
    let dir = config
        .as_ref()
        .and_then(|value| value.get("root"))
        .and_then(Value::as_str)
        .ok_or_else(|| CordisError::Validation("session-persistence-jsonl requires root".into()))?;
    std::fs::create_dir_all(dir).map_err(|error| CordisError::plugin(error.to_string()))?;
    dsh_session_persistence_jsonl::install(ctx, dir)?;
    Ok(())
}

fn apply_subprocess(ctx: &Context) -> Result<()> {
    if ctx.has_service(SubprocessRuntime::KEY) {
        return Ok(());
    }
    ctx.provide(Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess))))
}

fn apply_sandbox(ctx: &Context, config: Option<Value>) -> Result<()> {
    if ctx.has_service(SandboxRuntime::KEY) {
        return Ok(());
    }
    let root = config
        .as_ref()
        .and_then(|value| value.get("workspaceRoot"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into())
        });
    dsh_sandbox_local::install(ctx, root)?;
    Ok(())
}

fn apply_sandbox_policy(ctx: &Context, config: Option<Value>) -> Result<()> {
    let mode = config
        .as_ref()
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("workspace-write")
        .to_string();
    let root = config
        .as_ref()
        .and_then(|value| value.get("workspaceRoot"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into())
        });
    if !ctx.has_service(SandboxRuntime::KEY) {
        dsh_sandbox_local::install(ctx, root.clone())?;
    }
    ctx.provide(Arc::new(SandboxPolicyService {
        mode,
        workspace_root: root,
    }))
}

fn apply_approval(ctx: &Context, config: Option<Value>) -> Result<()> {
    let policy = config
        .as_ref()
        .and_then(|value| value.get("policy"))
        .and_then(Value::as_str)
        .unwrap_or("ask")
        .to_string();
    ctx.provide(Arc::new(ApprovalService { policy }))
}

fn sandbox_policy_text(mode: &str, workspace_root: &str) -> String {
    match mode {
        "read-only" => {
            "Current DSH file policy: read-only. Any available operation enforced by the DSH file sandbox cannot modify files in the standing mode. Do not refuse a required modification from this policy alone: try an available tool normally and follow any denial and escalation guidance it returns."
                .into()
        }
        "danger-full-access" => {
            "Current DSH file policy: danger-full-access. The DSH file sandbox does not restrict file modifications by available operations."
                .into()
        }
        _ => format!(
            "Current DSH file policy: workspace-write. Any available operation enforced by the DSH file sandbox may modify files under the session workspace: {}. Some platform temporary areas may also be writable.",
            serde_json::to_string(workspace_root).unwrap_or_else(|_| format!("\"{workspace_root}\""))
        ),
    }
}

fn approval_policy_text(policy: &str) -> String {
    if policy == "never" {
        "Approval prompts are disabled in this session: actions that require approval are rejected automatically — do not request sandbox escalation (do not set `sandbox_permissions`)."
            .into()
    } else {
        String::new()
    }
}

fn apply_permission(ctx: &Context, _config: Option<Value>) -> Result<()> {
    let preset = ctx
        .get::<SandboxPolicyService>()
        .map(|policy| policy.mode.clone())
        .unwrap_or_else(|| "workspace-write".into());
    ctx.provide(Arc::new(PermissionService { preset }))
}

fn apply_bash_sandbox(ctx: &Context) -> Result<()> {
    apply_subprocess(ctx)?;
    if !ctx.has_service(SandboxRuntime::KEY) {
        apply_sandbox(ctx, None)?;
    }
    if ctx.has_service(ShellRuntime::KEY) {
        return Ok(());
    }
    let (mode, workspace_root) = ctx
        .get::<SandboxPolicyService>()
        .map(|policy| (policy.mode.clone(), policy.workspace_root.clone()))
        .unwrap_or_else(|| {
            (
                "workspace-write".into(),
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| ".".into()),
            )
        });
    dsh_bash_sandbox::install(
        ctx,
        dsh_bash_sandbox::Config {
            mode,
            workspace_root,
        },
    )?;
    Ok(())
}

fn apply_shell(ctx: &Context) -> Result<()> {
    if ctx.has_service(ShellRuntime::KEY) {
        return Ok(());
    }
    apply_subprocess(ctx)?;
    let subprocess = ctx.service::<SubprocessRuntime>()?;
    ctx.provide(Arc::new(ShellRuntime::new(Arc::new(BashLocal::new(
        subprocess,
    )))))
}

fn ensure_tools(ctx: &Context) -> Result<Arc<ToolRuntime>> {
    if let Some(tools) = ctx.get::<ToolRuntime>() {
        return Ok(tools);
    }
    let tools = Arc::new(ToolRuntime::new());
    ctx.provide(Arc::clone(&tools))?;
    Ok(tools)
}

fn apply_tool_bash(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_shell(ctx)?;
    let tools = ensure_tools(ctx)?;
    let shell = ctx.service::<ShellRuntime>()?;
    let resolved =
        dsh_tool_bash::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    let jobs = ctx.get::<dsh_jobs::JobRegistry>();
    tools.insert(Arc::new(BashTool::with_jobs(
        shell,
        jobs,
        resolved.enable_run_in_background,
    )));
    Ok(())
}

fn apply_tool_jobs(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    dsh_tool_jobs::install(ctx, config.as_ref())
}

fn apply_tool_fs(ctx: &Context) -> Result<()> {
    if !ctx.has_service(FsRuntime::KEY) {
        dsh_fs_local::install(ctx)?;
    }
    let tools = ensure_tools(ctx)?;
    let fs = ctx.service::<FsRuntime>()?;
    tools.insert(Arc::new(ReadFileTool::new(Arc::clone(&fs))));
    tools.insert(Arc::new(WriteFileTool::new(fs)));
    Ok(())
}

fn apply_tool_fs_search(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_subprocess(ctx)?;
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_fs_search::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_fs_search::install(ctx, resolved)?;
    Ok(())
}

fn apply_tool_str_replace_editor(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service(FsRuntime::KEY) {
        dsh_fs_local::install(ctx)?;
    }
    ensure_tools(ctx)?;
    let resolved = dsh_tool_str_replace_editor::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_tool_str_replace_editor::install(ctx, resolved)?;
    Ok(())
}

fn ensure_system_prompt(ctx: &Context) -> Result<Arc<SystemPrompt>> {
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        return Ok(prompt);
    }
    let prompt = Arc::new(SystemPrompt::new());
    ctx.provide(Arc::clone(&prompt))?;
    Ok(prompt)
}

fn apply_system_prompt(ctx: &Context, config: Option<Value>) -> Result<()> {
    let prompt = ensure_system_prompt(ctx)?;
    if let Some(persona) = config
        .as_ref()
        .and_then(|value| value.get("persona"))
        .and_then(Value::as_str)
    {
        prompt.set_persona(persona);
    }
    if let Some(policy) = ctx.get::<SandboxPolicyService>() {
        prompt.register_context(dsh_system_prompt::PromptContext {
            name: "sandbox:policy".into(),
            text: sandbox_policy_text(&policy.mode, &policy.workspace_root),
            order: 10,
        });
    }
    if let Some(approval) = ctx.get::<ApprovalService>() {
        prompt.register_context(dsh_system_prompt::PromptContext {
            name: "approval:policy".into(),
            text: approval_policy_text(&approval.policy),
            order: 20,
        });
    }
    Ok(())
}

fn apply_goal(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved = dsh_goal::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_goal::GoalService::install(ctx, resolved)?;
    Ok(())
}

fn apply_tool_goal(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_goal::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_goal::install(ctx, resolved)
}

fn apply_subagent_provider(ctx: &Context, config: Option<Value>, inherits: bool) -> Result<()> {
    if !ctx.has_service(dsh_subagent::SubagentRuntime::KEY) {
        dsh_subagent::SubagentRuntime::install(ctx)?;
    }
    let name = config
        .as_ref()
        .and_then(|value| value.get("providerName"))
        .and_then(Value::as_str)
        .unwrap_or(if inherits { "fork" } else { "spawn" });
    dsh_subagent_inprocess::install(ctx, name, inherits)
}

fn apply_tool_subagent(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service(dsh_subagent::SubagentRuntime::KEY) {
        dsh_subagent::SubagentRuntime::install(ctx)?;
    }
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_subagent::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_subagent::install(ctx, resolved)
}

fn ensure_subagents(ctx: &Context) -> Result<()> {
    if !ctx.has_service(dsh_subagent::SubagentRuntime::KEY) {
        dsh_subagent::SubagentRuntime::install(ctx)?;
    }
    Ok(())
}

fn apply_tool_subagent_control(ctx: &Context) -> Result<()> {
    ensure_subagents(ctx)?;
    ensure_tools(ctx)?;
    dsh_tool_subagent_control::install(ctx)
}

fn apply_tool_subagent_list_agents(ctx: &Context) -> Result<()> {
    ensure_subagents(ctx)?;
    ensure_tools(ctx)?;
    dsh_tool_subagent_control::install_list_agents(ctx)
}

fn apply_tool_subagent_report(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_subagents(ctx)?;
    ensure_tools(ctx)?;
    let resolved = dsh_tool_subagent_report::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_tool_subagent_report::install(ctx, resolved)
}

fn apply_workflow(ctx: &Context, config: Option<Value>) -> Result<()> {
    let isolation = config
        .as_ref()
        .and_then(|value| value.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("spawn");
    dsh_workflow_local::install(ctx, isolation)?;
    Ok(())
}

fn apply_tool_workflow(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service("workflowEngine") {
        dsh_workflow_local::install(ctx, "in-process")?;
    }
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_workflow::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_workflow::install(ctx, resolved)
}

fn apply_tool_ralph(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_subagents(ctx)?;
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_ralph::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_ralph::install(ctx, resolved)
}

fn apply_repeat_tool_reminder(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved = dsh_repeat_tool_reminder::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_repeat_tool_reminder::install(ctx, resolved)
}

fn apply_tool_todo(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_tools(ctx)?;
    let resolved =
        dsh_tool_todo::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_todo::install(ctx, resolved)
}

fn apply_tool_result_pruner(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved = dsh_tool_result_pruner::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_tool_result_pruner::ToolResultPruner::install(
        ctx,
        resolved.threshold_chars,
        resolved.head_chars,
        resolved.tail_chars,
    )?;
    Ok(())
}

fn apply_plan_mode(ctx: &Context, config: Option<Value>) -> Result<()> {
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    dsh_plan_mode::apply(ctx, config.as_ref())?;
    Ok(())
}

fn apply_agent_instructions(ctx: &Context, config: Option<Value>) -> Result<()> {
    let resolved = dsh_agent_instructions::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_agent_instructions::install(ctx, resolved)
}

fn apply_skill(ctx: &Context) -> Result<()> {
    if ctx.has_service(dsh_skill::SkillRuntime::KEY) {
        return Ok(());
    }
    ctx.provide(Arc::new(dsh_skill::SkillRuntime::new()))
}

fn apply_skill_filesystem(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_skill(ctx)?;
    let resolved =
        dsh_skill_filesystem::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_skill_filesystem::install(ctx, resolved)
}

fn apply_tool_skill(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_skill(ctx)?;
    ensure_tools(ctx)?;
    let resolved =
        dsh_tool_skill::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_skill::install(ctx, resolved)
}

fn apply_token_meter(ctx: &Context, config: Option<Value>) -> Result<()> {
    if ctx.has_service("tokenMeter") {
        return Ok(());
    }
    let chars_per_token = match config.as_ref().and_then(|value| value.get("charsPerToken")) {
        None => 4,
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .ok_or_else(|| {
                CordisError::Validation(
                    "token-meter: charsPerToken must be a positive integer".into(),
                )
            })?,
    };
    ctx.provide(Arc::new(dsh_token_meter::TokenMeter::new(chars_per_token)))
}

fn apply_compaction_basic(ctx: &Context, config: Option<Value>) -> Result<()> {
    fn field(config: Option<&Value>, key: &str, default: usize) -> Result<usize> {
        match config.and_then(|value| value.get(key)) {
            None => Ok(default),
            Some(value) => value
                .as_u64()
                .filter(|value| *value > 0)
                .map(|value| value as usize)
                .ok_or_else(|| {
                    CordisError::Validation(format!(
                        "compaction-basic: {key} must be a positive integer"
                    ))
                }),
        }
    }
    let threshold_messages = field(config.as_ref(), "thresholdMessages", 40)?;
    let retain_tail = field(config.as_ref(), "retainTail", 8)?;
    dsh_compaction_basic::BasicCompactionEngine::install(ctx, threshold_messages, retain_tail)?;
    Ok(())
}

fn apply_command_compact(ctx: &Context) -> Result<()> {
    if !ctx.has_service(CommandRegistry::KEY) {
        ctx.provide(Arc::new(CommandRegistry::new()))?;
    }
    dsh_command_compact::install(ctx)
}

fn apply_web(ctx: &Context, config: Option<Value>) -> Result<()> {
    if ctx.has_service(dsh_web::WebRuntime::KEY) {
        return Ok(());
    }
    dsh_web::WebRuntime::install(ctx, dsh_web::WebRuntimeConfig::resolve(config.as_ref()))?;
    Ok(())
}

fn apply_web_search_deepseek(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service(dsh_web::WebRuntime::KEY) {
        apply_web(ctx, None)?;
    }
    let resolved = dsh_web_search_deepseek::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_web_search_deepseek::install(ctx, resolved)
}

fn apply_tool_web(ctx: &Context, config: Option<Value>) -> Result<()> {
    if !ctx.has_service(dsh_web::WebRuntime::KEY) {
        apply_web(ctx, None)?;
    }
    ensure_tools(ctx)?;
    ensure_system_prompt(ctx)?;
    let resolved =
        dsh_tool_web::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    dsh_tool_web::install(ctx, resolved)
}

fn apply_llm_deepseek(ctx: &Context) -> Result<()> {
    match dsh_llm_deepseek::DeepSeekAdapter::from_env() {
        Ok(adapter) => ctx.provide(Arc::new(LlmRuntime::new(Arc::new(adapter)))),
        Err(_) => Ok(()),
    }
}

fn apply_llm_replay(ctx: &Context, config: Option<Value>) -> Result<()> {
    if let Some(turns) = config
        .as_ref()
        .and_then(|value| value.get("turns"))
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<dsh_llm_replay::ReplayTurn>>(value).ok())
    {
        return ctx.provide(Arc::new(LlmRuntime::new(Arc::new(ReplayAdapter::new(
            turns,
        )))));
    }
    let text = config
        .as_ref()
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("pong");
    ctx.provide(Arc::new(LlmRuntime::new(Arc::new(ReplayAdapter::text(
        text,
    )))))
}
