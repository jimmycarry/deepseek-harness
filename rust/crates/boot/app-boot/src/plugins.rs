//! Apply functions for shipped plugin names.
//!
//! Rows that already have a Rust crate call that crate. Remaining names
//! mount as no-ops so the headless tree can load; their behavior is filled
//! in later without changing the composition identity.

use dsh_credentials::CredentialsRuntime;
use dsh_agent::{AgentDefaultModel, AgentRegistry};
use dsh_agent_loop::AgentLoop;
use dsh_bash_local::BashLocal;
use dsh_commands::CommandRegistry;
use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_fs::FsRuntime;
use dsh_llm::{resolve_retry_policy, LlmAdapter, LlmError, LlmRequest, LlmRuntime, StreamChunk};
use dsh_llm_replay::ReplayAdapter;
use dsh_sandbox::SandboxRuntime;
use dsh_session::SessionStore;
use dsh_shell::ShellRuntime;
use dsh_subprocess::SubprocessRuntime;
use dsh_subprocess_local::LocalSubprocess;
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tool_bash::BashTool;
use dsh_settings_file::SettingsRuntime;
use dsh_tool_fs::{EditTool, ReadTool, WriteTool};
use dsh_tools::ToolRuntime;
use futures::stream::BoxStream;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Dispatch one composed row.
pub fn apply_named(name: &str, ctx: &Context, config: Option<Value>) -> Result<()> {
    match name {
        "@deepseek-ai/cordis-plugin-timer" => provide_marker::<Timer>(ctx),
        "@deepseek-ai/cordis-plugin-hmr" => Ok(()),
        "@deepseek-ai/dsh-llm" => apply_llm(ctx),
        "@deepseek-ai/dsh-session" => {
            SessionStore::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-agent" => ctx.provide(Arc::new(AgentRegistry::new())),
        "@deepseek-ai/dsh-agent-default-model" => apply_default_model(ctx, config),
        "@deepseek-ai/dsh-jobs-local" => {
            dsh_jobs_local::install(ctx, config.as_ref())?;
            Ok(())
        }
        "@deepseek-ai/dsh-settings-file" => dsh_settings_file::install(ctx, config.as_ref()),
        "@deepseek-ai/dsh-session-telemetry-otel" => {
            dsh_session_telemetry_otel::install(ctx, config.as_ref())
        }
        "@deepseek-ai/dsh-credentials-local" => {
            dsh_credentials_local::install(ctx, config.as_ref())?;
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
        "@deepseek-ai/dsh-pwsh-sandbox" => apply_pwsh_sandbox(ctx, config),
        "@deepseek-ai/dsh-user-approval" => apply_approval(ctx, config),
        "@deepseek-ai/dsh-permission-presets" => apply_permission(ctx, config),
        "@deepseek-ai/dsh-shell-env" => dsh_shell_env::install(ctx, config.as_ref()),
        "@deepseek-ai/dsh-tool-bash" => apply_tool_bash(ctx, config),
        "@deepseek-ai/dsh-tool-pwsh" => apply_tool_pwsh(ctx, config),
        "@deepseek-ai/dsh-tool-jobs" => apply_tool_jobs(ctx, config),
        "@deepseek-ai/dsh-tool-fs" => apply_tool_fs(ctx),
        "@deepseek-ai/dsh-fs-observation-policy" => dsh_fs_observation_policy::install(ctx),
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
        "@deepseek-ai/dsh-commands" => {
            ctx.provide(Arc::new(CommandRegistry::new()))?;
            dsh_permission_presets::bind_command(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-command-feedback" => dsh_command_feedback::install(ctx),
        "@deepseek-ai/dsh-fs-sandbox" => apply_fs_sandbox(ctx),
        "@deepseek-ai/dsh-llm-deepseek" => apply_llm_deepseek(ctx, config),
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

struct CodeRuntime;
impl Service for CodeRuntime {
    const KEY: &'static str = "codeRuntime";
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
        dsh_session::SessionStore::install(ctx)?;
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
    let resolved = dsh_sandbox_policy::Config::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    if !ctx.has_service(SandboxRuntime::KEY) {
        dsh_sandbox_local::install(ctx, resolved.workspace_root.clone())?;
    }
    dsh_sandbox_policy::install(ctx, config.as_ref())?;
    Ok(())
}

fn apply_approval(ctx: &Context, config: Option<Value>) -> Result<()> {
    dsh_user_approval::install(ctx, config.as_ref())?;
    Ok(())
}

fn apply_permission(ctx: &Context, config: Option<Value>) -> Result<()> {
    dsh_permission_presets::install(ctx, config.as_ref())?;
    Ok(())
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
        .get::<dsh_sandbox_policy::SandboxPolicyService>()
        .map(|policy| {
            (
                policy.default_mode().as_str().to_string(),
                policy.workspace_root().to_string(),
            )
        })
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

fn apply_pwsh_sandbox(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_subprocess(ctx)?;
    if !ctx.has_service(SandboxRuntime::KEY) {
        apply_sandbox(ctx, None)?;
    }
    if ctx.has_service(ShellRuntime::KEY) {
        return Ok(());
    }
    let (mode, workspace_root) = ctx
        .get::<dsh_sandbox_policy::SandboxPolicyService>()
        .map(|policy| {
            (
                policy.default_mode().as_str().to_string(),
                policy.workspace_root().to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                "workspace-write".into(),
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| ".".into()),
            )
        });
    let pwsh_path = config
        .as_ref()
        .and_then(|value| value.get("pwshPath"))
        .and_then(Value::as_str)
        .map(str::to_string);
    dsh_pwsh_sandbox::install(
        ctx,
        dsh_pwsh_sandbox::Config {
            mode,
            workspace_root,
            pwsh_path,
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
    dsh_tool_bash::require_confining_policy(ctx, shell.as_ref()).map_err(CordisError::Validation)?;
    let resolved =
        dsh_tool_bash::Config::resolve(config.as_ref()).map_err(CordisError::Validation)?;
    let jobs = ctx.get::<dsh_jobs::JobRegistry>();
    let mut tool = BashTool::with_jobs(
        shell,
        jobs,
        resolved.enable_run_in_background,
    )
    .with_context(ctx.clone());
    if let Some(shell_env) = ctx.get::<dsh_shell_env::ShellEnvRegistry>() {
        tool = tool.with_shell_env(shell_env);
    }
    tools.insert(Arc::new(tool));
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: "tool:bash".into(),
            order: 105,
            text: "Check the [exit code: N] marker on every bash result; investigate failures before moving on.".into(),
        });
    }
    Ok(())
}

fn apply_tool_pwsh(ctx: &Context, config: Option<Value>) -> Result<()> {
    apply_shell(ctx)?;
    ensure_tools(ctx)?;
    dsh_tool_pwsh::install(ctx, config.as_ref())?;
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: "tool:pwsh".into(),
            order: 105,
            text: "Non-zero exits are reported as `[exit code: N]` markers; investigate failures before moving on. On Windows a killed process settles as `[exit code: 1]` without a signal marker; treat a bare exit 1 after an interruption as a termination, not a command failure.".into(),
        });
    }
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
    tools.insert(Arc::new(ReadTool::new(Arc::clone(&fs), ctx.clone())));
    tools.insert(Arc::new(
        WriteTool::try_new(Arc::clone(&fs), ctx.clone()).map_err(CordisError::Validation)?,
    ));
    tools.insert(Arc::new(
        EditTool::try_new(fs, ctx.clone()).map_err(CordisError::Validation)?,
    ));
    Ok(())
}

fn apply_fs_sandbox(ctx: &Context) -> Result<()> {
    let (mode, workspace_root) = ctx
        .get::<dsh_sandbox_policy::SandboxPolicyService>()
        .map(|policy| {
            (
                policy.default_mode().as_str().to_string(),
                policy.workspace_root().to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                "workspace-write".into(),
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| ".".into()),
            )
        });
    dsh_fs_sandbox::install(
        ctx,
        dsh_fs_sandbox::Config {
            mode,
            workspace_root,
        },
    )
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
    dsh_sandbox_policy::bind_prompt(ctx)?;
    dsh_user_approval::bind_prompt(ctx)?;
    dsh_subagent::bind_prompt(ctx)?;
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
    let policy = dsh_compaction_basic::CompactionPolicy::resolve(config.as_ref())
        .map_err(CordisError::Validation)?;
    dsh_compaction_basic::BasicCompactionEngine::install(ctx, policy)?;
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

fn apply_llm_deepseek(ctx: &Context, config: Option<Value>) -> Result<()> {
    if let Some(settings) = ctx.get::<SettingsRuntime>() {
        settings.register("llm-deepseek")?;
    }
    let catalog = dsh_llm_deepseek::resolve_catalog(config.as_ref()).map_err(CordisError::Validation)?;
    let retry_policy = resolve_retry_policy(
        config.as_ref().and_then(|value| value.get("retryPolicy")),
        "llm-deepseek.retryPolicy",
    )
    .map_err(CordisError::Validation)?;
    ctx.provide(Arc::new(LlmRuntime::new(Arc::new(LiveDeepSeekAdapter {
        settings: ctx.get::<SettingsRuntime>(),
        credentials: ctx.get::<CredentialsRuntime>(),
        plugin_config: config,
        last_good: Mutex::new(Some(catalog)),
        retry_policy,
    }))))
}

struct LiveDeepSeekAdapter {
    settings: Option<Arc<SettingsRuntime>>,
    credentials: Option<Arc<CredentialsRuntime>>,
    plugin_config: Option<Value>,
    last_good: Mutex<Option<(u32, Vec<dsh_llm_deepseek::CatalogModel>)>>,
    retry_policy: dsh_llm::RetryPolicy,
}

fn resolve_deepseek(
    settings: Option<&SettingsRuntime>,
    plugin: Option<&Value>,
) -> (String, String, String) {
    let section = dsh_llm_deepseek::merge_connection_config(
        plugin,
        settings.and_then(|settings| settings.section("llm-deepseek")).as_ref(),
    );
    let api_key_env = section
        .get("apiKeyEnv")
        .and_then(Value::as_str)
        .unwrap_or(dsh_llm_deepseek::DEFAULT_API_KEY_ENV)
        .to_string();
    let base_url = section
        .get("baseURL")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.deepseek.com".into());
    let model = section
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-chat")
        .to_string();
    (api_key_env, base_url, model)
}

fn catalog_for(adapter: &LiveDeepSeekAdapter) -> Result<(u32, Vec<dsh_llm_deepseek::CatalogModel>), LlmError> {
    let section = dsh_llm_deepseek::merge_connection_config(
        adapter.plugin_config.as_ref(),
        adapter
            .settings
            .as_ref()
            .and_then(|settings| settings.section("llm-deepseek"))
            .as_ref(),
    );
    match dsh_llm_deepseek::resolve_catalog(Some(&section)) {
        Ok(catalog) => {
            *adapter.last_good.lock().expect("llm-deepseek catalog") = Some(catalog.clone());
            Ok(catalog)
        }
        Err(error) => {
            if let Some(good) = adapter.last_good.lock().expect("llm-deepseek catalog").clone() {
                Ok(good)
            } else {
                Err(LlmError::Failure(dsh_llm::LlmFailure {
                    message: error,
                    code: "CONFIG".into(),
                    status: None,
                }))
            }
        }
    }
}

#[async_trait::async_trait]
impl LlmAdapter for LiveDeepSeekAdapter {
    async fn stream(
        &self,
        request: LlmRequest,
    ) -> std::result::Result<BoxStream<'static, StreamChunk>, LlmError> {
        let (api_key_env, base_url, model) =
            resolve_deepseek(self.settings.as_deref(), self.plugin_config.as_ref());
        let api_key = dsh_llm_deepseek::resolve_api_key(self.credentials.as_deref(), &api_key_env)?;
        dsh_llm_deepseek::DeepSeekAdapter {
            api_key,
            base_url,
            model,
        }
        .stream(request)
        .await
    }

    fn provider_retry_policy(&self, provider: &str) -> dsh_llm::RetryPolicy {
        if provider == dsh_llm_deepseek::PROVIDER {
            self.retry_policy.clone()
        } else {
            dsh_llm::RetryPolicy::default()
        }
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> std::result::Result<dsh_llm::LlmResolvedModelInfo, LlmError> {
        let (default_window, models) = catalog_for(self)?;
        Ok(dsh_llm::LlmResolvedModelInfo {
            context: Some(dsh_llm::LlmModelContext {
                context_window: dsh_llm_deepseek::context_window_for(
                    model,
                    default_window,
                    &models,
                ),
            }),
            ..dsh_llm::LlmResolvedModelInfo::identity(provider, model)
        })
    }
}

fn apply_llm_replay(ctx: &Context, config: Option<Value>) -> Result<()> {
    let mut adapter = if let Some(turns) = config
        .as_ref()
        .and_then(|value| value.get("turns"))
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<dsh_llm_replay::ReplayTurn>>(value).ok())
    {
        ReplayAdapter::new(turns)
    } else {
        let text = config
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("pong");
        ReplayAdapter::text(text)
    };
    if let Some(Value::Object(map)) = config.as_ref().and_then(|value| value.get("auxiliary")) {
        for (purpose, text) in map {
            let Some(text) = text.as_str() else {
                return Err(CordisError::Validation(
                    "llm-replay: auxiliary values must be strings".into(),
                ));
            };
            adapter = adapter.with_auxiliary(purpose, text);
        }
    }
    if let Some(providers) = config.as_ref().and_then(|value| value.get("providers")) {
        let parsed = serde_json::from_value::<Vec<dsh_llm_replay::ReplayProviderConfig>>(
            providers.clone(),
        )
        .map_err(|error| {
            CordisError::Validation(format!("llm-replay: invalid providers catalog: {error}"))
        })?;
        let array = providers.as_array().ok_or_else(|| {
            CordisError::Validation("llm-replay: providers must be an array".into())
        })?;
        let mut policies = std::collections::HashMap::new();
        for (index, provider) in parsed.iter().enumerate() {
            let raw = array.get(index).and_then(|item| item.get("retryPolicy"));
            let policy = resolve_retry_policy(
                raw,
                &format!("llm-replay: providers[{index}].retryPolicy"),
            )
            .map_err(CordisError::Validation)?;
            policies.insert(provider.id.clone(), policy);
        }
        adapter = adapter
            .with_providers(parsed)
            .with_retry_policies(policies);
    }
    ctx.provide(Arc::new(LlmRuntime::new(Arc::new(adapter))))
}
