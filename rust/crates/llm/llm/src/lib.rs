//! Provider-neutral message and streaming vocabulary plus `ctx.llm`.

use async_trait::async_trait;
use dsh_brand::Branded;
use dsh_cordis::Service;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;

/// Brand token for a tool-call correlation id.
pub struct CallIdBrand;
/// Provider-issued tool-call id.
pub type CallId = Branded<CallIdBrand>;

/// Brand a call id.
pub fn call_id(value: impl Into<String>) -> CallId {
    CallId::new(value)
}

/// Serializable provider or transport failure facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFailure {
    /// Human-readable provider or transport failure.
    pub message: String,
    /// Stable provider-neutral machine-routing code.
    pub code: String,
    /// HTTP status returned by the provider, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// Failures from the adapter seam.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Provider or transport failure.
    #[error("{0:?}")]
    Failure(LlmFailure),
}

impl LlmError {
    /// Context-window overflow as reported by the provider.
    pub fn context_window_exceeded(message: impl Into<String>) -> Self {
        Self::Failure(LlmFailure {
            message: message.into(),
            code: "CONTEXT_WINDOW_EXCEEDED".into(),
            status: None,
        })
    }
}

/// Plain text visible to the end user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    /// Visible text.
    pub text: String,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallBlock {
    /// Provider-issued call id.
    pub id: CallId,
    /// Tool name.
    pub name: String,
    /// Raw JSON arguments exactly as the model produced them.
    pub arguments: String,
}

/// Content block union. New modalities join only when every path supports them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// Visible text.
    Text {
        /// Text body.
        text: String,
    },
    /// Reasoning / thinking, distinct from visible text.
    Reasoning {
        /// Thinking body.
        text: String,
    },
    /// A tool invocation requested by the model.
    ToolCall {
        /// Provider-issued call id.
        id: CallId,
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        arguments: String,
    },
}

impl ContentBlock {
    /// Build a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// User-role message on the model-visible surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    /// Message content blocks.
    pub content: Vec<ContentBlock>,
    /// Distinguishes a human prompt from inject/steer sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Assembled assistant message for one step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AssistantMessage {
    /// Message content blocks.
    pub content: Vec<ContentBlock>,
}

impl AssistantMessage {
    /// Visible text concatenated from text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Tool calls requested in this message.
    pub fn tool_calls(&self) -> Vec<ToolCallBlock> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(ToolCallBlock {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Tool-result message projected onto the surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMessage {
    /// Matching tool-call id.
    pub tool_call_id: CallId,
    /// Result content.
    pub content: Vec<ContentBlock>,
    /// Whether the tool reported a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Role-tagged message used in `derive_messages` and adapter requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// User or injected context.
    User(UserMessage),
    /// Assistant completion.
    Assistant(AssistantMessage),
    /// Tool result.
    Tool(ToolResultMessage),
}

/// One streamed fragment from an adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StreamChunk {
    /// Incremental visible text.
    Text {
        /// Text delta.
        text: String,
    },
    /// Incremental reasoning.
    Reasoning {
        /// Reasoning delta.
        text: String,
    },
    /// A completed tool call in the stream.
    ToolCall {
        /// Call id.
        id: CallId,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// Terminal chunk. Adapters emit usage before this and nothing after.
    Finish {
        /// Why the stream ended.
        reason: FinishReason,
        /// Token accounting when the adapter reported it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
}

/// Token accounting reported by an adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TokenUsage {
    /// Prompt tokens.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Completion tokens.
    #[serde(default)]
    pub completion_tokens: u32,
}

/// Why a stream ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Token ceiling.
    MaxTokens,
    /// Tool calls pending.
    ToolCalls,
}

/// Tool schema advertised to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSchema {
    /// Tool name.
    pub name: String,
    /// Human description.
    pub description: String,
    /// JSON Schema parameters.
    pub parameters: Value,
}

/// Call configuration for one request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmCallConfig {
    /// Adapter route.
    pub provider: String,
    /// Model id.
    pub model: String,
}

impl Default for LlmCallConfig {
    fn default() -> Self {
        Self {
            provider: "replay".into(),
            model: "script".into(),
        }
    }
}

/// One prepared model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Route and sampling.
    pub config: LlmCallConfig,
    /// Rendered system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Derived conversation history.
    pub messages: Vec<Message>,
    /// Assembled tool schemas.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
    /// Optional request purpose (`compaction`, `session-title`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// A prepared adapter call that can be streamed.
#[async_trait]
pub trait PreparedCall: Send + Sync {
    /// Stream chunks for this request.
    async fn stream(&self, request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError>;
}

/// `ctx.llm` — adapter registry and stream entry.
pub struct LlmRuntime {
    adapter: Arc<dyn LlmAdapter>,
}

impl LlmRuntime {
    /// Wrap a single adapter as the runtime.
    pub fn new(adapter: Arc<dyn LlmAdapter>) -> Self {
        Self { adapter }
    }

    /// Prepare a call against the registered adapter.
    pub fn prepare_call(&self) -> Arc<dyn LlmAdapter> {
        Arc::clone(&self.adapter)
    }

    /// Stream a request through the registered adapter.
    pub async fn stream(&self, request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        self.adapter.stream(request).await
    }
}

impl Service for LlmRuntime {
    const KEY: &'static str = "llm";
}

/// Adapter that translates harness requests onto a provider.
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    /// Stream one request.
    async fn stream(&self, request: LlmRequest) -> Result<BoxStream<'static, StreamChunk>, LlmError>;
}

/// Assemble stream chunks into one assistant message.
#[derive(Default)]
pub struct BlockAssembler {
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCallBlock>,
}

impl BlockAssembler {
    /// Push one chunk.
    pub fn push(&mut self, chunk: &StreamChunk) {
        match chunk {
            StreamChunk::Text { text } => self.text.push_str(text),
            StreamChunk::Reasoning { text } => self.reasoning.push_str(text),
            StreamChunk::ToolCall {
                id,
                name,
                arguments,
            } => self.tool_calls.push(ToolCallBlock {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            StreamChunk::Finish { .. } => {}
        }
    }

    /// Finish the assembled message.
    pub fn finish(self) -> AssistantMessage {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::Reasoning {
                text: self.reasoning,
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }
        for call in self.tool_calls {
            content.push(ContentBlock::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            });
        }
        AssistantMessage { content }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_assistant_text_is_empty() {
        assert_eq!(AssistantMessage::default().text(), "");
    }

    #[test]
    fn assembler_joins_text_and_tool_calls() {
        let mut assembler = BlockAssembler::default();
        assembler.push(&StreamChunk::Text {
            text: "hi".into(),
        });
        assembler.push(&StreamChunk::ToolCall {
            id: call_id("c1"),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        assembler.push(&StreamChunk::Finish {
            reason: FinishReason::Stop,
            usage: None,
        });
        let message = assembler.finish();
        assert_eq!(message.text(), "hi");
        assert_eq!(message.tool_calls().len(), 1);
    }
}
