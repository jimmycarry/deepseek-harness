//! Compaction engine seam (`ctx.compaction`).

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

    #[test]
    fn trigger_names_stay_stable() {
        assert_ne!(
            CompactionTrigger::Pressure,
            CompactionTrigger::ContextOverflow
        );
    }
}
