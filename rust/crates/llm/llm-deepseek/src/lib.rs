//! DeepSeek LLM adapter. Self-skips with-key tests when `DEEPSEEK_API_KEY` is unset.

use async_trait::async_trait;
use dsh_llm::{
    ContentBlock, LlmAdapter, LlmError, LlmFailure, LlmRequest, Message, StreamChunk,
};
use futures::stream::{self, BoxStream};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                LlmError::Failure(LlmFailure {
                    message: "DEEPSEEK_API_KEY is not set".into(),
                    code: "MISSING_CREDENTIAL".into(),
                    status: None,
                })
            })?;
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
        assert!(DeepSeekAdapter::from_env().is_err());
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
}
