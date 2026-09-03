//! Model-facing `web_search` (and optional `web_fetch`) over `ctx.web`.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolError, ToolOutcome, ToolRuntime};
use dsh_web::{WebRuntime, WebSearchRequest, WebSearchResult};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-web"
}

/// Default upper bound on returned sources.
pub const WEB_SEARCH_MAX_RESULTS: usize = 8;
/// Default upper bound on queries in one call.
pub const WEB_SEARCH_MAX_QUERIES: usize = 4;

/// Consumer config.
#[derive(Debug, Clone)]
pub struct Config {
    /// Register `web_search`.
    pub search: bool,
    /// Register `web_fetch`. Base bundle sets this false.
    pub fetch: bool,
    /// Upper bound on sources returned by one call.
    pub search_max_results: usize,
    /// Upper bound on queries accepted by one call.
    pub search_max_queries: usize,
}

impl Config {
    /// Resolve plugin config. `fetch` defaults to true when omitted (TypeScript);
    /// the base patch sets `fetch: false`.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let search = value
            .and_then(|value| value.get("search"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let fetch = value
            .and_then(|value| value.get("fetch"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let search_max_results =
            optional_usize(value, "searchMaxResults", WEB_SEARCH_MAX_RESULTS)?;
        let search_max_queries =
            optional_usize(value, "searchMaxQueries", WEB_SEARCH_MAX_QUERIES)?;
        Ok(Self {
            search,
            fetch,
            search_max_results,
            search_max_queries,
        })
    }
}

fn optional_usize(value: Option<&Value>, field: &str, default: usize) -> std::result::Result<usize, String> {
    match value.and_then(|value| value.get(field)) {
        None => Ok(default),
        Some(item) => {
            let number = item.as_u64().ok_or_else(|| {
                format!("tool-web: {field} must be a positive integer")
            })?;
            if number < 1 {
                return Err(format!("tool-web: {field} must be a positive integer"));
            }
            Ok(number as usize)
        }
    }
}

/// Validate queries: non-empty, bounded, then dedupe first-occurrence order.
pub fn parse_search_args(
    args: &Value,
    max_queries: usize,
) -> std::result::Result<Vec<String>, String> {
    let queries = args
        .get("queries")
        .and_then(Value::as_array)
        .ok_or_else(|| "queries must contain at least one query".to_string())?;
    if queries.is_empty() {
        return Err("queries must contain at least one query".into());
    }
    if queries.len() > max_queries {
        let noun = if max_queries == 1 { "query" } else { "queries" };
        return Err(format!(
            "queries must contain at most {max_queries} {noun}"
        ));
    }
    let mut accepted = Vec::new();
    for query in queries {
        let text = query
            .as_str()
            .ok_or_else(|| "each query must be a non-empty string".to_string())?;
        if text.trim().is_empty() {
            return Err("each query must be a non-empty string".into());
        }
        if !accepted.iter().any(|existing| existing == text) {
            accepted.push(text.to_string());
        }
    }
    Ok(accepted)
}

/// Format a search result as one model-facing text block.
pub fn format_search_output(result: &WebSearchResult) -> String {
    let mut parts = Vec::new();
    if let Some(content) = &result.content {
        if !content.is_empty() {
            parts.push(content.clone());
        }
    }
    if !result.sources.is_empty() {
        let lines: Vec<String> = result
            .sources
            .iter()
            .map(|source| {
                let label = source
                    .title
                    .as_deref()
                    .filter(|title| !title.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| hostname(&source.url));
                let mut meta = Vec::new();
                if let Some(snippet) = &source.snippet {
                    if !snippet.is_empty() {
                        meta.push(snippet.clone());
                    }
                }
                if let Some(published) = &source.published_at {
                    if !published.is_empty() {
                        meta.push(format!("({published})"));
                    }
                }
                let suffix = if meta.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", meta.join(" "))
                };
                format!("- [{label}]({}){suffix}", source.url)
            })
            .collect();
        parts.push(format!("Sources:\n{}", lines.join("\n")));
    } else if result.content.as_ref().map(String::as_str).unwrap_or("").is_empty() {
        parts.push("No results found.".into());
    }
    if result.truncated {
        parts.push(format!(
            "(Showing the first {} sources. Refine the query for more.)",
            result.sources.len()
        ));
    }
    parts.push("Cite the relevant URLs above as markdown links in your answer.".into());
    parts.join("\n\n")
}

fn hostname(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Register enabled web tools.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let web = ctx.service::<WebRuntime>()?;
    if config.search {
        tools.insert(Arc::new(WebSearchTool {
            web: Arc::clone(&web),
            max_results: config.search_max_results,
            max_queries: config.search_max_queries,
        }));
        if let Some(prompt) = ctx.get::<SystemPrompt>() {
            let fetch_note = if config.fetch {
                " Use web_fetch only for a specific URL you already have."
            } else {
                ""
            };
            prompt.register_section(PromptSection {
                id: "tool:web".into(),
                text: format!(
                    "Use the web_search tool to discover current information on the web. The required queries array accepts 1–{} non-empty search queries. Use the returned source snippets when available, and cite the relevant URLs as markdown links.{fetch_note}",
                    config.search_max_queries
                ),
                order: 120,
            });
        }
    }
    if config.fetch {
        tools.insert(Arc::new(WebFetchTool { web }));
    }
    Ok(())
}

struct WebSearchTool {
    web: Arc<WebRuntime>,
    max_results: usize,
    max_queries: usize,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Provide 1–4 non-empty queries; results include source URLs to cite."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["queries"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        let queries = parse_search_args(&args, self.max_queries)
            .map_err(|error| ToolError::Body(format!("Error: {error}")))?;
        let mut merged = WebSearchResult {
            content: None,
            sources: Vec::new(),
            truncated: false,
        };
        let mut answers = Vec::new();
        for query in &queries {
            match self
                .web
                .search(WebSearchRequest {
                    query: query.clone(),
                    max_results: Some(self.max_results),
                })
                .await
            {
                Ok(result) => {
                    if let Some(content) = result.content {
                        if !content.is_empty() {
                            if queries.len() > 1 {
                                answers.push(format!("### {query}\n\n{content}"));
                            } else {
                                answers.push(content);
                            }
                        }
                    }
                    merged.sources.extend(result.sources);
                    merged.truncated |= result.truncated;
                }
                Err(error) => return Ok(ToolOutcome::error(format!("Error: {error}"))),
            }
        }
        if merged.sources.len() > self.max_results {
            merged.sources.truncate(self.max_results);
            merged.truncated = true;
        }
        if !answers.is_empty() {
            merged.content = Some(answers.join("\n\n"));
        }
        Ok(ToolOutcome::text(format_search_output(&merged)))
    }
}

struct WebFetchTool {
    web: Arc<WebRuntime>,
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
        json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_web::{WebError, WebFetcher, WebRuntimeConfig, WebSearchProvider};

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
            _request: WebSearchRequest,
        ) -> std::result::Result<WebSearchResult, WebError> {
            Ok(WebSearchResult {
                content: Some("answer".into()),
                sources: vec![dsh_web::WebSearchSource {
                    url: "https://example.test".into(),
                    title: Some("Example".into()),
                    snippet: Some("hello".into()),
                    published_at: None,
                }],
                truncated: false,
            })
        }
    }

    #[test]
    fn format_includes_cite_instruction() {
        let text = format_search_output(&WebSearchResult {
            content: None,
            sources: vec![],
            truncated: false,
        });
        assert!(text.contains("No results found."));
        assert!(text.contains("Cite the relevant URLs"));
    }

    #[tokio::test]
    async fn search_tool_formats_sources() {
        let ctx = Context::new();
        let web = WebRuntime::install(&ctx, WebRuntimeConfig::default()).unwrap();
        web.register_search_provider(Arc::new(StaticSearch)).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        install(
            &ctx,
            Config {
                search: true,
                fetch: false,
                search_max_results: 8,
                search_max_queries: 4,
            },
        )
        .unwrap();
        let tool = ctx.service::<ToolRuntime>().unwrap().get("web_search").unwrap();
        let outcome = tool
            .execute(json!({ "queries": ["rust"] }))
            .await
            .unwrap();
        let text = match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text,
            _ => panic!("text"),
        };
        assert!(text.contains("[Example](https://example.test)"));
        assert!(text.contains("hello"));
    }

    struct StaticFetcher;

    #[async_trait]
    impl WebFetcher for StaticFetcher {
        async fn fetch(&self, url: &str) -> std::result::Result<String, WebError> {
            Ok(format!("fetched {url}"))
        }
    }

    #[tokio::test]
    async fn fetches_url_when_enabled() {
        let web = Arc::new(WebRuntime::new(Arc::new(StaticFetcher)));
        let tool = WebFetchTool { web };
        let outcome = tool
            .execute(json!({ "url": "http://example.test" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
    }
}
