//! Web seam (`ctx.web`).

use async_trait::async_trait;
use dsh_cordis::Service;
use std::sync::Arc;
use thiserror::Error;

/// Failures from a web fetch.
#[derive(Debug, Error)]
pub enum WebError {
    /// Retrieval or decoding failed.
    #[error("{0}")]
    Fetch(String),
}

/// Provider that retrieves one URL.
#[async_trait]
pub trait WebFetcher: Send + Sync {
    /// Fetch `url` and return the decoded body.
    async fn fetch(&self, url: &str) -> Result<String, WebError>;
}

/// `ctx.web`.
pub struct WebRuntime {
    fetcher: Arc<dyn WebFetcher>,
}

impl WebRuntime {
    /// Wrap a fetch backend.
    pub fn new(fetcher: Arc<dyn WebFetcher>) -> Self {
        Self { fetcher }
    }

    /// Fetch through the registered backend.
    pub async fn fetch(&self, url: &str) -> Result<String, WebError> {
        self.fetcher.fetch(url).await
    }
}

impl Service for WebRuntime {
    const KEY: &'static str = "web";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    struct StaticFetcher(&'static str);

    #[async_trait]
    impl WebFetcher for StaticFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, WebError> {
            Ok(self.0.into())
        }
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(WebRuntime::new(Arc::new(StaticFetcher("ok")))))
            .unwrap();
        assert!(ctx.has_service("web"));
        ctx.dispose();
        assert!(!ctx.has_service("web"));
    }
}
