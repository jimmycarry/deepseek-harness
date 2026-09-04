//! MCP content projection and one public ToolRuntime registration.

use crate::name::public_tool_name;
use crate::protocol::McpSession;
use crate::{RegistrationFailure, ToolBridgeOptions};
use async_trait::async_trait;
use dsh_agent::AgentDefaultModel;
use dsh_attachment::{AttachmentStore, ImageMediaType, SaveImageAttachment};
use dsh_cordis::Context;
use dsh_llm::{ContentBlock, ImageContentRef, LlmRuntime};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CANONICAL_BASE64: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Live generation of public names this server currently owns.
#[derive(Clone, Default)]
pub struct LiveTools {
    inner: Arc<Mutex<HashMap<String, Arc<McpTool>>>>,
}

impl LiveTools {
    /// Whether `name` is in the current generation.
    pub fn contains(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(name)
    }

    pub(crate) fn replace(&self, next: HashMap<String, Arc<McpTool>>) {
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    /// Hide every public name from this server.
    pub(crate) fn replace_empty(&self) {
        self.replace(HashMap::new());
    }
}

/// Fetch the next generation, then swap it into `ctx.tools`.
pub async fn sync_tools(
    session: Arc<McpSession>,
    ctx: &Context,
    opts: &ToolBridgeOptions,
    live: &LiveTools,
    alive: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut definitions = HashMap::new();
    let mut cursor = None;
    loop {
        let response = session.list_tools(cursor.clone()).await?;
        let tools = response
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for tool in tools {
            let raw = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "mcp-client({}): server listed a tool without a name — invalid tool list",
                        opts.server_name
                    )
                })?
                .to_string();
            let public = public_tool_name(&opts.server_name, &raw);
            if definitions.contains_key(&public) {
                return Err(format!(
                    "mcp-client({}): server listed tool \"{raw}\" more than once — invalid tool list",
                    opts.server_name
                ));
            }
            let task_required = tool
                .get("execution")
                .and_then(|value| value.get("taskSupport"))
                .and_then(Value::as_str)
                == Some("required");
            definitions.insert(
                public.clone(),
                Arc::new(McpTool {
                    public_name: public,
                    raw_name: raw,
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    parameters: tool.get("inputSchema").cloned().unwrap_or(json!({
                        "type": "object",
                        "properties": {},
                    })),
                    output_schema: mcp_output_schema(tool.get("outputSchema")),
                    task_required,
                    session: Arc::clone(&session),
                    ctx: ctx.clone(),
                    timeout: Duration::from_millis(opts.tool_call_timeout_ms),
                    live: live.clone(),
                    alive: Arc::clone(alive),
                }),
            );
        }
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    let Some(tools) = ctx.get::<ToolRuntime>() else {
        return Err(format!(
            "mcp-client({}): ctx.tools is not configured",
            opts.server_name
        ));
    };
    let mut registered = HashMap::new();
    for (name, tool) in definitions {
        if tools.get(&name).is_some() && !live.contains(&name) {
            let error = format!(
                "mcp-client({}): tool registration failed, no tools registered: public name \"{name}\" is already registered",
                opts.server_name
            );
            tracing::error!("{error}");
            if opts.registration_failure == RegistrationFailure::Throw {
                return Err(error);
            }
            live.replace(HashMap::new());
            return Ok(());
        }
        tools.insert(Arc::clone(&tool) as Arc<dyn Tool>);
        registered.insert(name, tool);
    }
    live.replace(registered);
    Ok(())
}

pub(crate) struct McpTool {
    public_name: String,
    raw_name: String,
    description: String,
    parameters: Value,
    output_schema: Value,
    task_required: bool,
    session: Arc<McpSession>,
    ctx: Context,
    timeout: Duration,
    live: LiveTools,
    alive: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.public_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(self.output_schema.clone())
    }

    fn enabled_for(&self, _agent_id: Option<&str>) -> bool {
        self.alive.load(Ordering::SeqCst) && self.live.contains(&self.public_name)
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall::new(&self.public_name, args))
            .await
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        if self.task_required {
            return Err(ToolError::Body(format!(
                "Tool \"{}\" requires task-based execution, which this bridge does not support",
                self.raw_name
            )));
        }
        let args = if call.args.is_object() {
            call.args.clone()
        } else {
            json!({})
        };
        let result = self
            .session
            .call_tool(&self.raw_name, args, self.timeout)
            .await
            .map_err(ToolError::Body)?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = extract_text(result.get("content").unwrap_or(&json!([])), &self.raw_name);
            return Ok(ToolOutcome::error(text));
        }
        let content = result.get("content").cloned().unwrap_or(json!([]));
        if !content.is_array() {
            let rendered = result
                .get("toolResult")
                .map(ToString::to_string)
                .unwrap_or_else(|| "(no output)".into());
            let mut outcome = ToolOutcome::text(&rendered);
            outcome.value = Some(json!({ "content": [{ "type": "text", "text": rendered }] }));
            if let Some(structured) = result.get("structuredContent") {
                if let Some(object) = outcome.value.as_mut().and_then(Value::as_object_mut) {
                    object.insert("structuredContent".into(), structured.clone());
                }
            }
            return Ok(outcome);
        }
        let mut value = json!({ "content": content });
        if let Some(structured) = result.get("structuredContent") {
            value["structuredContent"] = structured.clone();
        }
        let projected = prepare_projection(
            &self.ctx,
            content.as_array().map(Vec::as_slice).unwrap_or(&[]),
            &self.raw_name,
        )
        .await;
        Ok(ToolOutcome {
            content: projected,
            is_error: false,
            value: Some(value),
        })
    }
}

/// Canonical MCP result schema. A supported advertised `outputSchema` becomes
/// `structuredContent`; unsupported vocabulary falls back to unconstrained JSON.
fn mcp_output_schema(candidate: Option<&Value>) -> Value {
    let structured = supported_output_schema(candidate).unwrap_or(json!({}));
    let required = if structured == json!({}) {
        json!(["content"])
    } else {
        json!(["content", "structuredContent"])
    };
    json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": {} },
            "structuredContent": structured,
        },
        "required": required,
        "additionalProperties": false,
    })
}

fn supported_output_schema(candidate: Option<&Value>) -> Option<Value> {
    let schema = candidate?;
    if schema_supported(schema) {
        Some(schema.clone())
    } else {
        None
    }
}

fn schema_supported(node: &Value) -> bool {
    let Some(object) = node.as_object() else {
        return false;
    };
    const CONSTRAINTS: &[&str] = &[
        "type",
        "oneOf",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
    ];
    const ANNOTATIONS: &[&str] = &["description", "title", "default", "examples"];
    for key in object.keys() {
        if CONSTRAINTS.contains(&key.as_str()) || ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }
        return false;
    }
    if object.contains_key("description") && !object["description"].is_string() {
        return false;
    }
    if object.contains_key("title") && !object["title"].is_string() {
        return false;
    }
    let has_type = object.contains_key("type");
    let has_one_of = object.contains_key("oneOf");
    if has_type && has_one_of {
        return false;
    }
    if !has_type && !has_one_of {
        return ![
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "const",
        ]
        .iter()
        .any(|key| object.contains_key(*key));
    }
    if has_one_of {
        let Some(branches) = object["oneOf"].as_array() else {
            return false;
        };
        if branches.len() < 2 {
            return false;
        }
        if [
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "const",
        ]
        .iter()
        .any(|key| object.contains_key(*key))
        {
            return false;
        }
        return branches.iter().all(schema_supported);
    }
    let Some(schema_type) = object["type"].as_str() else {
        return false;
    };
    match schema_type {
        "object" => {
            if object.contains_key("items")
                || object.contains_key("enum")
                || object.contains_key("const")
            {
                return false;
            }
            if object.contains_key("additionalProperties")
                && !object["additionalProperties"].is_boolean()
            {
                return false;
            }
            if let Some(required) = object.get("required") {
                let Some(keys) = required.as_array() else {
                    return false;
                };
                if keys.iter().any(|key| !key.is_string()) {
                    return false;
                }
                let declared = object
                    .get("properties")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if keys
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|key| !declared.contains_key(key))
                {
                    return false;
                }
            }
            object
                .get("properties")
                .map_or(true, |properties| match properties.as_object() {
                    Some(map) => map.values().all(schema_supported),
                    None => false,
                })
        }
        "array" => {
            if object.contains_key("properties")
                || object.contains_key("required")
                || object.contains_key("additionalProperties")
                || object.contains_key("enum")
                || object.contains_key("const")
            {
                return false;
            }
            object.get("items").map_or(true, schema_supported)
        }
        "string" | "number" | "integer" | "boolean" | "null" => {
            if object.contains_key("properties")
                || object.contains_key("required")
                || object.contains_key("additionalProperties")
                || object.contains_key("items")
            {
                return false;
            }
            if let Some(allowed) = object.get("enum") {
                let Some(values) = allowed.as_array() else {
                    return false;
                };
                if values.is_empty()
                    || values
                        .iter()
                        .any(|value| !scalar_matches(schema_type, value))
                {
                    return false;
                }
            }
            if let Some(declared) = object.get("const") {
                if !scalar_matches(schema_type, declared) {
                    return false;
                }
                if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
                    if !allowed.contains(declared) {
                        return false;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn scalar_matches(schema_type: &str, value: &Value) -> bool {
    match schema_type {
        "string" => value.is_string(),
        "number" => value.as_f64().is_some(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn extract_text(content: &Value, tool_name: &str) -> String {
    let blocks = project_content(
        content.as_array().map(Vec::as_slice).unwrap_or(&[]),
        tool_name,
        None,
    );
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn image_diagnostic(media_type: &str, reason: &str) -> String {
    format!(
        "[image unavailable: {media_type}; {reason}; raw image data remains available to programmatic callers]"
    )
}

fn decode_canonical_base64(data: &str) -> Result<Vec<u8>, String> {
    if data.len() % 4 != 0
        || !data
            .chars()
            .all(|ch| CANONICAL_BASE64.contains(ch) || ch == '=')
    {
        return Err("the image data is not canonical base64".into());
    }
    let mut bytes = Vec::new();
    let table = |ch: u8| -> Option<u8> {
        CANONICAL_BASE64
            .as_bytes()
            .iter()
            .position(|item| *item == ch)
            .map(|index| index as u8)
    };
    for chunk in data.as_bytes().chunks(4) {
        let a =
            table(chunk[0]).ok_or_else(|| "the image data is not canonical base64".to_string())?;
        let b =
            table(chunk[1]).ok_or_else(|| "the image data is not canonical base64".to_string())?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            table(chunk[2]).ok_or_else(|| "the image data is not canonical base64".to_string())?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            table(chunk[3]).ok_or_else(|| "the image data is not canonical base64".to_string())?
        };
        bytes.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            bytes.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            bytes.push((c << 6) | d);
        }
    }
    let encoded = encode_canonical_base64(&bytes);
    if encoded != data {
        return Err("the image data is not canonical base64".into());
    }
    Ok(bytes)
}

fn encode_canonical_base64(bytes: &[u8]) -> String {
    let table = CANONICAL_BASE64.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(table[(b0 >> 2) as usize] as char);
        out.push(table[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(table[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(table[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn project_content(
    content: &[Value],
    tool_name: &str,
    image: Option<&dyn Fn(&Value, usize) -> ContentBlock>,
) -> Vec<ContentBlock> {
    let mut projected = Vec::new();
    let mut text = Vec::new();
    let flush = |text: &mut Vec<String>, projected: &mut Vec<ContentBlock>| {
        if !text.is_empty() {
            projected.push(ContentBlock::text(text.join("\n")));
            text.clear();
        }
    };
    for (index, value) in content.iter().enumerate() {
        let Some(object) = value.as_object() else {
            text.push("[unsupported MCP content block: expected an object]".into());
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(item) = object.get("text").and_then(Value::as_str) {
                    text.push(item.to_string());
                }
            }
            Some("image") => {
                flush(&mut text, &mut projected);
                let block = if let Some(image) = image {
                    image(value, index)
                } else {
                    ContentBlock::text(image_diagnostic(
                        object
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown media type"),
                        "this result was not admitted to durable model context",
                    ))
                };
                projected.push(block);
            }
            Some("resource_link") => {
                match (
                    object.get("name").and_then(Value::as_str),
                    object.get("uri").and_then(Value::as_str),
                ) {
                    (Some(name), Some(uri)) => {
                        text.push(format!("Resource link: {name} ({uri})"));
                    }
                    _ => text.push(
                        "[resource link unavailable: the MCP block is missing its name or URI]"
                            .into(),
                    ),
                }
            }
            Some("audio") => {
                text.push(format!(
                    "[audio result unsupported: {}; raw audio data remains available to programmatic callers]",
                    object
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown media type")
                ));
            }
            Some("resource") => {
                text.push("[embedded resource unsupported; raw resource data remains available to programmatic callers]".into());
            }
            Some(other) => text.push(format!("[unsupported MCP content type: {other}]")),
            None => text.push("[unsupported MCP content block: expected an object]".into()),
        }
    }
    flush(&mut text, &mut projected);
    if projected.is_empty() {
        vec![ContentBlock::text(format!(
            "({tool_name} returned no model-visible content)"
        ))]
    } else {
        projected
    }
}

async fn prepare_projection(
    ctx: &Context,
    content: &[Value],
    tool_name: &str,
) -> Vec<ContentBlock> {
    if !content
        .iter()
        .any(|value| value.get("type").and_then(Value::as_str) == Some("image"))
    {
        return project_content(content, tool_name, None);
    }
    let mut decoded = Vec::new();
    let mut errors = HashMap::new();
    let mut indexes = Vec::new();
    for (index, value) in content.iter().enumerate() {
        if value.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        indexes.push(index);
        match decode_image(value) {
            Ok(image) => decoded.push(image),
            Err(reason) => {
                errors.insert(index, reason);
            }
        }
    }
    if !errors.is_empty() {
        return project_content(
            content,
            tool_name,
            Some(&|block, index| {
                ContentBlock::text(image_diagnostic(
                    block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown media type"),
                    errors
                        .get(&index)
                        .map(String::as_str)
                        .unwrap_or("another image in the same result was invalid"),
                ))
            }),
        );
    }
    let attachments = match resolve_image_admission(ctx).await {
        Ok(store) => store,
        Err(reason) => {
            return project_content(
                content,
                tool_name,
                Some(&|block, _| {
                    ContentBlock::text(image_diagnostic(
                        block
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown media type"),
                        &reason,
                    ))
                }),
            );
        }
    };
    match attachments.save_images(decoded) {
        Ok(refs) => {
            let by_index: HashMap<usize, dsh_attachment::ImageAttachmentRef> =
                indexes.into_iter().zip(refs).collect();
            project_content(
                content,
                tool_name,
                Some(&|_, index| {
                    let reference = by_index.get(&index).expect("image index");
                    ContentBlock::Image {
                        attachment: ImageContentRef {
                            attachment_id: reference.attachment_id.clone(),
                            media_type: reference.media_type.as_str().to_string(),
                            bytes: reference.bytes,
                            width: reference.width,
                            height: reference.height,
                            name: reference.name.clone(),
                        },
                    }
                }),
            )
        }
        Err(_) => project_content(
            content,
            tool_name,
            Some(&|block, _| {
                ContentBlock::text(image_diagnostic(
                    block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown media type"),
                    "durable image storage rejected the result",
                ))
            }),
        ),
    }
}

fn decode_image(block: &Value) -> Result<SaveImageAttachment, String> {
    let media = block
        .get("mimeType")
        .and_then(Value::as_str)
        .and_then(ImageMediaType::parse)
        .ok_or_else(|| "the declared media type is not PNG, JPEG, WebP, or GIF".to_string())?;
    let data = block
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "the image data is not canonical base64".to_string())?;
    Ok(SaveImageAttachment {
        data: decode_canonical_base64(data)?,
        media_type: media,
        name: block
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

async fn resolve_image_admission(ctx: &Context) -> Result<Arc<AttachmentStore>, String> {
    let attachments = ctx
        .get::<AttachmentStore>()
        .ok_or_else(|| "no attachment store is mounted".to_string())?;
    let routed = ctx
        .get::<AgentDefaultModel>()
        .ok_or_else(|| "the current model route could not be resolved".to_string())?;
    let llm = ctx
        .get::<LlmRuntime>()
        .ok_or_else(|| "the current model route could not be resolved".to_string())?;
    let info = llm
        .resolve_model_info(&routed.provider, &routed.model)
        .await
        .map_err(|_| "the current model route could not be verified".to_string())?;
    if !info
        .input_modalities
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| item == "image"))
    {
        return Err(format!(
            "model \"{}\" does not declare image input",
            routed.model
        ));
    }
    Ok(attachments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_diagnostic_is_the_typescript_sentence() {
        assert_eq!(
            image_diagnostic("image/png", "no attachment store is mounted"),
            "[image unavailable: image/png; no attachment store is mounted; raw image data remains available to programmatic callers]"
        );
    }

    #[test]
    fn project_content_keeps_text_and_resource_link_order() {
        let blocks = project_content(
            &[
                json!({ "type": "text", "text": "hello" }),
                json!({ "type": "resource_link", "name": "doc", "uri": "file://x" }),
                json!({ "type": "audio", "mimeType": "audio/wav" }),
            ],
            "echo",
            None,
        );
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("hello"));
                assert!(text.contains("Resource link: doc (file://x)"));
                assert!(text.contains("[audio result unsupported: audio/wav"));
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn output_schema_requires_structured_content_when_supported() {
        let schema = mcp_output_schema(Some(&json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
            "additionalProperties": false
        })));
        assert_eq!(schema["required"], json!(["content", "structuredContent"]));
        assert_eq!(schema["properties"]["structuredContent"]["type"], "object");
    }

    #[test]
    fn unsupported_output_schema_falls_back_to_unconstrained_json() {
        let schema = mcp_output_schema(Some(&json!({
            "type": "object",
            "$ref": "#/definitions/Result"
        })));
        assert_eq!(schema["required"], json!(["content"]));
        assert_eq!(schema["properties"]["structuredContent"], json!({}));
    }
}
