//! Append-only session log. Model-visible means logged.

use dsh_brand::Branded;
use dsh_cordis::Service;
use dsh_llm::{AssistantMessage, Message, StreamChunk, TokenUsage, ToolResultMessage, UserMessage};
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SurfaceOp {
    /// Tail append.
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

/// Merge-extensible session event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
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
            ignorable: false,
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
                SessionEventData::UserMessage(UserMessage {
                    content: vec![ContentBlock::text("hi")],
                    source: None,
                }),
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
                SessionEventData::UserMessage(UserMessage {
                    content: vec![ContentBlock::text("old")],
                    source: None,
                }),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage {
                    content: vec![ContentBlock::text("keep")],
                    source: None,
                }),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage {
                    content: vec![ContentBlock::text("summary")],
                    source: Some("compaction".into()),
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
}
