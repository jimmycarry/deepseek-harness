//! Model-facing web tools.

use async_trait::async_trait;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use dsh_web::WebRuntime;
use serde_json::Value;
use std::sync::Arc;

/// `web_fetch` over [`WebRuntime`].
pub struct WebFetchTool {
    web: Arc<WebRuntime>,
}

impl WebFetchTool {
    /// Bind to `ctx.web`.
    pub fn new(web: Arc<WebRuntime>) -> Self {
        Self { web }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its body."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("url required".into()))?;
        match self.web.fetch(url).await {
            Ok(body) => Ok(ToolOutcome::text(body)),
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-web"
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_web::{WebError, WebFetcher};

    struct StaticFetcher;

    #[async_trait]
    impl WebFetcher for StaticFetcher {
        async fn fetch(&self, url: &str) -> Result<String, WebError> {
            Ok(format!("fetched {url}"))
        }
    }

    #[tokio::test]
    async fn fetches_url() {
        let web = Arc::new(WebRuntime::new(Arc::new(StaticFetcher)));
        let tool = WebFetchTool::new(web);
        let outcome = tool
            .execute(serde_json::json!({ "url": "http://example.test" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
    }
}
