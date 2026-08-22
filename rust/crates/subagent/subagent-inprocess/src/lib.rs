//! In-process subagent provider.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_agent_spine::apply_replay;
use dsh_cordis::{Context, CordisError, Result};
use dsh_llm::{ContentBlock, UserMessage};
use dsh_session::SessionStore;
use dsh_subagent::SubagentRuntime;

/// Spawn a child spine, run one followup, and record the assistant text.
pub async fn delegate(
    runtime: &SubagentRuntime,
    prompt: &str,
    scripted_reply: &str,
) -> Result<String> {
    let child = Context::new();
    apply_replay(&child, scripted_reply).map_err(map_agent)?;
    let session = child.service::<SessionStore>()?.create_fresh();
    let handle = child
        .service::<AgentRegistry>()?
        .create(session)
        .map_err(map_agent)?;
    run_followup(
        handle.agent.as_ref(),
        UserMessage {
            content: vec![ContentBlock::text(prompt)],
            source: None,
        },
    )
    .await
    .map_err(map_agent)?;
    let result = handle
        .agent
        .session()
        .last_assistant_text()
        .unwrap_or_default();
    runtime.record(result.clone());
    Ok(result)
}

fn map_agent(error: impl std::fmt::Display) -> CordisError {
    CordisError::MissingService(error.to_string())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-subagent-inprocess"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delegate_records_replayed_text() {
        let runtime = SubagentRuntime::new();
        let result = delegate(&runtime, "ping", "pong").await.unwrap();
        assert_eq!(result, "pong");
        assert_eq!(runtime.results(), vec!["pong".to_string()]);
    }
}
