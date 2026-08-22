//! Model-facing bash tool. Depends on the shell Service Definition only.

use async_trait::async_trait;
use dsh_shell::{resolve, ShellRequest, ShellRuntime};
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `bash` tool.
pub struct BashTool {
    shell: Arc<ShellRuntime>,
}

impl BashTool {
    /// Bind to `ctx.shell`.
    pub fn new(shell: Arc<ShellRuntime>) -> Self {
        Self { shell }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a bash command in the workspace."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("command required".into()))?;
        let spec = resolve(ShellRequest {
            command: command.into(),
            cwd: None,
        });
        match self.shell.run(spec).await {
            Ok(stdout) => Ok(ToolOutcome::text(stdout)),
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

pub fn name() -> &'static str {
    "dsh-tool-bash"
}
