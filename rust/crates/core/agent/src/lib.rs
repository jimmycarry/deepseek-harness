//! Agent interface, live registry, and `agent/*` events (`ctx.agents`).

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_llm::UserMessage;
use dsh_session::{Session, SessionId};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Inbox routing target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxTarget {
    /// Starts a new turn.
    NextTurn,
    /// Continues the current turn at the next step.
    NextStep,
}

/// Live agent status mirrored on `agent/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// No driver or maintenance task.
    Idle,
    /// A turn is running.
    Running,
    /// Idle-phase maintenance.
    Maintenance,
}

/// Why the caller cancelled.
#[derive(Debug, Clone)]
pub struct AgentCancelCause {
    /// Stable caller intent.
    pub kind: String,
}

/// Errors from the agent handle.
#[derive(Debug, Error)]
pub enum AgentError {
    /// No factory is registered.
    #[error("no agent factory is registered")]
    NoFactory,
    /// Session is missing.
    #[error("unknown session")]
    UnknownSession,
}

/// One queued inbox entry.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    /// Message body.
    pub message: UserMessage,
    /// Routing target.
    pub target: InboxTarget,
    /// Whether this message wakes the driver.
    pub wakeup: bool,
}

/// Agent-owned pending work.
#[derive(Default)]
pub struct Inbox {
    next_turn: Mutex<VecDeque<InboxEntry>>,
    next_step: Mutex<VecDeque<InboxEntry>>,
}

impl Inbox {
    /// Push one entry.
    pub fn push(&self, entry: InboxEntry) {
        match entry.target {
            InboxTarget::NextTurn => self.next_turn.lock().expect("inbox").push_back(entry),
            InboxTarget::NextStep => self.next_step.lock().expect("inbox").push_back(entry),
        }
    }

    /// Claim pending next-step input plus one queued next-turn message.
    pub fn claim(&self, prefer: InboxTarget) -> Vec<UserMessage> {
        let mut claimed = Vec::new();
        {
            let mut next_step = self.next_step.lock().expect("inbox");
            while let Some(entry) = next_step.pop_front() {
                claimed.push(entry.message);
            }
        }
        if prefer == InboxTarget::NextTurn || claimed.is_empty() {
            if let Some(entry) = self.next_turn.lock().expect("inbox").pop_front() {
                claimed.push(entry.message);
            }
        }
        claimed
    }

    /// Whether any waking work remains.
    pub fn has_pending(&self) -> bool {
        !self.next_turn.lock().expect("inbox").is_empty()
            || self
                .next_step
                .lock()
                .expect("inbox")
                .iter()
                .any(|entry| entry.wakeup)
    }

    /// Whether next-step input is pending.
    pub fn next_step_pending(&self) -> bool {
        !self.next_step.lock().expect("inbox").is_empty()
    }
}

/// Public live-agent handle.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Identity shared with the session.
    fn id(&self) -> &SessionId;
    /// Live session this agent drives.
    fn session(&self) -> Arc<Session>;
    /// Inbox.
    fn inbox(&self) -> Arc<Inbox>;
    /// Current lifecycle state.
    fn status(&self) -> AgentStatus;
    /// Unified send.
    fn send(&self, message: UserMessage, target: InboxTarget, wakeup: bool);
    /// `send(..., NextTurn, true)`.
    fn followup(&self, message: UserMessage) {
        self.send(message, InboxTarget::NextTurn, true);
    }
    /// `send(..., NextStep, true)`.
    fn steer(&self, message: UserMessage) {
        self.send(message, InboxTarget::NextStep, true);
    }
    /// `send(..., NextStep, false)` — context without waking.
    fn inject(&self, message: UserMessage) {
        self.send(message, InboxTarget::NextStep, false);
    }
    /// Cancel the active turn.
    fn cancel(&self, cause: AgentCancelCause);
    /// Resolve after quiescence.
    async fn when_idle(&self);
    /// Drive until idle (used by the loop implementation).
    async fn run(&self) -> Result<(), AgentError>;
    /// Run idle-phase maintenance (compaction, title). Default is a no-op.
    async fn run_maintenance(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

/// Factory registered by the loop.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Build a new agent for `session`.
    fn create(&self, session: Arc<Session>) -> Arc<dyn Agent>;
}

/// Owner handle.
pub struct AgentHandle {
    /// Live agent.
    pub agent: Arc<dyn Agent>,
    dispose: Box<dyn FnOnce() + Send>,
}

impl AgentHandle {
    /// Build a handle.
    pub fn new(agent: Arc<dyn Agent>, dispose: impl FnOnce() + Send + 'static) -> Self {
        Self {
            agent,
            dispose: Box::new(dispose),
        }
    }

    /// Tear the agent down.
    pub fn dispose(self) {
        (self.dispose)();
    }
}

/// `ctx.agents`.
pub struct AgentRegistry {
    factory: Mutex<Option<Arc<dyn AgentFactory>>>,
    live: Arc<Mutex<HashMap<String, Arc<dyn Agent>>>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            factory: Mutex::new(None),
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the loop factory.
    pub fn set_factory(&self, factory: Arc<dyn AgentFactory>) {
        *self.factory.lock().expect("factory") = Some(factory);
    }

    /// Create an agent under a caller-supplied session.
    pub fn create(&self, session: Arc<Session>) -> Result<AgentHandle, AgentError> {
        let factory = self
            .factory
            .lock()
            .expect("factory")
            .clone()
            .ok_or(AgentError::NoFactory)?;
        let agent = factory.create(Arc::clone(&session));
        self.live
            .lock()
            .expect("live")
            .insert(session.id().as_str().to_string(), Arc::clone(&agent));
        let live = Arc::new(());
        let _ = live;
        let id = session.id().as_str().to_string();
        let live = Arc::clone(&self.live);
        Ok(AgentHandle::new(agent, move || {
            live.lock().expect("live").remove(&id);
        }))
    }

    /// Look up a live agent.
    pub fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        self.live.lock().expect("live").get(id.as_str()).cloned()
    }

    /// Resume a persisted session under a new live agent.
    pub fn resume(&self, session: Arc<Session>) -> Result<AgentHandle, AgentError> {
        self.create(session)
    }
}

impl Service for AgentRegistry {
    const KEY: &'static str = "agents";
}

/// Default model selection for Agents created without a session-specific model (`ctx.agentDefaultModel`).
pub struct AgentDefaultModel {
    /// Registered provider route.
    pub provider: String,
    /// Provider-owned model id.
    pub model: String,
}

impl AgentDefaultModel {
    /// Build from the composition entry.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

impl Service for AgentDefaultModel {
    const KEY: &'static str = "agentDefaultModel";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_does_not_count_as_pending_wake() {
        let inbox = Inbox::default();
        inbox.push(InboxEntry {
            message: UserMessage {
                content: vec![],
                source: Some("inject".into()),
            },
            target: InboxTarget::NextStep,
            wakeup: false,
        });
        assert!(!inbox.has_pending());
        assert!(inbox.next_step_pending());
    }
}
