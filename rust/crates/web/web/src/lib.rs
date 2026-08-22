//! Web access seam (`ctx.web`): search and fetch provider registries.

use async_trait::async_trait;
use dsh_cordis::{Context, Result, Service};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Failures from search or fetch selection and execution.
#[derive(Debug, Error)]
pub enum WebError {
    /// Duplicate provider id.
    #[error("a web provider with id \"{0}\" is already registered")]
    DuplicateProvider(String),
    /// No usable provider is registered.
    #[error("no usable web provider is registered")]
    ProviderUnavailable,
    /// More than one usable provider and none configured.
    #[error("multiple usable web providers are registered ({0}); configure one explicitly")]
    ProviderAmbiguous(String),
    /// Configured id is not registered.
    #[error("configured web provider \"{0}\" is not registered")]
    ConfiguredMissing(String),
    /// Configured id is registered but `available()` is false.
    #[error("configured web provider \"{0}\" is registered but unavailable")]
    ConfiguredUnavailable(String),
    /// Provider execution failed.
    #[error("{0}")]
    Provider(String),
    /// Retrieval or decoding failed.
    #[error("{0}")]
    Fetch(String),
}

impl WebError {
    /// Structured code matching TypeScript `WEB_*`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateProvider(_) => "WEB_DUPLICATE_PROVIDER",
            Self::ProviderUnavailable => "WEB_PROVIDER_UNAVAILABLE",
            Self::ProviderAmbiguous(_) => "WEB_PROVIDER_AMBIGUOUS",
            Self::ConfiguredMissing(_) => "WEB_PROVIDER_CONFIGURED_MISSING",
            Self::ConfiguredUnavailable(_) => "WEB_PROVIDER_CONFIGURED_UNAVAILABLE",
            Self::Provider(_) | Self::Fetch(_) => "WEB_PROVIDER_ERROR",
        }
    }
}

/// One citeable search source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchSource {
    /// Source URL.
    pub url: String,
    /// Optional title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional snippet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Optional ISO-8601 publication timestamp.
    #[serde(rename = "publishedAt", skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// Normalized search outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchResult {
    /// Optional provider-generated answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Citeable sources, already truncated.
    pub sources: Vec<WebSearchSource>,
    /// True when the seam dropped sources to honor `maxResults`.
    pub truncated: bool,
}

/// What one search backend is asked to search.
#[derive(Debug, Clone)]
pub struct WebSearchRequest {
    /// Query text.
    pub query: String,
    /// Upper bound on returned sources.
    pub max_results: Option<usize>,
}

/// Search-capable backend.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// Registry id.
    fn id(&self) -> &str;
    /// Whether this provider can serve a request now.
    fn available(&self) -> bool;
    /// Execute one query.
    async fn search(&self, request: WebSearchRequest) -> std::result::Result<WebSearchResult, WebError>;
}

/// Provider that retrieves one URL.
#[async_trait]
pub trait WebFetcher: Send + Sync {
    /// Fetch `url` and return the decoded body.
    async fn fetch(&self, url: &str) -> std::result::Result<String, WebError>;
}

/// Deployment-varying provider pins.
#[derive(Debug, Clone, Default)]
pub struct WebRuntimeConfig {
    /// Explicit search provider id.
    pub search_provider: Option<String>,
    /// Explicit fetch provider id.
    pub fetch_provider: Option<String>,
}

impl WebRuntimeConfig {
    /// Resolve config, then `$DSH_WEB_SEARCH_PROVIDER` / `$DSH_WEB_FETCH_PROVIDER`.
    pub fn resolve(value: Option<&serde_json::Value>) -> Self {
        let search_provider = value
            .and_then(|value| value.get("searchProvider"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| std::env::var("DSH_WEB_SEARCH_PROVIDER").ok());
        let fetch_provider = value
            .and_then(|value| value.get("fetchProvider"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| std::env::var("DSH_WEB_FETCH_PROVIDER").ok());
        Self {
            search_provider,
            fetch_provider,
        }
    }
}

/// `ctx.web`.
pub struct WebRuntime {
    config: WebRuntimeConfig,
    search: Mutex<HashMap<String, Arc<dyn WebSearchProvider>>>,
    fetcher: Mutex<Option<Arc<dyn WebFetcher>>>,
}

impl WebRuntime {
    /// Wrap a fetch backend (tests / older callers).
    pub fn new(fetcher: Arc<dyn WebFetcher>) -> Self {
        Self {
            config: WebRuntimeConfig::default(),
            search: Mutex::new(HashMap::new()),
            fetcher: Mutex::new(Some(fetcher)),
        }
    }

    /// Empty registries with explicit provider pins.
    pub fn with_config(config: WebRuntimeConfig) -> Self {
        Self {
            config,
            search: Mutex::new(HashMap::new()),
            fetcher: Mutex::new(None),
        }
    }

    /// Provide `ctx.web`.
    pub fn install(ctx: &Context, config: WebRuntimeConfig) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self::with_config(config));
        ctx.provide(Arc::clone(&runtime))?;
        Ok(runtime)
    }

    /// Register a search provider. Duplicate ids fail loud.
    pub fn register_search_provider(
        &self,
        provider: Arc<dyn WebSearchProvider>,
    ) -> std::result::Result<(), WebError> {
        let id = provider.id().to_string();
        let mut map = self.search.lock().expect("web search");
        if map.contains_key(&id) {
            return Err(WebError::DuplicateProvider(id));
        }
        map.insert(id, provider);
        Ok(())
    }

    /// Replace the fetch backend.
    pub fn set_fetcher(&self, fetcher: Arc<dyn WebFetcher>) {
        *self.fetcher.lock().expect("web fetch") = Some(fetcher);
    }

    /// Fetch through the registered backend.
    pub async fn fetch(&self, url: &str) -> std::result::Result<String, WebError> {
        let fetcher = self
            .fetcher
            .lock()
            .expect("web fetch")
            .clone()
            .ok_or(WebError::ProviderUnavailable)?;
        fetcher.fetch(url).await
    }

    /// Search through the selected provider, then truncate to `max_results`.
    pub async fn search(
        &self,
        request: WebSearchRequest,
    ) -> std::result::Result<WebSearchResult, WebError> {
        let provider = self.select_search()?;
        let max_results = request.max_results;
        let mut result = provider.search(request).await?;
        if let Some(max) = max_results {
            if result.sources.len() > max {
                result.sources.truncate(max);
                result.truncated = true;
            }
        }
        Ok(result)
    }

    fn select_search(&self) -> std::result::Result<Arc<dyn WebSearchProvider>, WebError> {
        let map = self.search.lock().expect("web search");
        if let Some(id) = &self.config.search_provider {
            return match map.get(id) {
                None => Err(WebError::ConfiguredMissing(id.clone())),
                Some(provider) if provider.available() => Ok(Arc::clone(provider)),
                Some(_) => Err(WebError::ConfiguredUnavailable(id.clone())),
            };
        }
        let usable: Vec<_> = map
            .values()
            .filter(|provider| provider.available())
            .cloned()
            .collect();
        match usable.len() {
            0 => Err(WebError::ProviderUnavailable),
            1 => Ok(Arc::clone(&usable[0])),
            _ => {
                let mut ids: Vec<_> = usable.iter().map(|provider| provider.id().to_string()).collect();
                ids.sort();
                Err(WebError::ProviderAmbiguous(ids.join(", ")))
            }
        }
    }
}

impl Service for WebRuntime {
    const KEY: &'static str = "web";
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticFetcher(&'static str);

    #[async_trait]
    impl WebFetcher for StaticFetcher {
        async fn fetch(&self, _url: &str) -> std::result::Result<String, WebError> {
            Ok(self.0.into())
        }
    }

    struct StaticSearch;

    #[async_trait]
    impl WebSearchProvider for StaticSearch {
        fn id(&self) -> &str {
            "fixture"
        }
        fn available(&self) -> bool {
            true
        }
        async fn search(
            &self,
            request: WebSearchRequest,
        ) -> std::result::Result<WebSearchResult, WebError> {
            Ok(WebSearchResult {
                content: Some(format!("answer for {}", request.query)),
                sources: vec![WebSearchSource {
                    url: "https://example.test".into(),
                    title: Some("Example".into()),
                    snippet: Some("hello".into()),
                    published_at: None,
                }],
                truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn search_selects_single_usable_provider() {
        let runtime = WebRuntime::with_config(WebRuntimeConfig::default());
        runtime
            .register_search_provider(Arc::new(StaticSearch))
            .unwrap();
        let result = runtime
            .search(WebSearchRequest {
                query: "q".into(),
                max_results: Some(8),
            })
            .await
            .unwrap();
        assert_eq!(result.sources.len(), 1);
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        WebRuntime::install(&ctx, WebRuntimeConfig::default()).unwrap();
        assert!(ctx.has_service("web"));
        ctx.dispose();
        assert!(!ctx.has_service("web"));
    }
}
