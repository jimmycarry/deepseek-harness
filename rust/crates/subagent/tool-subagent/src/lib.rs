//! Model-facing subagent tool.

use async_trait::async_trait;
use dsh_subagent::SubagentRuntime;
use dsh_subagent_inprocess::delegate;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `subagent` over the in-process provider.
pub struct DelegateTool {
    subagents: Arc<SubagentRuntime>,
    scripted_reply: String,
}

impl DelegateTool {
    /// Bind to `ctx.subagents` with a scripted child reply.
    pub fn new(subagents: Arc<SubagentRuntime>, scripted_reply: impl Into<String>) -> Self {
        Self {
            subagents,
            scripted_reply: scripted_reply.into(),
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Delegate a task to an in-process child agent."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "prompt": { "type": "string" } },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("prompt required".into()))?;
        match delegate(&self.subagents, prompt, &self.scripted_reply).await {
            Ok(text) => Ok(ToolOutcome::text(text)),
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-subagent"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delegates_and_records() {
        let runtime = Arc::new(SubagentRuntime::new());
        let tool = DelegateTool::new(Arc::clone(&runtime), "child-done");
        let outcome = tool
            .execute(serde_json::json!({ "prompt": "do it" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert_eq!(runtime.results(), vec!["child-done".to_string()]);
    }
}
