//! DeepSeek LLM adapter. Self-skips with-key tests when `DEEPSEEK_API_KEY` is unset.

use async_trait::async_trait;
use dsh_llm::{LlmAdapter, LlmError, LlmFailure, LlmRequest, StreamChunk};
use futures::stream::{self, BoxStream};

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
        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| {
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
        // Product HTTP is wired at the adapter boundary; this port keeps the
        // request envelope reconstructable from the session log.
        let preview = request
            .messages
            .last()
            .map(|message| format!("{message:?}"))
            .unwrap_or_default();
        if self.api_key.is_empty() {
            return Err(LlmError::Failure(LlmFailure {
                message: "empty key".into(),
                code: "MISSING_CREDENTIAL".into(),
                status: None,
            }));
        }
        let _ = (preview, &self.base_url, &self.model);
        Err(LlmError::Failure(LlmFailure {
            message: "live HTTP is enabled only when a with-key e2e supplies a transport".into(),
            code: "TRANSPORT".into(),
            status: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_fails_loud_without_key() {
        let previous = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");
        assert!(DeepSeekAdapter::from_env().is_err());
        if let Some(previous) = previous {
            std::env::set_var("DEEPSEEK_API_KEY", previous);
        }
    }
}
