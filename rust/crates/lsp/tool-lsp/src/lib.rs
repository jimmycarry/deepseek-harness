//! Model-facing lsp tool. Depends on the LSP Service Definition only.

use async_trait::async_trait;
use dsh_lsp::LspRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-lsp"
}

/// `lsp_initialize` tool.
pub struct LspInitializeTool {
    lsp: Arc<LspRuntime>,
}

impl LspInitializeTool {
    /// Bind to `ctx.lsp`.
    pub fn new(lsp: Arc<LspRuntime>) -> Self {
        Self { lsp }
    }
}

#[async_trait]
impl Tool for LspInitializeTool {
    fn name(&self) -> &str {
        "lsp_initialize"
    }

    fn description(&self) -> &str {
        "Initialize a language server for the workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "root": { "type": "string" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("lsp")
            .to_string();
        let result = self.lsp.initialize(name, args);
        Ok(ToolOutcome::text(result.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clone_name_before_moving_args() {
        let lsp = Arc::new(LspRuntime::new());
        let tool = LspInitializeTool::new(Arc::clone(&lsp));
        let args = serde_json::json!({ "name": "rust-analyzer", "root": "/tmp" });
        let outcome = tool.execute(args).await.unwrap();
        assert!(!outcome.is_error);
        let recorded = lsp.initialized().unwrap();
        assert_eq!(recorded.name, "rust-analyzer");
        assert_eq!(recorded.args["root"], "/tmp");
    }
}
