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
            dsh_jobs_local::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-settings-file" => provide_marker::<SettingsRuntime>(ctx),
        "@deepseek-ai/dsh-credentials-local" => {
            dsh_credentials_local::install(ctx)?;
            Ok(())
        }
        "@deepseek-ai/dsh-session-persistence-jsonl" => apply_persistence(ctx, config),
        "@deepseek-ai/dsh-subprocess-local" => apply_subprocess(ctx),
        "@deepseek-ai/dsh-sandbox-local" => apply_sandbox(ctx, config),
        "@deepseek-ai/dsh-sandbox-policy" => apply_sandbox_policy(ctx, config),
        "@deepseek-ai/dsh-bash-sandbox" => Ok(()),
        "@deepseek-ai/dsh-user-approval" => apply_approval(ctx, config),
        "@deepseek-ai/dsh-permission-presets" => apply_permission(ctx, config),
        "@deepseek-ai/dsh-shell-env" => apply_shell(ctx),
        "@deepseek-ai/dsh-tool-bash" => apply_tool_bash(ctx),
        "@deepseek-ai/dsh-tool-fs" => apply_tool_fs(ctx),
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
        "@deepseek-ai/dsh-code-runtime-worker-thread" => provide_marker::<CodeRuntime>(ctx),
        "@deepseek-ai/dsh-headless/startup" => dsh_bundle_headless::apply_startup(ctx, config),
        "@deepseek-ai/dsh-headless" => dsh_bundle_headless::apply_runner(ctx, config),
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
pub struct PermissionService;
impl Service for PermissionService {
    const KEY: &'static str = "permission";
}

struct UnsetAdapter;

#[async_trait::async_trait]
impl LlmAdapter for UnsetAdapter {
    async fn stream(&self, _request: LlmRequest) -> std::result::Result<BoxStream<'static, StreamChunk>, LlmError> {
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
        dsh_sandbox_local::install(ctx, root)?;
    }
    ctx.provide(Arc::new(SandboxPolicyService { mode }))
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

fn apply_permission(ctx: &Context, _config: Option<Value>) -> Result<()> {
    ctx.provide(Arc::new(PermissionService))
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

fn apply_tool_bash(ctx: &Context) -> Result<()> {
    apply_shell(ctx)?;
    let tools = ensure_tools(ctx)?;
    let shell = ctx.service::<ShellRuntime>()?;
    tools.insert(Arc::new(BashTool::new(shell)));
    Ok(())
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

fn apply_system_prompt(ctx: &Context, config: Option<Value>) -> Result<()> {
    let prompt = SystemPrompt::new();
    if let Some(persona) = config
        .as_ref()
        .and_then(|value| value.get("persona"))
        .and_then(Value::as_str)
    {
        prompt.set_persona(persona);
    }
    ctx.provide(Arc::new(prompt))
}

fn apply_llm_deepseek(ctx: &Context) -> Result<()> {
    match dsh_llm_deepseek::DeepSeekAdapter::from_env() {
        Ok(adapter) => ctx.provide(Arc::new(LlmRuntime::new(Arc::new(adapter)))),
        Err(_) => Ok(()),
    }
}

fn apply_llm_replay(ctx: &Context, config: Option<Value>) -> Result<()> {
    let text = config
        .as_ref()
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("pong");
    ctx.provide(Arc::new(LlmRuntime::new(Arc::new(ReplayAdapter::text(text)))))
}
