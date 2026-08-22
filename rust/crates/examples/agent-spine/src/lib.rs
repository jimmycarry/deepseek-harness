//! Runnable agent spine composition. Mirrors `dsh-agent-spine-demo`.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::AgentLoop;
use dsh_cordis::{Context, Result};
use dsh_llm::{LlmAdapter, LlmRuntime};
use dsh_llm_replay::ReplayAdapter;
use dsh_session::SessionStore;
use dsh_system_prompt::SystemPrompt;
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
    ctx.provide(Arc::new(AgentRegistry::new()))?;
    AgentLoop::install(ctx)?;
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
}
