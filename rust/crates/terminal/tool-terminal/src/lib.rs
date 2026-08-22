//! Model-facing terminal tool. Depends on the terminal Service Definition only.

use async_trait::async_trait;
use dsh_terminal::TerminalRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-terminal"
}

/// `terminal` tool: `open` a session or `write` into its history.
pub struct TerminalTool {
    terminal: Arc<TerminalRuntime>,
}

impl TerminalTool {
    /// Bind to `ctx.terminal`.
    pub fn new(terminal: Arc<TerminalRuntime>) -> Self {
        Self { terminal }
    }
}

#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }

    fn description(&self) -> &str {
        "Open a persistent terminal session or write text into one."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["open", "write"] },
                "id": { "type": "string" },
                "text": { "type": "string" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("open");
        match action {
            "write" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::Body("id required".into()))?;
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                self.terminal
                    .write(id, text)
                    .map_err(|error| ToolError::Body(error.to_string()))?;
                let history = self
                    .terminal
                    .history(id)
                    .map_err(|error| ToolError::Body(error.to_string()))?;
                Ok(ToolOutcome::text(history.join("")))
            }
            _ => {
                let id = self.terminal.open();
                Ok(ToolOutcome::text(id))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_then_write() {
        let terminal = Arc::new(TerminalRuntime::new());
        let tool = TerminalTool::new(Arc::clone(&terminal));
        let opened = tool
            .execute(serde_json::json!({ "action": "open" }))
            .await
            .unwrap();
        let id = match &opened.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text id"),
        };
        tool.execute(serde_json::json!({ "action": "write", "id": id, "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(terminal.history(&id).unwrap(), ["hi"]);
    }
}
