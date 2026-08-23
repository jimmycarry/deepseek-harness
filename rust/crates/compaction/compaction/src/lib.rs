//! Compaction engine seam (`ctx.compaction`).

mod tool_pairing;

pub use tool_pairing::{tool_pairing_balanced_after, tool_pairing_balanced_before};

use async_trait::async_trait;
use dsh_agent::Agent;
use dsh_cordis::Service;
use dsh_llm::ContentBlock;
use thiserror::Error;

/// Why automatic policy is asking a backend to consider compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Step-boundary token pressure.
    Pressure,
    /// Provider-confirmed context overflow.
    ContextOverflow,
}

/// Result of a successful compaction.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Shadowed surface seqs in surface order.
    pub shadowed_seqs: Vec<u64>,
    /// Estimated token count of the shadowed span.
    pub shadowed_token_count: u64,
    /// Summary content.
    pub summary: Vec<ContentBlock>,
    /// Seq of the log-only `compaction/summary` event.
    pub summary_seq: u64,
}

/// Manual compaction failures.
#[derive(Debug, Error)]
pub enum ManualCompactionError {
    /// A compaction lock is already held.
    #[error("busy")]
    Busy,
    /// No safe useful range.
    #[error("no range")]
    NoRange,
    /// The summarizer produced no useful checkpoint.
    #[error("summary")]
    Summary,
    /// Routed pressure cannot resolve a usable adapter capacity or retain budget.
    #[error("{message}")]
    PressureConfig {
        /// Exact `provider/model` route used as the warning key.
        target: String,
        /// Actionable configuration failure, matching TypeScript wording.
        message: String,
    },
    /// Automatic pressure exhausted its retry budget above the threshold.
    #[error(
        "compaction still above threshold after {attempts} compaction attempts ({tokens} estimated tokens >= threshold {threshold})"
    )]
    StillAbove {
        /// Number of compaction attempts that already landed.
        attempts: u32,
        /// Estimated tokens after the last attempt.
        tokens: u64,
        /// Routed threshold that was still unmet.
        threshold: u64,
    },
}

/// `ctx.compaction`.
#[async_trait]
pub trait CompactionEngine: Send + Sync {
    /// Consider automatic compaction for one trigger.
    async fn compact_if_needed(
        &self,
        agent: &dyn Agent,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, ManualCompactionError>;

    /// Compact useful history even below pressure. A manual `/compact`
    /// attempt passes its command id so the attempt's session events carry
    /// `sourceCommandId`.
    async fn compact_now(
        &self,
        agent: &dyn Agent,
        source_command_id: Option<&str>,
    ) -> Result<Option<CompactionResult>, ManualCompactionError>;
}

/// Service wrapper so a backend can be provided as `ctx.compaction`.
pub struct CompactionRuntime {
    engine: std::sync::Arc<dyn CompactionEngine>,
}

impl CompactionRuntime {
    /// Wrap an engine.
    pub fn new(engine: std::sync::Arc<dyn CompactionEngine>) -> Self {
        Self { engine }
    }

    /// Borrow the engine.
    pub fn engine(&self) -> &dyn CompactionEngine {
        &*self.engine
    }
}

impl Service for CompactionRuntime {
    const KEY: &'static str = "compaction";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{call_id, AssistantMessage, ContentBlock, ToolResultMessage, UserMessage};
    use dsh_session::{session_id, Session, SessionEventData, SurfaceOp};

    #[test]
    fn trigger_names_stay_stable() {
        assert_ne!(
            CompactionTrigger::Pressure,
            CompactionTrigger::ContextOverflow
        );
    }

    fn append_user(session: &Session, text: &str) {
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text(text)),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }

    fn append_closed_tool_step(session: &Session, call: &str) {
        session
            .append(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::ToolCall {
                            id: call_id(call),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        }],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::ToolResult {
                    turn: 1,
                    step: 1,
                    message: ToolResultMessage::new(
                        call_id(call),
                        vec![ContentBlock::text("done")],
                        false,
                    ),
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }

    #[test]
    fn closed_tool_step_is_balanced_only_outside_the_pair() {
        let session = Session::new(session_id("closed-tool-step"));
        append_user(&session, "go");
        append_closed_tool_step(&session, "c1");
        let nodes = session.surface().nodes;
        assert_eq!(nodes.len(), 3);
        assert!(tool_pairing_balanced_before(&session, nodes[0]).unwrap());
        assert!(tool_pairing_balanced_after(&session, nodes[0]).unwrap());
        assert!(tool_pairing_balanced_before(&session, nodes[1]).unwrap());
        assert!(!tool_pairing_balanced_after(&session, nodes[1]).unwrap());
        assert!(!tool_pairing_balanced_before(&session, nodes[2]).unwrap());
        assert!(tool_pairing_balanced_after(&session, nodes[2]).unwrap());
    }

    #[test]
    fn missing_surface_seq_fails_loud() {
        let session = Session::new(session_id("missing"));
        append_user(&session, "go");
        let err = tool_pairing_balanced_before(&session, 999).unwrap_err();
        assert!(err.contains("surface seq 999 not found"), "{err}");
    }
}
