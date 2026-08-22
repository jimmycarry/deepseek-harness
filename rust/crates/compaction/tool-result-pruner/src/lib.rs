//! Tool-result pruner (`ctx.toolResultPruner`).
//!
//! Runs inside compaction, not on the execute pipeline: `prune_session` scans
//! surface `tool/result` events over the threshold, appends a log-only
//! `compaction/prune` record, and immediately lands a `tool/result`
//! replacement (`surfaceOp: replace`, `sourceEventSeqs: [seq]`) carrying the
//! pruned content. Thresholds are Config at construction; the prune marker is
//! a protocol constant, not a tunable.

use dsh_cordis::{Context, Service};
use dsh_llm::ContentBlock;
use dsh_session::{Session, SessionEventData, SurfaceOp};
use dsh_token_meter::TokenMeter;
use dsh_tools::ToolOutcome;
use std::sync::Arc;

/// Fixed marker substituted for every removed middle span.
pub const PRUNE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";

/// Character budgets validated from cordis.yml.
#[derive(Debug, Clone)]
pub struct Config {
    /// Results at or under this many characters are never pruned.
    pub threshold_chars: usize,
    /// Characters kept from the head.
    pub head_chars: usize,
    /// Characters kept from the tail.
    pub tail_chars: usize,
}

impl Config {
    /// Validate raw cordis.yml config; missing fields take the TypeScript
    /// defaults (8192 / 4096 / 1024).
    ///
    /// # Errors
    /// A non-positive `thresholdChars`, negative budget, or an emitted size
    /// (`headChars` + marker + `tailChars`) above `thresholdChars`.
    pub fn resolve(config: Option<&serde_json::Value>) -> Result<Self, String> {
        fn field(
            config: Option<&serde_json::Value>,
            key: &str,
            default: usize,
        ) -> Result<usize, String> {
            match config.and_then(|value| value.get(key)) {
                None => Ok(default),
                Some(value) => value.as_u64().map(|value| value as usize).ok_or_else(|| {
                    format!("tool-result-pruner: {key} must be a non-negative integer")
                }),
            }
        }
        let threshold_chars = field(config, "thresholdChars", 8192)?;
        if threshold_chars == 0 {
            return Err("tool-result-pruner: thresholdChars must be a positive integer".into());
        }
        let head_chars = field(config, "headChars", 4096)?;
        let tail_chars = field(config, "tailChars", 1024)?;
        let emitted = head_chars + PRUNE_MARKER.chars().count() + tail_chars;
        if emitted > threshold_chars {
            return Err(format!(
                "tool-result-pruner: headChars + marker + tailChars ({emitted}) must be at most thresholdChars ({threshold_chars})"
            ));
        }
        Ok(Self {
            threshold_chars,
            head_chars,
            tail_chars,
        })
    }
}

/// Replay-safe, model-free tool-result pruning service.
pub struct ToolResultPruner {
    threshold_chars: usize,
    head_chars: usize,
    tail_chars: usize,
}

impl ToolResultPruner {
    /// Build from explicit character budgets.
    pub fn new(threshold_chars: usize, head_chars: usize, tail_chars: usize) -> Self {
        let emitted = head_chars + PRUNE_MARKER.chars().count() + tail_chars;
        if threshold_chars == 0 {
            panic!("ToolResultPruner: threshold_chars must be a positive integer");
        }
        if emitted > threshold_chars {
            panic!(
                "ToolResultPruner: head_chars + marker + tail_chars ({emitted}) must be at most threshold_chars ({threshold_chars})"
            );
        }
        Self {
            threshold_chars,
            head_chars,
            tail_chars,
        }
    }

    /// Provide `ctx.toolResultPruner`.
    pub fn install(
        ctx: &Context,
        threshold_chars: usize,
        head_chars: usize,
        tail_chars: usize,
    ) -> dsh_cordis::Result<Arc<Self>> {
        let pruner = Arc::new(Self::new(threshold_chars, head_chars, tail_chars));
        ctx.provide(Arc::clone(&pruner))?;
        Ok(pruner)
    }

    /// Prune every over-threshold surface `tool/result` in `session`.
    ///
    /// Each prune appends `compaction/prune` then the `tool/result`
    /// replacement; the pair is adjacent in the log. Returns the seqs of the
    /// replaced results.
    pub fn prune_session(&self, session: &Session, meter: Option<&TokenMeter>) -> Vec<u64> {
        let surface = session.surface();
        let mut pruned = Vec::new();
        for seq in surface.nodes {
            let Some(event) = session.events().into_iter().find(|event| event.seq == seq) else {
                continue;
            };
            let SessionEventData::ToolResult {
                turn,
                step,
                message,
            } = &event.data
            else {
                continue;
            };
            let blocks = message.result_blocks().to_vec();
            let replaced = self.prune_blocks(&blocks);
            if replaced == blocks {
                continue;
            }
            let shadowed_tokens = meter
                .map(|meter| meter.estimate_content(&blocks))
                .unwrap_or(0);
            let replacement = dsh_llm::ToolResultMessage::new(
                message
                    .tool_call_id()
                    .map(dsh_llm::call_id)
                    .unwrap_or_else(|| dsh_llm::call_id("")),
                replaced,
                message.is_error(),
            );
            let record = session.append(
                SessionEventData::CompactionPrune {
                    shadowed_range: serde_json::json!({ "start": seq, "end": seq }),
                    shadowed_seqs: vec![seq],
                    shadowed_token_count: shadowed_tokens as u64,
                },
                None,
            );
            if record.is_err() {
                continue;
            }
            let landed = session.append_cited(
                SessionEventData::ToolResult {
                    turn: *turn,
                    step: *step,
                    message: replacement,
                },
                SurfaceOp::Replace {
                    start: seq,
                    end: seq,
                },
                vec![seq],
            );
            if landed.is_ok() {
                pruned.push(seq);
            }
        }
        pruned
    }

    /// Replace an over-budget text middle while retaining head and tail.
    pub fn prune_text(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= self.threshold_chars {
            return text.to_string();
        }
        let head: String = chars.iter().take(self.head_chars).collect();
        let tail_start = chars.len().saturating_sub(self.tail_chars);
        let tail: String = chars[tail_start..].iter().collect();
        format!("{head}{PRUNE_MARKER}{tail}")
    }

    /// Prune text blocks; non-text blocks pass through unchanged.
    pub fn prune_blocks(&self, blocks: &[ContentBlock]) -> Vec<ContentBlock> {
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => ContentBlock::text(self.prune_text(text)),
                other => other.clone(),
            })
            .collect()
    }

    /// Prune the model-visible content of one tool outcome.
    pub fn prune_outcome(&self, outcome: ToolOutcome) -> ToolOutcome {
        ToolOutcome {
            content: self.prune_blocks(&outcome.content),
            is_error: outcome.is_error,
        }
    }
}

impl Service for ToolResultPruner {
    const KEY: &'static str = "toolResultPruner";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{call_id, ToolResultMessage};
    use dsh_session::session_id;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ToolResultPruner::new(64, 4, 4)))
            .unwrap();
        assert!(ctx.has_service("toolResultPruner"));
        ctx.dispose();
        assert!(!ctx.has_service("toolResultPruner"));
    }

    #[test]
    fn prune_long_text_inserts_marker() {
        let pruner = ToolResultPruner::new(64, 4, 4);
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ!!!!";
        let pruned = pruner.prune_text(long);
        assert!(pruned.contains(PRUNE_MARKER));
        assert!(pruned.starts_with("abcd"));
        assert!(pruned.ends_with("!!!!"));
        assert_eq!(pruner.prune_text("short"), "short");
    }

    #[test]
    fn prune_session_writes_prune_record_and_replacement() {
        let session = Session::new(session_id("prune"));
        let long = "x".repeat(100);
        session
            .append(
                SessionEventData::ToolResult {
                    turn: 1,
                    step: 1,
                    message: ToolResultMessage::new(
                        call_id("c1"),
                        vec![ContentBlock::text(long)],
                        false,
                    ),
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        let pruner = ToolResultPruner::new(64, 4, 4);
        let meter = TokenMeter::new(4);
        let pruned = pruner.prune_session(&session, Some(&meter));
        assert_eq!(pruned, vec![0]);
        let events = session.events();
        assert_eq!(events.len(), 3);
        match &events[1].data {
            SessionEventData::CompactionPrune {
                shadowed_seqs,
                shadowed_token_count,
                shadowed_range,
            } => {
                assert_eq!(shadowed_seqs, &vec![0]);
                assert!(*shadowed_token_count > 0);
                assert_eq!(shadowed_range["start"], 0);
                assert_eq!(shadowed_range["end"], 0);
            }
            other => panic!("expected compaction/prune, got {other:?}"),
        }
        assert_eq!(
            events[2].surface_op,
            Some(SurfaceOp::Replace { start: 0, end: 0 })
        );
        assert_eq!(events[2].source_event_seqs, Some(vec![0]));
        match &events[2].data {
            SessionEventData::ToolResult { message, .. } => {
                let text = match &message.result_blocks()[0] {
                    ContentBlock::Text { text } => text.clone(),
                    other => panic!("expected text, got {other:?}"),
                };
                assert!(text.contains(PRUNE_MARKER));
            }
            other => panic!("expected tool/result, got {other:?}"),
        }
        assert!(pruner.prune_session(&session, Some(&meter)).is_empty());
    }
}
