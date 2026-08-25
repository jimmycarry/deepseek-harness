//! Scripted LLM adapter. Mock only this boundary; keep the rest of the tree real.

use async_trait::async_trait;
use dsh_llm::{
    text_block, tool_block, ContentBlock, FinishReason, LlmAdapter, LlmError, LlmFailure,
    LlmModelContext, LlmResolvedModelInfo, LlmRequest, RetryPolicy, StreamChunk,
};
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};

/// One scripted model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayTurn {
    /// Visible text.
    #[serde(default)]
    pub text: String,
    /// Optional single tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ReplayToolCall>,
    /// Terminal finish when the script wants one recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<dsh_llm::FinishReason>,
    /// When set, this turn fails the stream instead of emitting chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReplayFailure>,
}

/// Scripted adapter failure replayed as `LlmError::Failure`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFailure {
    /// Human-readable provider or transport failure.
    pub message: String,
    /// Stable provider-neutral machine-routing code.
    pub code: String,
    /// HTTP status returned by the provider, when the script supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// Scripted tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayToolCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Raw arguments JSON.
    pub arguments: String,
}

/// One model exposed by a replay-only provider catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayModelConfig {
    /// Model id used for replay requests.
    pub id: String,
    /// Selector label; defaults to [`Self::id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional positive integer context capacity published by the adapter.
    #[serde(default, rename = "contextWindow", skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

/// One provider route exposed by the replay adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayProviderConfig {
    /// Provider route used for replay requests.
    pub id: String,
    /// Advisory models exposed to replay scenarios that exercise discovery.
    #[serde(default)]
    pub models: Vec<ReplayModelConfig>,
}

/// Plays recorded turns in order, then repeats the last one. Auxiliary
/// requests (a non-empty `purpose`) never consume the scripted turn queue:
/// they are served from the optional per-purpose script or rejected.
pub struct ReplayAdapter {
    turns: Vec<ReplayTurn>,
    cursor: std::sync::atomic::AtomicUsize,
    auxiliary: std::collections::HashMap<String, String>,
    providers: std::collections::HashMap<String, ReplayProviderConfig>,
    retry_policies: std::collections::HashMap<String, RetryPolicy>,
}

impl ReplayAdapter {
    /// Build from an ordered script.
    pub fn new(turns: Vec<ReplayTurn>) -> Self {
        Self {
            turns,
            cursor: std::sync::atomic::AtomicUsize::new(0),
            auxiliary: std::collections::HashMap::new(),
            providers: std::collections::HashMap::new(),
            retry_policies: std::collections::HashMap::new(),
        }
    }

    /// Single text reply.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![ReplayTurn {
            text: text.into(),
            tool: None,
            finish: None,
            error: None,
        }])
    }

    /// Serve auxiliary requests carrying `purpose` with one fixed text reply.
    pub fn with_auxiliary(mut self, purpose: impl Into<String>, text: impl Into<String>) -> Self {
        self.auxiliary.insert(purpose.into(), text.into());
        self
    }

    /// Publish a replay-only provider/model catalog for `resolve_model`.
    pub fn with_providers(mut self, providers: Vec<ReplayProviderConfig>) -> Self {
        self.providers = providers
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        self
    }

    /// Capture per-provider retry policies resolved at mount.
    pub fn with_retry_policies(
        mut self,
        policies: std::collections::HashMap<String, RetryPolicy>,
    ) -> Self {
        self.retry_policies = policies;
        self
    }
}

#[async_trait]
impl LlmAdapter for ReplayAdapter {
    async fn stream(&self, request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        if let Some(purpose) = request.purpose.as_deref() {
            let Some(text) = self.auxiliary.get(purpose) else {
                return Err(LlmError::Failure(dsh_llm::LlmFailure {
                    message: format!("replay adapter has no auxiliary script for purpose {purpose}"),
                    code: "REPLAY_NO_AUXILIARY".into(),
                    status: None,
                }));
            };
            let mut chunks = text_block(0, text.clone());
            chunks.push(StreamChunk::Finish {
                reason: FinishReason::Stop,
                replay_state: None,
            });
            return Ok(Box::pin(stream::iter(chunks)));
        }
        let index = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .min(self.turns.len().saturating_sub(1));
        let turn = self.turns.get(index).cloned().unwrap_or(ReplayTurn {
            text: String::new(),
            tool: None,
            finish: None,
            error: None,
        });
        if let Some(error) = turn.error {
            return Err(LlmError::Failure(LlmFailure {
                message: error.message,
                code: error.code,
                status: error.status,
            }));
        }
        let mut chunks = Vec::new();
        let mut index = 0u32;
        if !turn.text.is_empty() {
            chunks.extend(text_block(index, turn.text));
            index += 1;
        }
        let has_tool = turn.tool.is_some();
        if let Some(tool) = turn.tool {
            chunks.extend(tool_block(index, tool.id, tool.name, tool.arguments));
        }
        let reason = turn.finish.unwrap_or(if has_tool {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        });
        chunks.push(StreamChunk::Finish {
            reason,
            replay_state: None,
        });
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let Some(configured) = self.providers.get(provider) else {
            return Ok(LlmResolvedModelInfo::identity(provider, model));
        };
        let configured_model = configured
            .models
            .iter()
            .find(|candidate| candidate.id == model);
        Ok(LlmResolvedModelInfo {
            provider: provider.to_string(),
            id: model.to_string(),
            name: configured_model
                .and_then(|item| item.name.clone())
                .unwrap_or_else(|| model.to_string()),
            description: None,
            context: configured_model.and_then(|item| {
                item.context_window
                    .map(|context_window| LlmModelContext { context_window })
            }),
            default_max_tokens: None,
            input_modalities: None,
            reasoning: None,
        })
    }

    fn provider_retry_policy(&self, provider: &str) -> RetryPolicy {
        self.retry_policies
            .get(provider)
            .cloned()
            .unwrap_or_default()
    }
}

/// Project content blocks to text for snapshot assertions.
pub fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn replays_scripted_text() {
        let adapter = ReplayAdapter::text("pong");
        let stream = adapter
            .stream(LlmRequest {
                config: dsh_llm::LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![],
                tools: vec![],
                purpose: None,
            })
            .await
            .unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert!(matches!(
            chunks.first(),
            Some(StreamChunk::BlockStart {
                block_type,
                ..
            }) if block_type == "text"
        ));
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TextDelta { text, .. } if text == "pong"
        )));
        assert_eq!(
            chunks.last(),
            Some(&StreamChunk::Finish {
                reason: FinishReason::Stop,
                replay_state: None,
            })
        );
    }

    #[tokio::test]
    async fn throws_scripted_failure_then_plays_the_next_turn() {
        let adapter = ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: None,
                finish: None,
                error: Some(ReplayFailure {
                    message: "context overflow".into(),
                    code: "CONTEXT_WINDOW_EXCEEDED".into(),
                    status: None,
                }),
            },
            ReplayTurn {
                text: "recovered".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ]);
        let request = LlmRequest {
            config: dsh_llm::LlmCallConfig::default(),
            adapter_defaults: None,
            system: None,
            messages: vec![],
            tools: vec![],
            purpose: None,
        };
        match adapter.stream(request.clone()).await {
            Err(LlmError::Failure(failure)) => {
                assert_eq!(failure.code, "CONTEXT_WINDOW_EXCEEDED");
                assert_eq!(failure.message, "context overflow");
            }
            Ok(_) => panic!("expected CONTEXT_WINDOW_EXCEEDED"),
        }
        let stream = adapter.stream(request).await.unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TextDelta { text, .. } if text == "recovered"
        )));
    }

    #[tokio::test]
    async fn resolve_model_publishes_configured_context_window() {
        let adapter = ReplayAdapter::text("pong").with_providers(vec![ReplayProviderConfig {
            id: "replay".into(),
            models: vec![ReplayModelConfig {
                id: "script".into(),
                name: Some("Script".into()),
                context_window: Some(500),
            }],
        }]);
        let listed = adapter.resolve_model("replay", "script").await.unwrap();
        assert_eq!(listed.context.unwrap().context_window, 500);
        let unlisted = adapter.resolve_model("replay", "other").await.unwrap();
        assert!(unlisted.context.is_none());
        let missing = adapter.resolve_model("empty", "script").await.unwrap();
        assert!(missing.context.is_none());
    }
}
