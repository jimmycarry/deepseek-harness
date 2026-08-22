//! Model-facing filesystem tools. Depends on the FS Service Definition only.

use async_trait::async_trait;
use dsh_fs::FsRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `read_file` tool.
pub struct ReadFileTool {
    fs: Arc<FsRuntime>,
}

impl ReadFileTool {
    /// Bind to `ctx.fs`.
    pub fn new(fs: Arc<FsRuntime>) -> Self {
        Self { fs }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("path required".into()))?;
        match self.fs.read_text(path).await {
            Ok(text) => Ok(ToolOutcome::text(text)),
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

/// `write_file` tool.
pub struct WriteFileTool {
    fs: Arc<FsRuntime>,
}

impl WriteFileTool {
    /// Bind to `ctx.fs`.
    pub fn new(fs: Arc<FsRuntime>) -> Self {
        Self { fs }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a text file in the workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        self.fs
            .write_text(path, content)
            .await
            .map_err(|error| ToolError::Body(error.to_string()))?;
        Ok(ToolOutcome::text(format!("wrote {path}")))
    }
}

pub fn name() -> &'static str {
    "dsh-tool-fs"
}
