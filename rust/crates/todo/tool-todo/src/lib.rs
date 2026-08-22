//! `todo_write` tool.

use async_trait::async_trait;
use dsh_session::{Session, SessionEventData};
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `todo_write` appends an ignorable `todo/write` snapshot.
pub struct TodoWriteTool {
    session: Arc<Session>,
}

impl TodoWriteTool {
    /// Bind to the calling agent's session.
    pub fn new(session: Arc<Session>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Record and update a structured task list for the current work."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": { "type": "string" }
                        }
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let todos = args.get("todos").cloned().unwrap_or(Value::Array(vec![]));
        self.session
            .append_ignorable(SessionEventData::Extension {
                type_name: "todo/write".into(),
                data: serde_json::json!({ "todos": todos }),
            })
            .map_err(|error| ToolError::Body(error.to_string()))?;
        Ok(ToolOutcome::text("updated todos"))
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-todo"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::session_id;

    #[tokio::test]
    async fn appends_ignorable_todo_write() {
        let session = Arc::new(Session::new(session_id("s")));
        let tool = TodoWriteTool::new(Arc::clone(&session));
        tool.execute(serde_json::json!({
            "todos": [{ "content": "ship", "status": "pending" }]
        }))
        .await
        .unwrap();
        let event = &session.events()[0];
        assert!(event.ignorable);
        match &event.data {
            SessionEventData::Extension { type_name, data } => {
                assert_eq!(type_name, "todo/write");
                assert!(data.get("todos").is_some());
            }
            other => panic!("expected extension, got {other:?}"),
        }
    }
}
