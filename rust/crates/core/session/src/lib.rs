//! Append-only session log. Model-visible means logged.

use dsh_brand::Branded;
use dsh_cordis::{Context, Service};
use dsh_llm::{
    AssistantMessage, ContentBlock, Message, StreamChunk, TokenUsage, ToolResultMessage,
    UserMessage,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

mod repair;

pub use repair::{interrupted_turn_closers, TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN};

/// Firehose invoked after a store-backed append commits.
pub type SessionEventSink = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// Brand token for a session id.
pub struct SessionIdBrand;
/// Identifies one session in the store.
pub type SessionId = Branded<SessionIdBrand>;

/// Brand a session id.
pub fn session_id(value: impl Into<String>) -> SessionId {
    SessionId::new(value)
}

/// On-disk session format version. Unreleased: pinned at 0, no compatibility.
pub const SESSION_FORMAT_VERSION: u32 = 0;

/// Immutable session creation metadata, written once as the persisted header
/// line. Field order matches the TypeScript `HeaderLine`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionHeader {
    /// On-disk format version, [`SESSION_FORMAT_VERSION`] at write time.
    pub version: u32,
    /// Session identity; must equal the owning session's id.
    pub id: SessionId,
    /// Unix epoch milliseconds at creation.
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Working directory recorded at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Parent session for a subagent child.
    #[serde(
        rename = "parentSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<SessionId>,
    /// Number of seeded events copied from a resume, fork, or replay source.
    #[serde(
        rename = "seedLength",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub seed_length: Option<u64>,
    /// `subagent` when a delegation created this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Delegation depth: 0 for a top-level session, parent depth + 1 for a child.
    #[serde(rename = "delegationDepth", default)]
    pub delegation_depth: u32,
}

impl SessionHeader {
    /// Header for a fresh top-level session created now.
    pub fn new(id: SessionId, cwd: Option<String>) -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: now_ms(),
            cwd,
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: 0,
        }
    }

    /// Header for a child session created by a subagent provider.
    /// Allocates a UUID identity; use [`Self::for_subagent_child_id`] to reserve one.
    pub fn for_subagent_child(parent: Option<&SessionHeader>, parent_id: SessionId) -> Self {
        Self::for_subagent_child_id(parent, parent_id, session_id(Uuid::new_v4().to_string()))
    }

    /// Header for a child session with a caller-reserved durable id.
    pub fn for_subagent_child_id(
        parent: Option<&SessionHeader>,
        parent_id: SessionId,
        id: SessionId,
    ) -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: now_ms(),
            cwd: parent.and_then(|header| header.cwd.clone()).or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().to_string())
            }),
            parent_session: Some(parent_id),
            seed_length: None,
            origin: Some("subagent".into()),
            delegation_depth: parent
                .map(|header| header.delegation_depth + 1)
                .unwrap_or(1),
        }
    }
}

/// Current Unix epoch milliseconds; the envelope `time` and header `createdAt` source.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Why a turn ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    /// Natural completion.
    Completed,
    /// A step hit the token ceiling; sticky for the turn.
    MaxTokens,
    /// `agent/pre-step` rejected the batch.
    Blocked,
    /// Caller cancelled.
    Aborted {
        /// Stable caller intent.
        reason: String,
    },
    /// Structured failure.
    Error {
        /// Failure text.
        message: String,
        /// Machine code.
        code: String,
    },
    /// Persistence repaired a crash mid-turn.
    Interrupted,
}

/// How a surface event enters the ordered surface.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceOp {
    /// Tail append. Serialized as the string `"append"`.
    Append,
    /// Replace inclusive surface positions `[start, end]`.
    Replace {
        /// First surface seq.
        start: u64,
        /// Last surface seq.
        end: u64,
    },
}

impl SurfaceOp {
    /// Append marker used at write sites.
    pub fn append() -> Self {
        Self::Append
    }
}

impl Serialize for SurfaceOp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Append => serializer.serialize_str("append"),
            Self::Replace { start, end } => {
                #[derive(Serialize)]
                struct ReplaceOp {
                    start: u64,
                    end: u64,
                }
                ReplaceOp {
                    start: *start,
                    end: *end,
                }
                .serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for SurfaceOp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.as_str() == Some("append") {
            return Ok(Self::Append);
        }
        let start = value.get("start").and_then(Value::as_u64);
        let end = value.get("end").and_then(Value::as_u64);
        match (start, end) {
            (Some(start), Some(end)) => Ok(Self::Replace { start, end }),
            _ => Err(serde::de::Error::custom("invalid surfaceOp")),
        }
    }
}

/// Merge-extensible session event. Known members serialize as `{type, data}` like TypeScript.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum SessionEventData {
    /// Opens a turn before the first claim.
    #[serde(rename = "turn/start")]
    TurnStart {
        /// Turn number, 1-based.
        turn: u32,
    },
    /// Closes a turn.
    #[serde(rename = "turn/end")]
    TurnEnd {
        /// Turn number.
        turn: u32,
        /// Why it ended.
        reason: TurnEndReason,
    },
    /// Opens a step.
    #[serde(rename = "step/start")]
    StepStart {
        /// Owning turn.
        turn: u32,
        /// Step number, 1-based within the turn.
        step: u32,
    },
    /// Closes a step.
    #[serde(rename = "step/end")]
    StepEnd {
        /// Owning turn.
        turn: u32,
        /// Step number.
        step: u32,
    },
    /// User-role surface message.
    #[serde(rename = "user/message")]
    UserMessage(UserMessage),
    /// Raw stream chunk.
    #[serde(rename = "assistant/chunk")]
    AssistantChunk {
        /// Owning turn.
        turn: u32,
        /// Owning step.
        step: u32,
        /// Chunk body.
        chunk: StreamChunk,
    },
    /// Assembled assistant message.
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// Owning turn.
        turn: u32,
        /// Owning step.
        step: u32,
        /// Assembled message.
        message: AssistantMessage,
        /// Token usage when reported.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// Model requested one tool.
    #[serde(rename = "tool/call")]
    ToolCall {
        /// Owning turn.
        turn: u32,
        /// Owning step.
        step: u32,
        /// Correlation id.
        #[serde(rename = "callId")]
        call_id: String,
        /// Tool name.
        name: String,
        /// Raw arguments.
        arguments: String,
    },
    /// Completed tool result on the surface.
    #[serde(rename = "tool/result")]
    ToolResult {
        /// Owning turn.
        turn: u32,
        /// Owning step.
        step: u32,
        /// Result message.
        message: ToolResultMessage,
        /// Crash-recovery classification when this result is a synthetic closer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolRecoveryError>,
    },
    /// Permission preset selected for this session.
    #[serde(rename = "permission/preset")]
    PermissionPreset {
        /// Preset name (`danger-full-access`, `workspace-write`, `read-only`).
        preset: String,
    },
    /// File-sandbox mode written by the permission preset or an override.
    #[serde(rename = "sandbox/mode")]
    SandboxMode {
        /// Mode name.
        mode: String,
        /// `"delegation"` when a child start seeded this override; omitted for a runtime switch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Approval policy written by the permission preset or an override.
    #[serde(rename = "approval/policy")]
    ApprovalPolicy {
        /// Policy name (`ask` or `never`).
        policy: String,
        /// `"delegation"` when a child start seeded this override; omitted for a runtime switch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// An approval question was put to the answerer chain — log-only audit.
    #[serde(rename = "approval/asked")]
    ApprovalAsked {
        /// Pairs this ask with the matching [`Self::ApprovalDecided`].
        id: String,
        /// Tool the question is about.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Exact tool call when the asker had one.
        #[serde(rename = "callId", skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// Asker's human-readable explanation.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Outcome of a prior [`Self::ApprovalAsked`] with the same `id`.
    #[serde(rename = "approval/decided")]
    ApprovalDecided {
        /// Matching ask id.
        id: String,
        /// Closed outcome (`allowed-once`, `rejected`, `cancelled`, `unavailable`).
        outcome: String,
    },
    /// One normalized mutation of an agent's pending-message lists.
    #[serde(rename = "agent/inbox/spliced")]
    AgentInboxSpliced {
        /// Pending list (`next-turn` or `next-step`).
        target: String,
        /// Splice start index.
        start: u64,
        /// Messages removed at `start`. Omitted when the splice only inserts.
        #[serde(rename = "removedCount", skip_serializing_if = "Option::is_none")]
        removed_count: Option<u32>,
        /// Messages inserted at `start`.
        inserted: Vec<UserMessage>,
    },
    /// Frozen model-request header for one dispatch.
    #[serde(rename = "request/header")]
    RequestHeader {
        /// Effective call config, system prompt, and tools.
        header: Value,
        /// `initial`, `resume`, or `change`.
        reason: String,
    },
    /// Provider/model (and optional context window) for the last request.
    #[serde(rename = "request/context")]
    RequestContext {
        /// Provider route.
        provider: String,
        /// Model id.
        model: String,
        /// Context window in tokens, when the adapter reported one.
        #[serde(rename = "contextWindow", skip_serializing_if = "Option::is_none")]
        context_window: Option<u32>,
    },
    /// Session title. Fallback writes this before an optional LLM title provider.
    #[serde(rename = "session/title")]
    SessionTitle {
        /// Title text.
        title: String,
        /// Surface seqs of the human messages that produced the title.
        #[serde(rename = "messageSeqs")]
        message_seqs: Vec<u64>,
        /// Title source (`fallback`, `user`, or `provider`).
        source: Value,
    },
    /// Log-only pre-dispatch record of one session-title model request.
    #[serde(rename = "session/title-llm-request")]
    SessionTitleLlmRequest {
        /// Registered title-provider identity responsible for the request.
        #[serde(rename = "titleProvider")]
        title_provider: String,
        /// Exact human `user/message` seqs represented in `messages`.
        #[serde(rename = "messageSeqs")]
        message_seqs: Vec<u64>,
        /// Exact auxiliary LLM route (`{provider, model}`).
        route: Value,
        /// Exact auxiliary system prompt.
        system: String,
        /// Exact auxiliary message list.
        messages: Vec<Message>,
        /// Exact auxiliary output-token cap.
        #[serde(rename = "maxTokens")]
        max_tokens: u32,
    },
    /// Log-only structured todo snapshot written by `todo_write`.
    #[serde(rename = "todo/write")]
    TodoWrite {
        /// Complete replacement todo list (`content`, `status` items).
        todos: Value,
    },
    /// Log-only plan-mode transition committed at a step boundary.
    #[serde(rename = "plan/mode")]
    PlanMode {
        /// Whether plan mode is in force after this event.
        active: bool,
    },
    /// Log-only record of one tool-result prune; the `tool/result`
    /// replacement that lands the pruned content immediately follows.
    #[serde(rename = "compaction/prune")]
    CompactionPrune {
        /// Replaced surface range (`{start, end}` seqs).
        #[serde(rename = "shadowedRange")]
        shadowed_range: Value,
        /// Shadowed surface seqs in surface order.
        #[serde(rename = "shadowedSeqs")]
        shadowed_seqs: Vec<u64>,
        /// Estimated token count removed by the prune.
        #[serde(rename = "shadowedTokenCount")]
        shadowed_token_count: u64,
    },
    /// Compaction lock start.
    #[serde(rename = "compaction/start")]
    CompactionStart {
        /// Attempt identity shared by the start/summary/end triplet.
        #[serde(rename = "compactionId")]
        compaction_id: String,
        /// Originating `/compact` command id for a manual attempt.
        #[serde(
            rename = "sourceCommandId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        source_command_id: Option<String>,
        /// Open turn, or none for a manual attempt.
        turn: Option<u32>,
    },
    /// Compaction summary record (log-only).
    #[serde(rename = "compaction/summary")]
    CompactionSummary {
        /// Attempt identity shared by the start/summary/end triplet.
        #[serde(rename = "compactionId")]
        compaction_id: String,
        /// Originating `/compact` command id for a manual attempt.
        #[serde(
            rename = "sourceCommandId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        source_command_id: Option<String>,
        /// Safe text-only summary blocks before checkpoint framing.
        summary: Vec<ContentBlock>,
        /// Complete provider output before the text-only projection.
        #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
        raw_output: Option<Vec<ContentBlock>>,
        /// `true` when this result consumed exactly one `ctx.llm.stream()` call.
        #[serde(rename = "llmStreamCall", skip_serializing_if = "Option::is_none")]
        llm_stream_call: Option<bool>,
        /// Replaced surface range (`{start, end}` seqs).
        #[serde(rename = "shadowedRange")]
        shadowed_range: Value,
        /// Shadowed surface seqs in surface order.
        #[serde(rename = "shadowedSeqs")]
        shadowed_seqs: Vec<u64>,
        /// Estimated token count of the shadowed span.
        #[serde(rename = "shadowedTokenCount")]
        shadowed_token_count: u64,
        /// Provider route that wrote the summary.
        provider: String,
        /// Model that wrote the summary.
        model: String,
        /// Generation cap sent on the summarize call, when one applied.
        #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
        /// Provider-reported usage for the summarize call, when emitted.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// Constructor-seed boundary. A resumed or forked session appends this
    /// once after the seed when the stored log does not already end with it.
    #[serde(rename = "session/end-seed")]
    SessionEndSeed {},
    /// Compaction lock end.
    #[serde(rename = "compaction/end")]
    CompactionEnd {
        /// Attempt identity shared by the start/summary/end triplet.
        #[serde(rename = "compactionId")]
        compaction_id: String,
        /// Originating `/compact` command id for a manual attempt.
        #[serde(
            rename = "sourceCommandId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        source_command_id: Option<String>,
        /// Matching start attribution.
        turn: Option<u32>,
        /// Failure text when the attempt failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Plugin-merged log-only event. Wire form is `{type, data}` like every
    /// other member; the untagged arm is only the deserialize fallback for
    /// names this build does not have a typed variant for.
    #[serde(untagged)]
    Extension {
        /// Event type name.
        #[serde(rename = "type")]
        type_name: String,
        /// Payload nested under `data`, matching TypeScript JSONL.
        data: Value,
    },
}

/// Envelope stored in the log. `seq` and `time` are assigned by [`Session::append`].
/// Wire order matches TypeScript: `type`, `seq`, `time`, `data`,
/// `sourceEventSeqs`, `surfaceOp`, `ignorable`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvent {
    /// Contiguous sequence number, 0-based.
    pub seq: u64,
    /// Unix epoch milliseconds at append.
    pub time: u64,
    /// Event body.
    pub data: SessionEventData,
    /// Seqs of earlier events this surface event cites as sources.
    pub source_event_seqs: Option<Vec<u64>>,
    /// Surface membership, required for surface types.
    pub surface_op: Option<SurfaceOp>,
    /// Unknown required-on-read events may set this to keep older readers alive.
    pub ignorable: bool,
}

impl Serialize for SessionEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let body = serde_json::to_value(&self.data).map_err(serde::ser::Error::custom)?;
        let Value::Object(body) = body else {
            return Err(serde::ser::Error::custom("event body must be an object"));
        };
        let mut map = serializer.serialize_map(None)?;
        if let Some(type_name) = body.get("type") {
            map.serialize_entry("type", type_name)?;
        }
        map.serialize_entry("seq", &self.seq)?;
        map.serialize_entry("time", &self.time)?;
        for (key, value) in &body {
            if key != "type" {
                map.serialize_entry(key, value)?;
            }
        }
        if let Some(seqs) = &self.source_event_seqs {
            map.serialize_entry("sourceEventSeqs", seqs)?;
        }
        if let Some(op) = &self.surface_op {
            map.serialize_entry("surfaceOp", op)?;
        }
        if self.ignorable {
            map.serialize_entry("ignorable", &true)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SessionEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object_mut() else {
            return Err(serde::de::Error::custom("event must be an object"));
        };
        let seq = object
            .remove("seq")
            .and_then(|seq| seq.as_u64())
            .unwrap_or(0);
        let time = object
            .remove("time")
            .and_then(|time| time.as_u64())
            .unwrap_or(0);
        let source_event_seqs = match object.remove("sourceEventSeqs") {
            Some(seqs) => {
                Some(serde_json::from_value::<Vec<u64>>(seqs).map_err(serde::de::Error::custom)?)
            }
            None => None,
        };
        let surface_op = match object.remove("surfaceOp") {
            Some(op) => {
                Some(serde_json::from_value::<SurfaceOp>(op).map_err(serde::de::Error::custom)?)
            }
            None => None,
        };
        let ignorable = object
            .remove("ignorable")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);
        let data: SessionEventData =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            seq,
            time,
            data,
            source_event_seqs,
            surface_op,
            ignorable,
        })
    }
}

/// Classification carried on a synthetic interrupted `tool/result`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRecoveryError {
    /// TypeScript error `name`.
    pub name: String,
    /// TypeScript error `code`.
    pub code: String,
}

impl SessionEventData {
    /// Whether this type may carry a surface op.
    pub fn is_surface(&self) -> bool {
        matches!(
            self,
            Self::UserMessage(_) | Self::AssistantMessage { .. } | Self::ToolResult { .. }
        )
    }
}

/// Wire type name stored on a log event.
pub fn event_type_name(data: &SessionEventData) -> &str {
    match data {
        SessionEventData::TurnStart { .. } => "turn/start",
        SessionEventData::TurnEnd { .. } => "turn/end",
        SessionEventData::StepStart { .. } => "step/start",
        SessionEventData::StepEnd { .. } => "step/end",
        SessionEventData::UserMessage(_) => "user/message",
        SessionEventData::AssistantChunk { .. } => "assistant/chunk",
        SessionEventData::AssistantMessage { .. } => "assistant/message",
        SessionEventData::ToolCall { .. } => "tool/call",
        SessionEventData::ToolResult { .. } => "tool/result",
        SessionEventData::PermissionPreset { .. } => "permission/preset",
        SessionEventData::SandboxMode { .. } => "sandbox/mode",
        SessionEventData::ApprovalPolicy { .. } => "approval/policy",
        SessionEventData::ApprovalAsked { .. } => "approval/asked",
        SessionEventData::ApprovalDecided { .. } => "approval/decided",
        SessionEventData::AgentInboxSpliced { .. } => "agent/inbox/spliced",
        SessionEventData::RequestHeader { .. } => "request/header",
        SessionEventData::RequestContext { .. } => "request/context",
        SessionEventData::SessionTitle { .. } => "session/title",
        SessionEventData::SessionTitleLlmRequest { .. } => "session/title-llm-request",
        SessionEventData::TodoWrite { .. } => "todo/write",
        SessionEventData::PlanMode { .. } => "plan/mode",
        SessionEventData::CompactionPrune { .. } => "compaction/prune",
        SessionEventData::CompactionStart { .. } => "compaction/start",
        SessionEventData::CompactionSummary { .. } => "compaction/summary",
        SessionEventData::CompactionEnd { .. } => "compaction/end",
        SessionEventData::SessionEndSeed {} => "session/end-seed",
        SessionEventData::Extension { type_name, .. } => type_name.as_str(),
    }
}

/// Whether `data` is the constructor-seed marker, typed or as an extension.
pub fn is_end_seed(data: &SessionEventData) -> bool {
    matches!(data, SessionEventData::SessionEndSeed {})
        || matches!(
            data,
            SessionEventData::Extension { type_name, .. } if type_name == "session/end-seed"
        )
}

/// Event types this build reconstructs without an `ignorable` marker.
pub const KNOWN_SESSION_EVENT_TYPES: &[&str] = &[
    "agent-preset/selected",
    "agent/inbox/spliced",
    "approval/asked",
    "approval/decided",
    "approval/policy",
    "assistant/chunk",
    "assistant/message",
    "compaction/end",
    "compaction/prune",
    "compaction/start",
    "compaction/summary",
    "command/done",
    "command/run",
    "feedback/record",
    "goal/change",
    "hook/invoked",
    "hook/result",
    "llm/retry",
    "llm/retry-started",
    "permission/preset",
    "plan/mode",
    "request/context",
    "request/header",
    "sandbox/mode",
    "schedule/change",
    "session/end-seed",
    "session/title",
    "session/title-llm-request",
    "step/end",
    "step/start",
    "subagent/descriptor",
    "team/member",
    "team/message/delivered",
    "team/message/queued",
    "team/task",
    "tool-workflow/agent-end",
    "tool-workflow/agent-start",
    "tool-workflow/run-end",
    "tool-workflow/run-start",
    "todo/write",
    "tool/call",
    "tool/code-dispatch",
    "tool/code-dispatch-start",
    "tool/result",
    "turn/end",
    "turn/start",
    "user/message",
    "web/deepseek-search-llm-request",
];

/// Whether `type_name` is in [`KNOWN_SESSION_EVENT_TYPES`].
pub fn is_known_session_event_type(type_name: &str) -> bool {
    KNOWN_SESSION_EVENT_TYPES.contains(&type_name)
}

/// Refuse a required-on-read event this build does not know.
///
/// Unknown types are accepted only when `ignorable` is true. Silently skipping
/// a required event would reconstruct a wrong session.
pub fn refuse_unknown(type_name: &str, ignorable: bool) -> Result<(), SessionError> {
    if is_known_session_event_type(type_name) || ignorable {
        Ok(())
    } else {
        Err(SessionError::UnknownRequiredEvent(type_name.to_string()))
    }
}

/// Decode one logged JSON object, refusing unknown required-on-read types.
pub fn session_event_from_value(value: Value) -> Result<SessionEvent, SessionError> {
    let type_name = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ignorable = value
        .get("ignorable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    refuse_unknown(&type_name, ignorable)?;
    serde_json::from_value(value).map_err(|error| {
        SessionError::UnknownRequiredEvent(format!("malformed {type_name}: {error}"))
    })
}

/// Errors from append or fold.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Surface event missing `surfaceOp`.
    #[error("surface event requires surfaceOp")]
    MissingSurfaceOp,
    /// Non-surface event carried a surface op.
    #[error("non-surface event must not carry surfaceOp")]
    UnexpectedSurfaceOp,
    /// Non-surface event carried source-event citations.
    #[error("non-surface event must not carry sourceEventSeqs")]
    UnexpectedSourceSeqs,
    /// Replace cited a seq that is not on the current surface.
    #[error("replace range is not on the current surface")]
    InvalidReplace,
    /// Required-on-read event type this build does not know.
    #[error("unknown required-on-read event type `{0}`")]
    UnknownRequiredEvent(String),
    /// A `session/event` observer appended to the same session.
    #[error("session append cannot reenter while another append is being published")]
    ReentrantAppend,
}

/// Folded surface: current nodes and how many replacements have landed.
#[derive(Debug, Clone, Default)]
pub struct SessionSurface {
    /// Current surface seqs in visual order.
    pub nodes: Vec<u64>,
    /// Monotonic count of committed positional replacements.
    pub replace_generation: u64,
}

impl SessionSurface {
    fn apply(&mut self, seq: u64, op: &SurfaceOp) -> Result<(), SessionError> {
        match op {
            SurfaceOp::Append => {
                self.nodes.push(seq);
                Ok(())
            }
            SurfaceOp::Replace { start, end } => {
                let start_idx = self
                    .nodes
                    .iter()
                    .position(|node| *node == *start)
                    .ok_or(SessionError::InvalidReplace)?;
                let end_idx = self
                    .nodes
                    .iter()
                    .position(|node| *node == *end)
                    .ok_or(SessionError::InvalidReplace)?;
                if start_idx > end_idx {
                    return Err(SessionError::InvalidReplace);
                }
                self.nodes.splice(start_idx..=end_idx, [seq]);
                self.replace_generation += 1;
                Ok(())
            }
        }
    }
}

/// One append-only session.
pub struct Session {
    id: SessionId,
    header: SessionHeader,
    events: Mutex<Vec<SessionEvent>>,
    surface: Mutex<SessionSurface>,
    publisher: Mutex<Option<SessionEventSink>>,
    appending: AtomicBool,
    first_live_seq: AtomicU64,
}

impl Session {
    /// Create an empty session with a fresh top-level header.
    pub fn new(id: SessionId) -> Self {
        let header = SessionHeader::new(id.clone(), None);
        Self::with_header(header)
    }

    /// Create an empty session under explicit creation metadata.
    /// The header id names the session.
    pub fn with_header(header: SessionHeader) -> Self {
        Self {
            id: header.id.clone(),
            header,
            events: Mutex::new(Vec::new()),
            surface: Mutex::new(SessionSurface::default()),
            publisher: Mutex::new(None),
            appending: AtomicBool::new(false),
            first_live_seq: AtomicU64::new(0),
        }
    }

    /// Reconstruct a session from a constructor seed (resume, fork, or replay).
    ///
    /// `first_live_seq` is the seed length. When the seed does not already end
    /// with `session/end-seed`, that marker is appended at that seq and is
    /// itself a constructor write: it must land before the store publisher.
    pub fn from_seed(
        header: SessionHeader,
        events: impl IntoIterator<Item = SessionEvent>,
    ) -> Result<Self, SessionError> {
        let session = Self::with_header(header);
        for event in events {
            session.append_logged(event)?;
        }
        let first_live = session.events().len() as u64;
        session.first_live_seq.store(first_live, Ordering::SeqCst);
        let needs_marker = match session.events().last() {
            Some(event) if is_end_seed(&event.data) => false,
            _ => true,
        };
        if needs_marker {
            session.append(SessionEventData::SessionEndSeed {}, None)?;
        }
        Ok(session)
    }

    /// First seq appended in this process: the constructor seed length.
    pub fn first_live_seq(&self) -> u64 {
        self.first_live_seq.load(Ordering::SeqCst)
    }

    /// Attach the store's `session/event` firehose. Detached sessions stay silent.
    pub fn set_publisher(&self, sink: SessionEventSink) {
        *self.publisher.lock().expect("publisher") = Some(sink);
    }

    /// Session identity.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Immutable creation metadata persisted as the header line.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Append one event. `seq` equals the new log length minus one.
    pub fn append(
        &self,
        data: SessionEventData,
        surface_op: Option<SurfaceOp>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_inner(data, surface_op, None, false, None)
    }

    /// Append a surface event that cites earlier events as sources.
    pub fn append_cited(
        &self,
        data: SessionEventData,
        surface_op: SurfaceOp,
        source_event_seqs: Vec<u64>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_inner(data, Some(surface_op), Some(source_event_seqs), false, None)
    }

    /// Append a log-only event that unknown readers may skip.
    pub fn append_ignorable(&self, data: SessionEventData) -> Result<SessionEvent, SessionError> {
        self.append_inner(data, None, None, true, None)
    }

    /// Append a previously logged event after refusing unknown required types.
    pub fn append_logged(&self, event: SessionEvent) -> Result<SessionEvent, SessionError> {
        refuse_unknown(event_type_name(&event.data), event.ignorable)?;
        self.append_inner(
            event.data,
            event.surface_op,
            event.source_event_seqs,
            event.ignorable,
            Some(event.time),
        )
    }

    /// Reconstruct a session by appending each logged event in order.
    pub fn replay(
        id: SessionId,
        events: impl IntoIterator<Item = SessionEvent>,
    ) -> Result<Self, SessionError> {
        let session = Self::new(id);
        for event in events {
            session.append_logged(event)?;
        }
        Ok(session)
    }

    /// Copy this log into a child session under `child_id`.
    pub fn fork(&self, child_id: SessionId) -> Result<Self, SessionError> {
        Self::replay(child_id, self.events())
    }

    fn append_inner(
        &self,
        data: SessionEventData,
        surface_op: Option<SurfaceOp>,
        source_event_seqs: Option<Vec<u64>>,
        ignorable: bool,
        time: Option<u64>,
    ) -> Result<SessionEvent, SessionError> {
        if data.is_surface() && surface_op.is_none() {
            return Err(SessionError::MissingSurfaceOp);
        }
        if !data.is_surface() && surface_op.is_some() {
            return Err(SessionError::UnexpectedSurfaceOp);
        }
        if !data.is_surface() && source_event_seqs.is_some() {
            return Err(SessionError::UnexpectedSourceSeqs);
        }
        if self.appending.swap(true, Ordering::SeqCst) {
            return Err(SessionError::ReentrantAppend);
        }
        struct AppendGuard<'a>(&'a AtomicBool);
        impl Drop for AppendGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = AppendGuard(&self.appending);
        let event = {
            let mut events = self.events.lock().expect("log");
            let seq = events.len() as u64;
            if let Some(op) = &surface_op {
                self.surface.lock().expect("surface").apply(seq, op)?;
            }
            let event = SessionEvent {
                seq,
                time: time.unwrap_or_else(now_ms),
                data,
                source_event_seqs,
                surface_op,
                ignorable,
            };
            events.push(event.clone());
            event
        };
        if let Some(sink) = self.publisher.lock().expect("publisher").clone() {
            sink(&event);
        }
        Ok(event)
    }

    /// Borrow a snapshot of the log.
    pub fn events(&self) -> Vec<SessionEvent> {
        self.events.lock().expect("log").clone()
    }

    /// Current surface.
    pub fn surface(&self) -> SessionSurface {
        self.surface.lock().expect("surface").clone()
    }

    /// Project model history from the current surface.
    pub fn derive_messages(&self) -> Vec<Message> {
        let events = self.events();
        let surface = self.surface();
        surface
            .nodes
            .into_iter()
            .filter_map(|seq| {
                let event = events.get(seq as usize)?;
                derive_event_message(&event.data)
            })
            .collect()
    }

    /// Last assistant text on the current surface, if any.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.derive_messages()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => {
                    let text = assistant.text();
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                }
                _ => None,
            })
    }
}

/// Per-event projection used by `derive_messages` and external rebuilders.
pub fn derive_event_message(data: &SessionEventData) -> Option<Message> {
    match data {
        SessionEventData::UserMessage(message) => Some(Message::User(message.clone())),
        SessionEventData::AssistantMessage { message, .. } => {
            if message.content.is_empty() {
                None
            } else {
                Some(Message::Assistant(message.clone()))
            }
        }
        SessionEventData::ToolResult { message, .. } => Some(Message::Tool(message.clone())),
        _ => None,
    }
}

/// In-memory session store (`ctx.sessions`).
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    emit: Mutex<Option<Context>>,
}

impl SessionStore {
    /// Create an empty store. Appends stay silent until [`Self::install`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide `ctx.sessions` and publish `session/event` for later appends.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<Self>> {
        let store = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            emit: Mutex::new(Some(ctx.clone())),
        });
        ctx.provide(Arc::clone(&store))?;
        Ok(store)
    }

    fn make_sink(&self, id: &SessionId) -> Option<SessionEventSink> {
        let ctx = self.emit.lock().expect("session emit").clone()?;
        let session_id = id.as_str().to_string();
        Some(Arc::new(move |event: &SessionEvent| {
            ctx.emit(
                "session/event",
                serde_json::json!({
                    "sessionId": session_id,
                    "event": event,
                }),
            );
        }))
    }

    /// Create a session under a caller-supplied id, stamping the run cwd.
    pub fn create(&self, id: SessionId) -> Arc<Session> {
        let cwd = std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().to_string());
        self.publish(Session::with_header(SessionHeader::new(id, cwd)))
    }

    /// Publish a caller-constructed session (explicit header) into the store.
    pub fn publish(&self, session: Session) -> Arc<Session> {
        if let Some(sink) = self.make_sink(session.id()) {
            session.set_publisher(sink);
        }
        let session = Arc::new(session);
        self.sessions
            .lock()
            .expect("sessions")
            .insert(session.id().as_str().to_string(), Arc::clone(&session));
        if let Some(ctx) = self.emit.lock().expect("session emit").clone() {
            let mut payload = serde_json::json!({ "id": session.id().as_str() });
            if let Some(parent) = &session.header().parent_session {
                payload["parentSession"] = serde_json::json!(parent.as_str());
            }
            ctx.emit("session/created", payload);
        }
        session
    }

    /// Create a session with a fresh id.
    pub fn create_fresh(&self) -> Arc<Session> {
        self.create(session_id(Uuid::new_v4().to_string()))
    }

    /// Create a session with a fresh id and an explicit working directory.
    pub fn create_in(&self, cwd: Option<String>) -> Arc<Session> {
        self.publish(Session::with_header(SessionHeader::new(
            session_id(Uuid::new_v4().to_string()),
            cwd,
        )))
    }

    /// Look up a live session.
    pub fn get(&self, id: &SessionId) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .expect("sessions")
            .get(id.as_str())
            .cloned()
    }

    /// Remove a session from the store.
    pub fn remove(&self, id: &SessionId) {
        let removed = self
            .sessions
            .lock()
            .expect("sessions")
            .remove(id.as_str())
            .is_some();
        if removed {
            if let Some(ctx) = self.emit.lock().expect("session emit").clone() {
                ctx.emit("session/disposed", serde_json::json!({ "id": id.as_str() }));
            }
        }
    }

    /// Live sessions in arbitrary map order.
    pub fn live(&self) -> Vec<Arc<Session>> {
        self.sessions
            .lock()
            .expect("sessions")
            .values()
            .cloned()
            .collect()
    }
}

impl Service for SessionStore {
    const KEY: &'static str = "sessions";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::ContentBlock;

    #[test]
    fn seq_is_contiguous() {
        let session = Session::new(session_id("s"));
        let first = session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let second = session
            .append(SessionEventData::StepStart { turn: 1, step: 1 }, None)
            .unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
    }

    #[test]
    fn surface_event_requires_op() {
        let session = Session::new(session_id("s"));
        let err = session
            .append(SessionEventData::UserMessage(UserMessage::text("hi")), None)
            .unwrap_err();
        assert!(matches!(err, SessionError::MissingSurfaceOp));
    }

    #[test]
    fn empty_assistant_does_not_enter_history() {
        let session = Session::new(session_id("s"));
        session
            .append(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(vec![], "p", "m"),
                    usage: None,
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        assert!(session.derive_messages().is_empty());
    }

    #[test]
    fn replace_shadows_range_and_bumps_generation() {
        let session = Session::new(session_id("s"));
        let a = session
            .append(
                SessionEventData::UserMessage(UserMessage::text("old")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("keep")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::from_parts(
                    vec![ContentBlock::text("summary")],
                    dsh_llm::MessageSource::plugin("compaction"),
                )),
                Some(SurfaceOp::Replace {
                    start: a.seq,
                    end: a.seq,
                }),
            )
            .unwrap();
        let messages = session.derive_messages();
        assert_eq!(messages.len(), 2);
        match &messages[0] {
            Message::User(user) => match &user.content[0] {
                ContentBlock::Text { text } => assert_eq!(text, "summary"),
                _ => panic!("expected text"),
            },
            _ => panic!("expected user"),
        }
        assert_eq!(session.surface().replace_generation, 1);
    }

    #[test]
    fn refuse_unknown_required_event() {
        let err = refuse_unknown("future/event", false).unwrap_err();
        assert!(matches!(err, SessionError::UnknownRequiredEvent(name) if name == "future/event"));
        assert!(refuse_unknown("future/event", true).is_ok());
        assert!(refuse_unknown("turn/start", false).is_ok());
    }

    #[test]
    fn append_logged_replay_and_fork() {
        let source = Session::new(session_id("src"));
        source
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let replayed = Session::replay(session_id("replay"), source.events()).unwrap();
        assert_eq!(replayed.events().len(), 1);
        let child = source.fork(session_id("child")).unwrap();
        assert_eq!(child.id().as_str(), "child");
        assert_eq!(child.events(), source.events());
        let unknown = SessionEvent {
            seq: 0,
            time: 0,
            data: SessionEventData::Extension {
                type_name: "future/event".into(),
                data: serde_json::json!({}),
            },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        };
        let err = Session::new(session_id("bad"))
            .append_logged(unknown)
            .unwrap_err();
        assert!(matches!(err, SessionError::UnknownRequiredEvent(_)));
    }

    #[test]
    fn known_events_serialize_with_data_wrapper() {
        let session = Session::new(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("hi")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        let events = session.events();
        let first = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(first["type"], "turn/start");
        assert_eq!(first["seq"], 0);
        assert!(first["time"].as_u64().is_some());
        assert_eq!(first["data"], serde_json::json!({"turn":1}));
        let keys: Vec<&str> = first
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["type", "seq", "time", "data"]);
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap()["surfaceOp"],
            serde_json::json!("append")
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap()["data"]["source"],
            serde_json::json!({"kind":"user"})
        );
    }

    #[test]
    fn policy_source_omits_when_absent_and_round_trips_when_present() {
        let session = Session::new(session_id("s"));
        session
            .append(
                SessionEventData::SandboxMode {
                    mode: "read-only".into(),
                    source: None,
                },
                None,
            )
            .unwrap();
        session
            .append(
                SessionEventData::ApprovalPolicy {
                    policy: "never".into(),
                    source: Some("delegation".into()),
                },
                None,
            )
            .unwrap();
        let sandbox = serde_json::to_value(&session.events()[0]).unwrap();
        assert_eq!(sandbox["type"], "sandbox/mode");
        assert_eq!(sandbox["data"]["mode"], "read-only");
        assert!(sandbox["data"].get("source").is_none());
        let approval = serde_json::to_value(&session.events()[1]).unwrap();
        assert_eq!(
            approval["data"],
            serde_json::json!({ "policy": "never", "source": "delegation" })
        );
        let restored: SessionEventData = serde_json::from_value(serde_json::json!({
            "type": "sandbox/mode",
            "data": { "mode": "workspace-write", "source": "other" }
        }))
        .unwrap();
        match restored {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "workspace-write");
                assert_eq!(source.as_deref(), Some("other"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn extension_events_nest_payload_under_data() {
        let session = Session::new(session_id("s"));
        session
            .append(
                SessionEventData::Extension {
                    type_name: "goal/change".into(),
                    data: serde_json::json!({"kind":"goal/change","version":1}),
                },
                None,
            )
            .unwrap();
        let wire = serde_json::to_value(&session.events()[0]).unwrap();
        assert_eq!(wire["type"], "goal/change");
        assert_eq!(
            wire["data"],
            serde_json::json!({"kind":"goal/change","version":1})
        );
        assert!(wire.get("kind").is_none());
        let keys: Vec<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["type", "seq", "time", "data"]);
        let parsed: SessionEvent = serde_json::from_value(wire).unwrap();
        let SessionEventData::Extension { type_name, data } = parsed.data else {
            panic!("expected extension");
        };
        assert_eq!(type_name, "goal/change");
        assert_eq!(data["version"], 1);
    }

    #[test]
    fn cited_surface_event_round_trips_source_event_seqs() {
        let session = Session::new(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let cited = session
            .append_cited(
                SessionEventData::UserMessage(UserMessage::text("hi")),
                SurfaceOp::append(),
                vec![0],
            )
            .unwrap();
        let wire = serde_json::to_value(&cited).unwrap();
        assert_eq!(wire["sourceEventSeqs"], serde_json::json!([0]));
        let parsed: SessionEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.source_event_seqs, Some(vec![0]));
        let err = session
            .append_inner(
                SessionEventData::TurnStart { turn: 2 },
                None,
                Some(vec![0]),
                false,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, SessionError::UnexpectedSourceSeqs));
    }

    #[test]
    fn known_types_cover_typescript_vocabulary() {
        for name in [
            "agent-preset/selected",
            "approval/asked",
            "approval/decided",
            "hook/invoked",
            "hook/result",
            "schedule/change",
            "session/end-seed",
            "team/member",
            "team/message/delivered",
            "team/message/queued",
            "team/task",
            "tool/code-dispatch",
            "tool/code-dispatch-start",
        ] {
            assert!(
                is_known_session_event_type(name),
                "{name} must be readable without ignorable"
            );
        }
    }

    #[test]
    fn header_records_creation_metadata() {
        let session = Session::new(session_id("h"));
        assert_eq!(session.header().version, SESSION_FORMAT_VERSION);
        assert_eq!(session.header().id.as_str(), "h");
        assert_eq!(session.header().delegation_depth, 0);
        let store = SessionStore::new();
        let created = store.create(session_id("c"));
        assert!(created.header().cwd.is_some());
    }

    #[test]
    fn for_subagent_child_id_reserves_the_caller_identity() {
        let parent = SessionHeader::new(session_id("parent"), Some("/tmp".into()));
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        let header = SessionHeader::for_subagent_child_id(
            Some(&parent),
            parent.id.clone(),
            reserved.clone(),
        );
        assert_eq!(header.id, reserved);
        assert_eq!(header.parent_session.as_ref(), Some(&parent.id));
        assert_eq!(header.origin.as_deref(), Some("subagent"));
        assert_eq!(header.delegation_depth, 1);
        assert_eq!(header.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn from_seed_appends_end_seed_once_and_keeps_first_live_seq() {
        let source = Session::new(session_id("src"));
        source
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        source
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        let seeded = Session::from_seed(source.header().clone(), source.events()).unwrap();
        assert_eq!(seeded.first_live_seq(), 2);
        assert_eq!(seeded.events().len(), 3);
        assert!(is_end_seed(&seeded.events().last().unwrap().data));
        let reopened = Session::from_seed(seeded.header().clone(), seeded.events()).unwrap();
        assert_eq!(reopened.first_live_seq(), 3);
        assert_eq!(reopened.events().len(), 3);
        assert!(is_end_seed(&reopened.events().last().unwrap().data));
    }

    #[test]
    fn empty_seed_appends_end_seed_at_seq_zero() {
        let seeded =
            Session::from_seed(Session::new(session_id("empty")).header().clone(), []).unwrap();
        assert_eq!(seeded.first_live_seq(), 0);
        assert_eq!(seeded.events().len(), 1);
        let wire = serde_json::to_value(&seeded.events()[0]).unwrap();
        assert_eq!(wire["type"], "session/end-seed");
        assert_eq!(wire["seq"], 0);
        assert_eq!(wire["data"], serde_json::json!({}));
        let parsed: SessionEvent = serde_json::from_value(wire).unwrap();
        assert!(is_end_seed(&parsed.data));
    }

    #[test]
    fn constructor_seed_does_not_emit_after_publish() {
        let ctx = Context::new();
        let heard = Arc::new(Mutex::new(0u32));
        let count = Arc::clone(&heard);
        ctx.on("session/event", move |_| {
            *count.lock().expect("heard") += 1;
        })
        .unwrap();
        let source = Session::new(session_id("seed"));
        source
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let seeded = Session::from_seed(source.header().clone(), source.events()).unwrap();
        let store = SessionStore::install(&ctx).unwrap();
        let live = store.publish(seeded);
        assert_eq!(*heard.lock().expect("heard"), 0);
        live.append(
            SessionEventData::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            None,
        )
        .unwrap();
        assert_eq!(*heard.lock().expect("heard"), 1);
    }

    #[test]
    fn publish_emits_session_created_and_remove_emits_disposed() {
        let ctx = Context::new();
        let created = Arc::new(Mutex::new(Vec::new()));
        let disposed = Arc::new(Mutex::new(Vec::new()));
        let created_ids = Arc::clone(&created);
        let disposed_ids = Arc::clone(&disposed);
        ctx.on("session/created", move |payload| {
            created_ids
                .lock()
                .expect("created")
                .push(payload["id"].as_str().unwrap_or("").to_string());
        })
        .unwrap();
        ctx.on("session/disposed", move |payload| {
            disposed_ids
                .lock()
                .expect("disposed")
                .push(payload["id"].as_str().unwrap_or("").to_string());
        })
        .unwrap();
        let store = SessionStore::install(&ctx).unwrap();
        let session = store.create(session_id("created-1"));
        assert_eq!(
            *created.lock().expect("created"),
            vec!["created-1".to_string()]
        );
        store.remove(session.id());
        assert_eq!(
            *disposed.lock().expect("disposed"),
            vec!["created-1".to_string()]
        );
    }

    #[test]
    fn detached_session_does_not_emit() {
        let ctx = Context::new();
        let heard = Arc::new(Mutex::new(0u32));
        let count = Arc::clone(&heard);
        ctx.on("session/event", move |_| {
            *count.lock().expect("heard") += 1;
        })
        .unwrap();
        Session::new(session_id("detached"))
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        assert_eq!(*heard.lock().expect("heard"), 0);
    }

    #[test]
    fn store_backed_append_emits_session_event() {
        let ctx = Context::new();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::clone(&heard);
        ctx.on("session/event", move |payload| {
            events.lock().expect("heard").push(payload);
        })
        .unwrap();
        let store = SessionStore::install(&ctx).unwrap();
        let session = store.create(session_id("live"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let payloads = heard.lock().expect("heard");
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["sessionId"], "live");
        assert_eq!(payloads[0]["event"]["type"], "turn/start");
        assert_eq!(payloads[0]["event"]["seq"], 0);
    }

    #[test]
    fn constructor_seed_does_not_emit_on_publish() {
        let ctx = Context::new();
        let heard = Arc::new(Mutex::new(0u32));
        let count = Arc::clone(&heard);
        ctx.on("session/event", move |_| {
            *count.lock().expect("heard") += 1;
        })
        .unwrap();
        let seed = Session::new(session_id("seed"));
        seed.append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let store = SessionStore::install(&ctx).unwrap();
        let session = store.publish(Session::replay(session_id("replay"), seed.events()).unwrap());
        assert_eq!(*heard.lock().expect("heard"), 0);
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        assert_eq!(*heard.lock().expect("heard"), 1);
    }

    #[test]
    fn reentrant_append_from_session_event_fails_loud() {
        let ctx = Context::new();
        let store = SessionStore::install(&ctx).unwrap();
        let session = store.create(session_id("reentrant"));
        let inner = Arc::clone(&session);
        let result = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&result);
        ctx.on("session/event", move |_| {
            *slot.lock().expect("inner") =
                Some(inner.append(SessionEventData::TurnStart { turn: 2 }, None));
        })
        .unwrap();
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let inner = result.lock().expect("inner").take().unwrap();
        assert!(matches!(inner, Err(SessionError::ReentrantAppend)));
        assert_eq!(session.events().len(), 1);
    }
}

/// Write the three permission knob events a session starts with.
pub fn append_session_knobs(
    session: &Session,
    preset: impl Into<String>,
    mode: impl Into<String>,
    policy: impl Into<String>,
) -> Result<(), SessionError> {
    session.append(
        SessionEventData::PermissionPreset {
            preset: preset.into(),
        },
        None,
    )?;
    session.append(
        SessionEventData::SandboxMode {
            mode: mode.into(),
            source: None,
        },
        None,
    )?;
    session.append(
        SessionEventData::ApprovalPolicy {
            policy: policy.into(),
            source: None,
        },
        None,
    )?;
    Ok(())
}
