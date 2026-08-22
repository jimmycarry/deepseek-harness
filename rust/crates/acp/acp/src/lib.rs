//! Automation-only Agent Client Protocol server.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_cordis::Context;
use dsh_llm::UserMessage;
use dsh_session::SessionStore;
use serde_json::Value;

/// One ACP prompt exchange against the spine.
pub async fn prompt(ctx: &Context, text: &str) -> Result<Value, String> {
    let store = ctx.service::<SessionStore>().map_err(|error| error.to_string())?;
    let session = store.create_fresh();
    let agents = ctx.service::<AgentRegistry>().map_err(|error| error.to_string())?;
    let handle = agents.create(session).map_err(|error| error.to_string())?;
    run_followup(
        handle.agent.as_ref(),
        UserMessage::text(text),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "text": handle.agent.session().last_assistant_text(),
        "log": handle.agent.session().events(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_spine::apply_replay;

    #[tokio::test]
    async fn acp_prompt_returns_log() {
        let ctx = Context::new();
        apply_replay(&ctx, "ok").unwrap();
        let result = prompt(&ctx, "hi").await.unwrap();
        assert_eq!(result["text"], "ok");
        assert!(result["log"].as_array().unwrap().len() >= 4);
    }
}
