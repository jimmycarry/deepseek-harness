//! DeepSeek LLM adapter. Self-skips with-key tests when `DEEPSEEK_API_KEY` is unset.

use async_trait::async_trait;
use dsh_credentials::{Credential, CredentialsRuntime};
use dsh_llm::{
    ContentBlock, LlmAdapter, LlmError, LlmFailure, LlmModelContext, LlmResolvedModelInfo,
    LlmRequest, Message, StreamChunk,
};
use futures::stream::{self, BoxStream};
use serde_json::{json, Map, Value};
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
pub fn context_window_for(model: &str, default_context_window: u32, models: &[CatalogModel]) -> u32 {
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
    LlmError::Failure(LlmFailure {
        message: format!(
            "llm-deepseek: no API key for provider route \"{PROVIDER}\"; store {api_key_env} through the credentials service (the web Models page writes it), or export {api_key_env} in the launching environment"
        ),
        code: "MISSING_CREDENTIAL".into(),
        status: None,
    })
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
    LlmError::Failure(LlmFailure {
        message,
        code: "CONFIG".into(),
        status: None,
    })
}

/// DeepSeek chat adapter.
pub struct DeepSeekAdapter {
    /// API key resolved at construction.
    pub api_key: String,
    /// Optional base URL override.
    pub base_url: String,
    /// Model id.
    pub model: String,
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
        })
    }
}

#[async_trait]
impl LlmAdapter for DeepSeekAdapter {
    async fn stream(&self, request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Failure(LlmFailure {
                message: "empty key".into(),
                code: "MISSING_CREDENTIAL".into(),
                status: None,
            }));
        }
        let url = join_url(&self.base_url, "/chat/completions");
        let body = request_body(&self.model, &request);
        let raw = post_json(&url, &self.api_key, &body)
            .await
            .map_err(|message| {
                LlmError::Failure(LlmFailure {
                    message,
                    code: "TRANSPORT".into(),
                    status: None,
                })
            })?;
        let content = parse_content(&raw).map_err(|message| {
            LlmError::Failure(LlmFailure {
                message,
                code: "TRANSPORT".into(),
                status: None,
            })
        })?;
        Ok(Box::pin(stream::iter(StreamChunk::text_stream(content))))
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let (default_window, models) =
            resolve_catalog(None).map_err(catalog_error)?;
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

fn request_body(model: &str, request: &LlmRequest) -> String {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        match message {
            Message::User(user) => {
                messages.push(json!({ "role": "user", "content": blocks_text(&user.content) }));
            }
            Message::Assistant(assistant) => {
                messages.push(json!({ "role": "assistant", "content": assistant.text() }));
            }
            Message::Tool(tool) => {
                messages.push(json!({
                    "role": "tool",
                    "content": blocks_text(tool.result_blocks()),
                    "tool_call_id": tool.tool_call_id().unwrap_or(""),
                }));
            }
        }
    }
    json!({
        "model": model,
        "messages": messages,
        "stream": false,
    })
    .to_string()
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

fn parse_content(raw: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "missing choices[0].message.content".into())
}

async fn post_json(url: &str, api_key: &str, body: &str) -> Result<String, String> {
    let headers = [
        ("Authorization", format!("Bearer {api_key}")),
        ("Content-Type", "application/json".into()),
    ];
    if url.starts_with("https://") {
        curl_post(url, &headers, body).await
    } else if url.starts_with("http://") {
        tcp_post(url, &headers, body).await
    } else {
        Err(format!("unsupported url: {url}"))
    }
}

async fn tcp_post(url: &str, headers: &[(impl AsRef<str>, String)], body: &str) -> Result<String, String> {
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
    split_http_body(&buf)
}

async fn curl_post(url: &str, headers: &[(impl AsRef<str>, String)], body: &str) -> Result<String, String> {
    let mut command = tokio::process::Command::new("curl");
    command.arg("-sS").arg("--http1.1").arg("-X").arg("POST");
    for (name, value) in headers {
        command.arg("-H").arg(format!("{}: {}", name.as_ref(), value));
    }
    command.arg("--data-binary").arg(body).arg(url);
    let output = command
        .output()
        .await
        .map_err(|error| error.to_string())?;
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

fn split_http_body(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let body = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map(|(_, body)| body)
        .ok_or_else(|| "missing HTTP body".to_string())?;
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{LlmCallConfig, UserMessage};
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
        assert!(failure.message.contains("DEEPSEEK_API_KEY"), "{}", failure.message);
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
            let body = r#"{"choices":[{"message":{"content":"pong"}}]}"#;
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
        assert!(err.contains("defaultContextWindow must be a positive integer"), "{err}");
        let err = resolve_catalog(Some(&json!({
            "models": [{ "id": "m", "contextWindow": 0 }]
        })))
        .unwrap_err();
        assert!(err.contains("contextWindow must be a positive integer"), "{err}");
        let merged = merge_connection_config(
            Some(&json!({ "defaultContextWindow": 1000, "baseURL": "https://plugin.test" })),
            Some(&json!({ "defaultContextWindow": 2000 })),
        );
        assert_eq!(merged["defaultContextWindow"], 2000);
        assert_eq!(merged["baseURL"], "https://plugin.test");
    }
}
