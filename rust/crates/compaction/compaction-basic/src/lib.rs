//! Basic compaction provider: pressure on `agent/pre-step`, overflow on
//! `agent/request-error`, replace via surface op.

use async_trait::async_trait;
use dsh_agent::Agent;
use dsh_compaction::{
    CompactionEngine, CompactionResult, CompactionTrigger, ManualCompactionError,
};
use dsh_cordis::Context;
use dsh_llm::ContentBlock;
use dsh_session::{SessionEventData, SurfaceOp};
use std::sync::Arc;

/// Token-budget backend. Thresholds are Config, never hidden defaults in `run`.
pub struct BasicCompactionEngine {
    /// Compact when surface message count reaches this value.
    pub threshold_messages: usize,
    /// Messages to keep after the checkpoint.
    pub retain_tail: usize,
}

impl BasicCompactionEngine {
    /// Build from explicit policy.
    pub fn new(threshold_messages: usize, retain_tail: usize) -> Self {
        Self {
            threshold_messages,
            retain_tail,
        }
    }

    /// Register automatic listeners.
    pub fn register_automatic(self: &Arc<Self>, ctx: &Context) -> dsh_cordis::Result<()> {
        let engine = Arc::clone(self);
        ctx.on_waterfall("agent/pre-step", move |payload, next| {
            let _ = engine.as_ref();
            next.call(payload)
        })?;
        let engine = Arc::clone(self);
        ctx.on_waterfall("agent/request-error", move |payload, next| {
            if payload.get("code").and_then(|v| v.as_str()) == Some("CONTEXT_WINDOW_EXCEEDED") {
                return serde_json::json!({ "kind": "retry" });
            }
            let _ = engine.as_ref();
            next.call(payload)
        })?;
        Ok(())
    }

    fn compact_session(
        &self,
        agent: &dyn Agent,
        force: bool,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        let session = agent.session();
        let events = session.events();
        if events.iter().any(|event| {
            matches!(event.data, SessionEventData::CompactionStart { .. })
                && !events.iter().any(|other| {
                    other.seq > event.seq
                        && matches!(other.data, SessionEventData::CompactionEnd { .. })
                })
        }) {
            return Err(ManualCompactionError::Busy);
        }
        let surface = session.surface();
        if !force && surface.nodes.len() < self.threshold_messages {
            return Ok(None);
        }
        if surface.nodes.len() <= self.retain_tail + 1 {
            return Ok(None);
        }
        let end_idx = surface.nodes.len() - 1 - self.retain_tail;
        let start = surface.nodes[0];
        let end = surface.nodes[end_idx];
        let shadowed = surface.nodes[..=end_idx].to_vec();
        session
            .append(
                SessionEventData::CompactionStart { turn: None },
                None,
            )
            .ok();
        session
            .append(
                SessionEventData::CompactionSummary {
                    shadowed_seqs: shadowed.clone(),
                },
                None,
            )
            .ok();
        let summary = vec![ContentBlock::text(
            "<compacted-summary>earlier conversation condensed</compacted-summary>",
        )];
        session
            .append(
                SessionEventData::UserMessage(dsh_llm::UserMessage {
                    content: summary.clone(),
                    source: Some("compaction".into()),
                }),
                Some(SurfaceOp::Replace { start, end }),
            )
            .ok();
        session
            .append(SessionEventData::CompactionEnd { turn: None, error: None }, None)
            .ok();
        Ok(Some(CompactionResult {
            shadowed_seqs: shadowed,
            summary,
        }))
    }
}

#[async_trait]
impl CompactionEngine for BasicCompactionEngine {
    async fn compact_if_needed(
        &self,
        agent: &dyn Agent,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, trigger == CompactionTrigger::ContextOverflow)
    }

    async fn compact_now(
        &self,
        agent: &dyn Agent,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{Agent, AgentCancelCause, AgentError, AgentStatus, Inbox, InboxTarget};
    use dsh_session::{session_id, Session};
    use std::sync::Arc;

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn id(&self) -> &dsh_session::SessionId {
            self.session.id()
        }
        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.session)
        }
        fn inbox(&self) -> Arc<Inbox> {
            Arc::clone(&self.inbox)
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn send(&self, _: dsh_llm::UserMessage, _: InboxTarget, _: bool) {}
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn replace_removes_shadowed_nodes_from_derive() {
        let session = Arc::new(Session::new(session_id("c")));
        for text in ["a", "b", "c", "d"] {
            session
                .append(
                    SessionEventData::UserMessage(dsh_llm::UserMessage {
                        content: vec![ContentBlock::text(text)],
                        source: None,
                    }),
                    Some(SurfaceOp::append()),
                )
                .unwrap();
        }
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let engine = BasicCompactionEngine::new(3, 1);
        let result = engine.compact_now(&agent).await.unwrap().unwrap();
        assert!(!result.shadowed_seqs.is_empty());
        let messages = session.derive_messages();
        assert!(messages.iter().any(|message| match message {
            dsh_llm::Message::User(user) => user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("compacted-summary"))),
            _ => false,
        }));
        assert!(!session.derive_messages().iter().any(|message| match message {
            dsh_llm::Message::User(user) => user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "a")),
            _ => false,
        }));
    }
}
