//! Runnable agent spine composition. Mirrors `dsh-agent-spine-demo`.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::AgentLoop;
use dsh_bash_local::BashLocal;
use dsh_commands::CommandRegistry;
use dsh_cordis::{Context, Result};
use dsh_fs::FsRuntime;
use dsh_llm::{LlmAdapter, LlmRuntime};
use dsh_llm_replay::ReplayAdapter;
use dsh_session::SessionStore;
use dsh_shell::ShellRuntime;
use dsh_subprocess::SubprocessRuntime;
use dsh_subprocess_local::LocalSubprocess;
use dsh_system_prompt::SystemPrompt;
use dsh_tool_bash::BashTool;
use dsh_tool_fs::{ReadFileTool, WriteFileTool};
use dsh_tools::ToolRuntime;
use std::sync::Arc;

/// Mount the product spine on `ctx`. The loop is always last.
pub fn apply(ctx: &Context, adapter: Arc<dyn LlmAdapter>) -> Result<()> {
    ctx.provide(Arc::new(LlmRuntime::new(adapter)))?;
    ctx.provide(Arc::new(SessionStore::new()))?;
    let prompt = SystemPrompt::new();
    prompt.set_persona("You are DeepSeek Harness.");
    ctx.provide(Arc::new(prompt))?;
    ctx.provide(Arc::new(ToolRuntime::new()))?;
    ctx.provide(Arc::new(CommandRegistry::new()))?;
    ctx.provide(Arc::new(AgentRegistry::new()))?;
    AgentLoop::install(ctx)?;
    Ok(())
}

/// Mount sandbox, filesystem, shell, and their tool Consumers.
///
/// Tool crates depend only on Service Definitions. Local providers are wired
/// here. `workspace` is the sandbox root; relative tool paths must stay inside it.
pub fn apply_world(ctx: &Context, workspace: impl Into<String>) -> Result<()> {
    dsh_sandbox_local::install(ctx, workspace)?;
    dsh_fs_local::install(ctx)?;
    let subprocess = Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)));
    ctx.provide(Arc::clone(&subprocess))?;
    let shell = Arc::new(ShellRuntime::new(Arc::new(BashLocal::new(subprocess))));
    ctx.provide(Arc::clone(&shell))?;
    if let Some(tools) = ctx.get::<ToolRuntime>() {
        tools.insert(Arc::new(BashTool::new(shell)));
        if let Some(fs) = ctx.get::<FsRuntime>() {
            tools.insert(Arc::new(ReadFileTool::new(Arc::clone(&fs))));
            tools.insert(Arc::new(WriteFileTool::new(fs)));
        }
    }
    Ok(())
}

/// Spine with a scripted replay adapter.
pub fn apply_replay(ctx: &Context, text: &str) -> Result<()> {
    apply(ctx, Arc::new(ReplayAdapter::text(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::AgentRegistry;
    use dsh_agent_loop::run_followup;
    use dsh_llm::{ContentBlock, UserMessage};
    use dsh_session::SessionStore;

    #[tokio::test]
    async fn spine_runs_a_text_turn() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
        run_followup(
            handle.agent.as_ref(),
            UserMessage {
                content: vec![ContentBlock::text("ping")],
                source: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            handle.agent.session().last_assistant_text().as_deref(),
            Some("pong")
        );
    }

    #[tokio::test]
    async fn apply_world_registers_bash_and_fs_tools() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let root = std::env::temp_dir().join(format!("dsh-world-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
        let tools = ctx.service::<ToolRuntime>().unwrap();
        let names: Vec<_> = tools.schemas().into_iter().map(|schema| schema.name).collect();
        assert!(names.contains(&"bash".into()));
        assert!(names.contains(&"read_file".into()));
        assert!(names.contains(&"write_file".into()));
        assert!(ctx.has_service("sandbox"));
        assert!(ctx.has_service("fs"));
        assert!(ctx.has_service("shell"));
        assert!(ctx.has_service("subprocess"));
    }
}
