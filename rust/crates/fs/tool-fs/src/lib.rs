//! Model-facing filesystem tools. Depends on the FS Service Definition only.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{
    error_from_event, fs_event_payload, FsObservation, FsObservationActor, FsRuntime, FsWriteIntent,
    FS_OBSERVED, FS_WRITE_INTENT,
};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome};
use serde_json::{json, Value};
use std::sync::Arc;

/// `read_file` tool.
pub struct ReadFileTool {
    fs: Arc<FsRuntime>,
    ctx: Context,
}

impl ReadFileTool {
    /// Bind to `ctx.fs` and the plugin context used for `fs/observed`.
    pub fn new(fs: Arc<FsRuntime>, ctx: Context) -> Self {
        Self { fs, ctx }
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
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let path = call
            .args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("path required".into()))?;
        let actor = FsObservationActor::from_agent_id(call.agent_id.as_deref());
        let target = match self.fs.resolve(path).await {
            Ok(target) => target,
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
        match self.fs.read_text(&target.target_key).await {
            Ok(text) => {
                if let Ok(Some(version)) = self.fs.version_of(&target).await {
                    self.ctx.emit(
                        FS_OBSERVED,
                        fs_event_payload(
                            &target,
                            &actor,
                            Some(&FsObservation::Present { version }),
                        ),
                    );
                }
                Ok(ToolOutcome::text(text))
            }
            Err(error) => {
                if self.fs.stat(&target.target_key).await.ok().flatten().is_none() {
                    self.ctx.emit(
                        FS_OBSERVED,
                        fs_event_payload(&target, &actor, Some(&FsObservation::Absent)),
                    );
                }
                Ok(ToolOutcome::error(error.to_string()))
            }
        }
    }
}

/// `write_file` tool.
pub struct WriteFileTool {
    fs: Arc<FsRuntime>,
    ctx: Context,
}

impl WriteFileTool {
    /// Bind to `ctx.fs` and the plugin context used for `fs/*` events.
    pub fn new(fs: Arc<FsRuntime>, ctx: Context) -> Self {
        Self { fs, ctx }
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
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let path = call.args.get("path").and_then(Value::as_str).unwrap_or("");
        let content = call.args.get("content").and_then(Value::as_str).unwrap_or("");
        let actor = FsObservationActor::from_agent_id(call.agent_id.as_deref());
        let target = self
            .fs
            .resolve(path)
            .await
            .map_err(|error| ToolError::Body(error.to_string()))?;
        let intent = self
            .ctx
            .waterfall(
                FS_WRITE_INTENT,
                fs_event_payload(&target, &actor, None),
                |_| json!(null),
            )
            .ok()
            .and_then(|value| {
                if error_from_event(&value).is_some() {
                    None
                } else {
                    FsWriteIntent::from_value(&value)
                }
            });
        match self.fs.write_intended(&target, content, intent).await {
            Ok(outcome) => {
                self.ctx.emit(
                    FS_OBSERVED,
                    fs_event_payload(
                        &target,
                        &actor,
                        Some(&FsObservation::Present {
                            version: outcome.version,
                        }),
                    ),
                );
                Ok(ToolOutcome::text(format!("wrote {path}")))
            }
            Err(error) => Ok(ToolOutcome::error(error.remediate().to_string())),
        }
    }
}

pub fn name() -> &'static str {
    "dsh-tool-fs"
}
