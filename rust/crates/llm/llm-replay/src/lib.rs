//! Scripted LLM adapter. Mock only this boundary; keep the rest of the tree real.

use async_trait::async_trait;
use dsh_llm::{
    text_block, tool_block, ContentBlock, FinishReason, LlmAdapter, LlmError, LlmRequest,
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

/// Plays recorded turns in order, then repeats the last one.
pub struct ReplayAdapter {
    turns: Vec<ReplayTurn>,
    cursor: std::sync::atomic::AtomicUsize,
}

impl ReplayAdapter {
    /// Build from an ordered script.
    pub fn new(turns: Vec<ReplayTurn>) -> Self {
        Self {
            turns,
            cursor: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Single text reply.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![ReplayTurn {
            text: text.into(),
            tool: None,
            finish: None,
        }])
    }
}

#[async_trait]
impl LlmAdapter for ReplayAdapter {
    async fn stream(&self, _request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        let index = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .min(self.turns.len().saturating_sub(1));
        let turn = self.turns.get(index).cloned().unwrap_or(ReplayTurn {
            text: String::new(),
            tool: None,
            finish: None,
        });
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
}
