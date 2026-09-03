//! In-process spawn and fork providers.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_cordis::{Context, Result};
use dsh_llm::UserMessage;
use dsh_session::{event_type_name, SessionEventData, SessionHeader, SessionStore};
use dsh_subagent::{
    append_delegated_policy_overrides, capture_delegated_policy_overrides, SubagentError,
    SubagentProvider, SubagentResult, SubagentRuntime, SubagentStartRequest,
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

    fn supports_output_schema(&self) -> bool {
        true
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
        let parent = store.get(&request.parent_id);
        let inherited = capture_delegated_policy_overrides(&self.ctx, parent.as_deref());
        let header = SessionHeader::for_subagent_child(
            parent.as_ref().map(|session| session.header()),
            request.parent_id.clone(),
        );
        let child = store.publish(dsh_session::Session::with_header(header));
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
        append_delegated_policy_overrides(child.as_ref(), &inherited)
            .map_err(|error| SubagentError::NoProvider(error.to_string()))?;
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
        assert!(ctx
            .service::<SubagentRuntime>()
            .unwrap()
            .get_provider("spawn")
            .unwrap()
            .supports_output_schema());
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
        let child = ctx
            .service::<SessionStore>()
            .unwrap()
            .get(&result.id)
            .unwrap();
        assert!(child.events().iter().all(|event| {
            !matches!(
                event.data,
                SessionEventData::SandboxMode { .. } | SessionEventData::ApprovalPolicy { .. }
            )
        }));
        install(&ctx, "fork", true).unwrap();
        assert!(ctx
            .service::<SubagentRuntime>()
            .unwrap()
            .get_provider("fork")
            .unwrap()
            .supports_output_schema());
        assert!(ctx
            .service::<SubagentRuntime>()
            .unwrap()
            .get_provider("fork")
            .unwrap()
            .inherits_parent_context());
    }

    fn policy_root() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    fn inheritance_host(reply: &str) -> Context {
        let ctx = Context::new();
        ctx.provide(Arc::new(dsh_system_prompt::SystemPrompt::new()))
            .unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(ReplayAdapter::text(
            reply,
        )))))
        .unwrap();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&serde_json::json!({
                "mode": "workspace-write",
                "workspaceRoot": policy_root()
            })),
        )
        .unwrap();
        dsh_user_approval::install(&ctx, None).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        install(&ctx, "spawn", false).unwrap();
        install(&ctx, "fork", true).unwrap();
        ctx
    }

    fn policy_events(
        session: &dsh_session::Session,
    ) -> Vec<dsh_session::SessionEvent> {
        session
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event.data,
                    SessionEventData::SandboxMode { .. } | SessionEventData::ApprovalPolicy { .. }
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn spawn_records_parent_sandbox_override_and_approval_pin() {
        let ctx = inheritance_host("child-done");
        let parent = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("parent"));
        dsh_sandbox_policy::set_sandbox_mode(parent.as_ref(), dsh_sandbox::SandboxMode::ReadOnly)
            .unwrap();
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
        let child = ctx
            .service::<SessionStore>()
            .unwrap()
            .get(&result.id)
            .unwrap();
        let policy = policy_events(child.as_ref());
        assert_eq!(policy.len(), 2);
        match &policy[0].data {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "read-only");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match &policy[1].data {
            SessionEventData::ApprovalPolicy { policy, source } => {
                assert_eq!(policy, "never");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            ctx.service::<dsh_sandbox_policy::SandboxPolicyService>()
                .unwrap()
                .override_of(child.as_ref()),
            Some(dsh_sandbox::SandboxMode::ReadOnly)
        );
        assert_eq!(
            ctx.service::<dsh_user_approval::ApprovalService>()
                .unwrap()
                .override_of(child.as_ref()),
            Some(dsh_user_approval::ApprovalPolicy::Never)
        );
        let context = child
            .events()
            .into_iter()
            .find_map(|event| match event.data {
                SessionEventData::UserMessage(message) => match &message.source {
                    dsh_llm::MessageSource::Plugin { plugin, .. }
                        if plugin == "@deepseek-ai/dsh-system-prompt" =>
                    {
                        Some(message)
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("runtime context");
        let text: String = message_text(&context);
        assert!(text.contains("You are a delegated subagent"), "{text}");
        assert!(text.contains("Current DSH file policy: read-only"), "{text}");
        assert!(text.contains("Approval prompts are disabled"), "{text}");
    }

    fn message_text(message: &dsh_llm::UserMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                dsh_llm::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn spawn_leaves_unswitched_sandbox_on_the_deployment_default() {
        let ctx = inheritance_host("child-done");
        ctx.service::<SessionStore>()
            .unwrap()
            .create(session_id("parent"));
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
        let child = ctx
            .service::<SessionStore>()
            .unwrap()
            .get(&result.id)
            .unwrap();
        let policy = policy_events(child.as_ref());
        assert_eq!(policy.len(), 1);
        match &policy[0].data {
            SessionEventData::ApprovalPolicy { policy, source } => {
                assert_eq!(policy, "never");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_places_inherited_policy_after_the_seed_prefix() {
        let ctx = inheritance_host("child-done");
        let parent = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("parent"));
        dsh_sandbox_policy::set_sandbox_mode(
            parent.as_ref(),
            dsh_sandbox::SandboxMode::WorkspaceWrite,
        )
        .unwrap();
        parent
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        parent
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: dsh_session::TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        dsh_sandbox_policy::set_sandbox_mode(parent.as_ref(), dsh_sandbox::SandboxMode::ReadOnly)
            .unwrap();
        let result = ctx
            .service::<SubagentRuntime>()
            .unwrap()
            .start(
                "fork",
                SubagentStartRequest {
                    label: "t".into(),
                    prompt: "do it".into(),
                    parent_id: session_id("parent"),
                    seed: None,
                },
            )
            .await
            .unwrap();
        let child = ctx
            .service::<SessionStore>()
            .unwrap()
            .get(&result.id)
            .unwrap();
        let sandbox: Vec<_> = child
            .events()
            .into_iter()
            .filter(|event| matches!(event.data, SessionEventData::SandboxMode { .. }))
            .collect();
        assert_eq!(sandbox.len(), 2);
        match &sandbox[0].data {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "workspace-write");
                assert!(source.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        match &sandbox[1].data {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "read-only");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            dsh_sandbox_policy::effective_sandbox_mode(&child.events()),
            Some(dsh_sandbox::SandboxMode::ReadOnly)
        );
    }
}
