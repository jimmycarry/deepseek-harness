//! Scripted LLM adapter. Mock only this boundary; keep the rest of the tree real.

use async_trait::async_trait;
use dsh_llm::{
    text_block, tool_block, ContentBlock, FinishReason, LlmAdapter, LlmError, LlmFailure, LlmRequest,
    StreamChunk,
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

/// Plays recorded turns in order, then repeats the last one. Auxiliary
/// requests (a non-empty `purpose`) never consume the scripted turn queue:
/// they are served from the optional per-purpose script or rejected.
pub struct ReplayAdapter {
    turns: Vec<ReplayTurn>,
    cursor: std::sync::atomic::AtomicUsize,
    auxiliary: std::collections::HashMap<String, String>,
}

impl ReplayAdapter {
    /// Build from an ordered script.
    pub fn new(turns: Vec<ReplayTurn>) -> Self {
        Self {
            turns,
            cursor: std::sync::atomic::AtomicUsize::new(0),
            auxiliary: std::collections::HashMap::new(),
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
                status: None,
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
}
