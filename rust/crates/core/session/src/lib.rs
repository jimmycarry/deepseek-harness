//! Append-only session log. Model-visible means logged.

use dsh_brand::Branded;
use dsh_cordis::Service;
use dsh_llm::{AssistantMessage, Message, StreamChunk, TokenUsage, ToolResultMessage, UserMessage};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

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
    },
    /// Approval policy written by the permission preset or an override.
    #[serde(rename = "approval/policy")]
    ApprovalPolicy {
        /// Policy name (`ask` or `never`).
        policy: String,
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
    /// Compaction lock start.
    #[serde(rename = "compaction/start")]
    CompactionStart {
        /// Open turn, or none for a manual attempt.
        turn: Option<u32>,
    },
    /// Compaction summary record (log-only).
    #[serde(rename = "compaction/summary")]
    CompactionSummary {
        /// Shadowed surface seqs in surface order.
        #[serde(rename = "shadowedSeqs")]
        shadowed_seqs: Vec<u64>,
    },
    /// Compaction lock end.
    #[serde(rename = "compaction/end")]
    CompactionEnd {
        /// Matching start attribution.
        turn: Option<u32>,
        /// Failure text when the attempt failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Plugin-merged log-only event.
    #[serde(untagged)]
    Extension {
        /// Event type name.
        #[serde(rename = "type")]
        type_name: String,
        /// Payload.
        #[serde(flatten)]
        data: Value,
    },
}

/// Envelope stored in the log. `seq` is assigned by [`Session::append`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    /// Contiguous sequence number, 0-based.
    pub seq: u64,
    /// Event body.
    #[serde(flatten)]
    pub data: SessionEventData,
    /// Surface membership, required for surface types.
    #[serde(rename = "surfaceOp", skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<SurfaceOp>,
    /// Unknown required-on-read events may set this to keep older readers alive.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignorable: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
        SessionEventData::AgentInboxSpliced { .. } => "agent/inbox/spliced",
        SessionEventData::RequestHeader { .. } => "request/header",
        SessionEventData::RequestContext { .. } => "request/context",
        SessionEventData::SessionTitle { .. } => "session/title",
        SessionEventData::CompactionStart { .. } => "compaction/start",
        SessionEventData::CompactionSummary { .. } => "compaction/summary",
        SessionEventData::CompactionEnd { .. } => "compaction/end",
        SessionEventData::Extension { type_name, .. } => type_name.as_str(),
    }
}

/// Event types this build reconstructs without an `ignorable` marker.
pub const KNOWN_SESSION_EVENT_TYPES: &[&str] = &[
    "agent/inbox/spliced",
    "approval/policy",
    "assistant/chunk",
    "assistant/message",
    "compaction/end",
    "compaction/start",
    "compaction/summary",
    "goal/change",
    "permission/preset",
    "request/context",
    "request/header",
    "sandbox/mode",
    "session/title",
    "step/end",
    "step/start",
    "subagent/descriptor",
    "tool-workflow/agent-end",
    "tool-workflow/agent-start",
    "tool-workflow/run-end",
    "tool-workflow/run-start",
    "tool/call",
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
    if !is_known_session_event_type(&type_name) {
        return Ok(extension_from_value(value, type_name));
    }
    serde_json::from_value(value).map_err(|error| {
        SessionError::UnknownRequiredEvent(format!("malformed {type_name}: {error}"))
    })
}

fn extension_from_value(mut value: Value, type_name: String) -> SessionEvent {
    let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let surface_op = value
        .get("surfaceOp")
        .and_then(|op| serde_json::from_value(op.clone()).ok());
    if let Value::Object(map) = &mut value {
        map.remove("seq");
        map.remove("surfaceOp");
        map.remove("ignorable");
    }
    SessionEvent {
        seq,
        data: SessionEventData::Extension {
            type_name,
            data: value,
        },
        surface_op,
        ignorable: true,
    }
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
    /// Replace cited a seq that is not on the current surface.
    #[error("replace range is not on the current surface")]
    InvalidReplace,
    /// Required-on-read event type this build does not know.
    #[error("unknown required-on-read event type `{0}`")]
    UnknownRequiredEvent(String),
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
    events: Mutex<Vec<SessionEvent>>,
    surface: Mutex<SessionSurface>,
}

impl Session {
    /// Create an empty session.
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            events: Mutex::new(Vec::new()),
            surface: Mutex::new(SessionSurface::default()),
        }
    }

    /// Session identity.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Append one event. `seq` equals the new log length minus one.
    pub fn append(
        &self,
        data: SessionEventData,
        surface_op: Option<SurfaceOp>,
    ) -> Result<SessionEvent, SessionError> {
        self.append_inner(data, surface_op, false)
    }

    /// Append a log-only event that unknown readers may skip.
    pub fn append_ignorable(&self, data: SessionEventData) -> Result<SessionEvent, SessionError> {
        self.append_inner(data, None, true)
    }

    /// Append a previously logged event after refusing unknown required types.
    pub fn append_logged(&self, event: SessionEvent) -> Result<SessionEvent, SessionError> {
        refuse_unknown(event_type_name(&event.data), event.ignorable)?;
        self.append_inner(event.data, event.surface_op, event.ignorable)
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
        ignorable: bool,
    ) -> Result<SessionEvent, SessionError> {
        if data.is_surface() && surface_op.is_none() {
            return Err(SessionError::MissingSurfaceOp);
        }
        if !data.is_surface() && surface_op.is_some() {
            return Err(SessionError::UnexpectedSurfaceOp);
        }
        let mut events = self.events.lock().expect("log");
        let seq = events.len() as u64;
        if let Some(op) = &surface_op {
            self.surface.lock().expect("surface").apply(seq, op)?;
        }
        let event = SessionEvent {
            seq,
            data,
            surface_op,
            ignorable,
        };
        events.push(event.clone());
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
        self.derive_messages().into_iter().rev().find_map(|message| match message {
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
}

impl SessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session under a caller-supplied id.
    pub fn create(&self, id: SessionId) -> Arc<Session> {
        let session = Arc::new(Session::new(id.clone()));
        self.sessions
            .lock()
            .expect("sessions")
            .insert(id.as_str().to_string(), Arc::clone(&session));
        session
    }

    /// Create a session with a fresh id.
    pub fn create_fresh(&self) -> Arc<Session> {
        self.create(session_id(Uuid::new_v4().to_string()))
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
        self.sessions.lock().expect("sessions").remove(id.as_str());
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
            .append(
                SessionEventData::UserMessage(UserMessage::text("hi")),
                None,
            )
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
                    message: AssistantMessage::default(),
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
                SessionEventData::UserMessage(UserMessage {
                    content: vec![ContentBlock::text("summary")],
                    source: dsh_llm::MessageSource::plugin("compaction"),
                }),
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
            data: SessionEventData::Extension {
                type_name: "future/event".into(),
                data: serde_json::json!({}),
            },
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
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            serde_json::json!({"seq":0,"type":"turn/start","data":{"turn":1}})
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap()["surfaceOp"],
            serde_json::json!("append")
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap()["data"]["source"],
            serde_json::json!({"kind":"user"})
        );
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
        SessionEventData::SandboxMode { mode: mode.into() },
        None,
    )?;
    session.append(
        SessionEventData::ApprovalPolicy {
            policy: policy.into(),
        },
        None,
    )?;
    Ok(())
}
