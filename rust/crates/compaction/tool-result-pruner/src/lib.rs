//! Tool-result pruner (`ctx.toolResultPruner`).
//!
//! Thresholds are Config at construction. The prune marker is a protocol
//! constant, not a tunable.

use dsh_cordis::{Context, Service};
use dsh_llm::ContentBlock;
use dsh_tools::ToolOutcome;
use serde_json::Value;
use std::sync::Arc;

/// Fixed marker substituted for every removed middle span.
pub const PRUNE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";

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

    /// Provide `ctx.toolResultPruner` and register `tools/post-execute`.
    pub fn install(
        ctx: &Context,
        threshold_chars: usize,
        head_chars: usize,
        tail_chars: usize,
    ) -> dsh_cordis::Result<Arc<Self>> {
        let pruner = Arc::new(Self::new(threshold_chars, head_chars, tail_chars));
        pruner.register(ctx)?;
        ctx.provide(Arc::clone(&pruner))?;
        Ok(pruner)
    }

    /// Register the post-execute waterfall that prunes `text` / `content`.
    pub fn register(self: &Arc<Self>, ctx: &Context) -> dsh_cordis::Result<()> {
        let pruner = Arc::clone(self);
        ctx.on_waterfall("tools/post-execute", move |mut payload, next| {
            if let Some(text) = payload.get("text").and_then(Value::as_str) {
                let pruned = pruner.prune_text(text);
                payload["text"] = Value::String(pruned);
            }
            if let Some(content) = payload.get("content").and_then(Value::as_array).cloned() {
                let blocks: Vec<ContentBlock> = content
                    .into_iter()
                    .filter_map(|block| serde_json::from_value(block).ok())
                    .collect();
                let pruned = pruner.prune_blocks(&blocks);
                payload["content"] = serde_json::to_value(pruned).unwrap_or(Value::Null);
            }
            next.call(payload)
        })
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
    use dsh_cordis::Context;

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
    fn register_post_execute_prunes_text() {
        let ctx = Context::new();
        let pruner = ToolResultPruner::install(&ctx, 64, 4, 4).unwrap();
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ!!!!";
        let expected = pruner.prune_text(long);
        assert!(expected.contains(PRUNE_MARKER));
        let result = ctx
            .waterfall(
                "tools/post-execute",
                serde_json::json!({ "name": "bash", "text": long }),
                |payload| payload,
            )
            .unwrap();
        assert_eq!(result["text"].as_str(), Some(expected.as_str()));
    }
}
