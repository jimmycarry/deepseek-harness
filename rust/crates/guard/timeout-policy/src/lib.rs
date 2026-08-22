//! Tool-call timeout policy: enforce each tool's registration-declared
//! `timeoutMs` deadline. The policy carries no tunables of its own; a tool
//! without a declared deadline is left untouched. A deadline hit replaces the
//! body result with the model-visible failure
//! `Error: tool call timed out after {timeoutMs}ms` (code `TOOL_TIMEOUT`).

use dsh_cordis::{Context, Result, Service};
use std::sync::Arc;

pub use dsh_tools::{TOOL_TIMEOUT, TOOL_TIMEOUT_POLICY_KEY};

/// Marker service consulted by `ctx.tools` before arming a tool's deadline.
#[derive(Default)]
pub struct ToolCallTimeoutPolicy;

impl Service for ToolCallTimeoutPolicy {
    const KEY: &'static str = TOOL_TIMEOUT_POLICY_KEY;
}

/// Arm deadline enforcement for tools that declare `timeout_ms`.
///
/// # Errors
/// A duplicate policy registration.
pub fn install(ctx: &Context) -> Result<()> {
    ctx.provide(Arc::new(ToolCallTimeoutPolicy))
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-timeout-policy"
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_tools::{Tool, ToolError, ToolOutcome, ToolRuntime};
    use serde_json::{json, Value};

    struct SlowTool {
        delay_ms: u64,
        timeout_ms: Option<u64>,
    }

    #[async_trait]
    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn description(&self) -> &str {
            "slow"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }
        fn timeout_ms(&self) -> Option<u64> {
            self.timeout_ms
        }
        async fn execute(&self, _: Value) -> Result<ToolOutcome, ToolError> {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            Ok(ToolOutcome::text("done"))
        }
    }

    fn outcome_text(outcome: &ToolOutcome) -> String {
        outcome
            .content
            .iter()
            .filter_map(|block| match block {
                dsh_llm::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn deadline_hit_replaces_result_with_timeout_failure() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(SlowTool {
            delay_ms: 200,
            timeout_ms: Some(20),
        }));
        ctx.provide(Arc::clone(&tools)).unwrap();
        install(&ctx).unwrap();
        let outcome = tools.execute(&ctx, "slow", json!({})).await.unwrap();
        assert!(outcome.is_error);
        assert_eq!(
            outcome_text(&outcome),
            "Error: tool call timed out after 20ms"
        );
    }

    #[tokio::test]
    async fn undeclared_deadline_and_unmounted_policy_leave_bodies_untouched() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(SlowTool {
            delay_ms: 30,
            timeout_ms: None,
        }));
        ctx.provide(Arc::clone(&tools)).unwrap();
        install(&ctx).unwrap();
        let outcome = tools.execute(&ctx, "slow", json!({})).await.unwrap();
        assert!(!outcome.is_error);

        let bare = Context::new();
        let bare_tools = Arc::new(ToolRuntime::new());
        bare_tools.insert(Arc::new(SlowTool {
            delay_ms: 50,
            timeout_ms: Some(5),
        }));
        bare.provide(Arc::clone(&bare_tools)).unwrap();
        let outcome = bare_tools.execute(&bare, "slow", json!({})).await.unwrap();
        assert!(!outcome.is_error, "no policy service, no deadline");
    }
}
