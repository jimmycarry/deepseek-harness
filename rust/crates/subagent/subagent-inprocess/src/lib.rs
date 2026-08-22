//! In-process spawn and fork providers.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_cordis::{Context, Result};
use dsh_llm::UserMessage;
use dsh_session::{event_type_name, SessionEventData, SessionStore};
use dsh_subagent::{
    SubagentError, SubagentProvider, SubagentResult, SubagentRuntime, SubagentStartRequest,
};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-subagent-inprocess"
}

/// Register `spawn` and/or `fork` on `ctx.subagents`.
pub fn install(ctx: &Context, provider_name: &str, inherits: bool) -> Result<()> {
    let runtime = ctx.service::<SubagentRuntime>()?;
    runtime
        .register_provider(Arc::new(InProcessProvider {
            ctx: ctx.clone(),
            name: provider_name.to_string(),
            inherits,
        }))
        .map_err(|error| dsh_cordis::CordisError::Validation(error.to_string()))?;
    Ok(())
}

struct InProcessProvider {
    ctx: Context,
    name: String,
    inherits: bool,
}

#[async_trait]
impl SubagentProvider for InProcessProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn inherits_parent_context(&self) -> bool {
        self.inherits
    }

    fn supports_continuable(&self) -> bool {
        // Fork stays one-shot: its inherited history must remain the exact
        // parent prefix, which a continuable child's contributions would break.
        !self.inherits
    }

    async fn start(
        &self,
        request: SubagentStartRequest,
    ) -> std::result::Result<SubagentResult, SubagentError> {
        let store = self
            .ctx
            .get::<SessionStore>()
            .ok_or_else(|| SubagentError::NoProvider("sessions".into()))?;
        let agents = self
            .ctx
            .get::<AgentRegistry>()
            .ok_or_else(|| SubagentError::NoProvider("agents".into()))?;
        let child = store.create_fresh();
        let _ = child.append(
            SessionEventData::Extension {
                type_name: "subagent/descriptor".into(),
                data: serde_json::json!({
                    "version": 2,
                    "mode": "one-shot",
                    "provider": self.name,
                    "label": request.label,
                }),
            },
            None,
        );
        if self.inherits {
            if let Some(parent) = store.get(&request.parent_id) {
                for event in completed_turn_prefix(&parent.events()) {
                    let _ = child.append(event.data, event.surface_op);
                }
            }
        }
        let handle = agents
            .create(Arc::clone(&child))
            .map_err(|error| SubagentError::NoProvider(error.to_string()))?;
        run_followup(handle.agent.as_ref(), UserMessage::text(request.prompt))
            .await
            .map_err(|error| SubagentError::NoProvider(error.to_string()))?;
        let output = handle
            .agent
            .session()
            .last_assistant_text()
            .unwrap_or_default();
        let id = handle.agent.id().clone();
        handle.dispose();
        Ok(SubagentResult {
            output,
            id,
            stop_reason: "completed".into(),
        })
    }
}

fn completed_turn_prefix(events: &[dsh_session::SessionEvent]) -> Vec<dsh_session::SessionEvent> {
    let last_end = events
        .iter()
        .rposition(|event| event_type_name(&event.data) == "turn/end");
    match last_end {
        Some(index) => events[..=index].to_vec(),
        None => Vec::new(),
    }
}

/// One-shot helper used by crate tests.
pub async fn delegate(
    runtime: &SubagentRuntime,
    prompt: &str,
    _scripted_reply: &str,
) -> dsh_cordis::Result<String> {
    let result = runtime
        .start(
            "spawn",
            SubagentStartRequest {
                label: "test".into(),
                prompt: prompt.into(),
                parent_id: dsh_session::session_id("parent"),
                seed: None,
            },
        )
        .await
        .map_err(|error| dsh_cordis::CordisError::MissingService(error.to_string()))?;
    Ok(result.output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_loop::AgentLoop;
    use dsh_llm::LlmRuntime;
    use dsh_llm_replay::ReplayAdapter;
    use dsh_session::session_id;

    #[tokio::test]
    async fn spawn_runs_child_on_same_context() {
        let ctx = Context::new();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(ReplayAdapter::text(
            "child-done",
        )))))
        .unwrap();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        install(&ctx, "spawn", false).unwrap();
        let result = ctx
            .service::<SubagentRuntime>()
            .unwrap()
            .start(
                "spawn",
                SubagentStartRequest {
                    label: "t".into(),
                    prompt: "do it".into(),
                    parent_id: session_id("parent"),
                    seed: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.output, "child-done");
    }
}
