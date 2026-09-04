//! Model-facing `read`, `write`, and `edit` tools.

mod sandbox;

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{
    error_from_event, fs_event_payload, FsObservation, FsObservationActor, FsRuntime, FsWriteIntent,
    FS_EDIT_INTENT, FS_OBSERVED, FS_WRITE_INTENT,
};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome};
use serde_json::{json, Value};
use std::sync::Arc;

/// Default and maximum lines returned by one `read` call.
pub const READ_LIMIT: usize = 2000;

/// `read` tool.
pub struct ReadTool {
    fallback: Arc<FsRuntime>,
    ctx: Context,
}

impl ReadTool {
    /// Bind to `ctx.fs` and the plugin context used for `fs/observed`.
    pub fn new(fs: Arc<FsRuntime>, ctx: Context) -> Self {
        Self {
            fallback: fs,
            ctx,
        }
    }

    fn fs(&self) -> Arc<FsRuntime> {
        FsRuntime::from_context(&self.ctx, &self.fallback)
    }
}

/// Previous name of [`ReadTool`].
pub type ReadFileTool = ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file and return line-numbered content."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to read, resolved by the filesystem backend." },
                "offset": { "type": "number", "description": "1-based first line to return. Defaults to 1." },
                "limit": { "type": "number", "description": format!("Maximum number of lines to return. Defaults to {READ_LIMIT}.") }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let input = match parse_read_args(&call.args, READ_LIMIT) {
            Ok(input) => input,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let actor = FsObservationActor::from_agent_id(call.agent_id.as_deref());
        let fs = self.fs();
        let target = match fs.resolve(&input.file_path).await {
            Ok(target) => target,
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
        match fs.read_text(&target.target_key).await {
            Ok(text) => {
                if let Ok(Some(version)) = fs.version_of(&target).await {
                    self.ctx.emit(
                        FS_OBSERVED,
                        fs_event_payload(
                            &target,
                            &actor,
                            Some(&FsObservation::Present { version }),
                        ),
                    );
                }
                let lines: Vec<&str> = text.split('\n').collect();
                let total = if text.is_empty() { 0 } else { lines.len() };
                let start = input.offset.saturating_sub(1);
                let window: Vec<(usize, String)> = lines
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(input.limit)
                    .map(|(index, line)| (index + 1, (*line).to_string()))
                    .collect();
                Ok(ToolOutcome::text(format_read_output(
                    &target.display_path,
                    input.offset,
                    &window,
                    total,
                )))
            }
            Err(error) => {
                if fs.stat(&target.target_key).await.ok().flatten().is_none() {
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

/// `write` tool.
pub struct WriteTool {
    fallback: Arc<FsRuntime>,
    ctx: Context,
    sandbox: sandbox::FsSandboxController,
}

impl WriteTool {
    /// Bind to `ctx.fs` and the plugin context used for `fs/*` events.
    ///
    /// # Panics
    /// A confining `ctx.fs` is already mounted and `ctx.sandboxPolicy` is missing.
    pub fn new(fs: Arc<FsRuntime>, ctx: Context) -> Self {
        Self::try_new(fs, ctx).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Bind to `ctx.fs`, failing loud when a confining backend has no policy resolver.
    ///
    /// # Errors
    /// `ctx.fs` confines and `ctx.sandboxPolicy` is missing.
    pub fn try_new(fs: Arc<FsRuntime>, ctx: Context) -> Result<Self, String> {
        Ok(Self {
            sandbox: sandbox::FsSandboxController::new(ctx.clone(), Arc::clone(&fs))?,
            fallback: fs,
            ctx,
        })
    }

    fn fs(&self) -> Arc<FsRuntime> {
        FsRuntime::from_context(&self.ctx, &self.fallback)
    }
}

/// Previous name of [`WriteTool`].
pub type WriteFileTool = WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or fully replace a UTF-8 text file."
    }

    fn parameters(&self) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "file_path".into(),
            json!({ "type": "string", "description": "Path to write, resolved by the filesystem backend." }),
        );
        properties.insert(
            "content".into(),
            json!({ "type": "string", "description": "Full UTF-8 text content to write." }),
        );
        if self.sandbox.advertises_escalation() {
            properties.extend(self.sandbox.schema_fields());
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let path = match required_file_path(&call.args) {
            Ok(path) => path,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let content = call.args.get("content").and_then(Value::as_str).unwrap_or("");
        let policy = match self
            .sandbox
            .resolve_policy("write", &call.args, call.agent_id.as_deref())
            .await
        {
            Ok(policy) => policy,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let fence = policy.as_ref().map(sandbox::write_policy_from);
        let actor = FsObservationActor::from_agent_id(call.agent_id.as_deref());
        let fs = self.fs();
        let target = match fs.resolve(&path).await {
            Ok(target) => target,
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
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
        match fs
            .write_intended_with_policy(&target, content, intent, fence.as_ref())
            .await
        {
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
                Ok(ToolOutcome::text(format_write_output(
                    &target.display_path,
                    outcome.operation,
                )))
            }
            Err(error) => Ok(ToolOutcome::error(
                self.sandbox.map_error(error, policy.as_ref()).to_string(),
            )),
        }
    }
}

/// `edit` tool.
pub struct EditTool {
    fallback: Arc<FsRuntime>,
    ctx: Context,
    sandbox: sandbox::FsSandboxController,
}

impl EditTool {
    /// Bind to `ctx.fs` and the plugin context used for `fs/*` events.
    ///
    /// # Panics
    /// A confining `ctx.fs` is already mounted and `ctx.sandboxPolicy` is missing.
    pub fn new(fs: Arc<FsRuntime>, ctx: Context) -> Self {
        Self::try_new(fs, ctx).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Bind to `ctx.fs`, failing loud when a confining backend has no policy resolver.
    ///
    /// # Errors
    /// `ctx.fs` confines and `ctx.sandboxPolicy` is missing.
    pub fn try_new(fs: Arc<FsRuntime>, ctx: Context) -> Result<Self, String> {
        Ok(Self {
            sandbox: sandbox::FsSandboxController::new(ctx.clone(), Arc::clone(&fs))?,
            fallback: fs,
            ctx,
        })
    }

    fn fs(&self) -> Arc<FsRuntime> {
        FsRuntime::from_context(&self.ctx, &self.fallback)
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit an existing UTF-8 text file by replacing literal text."
    }

    fn parameters(&self) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "file_path".into(),
            json!({ "type": "string", "description": "Path to edit, resolved by the filesystem backend." }),
        );
        properties.insert(
            "old_string".into(),
            json!({ "type": "string", "description": "Literal text to replace. Must match exactly." }),
        );
        properties.insert(
            "new_string".into(),
            json!({ "type": "string", "description": "Literal replacement text. Use an empty string to delete the match." }),
        );
        properties.insert(
            "replace_all".into(),
            json!({ "type": "boolean", "description": "Replace all matches. Defaults to false; when false, old_string must appear exactly once." }),
        );
        if self.sandbox.advertises_escalation() {
            properties.extend(self.sandbox.schema_fields());
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let input = match parse_edit_args(&call.args) {
            Ok(input) => input,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let policy = match self
            .sandbox
            .resolve_policy("edit", &call.args, call.agent_id.as_deref())
            .await
        {
            Ok(policy) => policy,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let fence = policy.as_ref().map(sandbox::write_policy_from);
        let actor = FsObservationActor::from_agent_id(call.agent_id.as_deref());
        let fs = self.fs();
        let target = match fs.resolve(&input.file_path).await {
            Ok(target) => target,
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
        let intent = match self.ctx.waterfall(
            FS_EDIT_INTENT,
            fs_event_payload(&target, &actor, None),
            |_| json!(null),
        ) {
            Ok(value) => {
                if let Some(error) = error_from_event(&value) {
                    return Ok(ToolOutcome::error(error.remediate().to_string()));
                }
                value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(|version| FsWriteIntent::ReplaceIfVersion {
                        version: version.to_string(),
                    })
            }
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
        let before = match fs.read_text(&target.target_key).await {
            Ok(text) => text,
            Err(error) => return Ok(ToolOutcome::error(error.to_string())),
        };
        let after = match apply_edit(&before, &input.old_string, &input.new_string, input.replace_all, &target.display_path)
        {
            Ok(text) => text,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        match fs
            .write_intended_with_policy(&target, &after, intent, fence.as_ref())
            .await
        {
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
                Ok(ToolOutcome::text(format_edit_output(
                    &target.display_path,
                    input.replace_all,
                )))
            }
            Err(error) => Ok(ToolOutcome::error(
                self.sandbox.map_error(error, policy.as_ref()).to_string(),
            )),
        }
    }
}

#[derive(Debug)]
struct ReadInput {
    file_path: String,
    offset: usize,
    limit: usize,
}

#[derive(Debug)]
struct EditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

fn required_file_path(args: &Value) -> Result<String, String> {
    let path = args
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| "file_path must be a non-empty string".to_string())?;
    if path.trim().is_empty() {
        return Err("file_path must be a non-empty string".into());
    }
    Ok(path.to_string())
}

/// Validate `read` arguments after defaulting.
fn parse_read_args(args: &Value, max_limit: usize) -> Result<ReadInput, String> {
    let file_path = required_file_path(args)?;
    let offset = optional_positive(args, "offset")?.unwrap_or(1);
    let limit = optional_positive(args, "limit")?.unwrap_or(max_limit);
    if limit > max_limit {
        return Err(format!("limit must be less than or equal to {max_limit}"));
    }
    Ok(ReadInput {
        file_path,
        offset,
        limit,
    })
}

/// Validate `edit` arguments after defaulting `replace_all`.
fn parse_edit_args(args: &Value) -> Result<EditInput, String> {
    let file_path = required_file_path(args)?;
    let old_string = args
        .get("old_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "old_string must be a non-empty string".to_string())?;
    if old_string.is_empty() {
        return Err("old_string must be a non-empty string".into());
    }
    let new_string = args
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "new_string is required".to_string())?;
    if old_string == new_string {
        return Err("old_string and new_string must differ".into());
    }
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(EditInput {
        file_path,
        old_string: old_string.to_string(),
        new_string: new_string.to_string(),
        replace_all,
    })
}

fn optional_positive(args: &Value, name: &str) -> Result<Option<usize>, String> {
    let Some(value) = args.get(name) else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .filter(|value| *value >= 1)
        .ok_or_else(|| format!("{name} must be a positive integer"))?;
    Ok(Some(number as usize))
}

/// Format a windowed read as the TypeScript `<path>` / numbered-line envelope.
pub fn format_read_output(
    display_path: &str,
    offset: usize,
    lines: &[(usize, String)],
    total_lines: usize,
) -> String {
    let end_line = lines
        .last()
        .map(|(number, _)| *number)
        .unwrap_or_else(|| offset.saturating_sub(1));
    let footer = if end_line < total_lines {
        format!(
            "(Showing lines {offset}-{end_line} of {total_lines}. Use offset={} to continue.)",
            end_line + 1
        )
    } else {
        format!("(End of file - total {total_lines} lines)")
    };
    let body = if lines.is_empty() {
        footer
    } else {
        let numbered = lines
            .iter()
            .map(|(number, text)| format!("{number}: {text}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{numbered}\n\n{footer}")
    };
    format!(
        "<path>{display_path}</path>\n<type>file</type>\n<content>\n{body}\n</content>"
    )
}

/// Format a write outcome as the TypeScript Created/Updated envelope.
pub fn format_write_output(display_path: &str, operation: &str) -> String {
    let verb = if operation == "create" {
        "Created"
    } else {
        "Updated"
    };
    format!("<path>{display_path}</path>\n<type>file</type>\n<content>\n{verb} file\n</content>")
}

/// Format an edit success sentence.
pub fn format_edit_output(display_path: &str, replace_all: bool) -> String {
    if replace_all {
        format!(
            "The file {display_path} has been updated. All occurrences were successfully replaced."
        )
    } else {
        format!("The file {display_path} has been updated successfully.")
    }
}

fn apply_edit(
    text: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    display_path: &str,
) -> Result<String, String> {
    let matches = text.matches(old_string).count();
    if matches == 0 {
        return Err(format!("old_string was not found in \"{display_path}\""));
    }
    if matches > 1 && !replace_all {
        return Err(format!(
            "old_string matched {matches} times in \"{display_path}\"; provide a more specific old_string or set replace_all to true"
        ));
    }
    Ok(if replace_all {
        text.replace(old_string, new_string)
    } else {
        text.replacen(old_string, new_string, 1)
    })
}

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "dsh-tool-fs"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_fs::FsWriteOutcome;

    #[test]
    fn names_match_typescript() {
        let ctx = Context::new();
        let fs = dummy_fs();
        assert_eq!(ReadTool::new(Arc::clone(&fs), ctx.clone()).name(), "read");
        assert_eq!(WriteTool::new(Arc::clone(&fs), ctx.clone()).name(), "write");
        assert_eq!(EditTool::new(fs, ctx).name(), "edit");
    }

    #[test]
    fn parse_read_defaults_and_rejects() {
        let parsed = parse_read_args(&json!({ "file_path": "a.txt" }), 10).unwrap();
        assert_eq!(parsed.offset, 1);
        assert_eq!(parsed.limit, 10);
        assert!(parse_read_args(&json!({ "file_path": "  " }), 10).is_err());
        assert!(parse_read_args(&json!({ "file_path": "a.txt", "limit": 11 }), 10).is_err());
        assert!(parse_read_args(&json!({ "file_path": "a.txt", "offset": 0 }), 10).is_err());
    }

    #[test]
    fn parse_edit_rejects_noop_and_empty() {
        assert!(parse_edit_args(&json!({
            "file_path": "a.txt",
            "old_string": "x",
            "new_string": "x"
        }))
        .unwrap_err()
        .contains("must differ"));
        assert!(parse_edit_args(&json!({
            "file_path": "a.txt",
            "old_string": "",
            "new_string": "x"
        }))
        .unwrap_err()
        .contains("old_string must be a non-empty string"));
    }

    #[test]
    fn formats_match_typescript_envelopes() {
        assert_eq!(
            format_write_output("/tmp/a.txt", "create"),
            "<path>/tmp/a.txt</path>\n<type>file</type>\n<content>\nCreated file\n</content>"
        );
        assert_eq!(
            format_write_output("/tmp/a.txt", "update"),
            "<path>/tmp/a.txt</path>\n<type>file</type>\n<content>\nUpdated file\n</content>"
        );
        assert_eq!(
            format_edit_output("/tmp/a.txt", false),
            "The file /tmp/a.txt has been updated successfully."
        );
        assert_eq!(
            format_edit_output("/tmp/a.txt", true),
            "The file /tmp/a.txt has been updated. All occurrences were successfully replaced."
        );
        let rendered = format_read_output("/tmp/a.txt", 1, &[(1, "hello".into())], 1);
        assert!(rendered.contains("<path>/tmp/a.txt</path>"));
        assert!(rendered.contains("1: hello"));
        assert!(rendered.contains("(End of file - total 1 lines)"));
    }

    #[test]
    fn apply_edit_requires_unique_match() {
        let err = apply_edit("aa", "a", "b", false, "x.txt").unwrap_err();
        assert!(err.contains("matched 2 times"));
        assert_eq!(apply_edit("aa", "a", "b", true, "x.txt").unwrap(), "bb");
        assert!(apply_edit("z", "a", "b", false, "x.txt")
            .unwrap_err()
            .contains("was not found"));
    }

    #[tokio::test]
    async fn execute_uses_fs_service_mounted_after_the_tool() {
        let ctx = Context::new();
        let allowing = Arc::new(FsRuntime::new(Arc::new(DummyFs)));
        let tool = WriteTool::new(allowing, ctx.clone());
        ctx.provide(Arc::new(FsRuntime::new(Arc::new(DenyingFs))))
            .unwrap();
        let outcome = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args: json!({ "file_path": "fresh.txt", "content": "x" }),
                agent_id: None,
                call_id: None,
            })
            .await
            .unwrap();
        assert!(outcome.is_error, "{outcome:?}");
        assert!(
            format!("{outcome:?}").contains("later-provider-denied"),
            "{outcome:?}"
        );
    }

    fn text(outcome: &ToolOutcome) -> String {
        match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn confining_without_policy_fails_loud() {
        let ctx = Context::new();
        let fs = Arc::new(
            FsRuntime::new(Arc::new(DummyFs)).with_sandbox_mode(dsh_sandbox::SandboxMode::ReadOnly),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        let err = match WriteTool::try_new(fs, ctx) {
            Ok(_) => panic!("expected fail-loud without sandboxPolicy"),
            Err(err) => err,
        };
        assert!(err.contains(
            "tool-fs: the mounted filesystem confines but ctx.sandboxPolicy is missing"
        ));
    }

    #[test]
    fn advertises_no_escalation_fields_under_a_non_confining_backend() {
        let ctx = Context::new();
        let tool = WriteTool::new(dummy_fs(), ctx);
        let parameters = tool.parameters();
        let props = parameters["properties"].as_object().unwrap();
        assert!(props.get("sandbox_permissions").is_none());
        assert!(props.get("justification").is_none());
    }

    #[test]
    fn advertises_closed_targets_under_a_confining_backend() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "workspace-write", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let fs = Arc::new(
            FsRuntime::new(Arc::new(DummyFs))
                .with_sandbox_mode(dsh_sandbox::SandboxMode::WorkspaceWrite),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        let write = WriteTool::try_new(Arc::clone(&fs), ctx.clone()).unwrap();
        let edit = EditTool::try_new(fs, ctx).unwrap();
        for tool in [
            write.parameters()["properties"].clone(),
            edit.parameters()["properties"].clone(),
        ] {
            let enum_values = tool["sandbox_permissions"]["enum"].as_array().unwrap();
            assert_eq!(
                enum_values,
                &vec![json!("workspace-write"), json!("danger-full-access")]
            );
            assert!(tool.get("justification").is_some());
        }
    }

    #[test]
    fn live_fs_after_install_advertises_escalation() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "read-only", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let tool = WriteTool::new(dummy_fs(), ctx.clone());
        assert!(tool.parameters()["properties"]
            .as_object()
            .unwrap()
            .get("sandbox_permissions")
            .is_none());
        ctx.provide(Arc::new(
            FsRuntime::new(Arc::new(DummyFs)).with_sandbox_mode(dsh_sandbox::SandboxMode::ReadOnly),
        ))
        .unwrap();
        let parameters = tool.parameters();
        let enum_values = parameters["properties"]["sandbox_permissions"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(
            enum_values,
            &vec![json!("workspace-write"), json!("danger-full-access")]
        );
    }

    #[tokio::test]
    async fn denied_write_maps_to_marker_plus_hint() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "workspace-write", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let backend = Arc::new(RecordingFs {
            stamped: std::sync::Mutex::new(Vec::new()),
            deny: std::sync::Mutex::new(true),
        });
        let fs = Arc::new(
            FsRuntime::new(Arc::clone(&backend) as Arc<dyn dsh_fs::FsProvider>)
                .with_sandbox_mode(dsh_sandbox::SandboxMode::WorkspaceWrite),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        let tool = WriteTool::try_new(fs, ctx).unwrap();
        let outcome = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args: json!({ "file_path": "a.txt", "content": "x" }),
                agent_id: None,
                call_id: None,
            })
            .await
            .unwrap();
        assert!(outcome.is_error);
        let body = text(&outcome);
        assert!(body.contains("[sandbox: file access denied under workspace-write mode]"));
        assert!(body.contains("retry this exact operation once with sandbox_permissions"));
    }

    #[tokio::test]
    async fn pairing_and_unadvertised_fields_fail_closed() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "workspace-write", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let fs = Arc::new(
            FsRuntime::new(Arc::new(DummyFs))
                .with_sandbox_mode(dsh_sandbox::SandboxMode::WorkspaceWrite),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        let tool = WriteTool::try_new(fs, ctx.clone()).unwrap();
        let missing = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args: json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write"
                }),
                agent_id: None,
                call_id: None,
            })
            .await
            .unwrap();
        assert!(text(&missing).contains("sandbox_permissions requires a justification"));

        let plain = WriteTool::new(dummy_fs(), Context::new());
        let unadvertised = plain
            .execute_call(&ToolCall {
                name: "write".into(),
                args: json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "why"
                }),
                agent_id: None,
                call_id: None,
            })
            .await
            .unwrap();
        assert!(text(&unadvertised).contains("not available in this composition"));
    }

    #[tokio::test]
    async fn escalation_without_approval_or_agent_fails_closed() {
        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "workspace-write", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        let fs = Arc::new(
            FsRuntime::new(Arc::new(DummyFs))
                .with_sandbox_mode(dsh_sandbox::SandboxMode::WorkspaceWrite),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        let tool = WriteTool::try_new(Arc::clone(&fs), ctx.clone()).unwrap();
        let args = json!({
            "file_path": "a.txt",
            "content": "x",
            "sandbox_permissions": "danger-full-access",
            "justification": "why"
        });
        let without_service = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args: args.clone(),
                agent_id: Some("sess-fs-esc".into()),
                call_id: None,
            })
            .await
            .unwrap();
        assert!(text(&without_service).contains("no approval service is composed"));

        dsh_user_approval::install(&ctx, Some(&json!({ "policy": "ask" }))).unwrap();
        let tool = WriteTool::try_new(fs, ctx).unwrap();
        let without_agent = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args,
                agent_id: None,
                call_id: None,
            })
            .await
            .unwrap();
        assert!(text(&without_agent).contains("no agent to route it through"));
    }

    #[tokio::test]
    async fn approved_escalation_stamps_the_granted_mode() {
        use dsh_agent::{
            Agent, AgentCancelCause, AgentError, AgentFactory, AgentRegistry, AgentStatus, Inbox,
            InboxTarget,
        };
        use dsh_session::{Session, SessionEventData, SessionStore};

        struct StubAgent {
            session: Arc<Session>,
            inbox: Arc<Inbox>,
        }
        #[async_trait]
        impl Agent for StubAgent {
            fn id(&self) -> &dsh_session::SessionId {
                self.session.id()
            }
            fn session(&self) -> Arc<Session> {
                Arc::clone(&self.session)
            }
            fn inbox(&self) -> Arc<Inbox> {
                Arc::clone(&self.inbox)
            }
            fn status(&self) -> AgentStatus {
                AgentStatus::Idle
            }
            fn send(&self, _: dsh_llm::UserMessage, _: InboxTarget, _: bool) {}
            fn cancel(&self, _: AgentCancelCause) {}
            async fn when_idle(&self) {}
            async fn run(&self) -> Result<(), AgentError> {
                Ok(())
            }
        }
        struct StubFactory;
        impl AgentFactory for StubFactory {
            fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
                Arc::new(StubAgent {
                    inbox: Arc::new(Inbox::for_session(Arc::clone(&session))),
                    session,
                })
            }
        }

        let ctx = Context::new();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&json!({ "mode": "read-only", "workspaceRoot": "/tmp" })),
        )
        .unwrap();
        dsh_user_approval::install(&ctx, Some(&json!({ "policy": "ask" }))).unwrap();
        ctx.on_waterfall("approval/request", |_payload, _next| json!("allowed-once"))
            .unwrap();
        let backend = Arc::new(RecordingFs {
            stamped: std::sync::Mutex::new(Vec::new()),
            deny: std::sync::Mutex::new(false),
        });
        let fs = Arc::new(
            FsRuntime::new(Arc::clone(&backend) as Arc<dyn dsh_fs::FsProvider>)
                .with_sandbox_mode(dsh_sandbox::SandboxMode::ReadOnly),
        );
        ctx.provide(Arc::clone(&fs)).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        ctx.service::<AgentRegistry>()
            .unwrap()
            .set_factory(Arc::new(StubFactory));
        let session = SessionStore::new().create_in(Some("/session-project".into()));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        let tool = WriteTool::try_new(fs, ctx).unwrap();
        let outcome = tool
            .execute_call(&ToolCall {
                name: "write".into(),
                args: json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "the test needs it"
                }),
                agent_id: Some(handle.agent.session().id().as_str().to_string()),
                call_id: None,
            })
            .await
            .unwrap();
        assert!(!outcome.is_error, "{}", text(&outcome));
        let stamped = backend.stamped.lock().expect("stamped").clone();
        assert_eq!(stamped, vec![Some("workspace-write".to_string())]);
    }

    fn dummy_fs() -> Arc<FsRuntime> {
        Arc::new(FsRuntime::new(Arc::new(DummyFs)))
    }

    struct DummyFs;

    #[async_trait]
    impl dsh_fs::FsProvider for DummyFs {
        async fn read_text(&self, _path: &str) -> Result<String, dsh_fs::FsError> {
            Ok(String::new())
        }
        async fn write_text(&self, _path: &str, _content: &str) -> Result<(), dsh_fs::FsError> {
            Ok(())
        }
        async fn exists(&self, _path: &str) -> Result<bool, dsh_fs::FsError> {
            Ok(false)
        }
        async fn stat(&self, _path: &str) -> Result<Option<dsh_fs::FsInfo>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn list_dir(&self, _path: &str) -> Result<Vec<dsh_fs::DirEntry>, dsh_fs::FsError> {
            Ok(vec![])
        }
        async fn resolve(&self, path: &str) -> Result<dsh_fs::FsTarget, dsh_fs::FsError> {
            Ok(dsh_fs::FsTarget::new(path, path))
        }
        async fn version_of(
            &self,
            _target: &dsh_fs::FsTarget,
        ) -> Result<Option<String>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn write_intended(
            &self,
            target: &dsh_fs::FsTarget,
            content: &str,
            _intent: Option<FsWriteIntent>,
        ) -> Result<FsWriteOutcome, dsh_fs::FsError> {
            let _ = (target, content);
            Ok(FsWriteOutcome {
                operation: "create",
                version: "1".into(),
            })
        }
    }

    struct RecordingFs {
        stamped: std::sync::Mutex<Vec<Option<String>>>,
        deny: std::sync::Mutex<bool>,
    }

    #[async_trait]
    impl dsh_fs::FsProvider for RecordingFs {
        async fn read_text(&self, _path: &str) -> Result<String, dsh_fs::FsError> {
            Ok(String::new())
        }
        async fn write_text(&self, _path: &str, _content: &str) -> Result<(), dsh_fs::FsError> {
            Ok(())
        }
        async fn exists(&self, _path: &str) -> Result<bool, dsh_fs::FsError> {
            Ok(false)
        }
        async fn stat(&self, _path: &str) -> Result<Option<dsh_fs::FsInfo>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn list_dir(&self, _path: &str) -> Result<Vec<dsh_fs::DirEntry>, dsh_fs::FsError> {
            Ok(vec![])
        }
        async fn resolve(&self, path: &str) -> Result<dsh_fs::FsTarget, dsh_fs::FsError> {
            Ok(dsh_fs::FsTarget::new(path, path))
        }
        async fn version_of(
            &self,
            _target: &dsh_fs::FsTarget,
        ) -> Result<Option<String>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn write_intended(
            &self,
            target: &dsh_fs::FsTarget,
            content: &str,
            intent: Option<FsWriteIntent>,
        ) -> Result<FsWriteOutcome, dsh_fs::FsError> {
            self.write_intended_with_policy(target, content, intent, None)
                .await
        }
        async fn write_intended_with_policy(
            &self,
            _target: &dsh_fs::FsTarget,
            _content: &str,
            _intent: Option<FsWriteIntent>,
            policy: Option<&dsh_fs::FsWritePolicy>,
        ) -> Result<FsWriteOutcome, dsh_fs::FsError> {
            self.stamped
                .lock()
                .expect("stamped")
                .push(policy.map(|policy| policy.mode.clone()));
            if *self.deny.lock().expect("deny") {
                return Err(dsh_fs::FsError::sandbox_denied("denied"));
            }
            Ok(FsWriteOutcome {
                operation: "create",
                version: "1".into(),
            })
        }
    }

    struct DenyingFs;

    #[async_trait]
    impl dsh_fs::FsProvider for DenyingFs {
        async fn read_text(&self, _path: &str) -> Result<String, dsh_fs::FsError> {
            Ok(String::new())
        }
        async fn write_text(&self, _path: &str, _content: &str) -> Result<(), dsh_fs::FsError> {
            Err(dsh_fs::FsError::Denied("later-provider-denied".into()))
        }
        async fn exists(&self, _path: &str) -> Result<bool, dsh_fs::FsError> {
            Ok(false)
        }
        async fn stat(&self, _path: &str) -> Result<Option<dsh_fs::FsInfo>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn list_dir(&self, _path: &str) -> Result<Vec<dsh_fs::DirEntry>, dsh_fs::FsError> {
            Ok(vec![])
        }
        async fn resolve(&self, path: &str) -> Result<dsh_fs::FsTarget, dsh_fs::FsError> {
            Ok(dsh_fs::FsTarget::new(path, path))
        }
        async fn version_of(
            &self,
            _target: &dsh_fs::FsTarget,
        ) -> Result<Option<String>, dsh_fs::FsError> {
            Ok(None)
        }
        async fn write_intended(
            &self,
            _target: &dsh_fs::FsTarget,
            _content: &str,
            _intent: Option<FsWriteIntent>,
        ) -> Result<FsWriteOutcome, dsh_fs::FsError> {
            Err(dsh_fs::FsError::Denied("later-provider-denied".into()))
        }
        async fn write_intended_with_policy(
            &self,
            _target: &dsh_fs::FsTarget,
            _content: &str,
            _intent: Option<FsWriteIntent>,
            _policy: Option<&dsh_fs::FsWritePolicy>,
        ) -> Result<FsWriteOutcome, dsh_fs::FsError> {
            Err(dsh_fs::FsError::Denied("later-provider-denied".into()))
        }
    }
}
