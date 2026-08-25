//! Provider-neutral message and streaming vocabulary plus `ctx.llm`.

use async_trait::async_trait;
use dsh_brand::Branded;
use dsh_cordis::Service;
use futures::stream::BoxStream;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

    /// Adapter-returned exact-model metadata that failed runtime validation.
    pub fn invalid_model_info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failure(LlmFailure {
            message: message.into(),
            code: code.into(),
            status: None,
        })
    }
}

/// Provider-owned context capacity for one exact provider/model route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelContext {
    /// Maximum combined request and response context in tokens.
    pub context_window: u32,
}

/// Display metadata for one adapter-owned reasoning effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmReasoningEffortInfo {
    /// Opaque stable value accepted as `reasoningEffort`.
    pub id: String,
    /// Human-readable effort name.
    pub name: String,
    /// Optional distinction from otherwise similar efforts.
    pub description: Option<String>,
}

/// Selectable reasoning efforts for one exact provider/model route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelReasoningInfo {
    /// Supported efforts in adapter-preferred display order.
    pub efforts: Vec<LlmReasoningEffortInfo>,
    /// Adapter-configured default when callers omit an effort.
    pub default_effort: Option<String>,
}

/// Exact-route model metadata resolved by its owning adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResolvedModelInfo {
    /// Provider route that owns this model.
    pub provider: String,
    /// Exact model id passed to the adapter.
    pub id: String,
    /// Human-readable model name.
    pub name: String,
    /// Optional user-facing distinction from otherwise similar models.
    pub description: Option<String>,
    /// Provider-owned context capacity when known.
    pub context: Option<LlmModelContext>,
    /// Adapter-configured per-request output cap when callers omit one.
    pub default_max_tokens: Option<u32>,
    /// Accepted request modalities; absent means unknown.
    pub input_modalities: Option<Vec<String>>,
    /// Adapter-owned selectable reasoning levels when exposed.
    pub reasoning: Option<LlmModelReasoningInfo>,
}

impl LlmResolvedModelInfo {
    /// Identity-only result when an adapter publishes no extra metadata.
    #[must_use]
    pub fn identity(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            provider: provider.into(),
            id: model.clone(),
            name: model,
            description: None,
            context: None,
            default_max_tokens: None,
            input_modalities: None,
            reasoning: None,
        }
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
    /// Completed tool output carried inside a user-role tool-result message.
    ToolResult {
        /// Matching tool-call id.
        #[serde(rename = "toolCallId")]
        tool_call_id: CallId,
        /// Tool output blocks.
        content: Vec<ContentBlock>,
        /// Whether the tool reported a failure.
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

impl ContentBlock {
    /// Build a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Named contribution inside a `snapshot` plugin source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotSection {
    /// Contributing subsystem name.
    pub name: String,
    /// Model-facing text for this contribution.
    pub text: String,
}

/// One skill-catalog entry carried on a `skill-catalog` message source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillCatalogEntry {
    /// Exact loadable skill name.
    pub name: String,
    /// Truncated model-facing description.
    pub description: String,
}

/// Who produced a message. Discriminated by `kind`, same as TypeScript `MessageSource`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum MessageSource {
    /// Human prompt.
    #[serde(rename = "user")]
    User,
    /// Plugin-produced context or steer/inject.
    #[serde(rename = "plugin")]
    Plugin {
        /// Plugin name that produced the message.
        plugin: String,
        /// Semantic form (`snapshot`, `notice`, …) when the producer declared one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        form: Option<String>,
        /// One-line notice label when `form` is `notice`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Snapshot contributions, required when `form` is `snapshot`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sections: Vec<SnapshotSection>,
        /// Compaction attempt identity on a `compact` checkpoint message.
        #[serde(
            rename = "compactionId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        compaction_id: Option<String>,
        /// Originating `/compact` command id on a manual checkpoint message.
        #[serde(
            rename = "sourceCommandId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        source_command_id: Option<String>,
    },
    /// Skill-catalog publication injected before a step.
    #[serde(rename = "skill-catalog")]
    SkillCatalog {
        /// Publication form; always `catalog`.
        form: String,
        /// Present and `true` when a changed catalog replaces an earlier one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<bool>,
        /// Complete catalog entries in rank order.
        entries: Vec<SkillCatalogEntry>,
    },
    /// A message another agent's tool call addressed to this one.
    #[serde(rename = "coordinator")]
    Coordinator {
        /// Relay context form; always `relay`.
        form: String,
        /// Session id of the agent whose tool call produced the follow-up.
        #[serde(rename = "senderSessionId")]
        sender_session_id: String,
    },
    /// A continuable child's explicit report to its direct parent.
    #[serde(rename = "subagent-report")]
    SubagentReport {
        /// Relay context form; always `relay`.
        form: String,
        /// Session id of the reporting child.
        #[serde(rename = "senderSessionId")]
        sender_session_id: String,
    },
    /// The runtime's account of a continuable child settling.
    #[serde(rename = "subagent-settled")]
    SubagentSettled {
        /// Notice context form; always `notice`.
        form: String,
        /// One-line account of how the child ended.
        summary: String,
        /// Session id of the child that settled.
        #[serde(rename = "senderSessionId")]
        sender_session_id: String,
    },
    /// Same-session goal-round continuation.
    #[serde(rename = "goal")]
    Goal {
        /// Durable goal id (`goal-{uuid}`).
        #[serde(rename = "goalId")]
        goal_id: String,
        /// CAS revision the round was admitted against.
        revision: u64,
        /// Positive admitted round number.
        round: u32,
    },
    /// Assistant output from a routed model.
    #[serde(rename = "model")]
    Model {
        /// Provider route.
        provider: String,
        /// Model id.
        model: String,
    },
    /// Tool-result provenance.
    #[serde(rename = "tool")]
    Tool {
        /// Matching tool-call id.
        #[serde(rename = "callId")]
        call_id: String,
    },
    /// Workspace-instruction context from `agent-instructions`.
    #[serde(rename = "agent-instructions")]
    AgentInstructions {
        /// Context form; always `instructions`.
        form: String,
        /// Present and `true` on a complete baseline replacement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baseline: Option<bool>,
        /// Discovery identity that produced this baseline.
        #[serde(
            rename = "baselineIdentity",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        baseline_identity: Option<String>,
        /// Per-file set / replace / remove records for this message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<Value>,
    },
}

impl MessageSource {
    /// Human prompt source.
    pub fn user() -> Self {
        Self::User
    }

    /// Plugin source without a declared form.
    pub fn plugin(plugin: impl Into<String>) -> Self {
        Self::Plugin {
            plugin: plugin.into(),
            form: None,
            summary: None,
            sections: Vec::new(),
            compaction_id: None,
            source_command_id: None,
        }
    }

    /// Plugin snapshot source with named sections.
    pub fn snapshot(plugin: impl Into<String>, sections: Vec<SnapshotSection>) -> Self {
        Self::Plugin {
            plugin: plugin.into(),
            form: Some("snapshot".into()),
            summary: None,
            sections,
            compaction_id: None,
            source_command_id: None,
        }
    }

    /// Plugin notice source with a one-line summary.
    pub fn notice(plugin: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::Plugin {
            plugin: plugin.into(),
            form: Some("notice".into()),
            summary: Some(summary.into()),
            sections: Vec::new(),
            compaction_id: None,
            source_command_id: None,
        }
    }

    /// Workspace-instruction source, optionally a complete baseline.
    pub fn agent_instructions(
        changes: Vec<Value>,
        baseline: bool,
        baseline_identity: Option<String>,
    ) -> Self {
        Self::AgentInstructions {
            form: "instructions".into(),
            baseline: baseline.then_some(true),
            baseline_identity,
            changes,
        }
    }

    /// Goal-round source for an admitted continuation.
    pub fn goal(goal_id: impl Into<String>, revision: u64, round: u32) -> Self {
        Self::Goal {
            goal_id: goal_id.into(),
            revision,
            round,
        }
    }

    /// Coordinator relay source for a parent's follow-up to its child.
    pub fn coordinator(sender_session_id: impl Into<String>) -> Self {
        Self::Coordinator {
            form: "relay".into(),
            sender_session_id: sender_session_id.into(),
        }
    }

    /// Relay source for a continuable child's explicit report.
    pub fn subagent_report(sender_session_id: impl Into<String>) -> Self {
        Self::SubagentReport {
            form: "relay".into(),
            sender_session_id: sender_session_id.into(),
        }
    }

    /// Notice source for the runtime's account of a child settling.
    pub fn subagent_settled(
        summary: impl Into<String>,
        sender_session_id: impl Into<String>,
    ) -> Self {
        Self::SubagentSettled {
            form: "notice".into(),
            summary: bound_context_summary(&summary.into()),
            sender_session_id: sender_session_id.into(),
        }
    }
}

/// Longest `notice` summary a context row renders without truncation.
pub const CONTEXT_SUMMARY_MAX_CHARS: usize = 120;

/// Bound one `notice` summary to [`CONTEXT_SUMMARY_MAX_CHARS`] characters,
/// replacing the overflow with a single trailing `…`.
pub fn bound_context_summary(summary: &str) -> String {
    let chars: Vec<char> = summary.chars().collect();
    if chars.len() <= CONTEXT_SUMMARY_MAX_CHARS {
        summary.to_string()
    } else {
        let mut bounded: String = chars[..CONTEXT_SUMMARY_MAX_CHARS - 1].iter().collect();
        bounded.push('…');
        bounded
    }
}

/// Fresh message identity: a UUID v4 assigned at construction.
pub fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn user_role() -> String {
    "user".into()
}

fn assistant_role() -> String {
    "assistant".into()
}

/// User-role message on the model-visible surface.
/// Field order matches the TypeScript wire: `content`, `source`, `role`, `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    /// Message content blocks.
    pub content: Vec<ContentBlock>,
    /// Distinguishes a human prompt from inject/steer sources.
    pub source: MessageSource,
    /// Constant `user` wire role.
    #[serde(default = "user_role")]
    pub role: String,
    /// Message identity, a UUID assigned at construction.
    #[serde(default = "new_message_id")]
    pub id: String,
}

impl UserMessage {
    /// Build from explicit content and source with a fresh identity.
    pub fn from_parts(content: Vec<ContentBlock>, source: MessageSource) -> Self {
        Self {
            content,
            source,
            role: user_role(),
            id: new_message_id(),
        }
    }

    /// Human text prompt.
    pub fn text(text: impl Into<String>) -> Self {
        Self::from_parts(vec![ContentBlock::text(text)], MessageSource::User)
    }

    /// Plugin notice injected onto the next model step.
    pub fn notice(
        plugin: impl Into<String>,
        text: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self::from_parts(
            vec![ContentBlock::text(text)],
            MessageSource::notice(plugin, summary),
        )
    }

    /// Goal-round continuation prompt.
    pub fn goal_round(
        text: impl Into<String>,
        goal_id: impl Into<String>,
        revision: u64,
        round: u32,
    ) -> Self {
        Self::from_parts(
            vec![ContentBlock::text(text)],
            MessageSource::goal(goal_id, revision, round),
        )
    }
}

/// Assembled assistant message for one step.
/// Field order matches the TypeScript wire: `role`, `content`, `source`, `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    /// Constant `assistant` wire role.
    #[serde(default = "assistant_role")]
    pub role: String,
    /// Message content blocks.
    pub content: Vec<ContentBlock>,
    /// Producing route (`{kind:"model", provider, model}` for adapter output).
    pub source: MessageSource,
    /// Message identity, a UUID assigned at construction.
    #[serde(default = "new_message_id")]
    pub id: String,
}

impl AssistantMessage {
    /// Build a model-sourced assistant message with a fresh identity.
    pub fn model(
        content: Vec<ContentBlock>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            role: assistant_role(),
            content,
            source: MessageSource::Model {
                provider: provider.into(),
                model: model.into(),
            },
            id: new_message_id(),
        }
    }

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

/// Tool-result message projected onto the surface: a user-role message whose
/// content is one `tool-result` block and whose source is `{kind:"tool", callId}`.
/// Field order matches the TypeScript wire: `source`, `content`, `role`, `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMessage {
    /// Tool provenance carrying the matching call id.
    pub source: MessageSource,
    /// One `tool-result` content block wrapping the tool output.
    pub content: Vec<ContentBlock>,
    /// Constant `user` wire role.
    #[serde(default = "user_role")]
    pub role: String,
    /// Message identity, a UUID assigned at construction.
    #[serde(default = "new_message_id")]
    pub id: String,
}

impl ToolResultMessage {
    /// Wrap one tool outcome as the surface message for `call_id`.
    pub fn new(call_id: CallId, content: Vec<ContentBlock>, is_error: bool) -> Self {
        Self {
            source: MessageSource::Tool {
                call_id: call_id.as_str().to_string(),
            },
            content: vec![ContentBlock::ToolResult {
                tool_call_id: call_id,
                content,
                is_error,
            }],
            role: user_role(),
            id: new_message_id(),
        }
    }

    /// Matching tool-call id from the `tool` source.
    pub fn tool_call_id(&self) -> Option<&str> {
        match &self.source {
            MessageSource::Tool { call_id } => Some(call_id.as_str()),
            _ => None,
        }
    }

    /// Tool output blocks inside the first `tool-result` content block.
    pub fn result_blocks(&self) -> &[ContentBlock] {
        self.content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }

    /// Whether the wrapped tool outcome reported a failure.
    pub fn is_error(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
    }
}

/// Message union used in `derive_messages` and adapter requests. Each member
/// carries its own wire `role`; a tool result is a user-role message with a
/// `tool` source, so deserialization discriminates on `role` then `source.kind`.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// User or injected context.
    User(UserMessage),
    /// Assistant completion.
    Assistant(AssistantMessage),
    /// Tool result.
    Tool(ToolResultMessage),
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::User(message) => message.serialize(serializer),
            Self::Assistant(message) => message.serialize(serializer),
            Self::Tool(message) => message.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "assistant" {
            return serde_json::from_value(value)
                .map(Self::Assistant)
                .map_err(serde::de::Error::custom);
        }
        let source_kind = value
            .get("source")
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str);
        if source_kind == Some("tool") {
            return serde_json::from_value(value)
                .map(Self::Tool)
                .map_err(serde::de::Error::custom);
        }
        serde_json::from_value(value)
            .map(Self::User)
            .map_err(serde::de::Error::custom)
    }
}

/// One streamed fragment from an adapter. Wire tag is `type`, matching TypeScript `StreamChunk`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StreamChunk {
    /// Opens a content block. `index` correlates later deltas and the matching `block-end`.
    BlockStart {
        /// Block index in first-seen stream order.
        index: u32,
        /// Content-block type (`text`, `reasoning`, `tool-call`).
        #[serde(rename = "blockType")]
        block_type: String,
    },
    /// Incremental visible text.
    TextDelta {
        /// Owning block index.
        index: u32,
        /// Text delta.
        text: String,
    },
    /// Incremental reasoning.
    ReasoningDelta {
        /// Owning block index.
        index: u32,
        /// Reasoning delta.
        text: String,
    },
    /// Incremental tool-call arguments. `name` is set on the first delta.
    ToolCallDelta {
        /// Owning block index.
        index: u32,
        /// Provider-issued call id.
        id: CallId,
        /// Tool name, present on the first delta.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Raw JSON argument fragment.
        #[serde(rename = "argumentsDelta")]
        arguments_delta: String,
    },
    /// Closes a block and carries the assembled content.
    BlockEnd {
        /// Owning block index.
        index: u32,
        /// Assembled block.
        block: ContentBlock,
    },
    /// Token accounting. Adapters emit this before the terminal `finish`.
    Usage {
        /// Token counts for this request.
        usage: TokenUsage,
    },
    /// Terminal chunk. Adapters emit nothing afterward.
    Finish {
        /// Why the stream ended.
        reason: FinishReason,
        /// Adapter-private replay metadata for a successful response.
        #[serde(
            rename = "replayState",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        replay_state: Option<Value>,
    },
}

impl StreamChunk {
    /// One text block plus a `stop` finish. Adapters that only have a complete string use this.
    pub fn text_stream(text: impl Into<String>) -> Vec<Self> {
        let text = text.into();
        let mut chunks = text_block(0, text);
        chunks.push(Self::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        });
        chunks
    }

    /// One tool-call block plus a `tool-calls` finish.
    pub fn tool_stream(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Vec<Self> {
        let mut chunks = tool_block(0, id, name, arguments);
        chunks.push(Self::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        });
        chunks
    }
}

/// Start, delta, and end for one text block. The caller appends `usage` / `finish`.
pub fn text_block(index: u32, text: impl Into<String>) -> Vec<StreamChunk> {
    let text = text.into();
    vec![
        StreamChunk::BlockStart {
            index,
            block_type: "text".into(),
        },
        StreamChunk::TextDelta {
            index,
            text: text.clone(),
        },
        StreamChunk::BlockEnd {
            index,
            block: ContentBlock::text(text),
        },
    ]
}

/// Start, delta, and end for one tool-call block. The caller appends `usage` / `finish`.
pub fn tool_block(
    index: u32,
    id: impl Into<String>,
    name: impl Into<String>,
    arguments: impl Into<String>,
) -> Vec<StreamChunk> {
    let id = call_id(id);
    let name = name.into();
    let arguments = arguments.into();
    vec![
        StreamChunk::BlockStart {
            index,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index,
            id: id.clone(),
            name: Some(name.clone()),
            arguments_delta: arguments.clone(),
        },
        StreamChunk::BlockEnd {
            index,
            block: ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        },
    ]
}

/// Token accounting reported by an adapter. Counts are disjoint: `input_tokens` is uncached input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TokenUsage {
    /// Uncached input tokens.
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: u32,
    /// Output tokens.
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: u32,
    /// Cached input tokens read.
    #[serde(
        rename = "cacheReadTokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read_tokens: Option<u32>,
    /// Cached input tokens written.
    #[serde(
        rename = "cacheWriteTokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_tokens: Option<u32>,
    /// Reasoning tokens when the provider reports them separately.
    #[serde(
        rename = "reasoningTokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_tokens: Option<u32>,
}

/// Why a stream ended. Wire tag is `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Tool calls pending.
    ToolCalls,
    /// Token ceiling.
    MaxTokens,
    /// Caller or runtime abort.
    Aborted {
        /// Failure facts.
        failure: LlmFailure,
    },
    /// Provider or transport failure.
    Error {
        /// Failure facts.
        failure: LlmFailure,
    },
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
    /// Requested reasoning effort when the deployment configures one.
    #[serde(
        rename = "reasoningEffort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_effort: Option<String>,
    /// Output-token cap when the deployment configures one.
    #[serde(rename = "maxTokens", default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Default for LlmCallConfig {
    fn default() -> Self {
        Self {
            provider: "replay".into(),
            model: "script".into(),
            reasoning_effort: None,
            max_tokens: None,
        }
    }
}

/// Default transient codes a `normal` retry policy accepts.
pub const DEFAULT_RETRYABLE_CODES: &[&str] = &[
    "EMPTY_RESPONSE",
    "RATE_LIMIT",
    "SERVER",
    "TIMEOUT",
    "TRANSPORT",
];

/// Provider-owned request-retry policy captured on the adapter route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryPolicy {
    /// `normal` (bounded, code-filtered) or `always` (unbounded).
    pub mode: String,
    /// Maximum eligible retries after the first request (`normal` only).
    #[serde(rename = "maxRetries", default = "default_max_retries")]
    pub max_retries: u32,
    /// Stable failure codes eligible under `normal`.
    #[serde(rename = "retryableCodes", default = "default_retryable_codes")]
    pub retryable_codes: Vec<String>,
    /// Initial local exponential-backoff delay in milliseconds.
    #[serde(rename = "initialDelayMs", default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    /// Maximum locally scheduled or accepted provider delay in milliseconds.
    #[serde(rename = "maxDelayMs", default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    /// Symmetric random multiplier range around one.
    #[serde(rename = "jitterRatio", default = "default_jitter_ratio")]
    pub jitter_ratio: f64,
}

fn default_max_retries() -> u32 {
    5
}
fn default_retryable_codes() -> Vec<String> {
    DEFAULT_RETRYABLE_CODES
        .iter()
        .map(|code| (*code).to_string())
        .collect()
}
fn default_initial_delay_ms() -> u64 {
    500
}
fn default_max_delay_ms() -> u64 {
    10_000
}
fn default_jitter_ratio() -> f64 {
    0.1
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            mode: "normal".into(),
            max_retries: default_max_retries(),
            retryable_codes: default_retryable_codes(),
            initial_delay_ms: default_initial_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
            jitter_ratio: default_jitter_ratio(),
        }
    }
}

/// `JSON.stringify` a JS number: a whole value is an integer token (`0`, not `0.0`).
fn json_js_number(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        Value::from(value as i64)
    } else {
        serde_json::json!(value)
    }
}

impl RetryPolicy {
    /// Stable policy identity written on each `llm/retry` event.
    ///
    /// The string matches TypeScript `JSON.stringify` of the same tuple,
    /// including whole-number `jitterRatio` as a JSON integer.
    pub fn policy_key(&self) -> String {
        if self.mode == "always" {
            serde_json::to_string(&[
                serde_json::json!(self.mode),
                serde_json::json!(self.initial_delay_ms),
                serde_json::json!(self.max_delay_ms),
                json_js_number(self.jitter_ratio),
            ])
            .unwrap_or_default()
        } else {
            let mut codes = self.retryable_codes.clone();
            codes.sort();
            serde_json::to_string(&[
                serde_json::json!(self.mode),
                serde_json::json!(self.max_retries),
                serde_json::json!(codes),
                serde_json::json!(self.initial_delay_ms),
                serde_json::json!(self.max_delay_ms),
                json_js_number(self.jitter_ratio),
            ])
            .unwrap_or_default()
        }
    }

    /// Local exponential delay for `retry` (1-based), with optional jitter sample.
    pub fn local_delay(&self, retry: u32, random: f64) -> u64 {
        let exponent = u32::min(retry.saturating_sub(1), 1024);
        let exponential = self
            .initial_delay_ms
            .saturating_mul(2u64.saturating_pow(exponent))
            .min(self.max_delay_ms);
        let jitter = 1.0 - self.jitter_ratio + 2.0 * self.jitter_ratio * random.clamp(0.0, 1.0);
        ((exponential as f64) * jitter).min(self.max_delay_ms as f64) as u64
    }
}

const RETRY_POLICY_KEYS: &[&str] = &["mode", "maxRetries", "retryableCodes", "backoff"];
const BACKOFF_KEYS: &[&str] = &["initialDelayMs", "maxDelayMs", "jitterRatio"];

fn unknown_keys(value: &serde_json::Map<String, Value>, allowed: &[&str], path: &str) -> Result<(), String> {
    for key in value.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path}: unknown key \"{key}\""));
        }
    }
    Ok(())
}

fn resolve_backoff(config: Option<&Value>, path: &str) -> Result<(u64, u64, f64), String> {
    if let Some(value) = config {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{path} must be a mapping"))?;
        unknown_keys(object, BACKOFF_KEYS, path)?;
    }
    let initial_delay_ms = positive_delay(
        config.and_then(|value| value.get("initialDelayMs")),
        default_initial_delay_ms(),
        &format!("{path}.initialDelayMs"),
    )?;
    let max_delay_ms = positive_delay(
        config.and_then(|value| value.get("maxDelayMs")),
        default_max_delay_ms(),
        &format!("{path}.maxDelayMs"),
    )?;
    if initial_delay_ms > max_delay_ms {
        return Err(format!(
            "{path}.initialDelayMs must be less than or equal to maxDelayMs"
        ));
    }
    let jitter_ratio = match config.and_then(|value| value.get("jitterRatio")) {
        None => default_jitter_ratio(),
        Some(value) => {
            let ratio = value.as_f64().ok_or_else(|| {
                format!("{path}.jitterRatio must be between 0 and 1")
            })?;
            if !(0.0..=1.0).contains(&ratio) {
                return Err(format!("{path}.jitterRatio must be between 0 and 1"));
            }
            ratio
        }
    };
    Ok((initial_delay_ms, max_delay_ms, jitter_ratio))
}

fn positive_delay(value: Option<&Value>, default: u64, path: &str) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    let number = value.as_f64().ok_or_else(|| {
        format!(
            "{path} must be a positive finite number no greater than {}",
            dsh_timeout::MAX_TIMER_DELAY_MS
        )
    })?;
    if !number.is_finite() || number <= 0.0 || number > dsh_timeout::MAX_TIMER_DELAY_MS as f64 {
        return Err(format!(
            "{path} must be a positive finite number no greater than {}",
            dsh_timeout::MAX_TIMER_DELAY_MS
        ));
    }
    Ok(number as u64)
}

/// Validate, default, and flatten one provider-owned retry policy.
///
/// Omission selects `normal` defaults. YAML `backoff` is flattened onto the
/// resolved policy. Invalid values fail at the named `path`.
///
/// # Errors
/// Unknown keys, a mode other than `normal`/`always`, empty or duplicate
/// retryable codes, a negative `maxRetries`, or an out-of-range backoff field.
pub fn resolve_retry_policy(config: Option<&Value>, path: &str) -> Result<RetryPolicy, String> {
    let Some(config) = config.filter(|value| !value.is_null()) else {
        return Ok(RetryPolicy::default());
    };
    let object = config
        .as_object()
        .ok_or_else(|| format!("{path} must be a mapping"))?;
    unknown_keys(object, RETRY_POLICY_KEYS, path)?;
    let mode = object
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("normal");
    let (initial_delay_ms, max_delay_ms, jitter_ratio) =
        resolve_backoff(object.get("backoff"), &format!("{path}.backoff"))?;
    match mode {
        "always" => Ok(RetryPolicy {
            mode: "always".into(),
            max_retries: default_max_retries(),
            retryable_codes: default_retryable_codes(),
            initial_delay_ms,
            max_delay_ms,
            jitter_ratio,
        }),
        "normal" => {
            let max_retries = match object.get("maxRetries") {
                None => default_max_retries(),
                Some(value) => {
                    let number = value.as_i64().ok_or_else(|| {
                        format!("{path}.maxRetries must be a non-negative safe integer")
                    })?;
                    if number < 0 || number > i64::from(u32::MAX) {
                        return Err(format!(
                            "{path}.maxRetries must be a non-negative safe integer"
                        ));
                    }
                    number as u32
                }
            };
            let retryable_codes = match object.get("retryableCodes") {
                None => default_retryable_codes(),
                Some(value) => {
                    let items = value.as_array().ok_or_else(|| {
                        format!("{path}.retryableCodes must contain only non-empty strings")
                    })?;
                    if items.is_empty() {
                        return Err(format!("{path}.retryableCodes must not be empty"));
                    }
                    let mut codes = Vec::with_capacity(items.len());
                    for item in items {
                        let code = item.as_str().unwrap_or("");
                        if code.is_empty() {
                            return Err(format!(
                                "{path}.retryableCodes must contain only non-empty strings"
                            ));
                        }
                        codes.push(code.to_string());
                    }
                    if codes.len() != codes.iter().collect::<std::collections::BTreeSet<_>>().len()
                    {
                        return Err(format!("{path}.retryableCodes must not contain duplicates"));
                    }
                    codes
                }
            };
            Ok(RetryPolicy {
                mode: "normal".into(),
                max_retries,
                retryable_codes,
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            })
        }
        _ => Err(format!("{path}.mode must be \"normal\" or \"always\"")),
    }
}

/// One prepared model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Route and sampling.
    pub config: LlmCallConfig,
    /// Adapter-declared config defaults echoed into `request/header` when present.
    #[serde(
        rename = "adapterDefaults",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub adapter_defaults: Option<Value>,
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
    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<BoxStream<'static, StreamChunk>, LlmError>;
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
    pub async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        self.adapter.stream(request).await
    }

    /// Resolve and validate exact-model metadata from the registered adapter.
    pub async fn resolve_model_info(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let resolved = self.adapter.resolve_model(provider, model).await?;
        normalize_model_info(provider, model, resolved)
    }

    /// Provider-owned retry policy captured on the registered adapter route.
    pub fn provider_retry_policy(&self, provider: &str) -> RetryPolicy {
        self.adapter.provider_retry_policy(provider)
    }
}

impl Service for LlmRuntime {
    const KEY: &'static str = "llm";
}

/// Adapter that translates harness requests onto a provider.
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    /// Stream one request.
    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<BoxStream<'static, StreamChunk>, LlmError>;

    /// Resolve all metadata available for one exact model.
    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        Ok(LlmResolvedModelInfo::identity(provider, model))
    }

    /// Retry policy for `provider`. Omission is [`RetryPolicy::default`].
    fn provider_retry_policy(&self, _provider: &str) -> RetryPolicy {
        RetryPolicy::default()
    }
}

fn normalize_model_info(
    provider: &str,
    model: &str,
    resolved: LlmResolvedModelInfo,
) -> Result<LlmResolvedModelInfo, LlmError> {
    if resolved.provider != provider || resolved.id != model || resolved.name.is_empty() {
        return Err(LlmError::invalid_model_info(
            "INVALID_MODEL_INFO",
            format!(
                "adapter returned invalid exact model metadata for provider \"{provider}\" model \"{model}\""
            ),
        ));
    }
    if let Some(context) = &resolved.context {
        if context.context_window == 0 {
            return Err(LlmError::invalid_model_info(
                "INVALID_MODEL_CONTEXT",
                format!(
                    "adapter returned invalid context metadata for provider \"{provider}\" model \"{model}\""
                ),
            ));
        }
    }
    if let Some(max_tokens) = resolved.default_max_tokens {
        if max_tokens == 0 {
            return Err(LlmError::invalid_model_info(
                "INVALID_MODEL_MAX_TOKENS",
                format!(
                    "adapter returned invalid default maxTokens for provider \"{provider}\" model \"{model}\""
                ),
            ));
        }
    }
    if let Some(reasoning) = &resolved.reasoning {
        if reasoning.efforts.is_empty() {
            return Err(LlmError::invalid_model_info(
                "INVALID_MODEL_REASONING",
                format!(
                    "adapter returned invalid reasoning metadata for provider \"{provider}\" model \"{model}\""
                ),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for effort in &reasoning.efforts {
            if effort.id.is_empty() || effort.name.is_empty() || !seen.insert(effort.id.as_str()) {
                return Err(LlmError::invalid_model_info(
                    "INVALID_MODEL_REASONING",
                    format!(
                        "adapter returned invalid or duplicate reasoning effort metadata for provider \"{provider}\" model \"{model}\""
                    ),
                ));
            }
        }
        if let Some(default) = &reasoning.default_effort {
            if !seen.contains(default.as_str()) {
                return Err(LlmError::invalid_model_info(
                    "INVALID_MODEL_REASONING",
                    format!(
                        "adapter returned an unknown default reasoning effort for provider \"{provider}\" model \"{model}\""
                    ),
                ));
            }
        }
    }
    Ok(resolved)
}

/// In-progress block keyed by stream index.
enum Assembling {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: Option<CallId>,
        name: Option<String>,
        arguments: String,
    },
    Done(ContentBlock),
}

/// Assemble stream chunks into one assistant message.
#[derive(Default)]
pub struct BlockAssembler {
    blocks: std::collections::BTreeMap<u32, Assembling>,
    order: Vec<u32>,
    usage: Option<TokenUsage>,
}

impl BlockAssembler {
    fn touch(&mut self, index: u32) {
        if !self.order.contains(&index) {
            self.order.push(index);
        }
    }

    /// Push one chunk.
    pub fn push(&mut self, chunk: &StreamChunk) {
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                self.touch(*index);
                self.blocks.insert(
                    *index,
                    match block_type.as_str() {
                        "reasoning" => Assembling::Reasoning(String::new()),
                        "tool-call" => Assembling::ToolCall {
                            id: None,
                            name: None,
                            arguments: String::new(),
                        },
                        _ => Assembling::Text(String::new()),
                    },
                );
            }
            StreamChunk::TextDelta { index, text } => {
                self.touch(*index);
                match self.blocks.get_mut(index) {
                    Some(Assembling::Text(body)) => body.push_str(text),
                    None => {
                        self.blocks.insert(*index, Assembling::Text(text.clone()));
                    }
                    _ => {}
                }
            }
            StreamChunk::ReasoningDelta { index, text } => {
                self.touch(*index);
                match self.blocks.get_mut(index) {
                    Some(Assembling::Reasoning(body)) => body.push_str(text),
                    None => {
                        self.blocks
                            .insert(*index, Assembling::Reasoning(text.clone()));
                    }
                    _ => {}
                }
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                self.touch(*index);
                match self.blocks.get_mut(index) {
                    Some(Assembling::ToolCall {
                        id: slot_id,
                        name: slot_name,
                        arguments,
                    }) => {
                        *slot_id = Some(id.clone());
                        if let Some(name) = name {
                            *slot_name = Some(name.clone());
                        }
                        arguments.push_str(arguments_delta);
                    }
                    None => {
                        self.blocks.insert(
                            *index,
                            Assembling::ToolCall {
                                id: Some(id.clone()),
                                name: name.clone(),
                                arguments: arguments_delta.clone(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            StreamChunk::BlockEnd { index, block } => {
                self.touch(*index);
                self.blocks.insert(*index, Assembling::Done(block.clone()));
            }
            StreamChunk::Usage { usage } => self.usage = Some(usage.clone()),
            StreamChunk::Finish { .. } => {}
        }
    }

    /// Token usage reported by a `usage` chunk, if any.
    pub fn take_usage(&mut self) -> Option<TokenUsage> {
        self.usage.take()
    }

    /// Finish the assembled content blocks in first-seen stream order.
    pub fn finish(self) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        for index in self.order {
            let Some(block) = self.blocks.get(&index) else {
                continue;
            };
            match block {
                Assembling::Done(block) => content.push(block.clone()),
                Assembling::Text(text) if !text.is_empty() => {
                    content.push(ContentBlock::Text { text: text.clone() });
                }
                Assembling::Reasoning(text) if !text.is_empty() => {
                    content.push(ContentBlock::Reasoning { text: text.clone() });
                }
                Assembling::ToolCall {
                    id: Some(id),
                    name: Some(name),
                    arguments,
                } => content.push(ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => {}
            }
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_assistant_text_is_empty() {
        assert_eq!(AssistantMessage::model(vec![], "p", "m").text(), "");
    }

    #[test]
    fn omitted_retry_policy_is_normal_defaults() {
        let policy = resolve_retry_policy(None, "provider.retryPolicy").unwrap();
        assert_eq!(policy.mode, "normal");
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.initial_delay_ms, 500);
        assert_eq!(policy.max_delay_ms, 10_000);
        assert_eq!(policy.jitter_ratio, 0.1);
    }

    #[test]
    fn nested_backoff_flattens_on_always_mode() {
        let policy = resolve_retry_policy(
            Some(&serde_json::json!({
                "mode": "always",
                "backoff": { "initialDelayMs": 25, "maxDelayMs": 100, "jitterRatio": 0.2 }
            })),
            "provider.retryPolicy",
        )
        .unwrap();
        assert_eq!(policy.mode, "always");
        assert_eq!(policy.initial_delay_ms, 25);
        assert_eq!(policy.max_delay_ms, 100);
        assert_eq!(policy.jitter_ratio, 0.2);
    }

    #[test]
    fn negative_max_retries_fails_loud() {
        let error = resolve_retry_policy(
            Some(&serde_json::json!({ "mode": "normal", "maxRetries": -1 })),
            "llm-deepseek: retryPolicy",
        )
        .unwrap_err();
        assert!(error.contains("maxRetries"), "{error}");
    }

    #[test]
    fn policy_key_stringifies_jitter_like_javascript() {
        let policy = RetryPolicy {
            mode: "normal".into(),
            max_retries: 1,
            retryable_codes: vec!["RATE_LIMIT".into()],
            initial_delay_ms: 1,
            max_delay_ms: 1,
            jitter_ratio: 0.0,
        };
        assert_eq!(policy.policy_key(), r#"["normal",1,["RATE_LIMIT"],1,1,0]"#);
        let fractional = RetryPolicy {
            jitter_ratio: 0.1,
            ..policy.clone()
        };
        assert_eq!(
            fractional.policy_key(),
            r#"["normal",1,["RATE_LIMIT"],1,1,0.1]"#
        );
        let always = RetryPolicy {
            mode: "always".into(),
            jitter_ratio: 0.0,
            ..policy
        };
        assert_eq!(always.policy_key(), r#"["always",1,1,0]"#);
    }

    #[test]
    fn assembler_joins_text_and_tool_calls() {
        let mut assembler = BlockAssembler::default();
        for chunk in StreamChunk::text_stream("hi")
            .into_iter()
            .filter(|chunk| !matches!(chunk, StreamChunk::Finish { .. }))
        {
            assembler.push(&chunk);
        }
        for chunk in tool_block(1, "c1", "bash", "{}") {
            assembler.push(&chunk);
        }
        assembler.push(&StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        });
        let message = AssistantMessage::model(assembler.finish(), "p", "m");
        assert_eq!(message.text(), "hi");
        assert_eq!(message.tool_calls().len(), 1);
    }

    #[test]
    fn message_wire_matches_typescript_roles_and_sources() {
        let user = serde_json::to_value(Message::User(UserMessage::text("hi"))).unwrap();
        assert_eq!(user["role"], "user");
        assert_eq!(user["source"]["kind"], "user");
        assert!(user["id"].as_str().is_some());
        let tool = ToolResultMessage::new(call_id("c1"), vec![ContentBlock::text("ok")], false);
        let wire = serde_json::to_value(Message::Tool(tool.clone())).unwrap();
        assert_eq!(wire["role"], "user");
        assert_eq!(
            wire["source"],
            serde_json::json!({"kind":"tool","callId":"c1"})
        );
        assert_eq!(wire["content"][0]["type"], "tool-result");
        assert_eq!(wire["content"][0]["toolCallId"], "c1");
        assert_eq!(wire["content"][0]["isError"], false);
        assert_eq!(tool.tool_call_id(), Some("c1"));
        assert_eq!(tool.result_blocks(), &[ContentBlock::text("ok")]);
        assert!(!tool.is_error());
        let round_trip: Message = serde_json::from_value(wire).unwrap();
        assert!(matches!(round_trip, Message::Tool(_)));
        let assistant = serde_json::to_value(Message::Assistant(AssistantMessage::model(
            vec![ContentBlock::text("pong")],
            "deepseek",
            "chat",
        )))
        .unwrap();
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant["source"],
            serde_json::json!({"kind":"model","provider":"deepseek","model":"chat"})
        );
        let round_trip: Message = serde_json::from_value(assistant).unwrap();
        assert!(matches!(round_trip, Message::Assistant(_)));
    }

    #[test]
    fn stream_chunk_wire_uses_type_and_finish_kind_object() {
        let chunk = StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        };
        assert_eq!(
            serde_json::to_value(&chunk).unwrap(),
            serde_json::json!({"type":"finish","reason":{"kind":"tool-calls"}})
        );
        let usage = TokenUsage {
            input_tokens: 11,
            output_tokens: 3,
            cache_read_tokens: Some(2),
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            serde_json::json!({"inputTokens":11,"outputTokens":3,"cacheReadTokens":2})
        );
    }

    struct InfoAdapter {
        info: LlmResolvedModelInfo,
    }

    #[async_trait]
    impl LlmAdapter for InfoAdapter {
        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn resolve_model(
            &self,
            _provider: &str,
            _model: &str,
        ) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(self.info.clone())
        }
    }

    #[tokio::test]
    async fn resolve_model_info_accepts_identity_and_capacity() {
        let runtime = LlmRuntime::new(std::sync::Arc::new(InfoAdapter {
            info: LlmResolvedModelInfo {
                context: Some(LlmModelContext {
                    context_window: 128_000,
                }),
                ..LlmResolvedModelInfo::identity("replay", "script")
            },
        }));
        let info = runtime.resolve_model_info("replay", "script").await.unwrap();
        assert_eq!(info.context.unwrap().context_window, 128_000);
    }

    #[tokio::test]
    async fn resolve_model_info_rejects_mismatched_identity_and_zero_window() {
        let runtime = LlmRuntime::new(std::sync::Arc::new(InfoAdapter {
            info: LlmResolvedModelInfo::identity("other", "script"),
        }));
        let err = runtime.resolve_model_info("replay", "script").await.unwrap_err();
        match err {
            LlmError::Failure(failure) => assert_eq!(failure.code, "INVALID_MODEL_INFO"),
        }
        let runtime = LlmRuntime::new(std::sync::Arc::new(InfoAdapter {
            info: LlmResolvedModelInfo {
                context: Some(LlmModelContext { context_window: 0 }),
                ..LlmResolvedModelInfo::identity("replay", "script")
            },
        }));
        let err = runtime.resolve_model_info("replay", "script").await.unwrap_err();
        match err {
            LlmError::Failure(failure) => assert_eq!(failure.code, "INVALID_MODEL_CONTEXT"),
        }
    }
}
