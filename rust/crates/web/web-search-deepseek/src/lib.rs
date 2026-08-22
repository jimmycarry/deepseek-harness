//! DeepSeek official search provider (`deepseek-official`).
//!
//! Live calls POST `${baseURL}/messages` with Anthropic `web_search_20250305`.
//! A `replay` config serves keyless snapshots without the network.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_session::{SessionEventData, SessionStore};
use dsh_web::{
    WebError, WebRuntime, WebSearchProvider, WebSearchRequest, WebSearchResult, WebSearchSource,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-web-search-deepseek"
}

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/anthropic/v1";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";
const DEFAULT_API_VERSION: &str = "2023-06-01";

/// Provider construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Environment variable that holds the API key.
    pub api_key_env: String,
    /// Messages endpoint prefix.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Anthropic version header.
    pub api_version: String,
    /// Optional fixture result for keyless replay.
    pub replay: Option<WebSearchResult>,
}

impl Config {
    /// Resolve plugin config. `replay` is optional.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let api_key_env = value
            .and_then(|value| value.get("apiKeyEnv"))
            .and_then(Value::as_str)
            .unwrap_or("DEEPSEEK_API_KEY")
            .to_string();
        let base_url = value
            .and_then(|value| value.get("baseURL"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| std::env::var("DEEPSEEK_SEARCH_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let model = value
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_MODEL)
            .to_string();
        let api_version = value
            .and_then(|value| value.get("apiVersion"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_API_VERSION)
            .to_string();
        let replay = value
            .and_then(|value| value.get("replay"))
            .map(|item| {
                serde_json::from_value::<WebSearchResult>(item.clone())
                    .map_err(|error| format!("web-search-deepseek: invalid replay: {error}"))
            })
            .transpose()?;
        Ok(Self {
            api_key_env,
            base_url,
            model,
            api_version,
            replay,
        })
    }
}

/// DeepSeek search backend.
pub struct DeepSeekSearch {
    config: Config,
    sessions: Option<Arc<SessionStore>>,
}

impl DeepSeekSearch {
    /// Bind config and an optional session store for request logging.
    pub fn new(config: Config, sessions: Option<Arc<SessionStore>>) -> Self {
        Self { config, sessions }
    }
}

/// Register on `ctx.web`.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let web = ctx.service::<WebRuntime>()?;
    let sessions = ctx.get::<SessionStore>();
    web.register_search_provider(Arc::new(DeepSeekSearch::new(config, sessions)))
        .map_err(|error| dsh_cordis::CordisError::Validation(error.to_string()))?;
    Ok(())
}

#[async_trait]
impl WebSearchProvider for DeepSeekSearch {
    fn id(&self) -> &str {
        "deepseek-official"
    }

    fn available(&self) -> bool {
        self.config.replay.is_some() || std::env::var(&self.config.api_key_env).is_ok()
    }

    async fn search(
        &self,
        request: WebSearchRequest,
    ) -> std::result::Result<WebSearchResult, WebError> {
        if let Some(replay) = &self.config.replay {
            return Ok(replay.clone());
        }
        let api_key = std::env::var(&self.config.api_key_env).map_err(|_| {
            WebError::Provider(format!(
                "DeepSeek search has no API key for \"{}\"; store it through the credentials service (the web Models page writes it), export it in the launching environment, or set a literal \"apiKey\" in the web-search-deepseek config",
                self.config.api_key_env
            ))
        })?;
        let endpoint = format!("{}/messages", self.config.base_url.trim_end_matches('/'));
        let body = json!({
            "model": self.config.model,
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("Perform a web search for the query: {}", request.query)
                }]
            }],
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 5
            }]
        });
        if let Some(store) = &self.sessions {
            if let Some(session) = store.live().first() {
                let _ = session.append(
                    SessionEventData::Extension {
                        type_name: "web/deepseek-search-llm-request".into(),
                        data: json!({
                            "endpoint": endpoint,
                            "apiVersion": self.config.api_version,
                            "body": body,
                        }),
                    },
                    None,
                );
            }
        }
        let output = tokio::process::Command::new("curl")
            .arg("-sS")
            .arg("--http1.1")
            .arg("-X")
            .arg("POST")
            .arg(&endpoint)
            .arg("-H")
            .arg(format!("x-api-key: {api_key}"))
            .arg("-H")
            .arg(format!("Authorization: Bearer {api_key}"))
            .arg("-H")
            .arg(format!("anthropic-version: {}", self.config.api_version))
            .arg("-H")
            .arg("content-type: application/json")
            .arg("-d")
            .arg(body.to_string())
            .output()
            .await
            .map_err(|error| WebError::Provider(format!("DeepSeek search request failed: {error}")))?;
        if !output.status.success() {
            return Err(WebError::Provider(format!(
                "DeepSeek API error ({})",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let parsed: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            WebError::Provider(format!("DeepSeek search request failed: {error}"))
        })?;
        parse_search_response(&parsed)
    }
}

fn parse_search_response(value: &Value) -> std::result::Result<WebSearchResult, WebError> {
    let mut sources = Vec::new();
    let mut content = None;
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut saw_result = false;
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str);
        if kind == Some("web_search_tool_result") {
            saw_result = true;
            if let Some(items) = block.get("content").and_then(Value::as_array) {
                for item in items {
                    if let Some(url) = item.get("url").and_then(Value::as_str) {
                        sources.push(WebSearchSource {
                            url: url.into(),
                            title: item
                                .get("title")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            snippet: item
                                .get("snippet")
                                .or_else(|| item.get("cited_text"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            published_at: item
                                .get("published_at")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        });
                    }
                }
            }
        }
        if kind == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                content = Some(text.to_string());
            }
        }
    }
    if !saw_result {
        return Err(WebError::Provider(
            "DeepSeek returned no web_search_tool_result blocks; the request may not have triggered native web search"
                .into(),
        ));
    }
    Ok(WebSearchResult {
        content,
        sources,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_web::WebRuntimeConfig;

    #[test]
    fn replay_is_available_without_key() {
        let config = Config::resolve(Some(&json!({
            "replay": {
                "content": "answer",
                "sources": [{ "url": "https://example.test", "title": "Example" }],
                "truncated": false
            }
        })))
        .unwrap();
        let provider = DeepSeekSearch::new(config, None);
        assert!(provider.available());
    }

    #[tokio::test]
    async fn replay_returns_fixture() {
        let ctx = Context::new();
        WebRuntime::install(&ctx, WebRuntimeConfig::default()).unwrap();
        install(
            &ctx,
            Config::resolve(Some(&json!({
                "replay": {
                    "sources": [{ "url": "https://example.test" }],
                    "truncated": false
                }
            })))
            .unwrap(),
        )
        .unwrap();
        let result = ctx
            .service::<WebRuntime>()
            .unwrap()
            .search(WebSearchRequest {
                query: "q".into(),
                max_results: Some(8),
            })
            .await
            .unwrap();
        assert_eq!(result.sources[0].url, "https://example.test");
    }
}
