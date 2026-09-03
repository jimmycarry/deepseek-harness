//! DeepSeek LLM adapter. Chat uses SSE (`stream: true`); a missing `[DONE]` is
//! `STREAM_CLOSED`. Vision models send user images as `image_url` data-URLs.
//! Self-skips with-key tests when `DEEPSEEK_API_KEY` is unset.

mod sse;
mod translate;

pub use sse::{parse_sse, DONE};
pub use translate::{map_finish_reason, map_usage, translate};

use async_trait::async_trait;
use dsh_credentials::{Credential, CredentialsRuntime};
use dsh_llm::{
    content_has_image, is_context_window_exceeded_error, is_quota_exceeded_error,
    provider_retry_after_ms, ContentBlock, LlmAdapter, LlmError, LlmFailure, LlmModelContext,
    LlmRequest, LlmResolvedModelInfo, Message, StreamChunk, CONTEXT_WINDOW_EXCEEDED_CODE,
    QUOTA_EXCEEDED_CODE,
};
use futures::stream::{self, BoxStream};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The single provider route this plugin owns.
pub const PROVIDER: &str = "deepseek-official";

/// Default environment variable that holds the API key.
pub const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Positive context capacity used when a catalog entry has none (TypeScript default).
pub const DEFAULT_CONTEXT_WINDOW: u32 = 1_000_000;

/// One advisory catalog entry used by `resolve_model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// Wire model id.
    pub id: String,
    /// Combined request/response context when configured.
    pub context_window: Option<u32>,
}

/// Default V4 Flash / Pro / Flash-Vision catalog.
pub fn default_models() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            id: "deepseek-v4-flash".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
        CatalogModel {
            id: "deepseek-v4-pro".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
        CatalogModel {
            id: "deepseek-v4-flash-vision-exp".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
    ]
}

/// Validate `defaultContextWindow` and `models` from plugin config or a settings section.
///
/// # Errors
/// A non-positive `defaultContextWindow`, a non-array `models` value, an empty or
/// duplicate catalog id, or a non-positive per-model `contextWindow`.
pub fn resolve_catalog(config: Option<&Value>) -> Result<(u32, Vec<CatalogModel>), String> {
    let default_context_window = match config.and_then(|value| value.get("defaultContextWindow")) {
        None => DEFAULT_CONTEXT_WINDOW,
        Some(value) => positive_u32(value).ok_or_else(|| {
            "llm-deepseek: defaultContextWindow must be a positive integer".to_string()
        })?,
    };
    let models = match config.and_then(|value| value.get("models")) {
        None => default_models(),
        Some(value) => resolve_models(value)?,
    };
    Ok((default_context_window, models))
}

/// Context capacity for `model`: exact catalog value, else `default_context_window`.
pub fn context_window_for(
    model: &str,
    default_context_window: u32,
    models: &[CatalogModel],
) -> u32 {
    models
        .iter()
        .find(|entry| entry.id == model)
        .and_then(|entry| entry.context_window)
        .unwrap_or(default_context_window)
}

fn resolve_models(value: &Value) -> Result<Vec<CatalogModel>, String> {
    let Some(items) = value.as_array() else {
        return Err("llm-deepseek: models must be an array".into());
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut models = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            return Err("llm-deepseek: catalog model ids must be non-empty".into());
        }
        if !seen.insert(id.clone()) {
            return Err(format!("llm-deepseek: duplicate catalog model \"{id}\""));
        }
        let context_window = match item.get("contextWindow") {
            None => None,
            Some(value) => Some(positive_u32(value).ok_or_else(|| {
                format!(
                    "llm-deepseek: catalog model \"{id}\" contextWindow must be a positive integer"
                )
            })?),
        };
        models.push(CatalogModel { id, context_window });
    }
    Ok(models)
}

fn positive_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number > 0)
}

fn overlay_section(plugin: Option<&Value>, settings: Option<&Value>) -> Value {
    let mut map = match plugin {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    if let Some(Value::Object(section)) = settings {
        for (key, value) in section {
            map.insert(key.clone(), value.clone());
        }
    }
    Value::Object(map)
}

/// Layer a live `llm-deepseek` settings section over plugin config.
pub fn merge_connection_config(plugin: Option<&Value>, settings: Option<&Value>) -> Value {
    overlay_section(plugin, settings)
}

/// Exact TypeScript `MISSING_CREDENTIAL` failure for a missing API key.
pub fn missing_api_key(api_key_env: &str) -> LlmError {
    LlmError::Failure(LlmFailure::new(
        format!(
            "llm-deepseek: no API key for provider route \"{PROVIDER}\"; store {api_key_env} through the credentials service (the web Models page writes it), or export {api_key_env} in the launching environment"
        ),
        "MISSING_CREDENTIAL",
    ))
}

/// Resolve the API key through `ctx.credentials` when mounted, else the launch env.
///
/// # Errors
/// [`MISSING_CREDENTIAL`](missing_api_key) when no usable key exists.
pub fn resolve_api_key(
    credentials: Option<&CredentialsRuntime>,
    api_key_env: &str,
) -> std::result::Result<String, LlmError> {
    if let Some(credentials) = credentials {
        match credentials.resolve(api_key_env) {
            Credential::Set(value) => Ok(value),
            Credential::Unset => Err(missing_api_key(api_key_env)),
        }
    } else {
        std::env::var(api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| missing_api_key(api_key_env))
    }
}

fn catalog_error(message: String) -> LlmError {
    LlmError::Failure(LlmFailure::new(message, "CONFIG"))
}

/// Map an HTTP status to a stable `LlmError` code.
#[must_use]
pub fn http_error_code(status: u16, error: Option<&WireErrorDetail>) -> String {
    if status == 401 || status == 403 {
        return "AUTH".into();
    }
    if status == 413 {
        return "INVALID_REQUEST".into();
    }
    let detail = [
        error.and_then(|item| item.code.as_deref()),
        error.and_then(|item| item.r#type.as_deref()),
        error.and_then(|item| item.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    if is_quota_exceeded_error(&detail) {
        return QUOTA_EXCEEDED_CODE.into();
    }
    if status == 429 {
        return "RATE_LIMIT".into();
    }
    if status == 400 {
        if is_context_window_exceeded_error(&detail) {
            return CONTEXT_WINDOW_EXCEEDED_CODE.into();
        }
        return "INVALID_REQUEST".into();
    }
    if status >= 500 {
        return "SERVER".into();
    }
    format!("HTTP_{status}")
}

/// Provider `error` object fields used for classification.
#[derive(Debug, Clone, Default)]
pub struct WireErrorDetail {
    /// Provider machine code.
    pub code: Option<String>,
    /// Provider error type.
    pub r#type: Option<String>,
    /// Provider human-readable message.
    pub message: Option<String>,
}

/// DeepSeek chat adapter.
pub struct DeepSeekAdapter {
    /// API key resolved at construction.
    pub api_key: String,
    /// Optional base URL override.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Prepared request-image bytes keyed by attachment id.
    pub images: HashMap<String, Vec<u8>>,
}

/// Whether this catalog model accepts image input.
pub fn model_accepts_image(model: &str) -> bool {
    model.contains("vision")
}

impl DeepSeekAdapter {
    /// Build from the process environment. Missing key fails loud.
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = resolve_api_key(None, DEFAULT_API_KEY_ENV)?;
        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".into());
        Ok(Self {
            api_key,
            base_url,
            model: "deepseek-chat".into(),
            images: HashMap::new(),
        })
    }
}

#[async_trait]
impl LlmAdapter for DeepSeekAdapter {
    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Failure(LlmFailure::new(
                "empty key",
                "MISSING_CREDENTIAL",
            )));
        }
        let url = join_url(&self.base_url, "/chat/completions");
        let body = request_body(&self.model, &request, &self.images)?;
        let raw = post_json(&url, &self.api_key, &body).await?;
        let payloads = parse_sse(&raw)?;
        let chunks = translate(&payloads)?;
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let (default_window, models) = resolve_catalog(None).map_err(catalog_error)?;
        Ok(LlmResolvedModelInfo {
            context: Some(LlmModelContext {
                context_window: context_window_for(model, default_window, &models),
            }),
            ..LlmResolvedModelInfo::identity(provider, model)
        })
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn request_body(
    model: &str,
    request: &LlmRequest,
    images: &HashMap<String, Vec<u8>>,
) -> Result<String, LlmError> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        match message {
            Message::User(user) => {
                messages.push(json!({
                    "role": "user",
                    "content": user_content(&user.content, model, images)?,
                }));
            }
            Message::Assistant(assistant) => {
                if content_has_image(&assistant.content) {
                    return Err(unsupported_image_role("assistant"));
                }
                messages.push(json!({ "role": "assistant", "content": assistant.text() }));
            }
            Message::Tool(tool) => {
                let blocks = tool.result_blocks();
                if content_has_image(blocks) {
                    return Err(unsupported_image_role("tool"));
                }
                messages.push(json!({
                    "role": "tool",
                    "content": blocks_text(blocks),
                    "tool_call_id": tool.tool_call_id().unwrap_or(""),
                }));
            }
        }
    }
    Ok(json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    })
    .to_string())
}

fn unsupported_image_role(role: &str) -> LlmError {
    LlmError::Failure(LlmFailure::new(
        format!(
            "The DeepSeek chat-completions adapter cannot represent image content in a {role} message."
        ),
        "UNSUPPORTED_CONTENT",
    ))
}

fn user_content(
    blocks: &[ContentBlock],
    model: &str,
    images: &HashMap<String, Vec<u8>>,
) -> Result<Value, LlmError> {
    if !content_has_image(blocks) {
        return Ok(Value::String(blocks_text(blocks)));
    }
    if !model_accepts_image(model) {
        return Err(LlmError::Failure(LlmFailure::new(
            format!("DeepSeek model \"{model}\" does not accept image input."),
            "UNSUPPORTED_CONTENT",
        )));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                parts.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Image { attachment } => {
                let Some(bytes) = images.get(&attachment.attachment_id) else {
                    return Err(LlmError::Failure(LlmFailure::new(
                        format!(
                            "DeepSeek request image {} was not prepared.",
                            attachment.attachment_id
                        ),
                        "INVALID_REQUEST",
                    )));
                };
                parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!(
                            "data:{};base64,{}",
                            attachment.media_type,
                            encode_base64(bytes)
                        )
                    }
                }));
            }
            _ => {}
        }
    }
    Ok(Value::Array(parts))
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if b1.is_none() {
            out.push('=');
            out.push('=');
        } else {
            out.push(
                TABLE[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            );
            if b2.is_none() {
                out.push('=');
            } else {
                out.push(TABLE[(b2.unwrap_or(0) & 0x3f) as usize] as char);
            }
        }
        i += 3;
    }
    out
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn post_json(url: &str, api_key: &str, body: &str) -> Result<String, LlmError> {
    let headers = [
        ("Authorization", format!("Bearer {api_key}")),
        ("Content-Type", "application/json".into()),
        ("Accept", "text/event-stream".into()),
    ];
    let raw = if url.starts_with("https://") {
        curl_post(url, &headers, body).await
    } else if url.starts_with("http://") {
        tcp_post(url, &headers, body).await
    } else {
        Err(format!("unsupported url: {url}"))
    }
    .map_err(|message| LlmError::Failure(LlmFailure::new(message, "TRANSPORT")))?;
    let response = parse_http_response(&raw)
        .map_err(|message| LlmError::Failure(LlmFailure::new(message, "TRANSPORT")))?;
    classify_or_body(response)
}

fn classify_or_body(response: HttpResponse) -> Result<String, LlmError> {
    if (200..300).contains(&response.status) {
        return Ok(response.body);
    }
    Err(http_failure(response))
}

fn http_failure(response: HttpResponse) -> LlmError {
    let (provider_message, detail) = parse_wire_error(&response.body);
    let message = provider_message
        .unwrap_or_else(|| format!("DeepSeek API error (HTTP {})", response.status));
    let delay = response
        .retry_after
        .as_deref()
        .and_then(|value| provider_retry_after_ms(value, std::time::SystemTime::now()));
    LlmError::Failure(LlmFailure {
        message,
        code: http_error_code(response.status, detail.as_ref()),
        status: Some(response.status),
        provider_retry_after_ms: delay,
        request_id: response.request_id,
    })
}

fn parse_wire_error(body: &str) -> (Option<String>, Option<WireErrorDetail>) {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return (None, None);
    };
    let Some(error) = value.get("error") else {
        return (None, None);
    };
    let detail = WireErrorDetail {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string),
        r#type: error
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    (detail.message.clone(), Some(detail))
}

async fn tcp_post(
    url: &str,
    headers: &[(impl AsRef<str>, String)],
    body: &str,
) -> Result<String, String> {
    let parsed = parse_http_url(url)?;
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .map_err(|error| error.to_string())?;
    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        parsed.path,
        host_header,
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name.as_ref(), value));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn curl_post(
    url: &str,
    headers: &[(impl AsRef<str>, String)],
    body: &str,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new("curl");
    command
        .arg("-sS")
        .arg("--http1.1")
        .arg("-i")
        .arg("-X")
        .arg("POST");
    for (name, value) in headers {
        command
            .arg("-H")
            .arg(format!("{}: {}", name.as_ref(), value));
    }
    command.arg("--data-binary").arg(body).arg(url);
    let output = command.output().await.map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<HttpUrl, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not an http url: {url}"))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hostport, path)) => (hostport, format!("/{path}")),
        None => (rest, "/".into()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().map_err(|error| error.to_string())?,
        ),
        None => (hostport.to_string(), 80),
    };
    Ok(HttpUrl { host, port, path })
}

struct HttpResponse {
    status: u16,
    retry_after: Option<String>,
    request_id: Option<String>,
    body: String,
}

fn parse_http_status_line(status_line: &str) -> Option<u16> {
    status_line.split_whitespace().nth(1)?.parse().ok()
}

fn parse_http_response(raw: &str) -> Result<HttpResponse, String> {
    let (header, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .ok_or_else(|| "missing HTTP body".to_string())?;
    let mut lines = header.lines();
    let status_line = lines.next().unwrap_or("");
    let status = parse_http_status_line(status_line)
        .ok_or_else(|| format!("missing HTTP status: {status_line}"))?;
    let mut retry_after = None;
    let mut request_id = None;
    let mut deepseek_request_id = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("retry-after") {
            retry_after = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-request-id") {
            request_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-deepseek-request-id") {
            deepseek_request_id = Some(value.to_string());
        }
    }
    Ok(HttpResponse {
        status,
        retry_after,
        request_id: request_id.or(deepseek_request_id),
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{LlmCallConfig, MessageSource, UserMessage};
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn from_env_fails_loud_without_key() {
        let previous = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");
        let Err(error) = DeepSeekAdapter::from_env() else {
            panic!("from_env must fail when DEEPSEEK_API_KEY is unset");
        };
        let LlmError::Failure(failure) = error;
        assert_eq!(failure.code, "MISSING_CREDENTIAL");
        assert!(
            failure
                .message
                .contains("no API key for provider route \"deepseek-official\""),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("DEEPSEEK_API_KEY"),
            "{}",
            failure.message
        );
        if let Some(previous) = previous {
            std::env::set_var("DEEPSEEK_API_KEY", previous);
        }
    }

    #[tokio::test]
    async fn stream_posts_http_and_parses_content() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16_384];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let adapter = DeepSeekAdapter {
            api_key: "test-key".into(),
            base_url: format!("http://{addr}"),
            model: "deepseek-chat".into(),
            images: HashMap::new(),
        };
        let stream = adapter
            .stream(LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::text("ping"))],
                tools: vec![],
                purpose: None,
            })
            .await
            .unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TextDelta { text, .. } if text == "pong"
        )));
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer test-key"));
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("text/event-stream"));
    }

    #[test]
    fn default_catalog_windows_and_settings_overrides() {
        let (default_window, models) = resolve_catalog(None).unwrap();
        assert_eq!(default_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(
            context_window_for("deepseek-v4-flash", default_window, &models),
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(
            context_window_for("unlisted-pass-through", default_window, &models),
            DEFAULT_CONTEXT_WINDOW
        );
        let (default_window, models) = resolve_catalog(Some(&json!({
            "defaultContextWindow": 256_000,
            "models": [
                { "id": "private-fast", "contextWindow": 32_000 },
                { "id": "inherits-default" }
            ]
        })))
        .unwrap();
        assert_eq!(default_window, 256_000);
        assert_eq!(
            context_window_for("private-fast", default_window, &models),
            32_000
        );
        assert_eq!(
            context_window_for("inherits-default", default_window, &models),
            256_000
        );
        assert_eq!(
            context_window_for("unlisted-pass-through", default_window, &models),
            256_000
        );
        let err = resolve_catalog(Some(&json!({ "defaultContextWindow": 0 }))).unwrap_err();
        assert!(
            err.contains("defaultContextWindow must be a positive integer"),
            "{err}"
        );
        let err = resolve_catalog(Some(&json!({
            "models": [{ "id": "m", "contextWindow": 0 }]
        })))
        .unwrap_err();
        assert!(
            err.contains("contextWindow must be a positive integer"),
            "{err}"
        );
        let merged = merge_connection_config(
            Some(&json!({ "defaultContextWindow": 1000, "baseURL": "https://plugin.test" })),
            Some(&json!({ "defaultContextWindow": 2000 })),
        );
        assert_eq!(merged["defaultContextWindow"], 2000);
        assert_eq!(merged["baseURL"], "https://plugin.test");
    }

    fn sample_image() -> (ContentBlock, HashMap<String, Vec<u8>>) {
        let id = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let block = ContentBlock::Image {
            attachment: dsh_llm::ImageContentRef {
                attachment_id: id.into(),
                media_type: "image/png".into(),
                bytes: 4,
                width: 1,
                height: 1,
                name: None,
            },
        };
        let mut images = HashMap::new();
        images.insert(id.into(), vec![1, 2, 3, 4]);
        (block, images)
    }

    #[test]
    fn vision_user_image_becomes_data_url() {
        let (block, images) = sample_image();
        let body = request_body(
            "deepseek-v4-flash-vision-exp",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::from_parts(
                    vec![ContentBlock::text("see"), block],
                    MessageSource::User,
                ))],
                tools: vec![],
                purpose: None,
            },
            &images,
        )
        .unwrap();
        assert!(body.contains("\"stream\":true"));
        assert!(body.contains("image_url"));
        assert!(body.contains("data:image/png;base64,"));
    }

    #[test]
    fn non_vision_model_refuses_image_input() {
        let (block, images) = sample_image();
        let err = request_body(
            "deepseek-chat",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::from_parts(
                    vec![block],
                    MessageSource::User,
                ))],
                tools: vec![],
                purpose: None,
            },
            &images,
        )
        .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "UNSUPPORTED_CONTENT");
        assert_eq!(
            failure.message,
            "DeepSeek model \"deepseek-chat\" does not accept image input."
        );
    }

    #[test]
    fn missing_prepared_image_fails_loud() {
        let (block, _) = sample_image();
        let err = request_body(
            "deepseek-v4-flash-vision-exp",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::from_parts(
                    vec![block],
                    MessageSource::User,
                ))],
                tools: vec![],
                purpose: None,
            },
            &HashMap::new(),
        )
        .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "INVALID_REQUEST");
        assert!(
            failure.message.contains("was not prepared"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn assistant_and_tool_images_are_unsupported() {
        let (block, images) = sample_image();
        let assistant = request_body(
            "deepseek-v4-flash-vision-exp",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::Assistant(dsh_llm::AssistantMessage::model(
                    vec![block.clone()],
                    "deepseek-official",
                    "deepseek-v4-flash-vision-exp",
                ))],
                tools: vec![],
                purpose: None,
            },
            &images,
        )
        .unwrap_err();
        let LlmError::Failure(failure) = assistant;
        assert_eq!(failure.code, "UNSUPPORTED_CONTENT");
        assert!(
            failure
                .message
                .contains("cannot represent image content in a assistant message."),
            "{}",
            failure.message
        );
    }

    #[test]
    fn classifies_http_status_and_provider_detail() {
        assert_eq!(http_error_code(401, None), "AUTH");
        assert_eq!(http_error_code(403, None), "AUTH");
        assert_eq!(
            http_error_code(
                400,
                Some(&WireErrorDetail {
                    message: Some("request too large for model context".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            CONTEXT_WINDOW_EXCEEDED_CODE
        );
        assert_eq!(
            http_error_code(
                400,
                Some(&WireErrorDetail {
                    message: Some(
                        "invalid input: temperature exceeds maximum allowed value".into()
                    ),
                    ..WireErrorDetail::default()
                }),
            ),
            "INVALID_REQUEST"
        );
        assert_eq!(
            http_error_code(
                413,
                Some(&WireErrorDetail {
                    code: Some("context_length_exceeded".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            "INVALID_REQUEST"
        );
        assert_eq!(
            http_error_code(
                429,
                Some(&WireErrorDetail {
                    code: Some("insufficient_quota".into()),
                    message: Some("account credits exhausted".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            QUOTA_EXCEEDED_CODE
        );
        assert_eq!(http_error_code(429, None), "RATE_LIMIT");
        assert_eq!(http_error_code(503, None), "SERVER");
        assert_eq!(http_error_code(418, None), "HTTP_418");
    }

    #[tokio::test]
    async fn retains_status_retry_after_seconds_and_request_id() {
        let failure = stream_http_error(
            429,
            r#"{"error":{"message":"slow down"}}"#,
            &[("Retry-After", "2"), ("x-request-id", "req-429")],
        )
        .await;
        assert_eq!(failure.code, "RATE_LIMIT");
        assert_eq!(failure.message, "slow down");
        assert_eq!(failure.status, Some(429));
        assert_eq!(failure.provider_retry_after_ms, Some(2_000));
        assert_eq!(failure.request_id.as_deref(), Some("req-429"));
    }

    #[tokio::test]
    async fn parses_future_retry_after_http_date_and_deepseek_request_id() {
        let when = std::time::SystemTime::now() + std::time::Duration::from_secs(3);
        let header = retry_after_imf(when);
        let failure = stream_http_error(
            503,
            r#"{"error":{"message":"come back later"}}"#,
            &[
                ("Retry-After", header.as_str()),
                ("x-deepseek-request-id", "deepseek-503"),
            ],
        )
        .await;
        assert_eq!(failure.code, "SERVER");
        assert_eq!(failure.message, "come back later");
        assert_eq!(failure.status, Some(503));
        let delay = failure.provider_retry_after_ms.expect("HTTP-date delay");
        assert!(
            (1_000..=5_000).contains(&delay),
            "expected ~3000ms, got {delay}"
        );
        assert_eq!(failure.request_id.as_deref(), Some("deepseek-503"));
    }

    #[tokio::test]
    async fn omits_zero_invalid_and_past_retry_after() {
        for value in [
            "0",
            &"9".repeat(400),
            "not-a-date",
            "Thu, 01 Jan 1970 00:00:00 GMT",
        ] {
            let failure = stream_http_error(
                429,
                r#"{"error":{"message":"retry later"}}"#,
                &[("Retry-After", value)],
            )
            .await;
            assert_eq!(failure.code, "RATE_LIMIT");
            assert_eq!(failure.status, Some(429));
            assert_eq!(failure.provider_retry_after_ms, None, "{value}");
        }
    }

    async fn stream_http_error(status: u16, body: &str, headers: &[(&str, &str)]) -> LlmFailure {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let extra = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16_384];
            let _ = socket.read(&mut buf).await.unwrap();
            let response = format!(
                "HTTP/1.1 {status} ERR\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = DeepSeekAdapter {
            api_key: "test-key".into(),
            base_url: format!("http://{addr}"),
            model: "deepseek-chat".into(),
            images: HashMap::new(),
        };
        let error = adapter
            .stream(LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::text("ping"))],
                tools: vec![],
                purpose: None,
            })
            .await
            .err()
            .expect("non-2xx must fail");
        let LlmError::Failure(failure) = error;
        failure
    }

    fn retry_after_imf(when: std::time::SystemTime) -> String {
        const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = (secs / 86_400) as i64;
        let tod = secs % 86_400;
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u32;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = y + i64::from(m <= 2);
        format!(
            "{}, {d:02} {} {year} {:02}:{:02}:{:02} GMT",
            WEEKDAYS[days.rem_euclid(7) as usize],
            MONTHS[(m - 1) as usize],
            tod / 3_600,
            (tod % 3_600) / 60,
            tod % 60
        )
    }
}
