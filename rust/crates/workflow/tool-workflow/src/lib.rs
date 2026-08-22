//! workflow/ralph tools.

use async_trait::async_trait;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use dsh_workflow::WorkflowRuntime;
use serde_json::Value;
use std::sync::Arc;

/// `workflow` over [`WorkflowRuntime`].
pub struct RunWorkflowTool {
    engine: Arc<WorkflowRuntime>,
}

impl RunWorkflowTool {
    /// Bind to `ctx.workflowEngine`.
    pub fn new(engine: Arc<WorkflowRuntime>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }

    fn description(&self) -> &str {
        "Run a workflow script and return its result."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "script": { "type": "string" } },
            "required": ["script"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let script = args
            .get("script")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("script required".into()))?;
        Ok(ToolOutcome::text(self.engine.run(script).await))
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-workflow"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_workflow::WorkflowConfig;

    #[tokio::test]
    async fn runs_script_with_isolation() {
        let engine = Arc::new(WorkflowRuntime::new(WorkflowConfig {
            isolation: "in-process".into(),
        }));
        let tool = RunWorkflowTool::new(engine);
        let outcome = tool
            .execute(serde_json::json!({ "script": "return 1" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
    }
}
