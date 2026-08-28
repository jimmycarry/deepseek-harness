//! Subagent registry and continuable-child lifecycle (`ctx.subagents`).

use async_trait::async_trait;
use dsh_agent::{Agent, AgentHandle, AgentRegistry, AgentStatus};
use dsh_cordis::{Context, Result, Service};
use dsh_llm::{ContentBlock, MessageSource, UserMessage};
use dsh_session::{
    Session, SessionEventData, SessionHeader, SessionId, SessionStore, TurnEndReason,
};
use dsh_session_projection::{subagent_identity_unit, SessionProjectionRegistry};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

mod delegation;

pub use delegation::{
    append_delegated_policy_overrides, bind_prompt, capture_delegated_policy_overrides,
    DelegatedPolicyOverrides, SUBAGENT_DELEGATION_CONTEXT,
};

/// Registry failures.
#[derive(Debug, Error)]
pub enum SubagentError {
    /// Duplicate provider name.
    #[error("duplicate subagent provider \"{0}\"")]
    DuplicateProvider(String),
    /// Named provider is not registered.
    #[error("no subagent provider \"{0}\"")]
    NoProvider(String),
}

/// One finished child run.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    /// Joined assistant text.
    pub output: String,
    /// Child session id.
    pub id: SessionId,
    /// Why the child stopped.
    pub stop_reason: String,
}

/// What `start` needs from the parent.
pub struct SubagentStartRequest {
    /// Human-facing label.
    pub label: String,
    /// Child prompt.
    pub prompt: String,
    /// Parent session id.
    pub parent_id: SessionId,
    /// Optional seed events (fork).
    pub seed: Option<Vec<dsh_session::SessionEvent>>,
}

/// One registered backend.
#[async_trait]
pub trait SubagentProvider: Send + Sync {
    /// Registry name (`spawn`, `fork`).
    fn name(&self) -> &str;
    /// Whether the child inherits completed parent turns.
    fn inherits_parent_context(&self) -> bool;
    /// Whether this provider can establish continuable background children.
    /// Fork children stay one-shot: a continuable child's `report` contribution
    /// would precede the inherited history a fork exists to reuse.
    fn supports_continuable(&self) -> bool {
        false
    }
    /// Whether `start` can honor a structured-output schema.
    ///
    /// Spawn and fork advertise this; Ralph requires it so each round can
    /// return a validated report object instead of free text.
    fn supports_output_schema(&self) -> bool {
        false
    }
    /// Run one one-shot child.
    async fn start(
        &self,
        request: SubagentStartRequest,
    ) -> std::result::Result<SubagentResult, SubagentError>;
}

/// The current `subagent/descriptor` payload format version.
pub const SUBAGENT_DESCRIPTOR_VERSION: u64 = 2;

/// Identities returned once a continuable child accepted its initial prompt.
#[derive(Debug, Clone)]
pub struct ContinuableStart {
    /// The durable child session id, stable across activations.
    pub child_id: SessionId,
    /// The accepted initial prompt's message id.
    pub message_id: String,
}

/// One durable child row projected from the session catalog.
#[derive(Debug, Clone)]
pub struct SubagentListEntry {
    /// Durable child session id.
    pub id: SessionId,
    /// Creation label from the descriptor.
    pub label: String,
    /// Descriptor mode (`one-shot` or `continuable`).
    pub mode: String,
    /// Durable direct-parent session id.
    pub parent: SessionId,
    /// Depth below the listing agent (1 for a direct child).
    pub depth: u32,
}

/// One resident continuable child epoch: the retained live handle plus the
/// durable identities settlement delivery needs after disposal.
struct Activation {
    child_id: SessionId,
    parent_id: SessionId,
    handle: AgentHandle,
}

/// `ctx.subagents`.
#[derive(Default)]
pub struct SubagentRuntime {
    providers: Mutex<HashMap<String, Arc<dyn SubagentProvider>>>,
    results: Mutex<Vec<String>>,
    /// Host context captured at install; continuation methods resolve
    /// `ctx.sessions` and `ctx.agents` through it.
    ctx: Mutex<Option<Context>>,
    /// Resident continuable children in creation order.
    activations: Mutex<Vec<Activation>>,
}

impl SubagentRuntime {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide `ctx.subagents`.
    pub fn install(ctx: &Context) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self::new());
        *runtime.ctx.lock().expect("subagents ctx") = Some(ctx.clone());
        if let Some(projections) = ctx.get::<SessionProjectionRegistry>() {
            projections
                .register(subagent_identity_unit())
                .map_err(|error| dsh_cordis::CordisError::Validation(error))?;
        }
        ctx.provide(Arc::clone(&runtime))?;
        bind_prompt(ctx)?;
        Ok(runtime)
    }

    /// Register a provider. Duplicate names fail loud.
    pub fn register_provider(
        &self,
        provider: Arc<dyn SubagentProvider>,
    ) -> std::result::Result<(), SubagentError> {
        let name = provider.name().to_string();
        let mut map = self.providers.lock().expect("subagents");
        if map.contains_key(&name) {
            return Err(SubagentError::DuplicateProvider(name));
        }
        map.insert(name, provider);
        Ok(())
    }

    /// Provider names in insertion order is not guaranteed; sorted for tests.
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .providers
            .lock()
            .expect("subagents")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Look up a provider.
    pub fn get_provider(&self, name: &str) -> Option<Arc<dyn SubagentProvider>> {
        self.providers.lock().expect("subagents").get(name).cloned()
    }

    /// Start a one-shot child on `name`.
    pub async fn start(
        &self,
        name: &str,
        request: SubagentStartRequest,
    ) -> std::result::Result<SubagentResult, SubagentError> {
        let provider = self
            .get_provider(name)
            .ok_or_else(|| SubagentError::NoProvider(name.into()))?;
        let parent_id = request.parent_id.clone();
        let result = provider.start(request).await?;
        self.emit_end(name, &result, &parent_id, true);
        self.results
            .lock()
            .expect("subagents")
            .push(result.output.clone());
        Ok(result)
    }

    /// Publish `subagent/end` for an in-process child that has settled.
    fn emit_end(
        &self,
        provider: &str,
        result: &SubagentResult,
        parent_id: &SessionId,
        local: bool,
    ) {
        let Some(ctx) = self.ctx.lock().ok().and_then(|guard| guard.clone()) else {
            return;
        };
        let mut payload = serde_json::json!({
            "id": result.id.as_str(),
            "provider": provider,
            "local": local,
            "stopReason": result.stop_reason,
            "parentSessionId": parent_id.as_str(),
        });
        if !result.output.is_empty() {
            payload["lastAssistantMessage"] = serde_json::json!([
                { "type": "text", "text": result.output }
            ]);
        }
        ctx.emit("subagent/end", payload);
    }

    /// Finished child texts in record order.
    pub fn results(&self) -> Vec<String> {
        self.results.lock().expect("subagents").clone()
    }

    /// Host services captured at install, or a loud continuation failure.
    fn host(&self) -> std::result::Result<(Arc<SessionStore>, Arc<AgentRegistry>), String> {
        let ctx = self
            .ctx
            .lock()
            .expect("subagents ctx")
            .clone()
            .ok_or_else(|| {
                "continuable subagents require an installed subagent service".to_string()
            })?;
        let sessions = ctx
            .get::<SessionStore>()
            .ok_or_else(|| "continuable subagents require ctx.sessions".to_string())?;
        let agents = ctx
            .get::<AgentRegistry>()
            .ok_or_else(|| "continuable subagents require ctx.agents".to_string())?;
        Ok((sessions, agents))
    }

    /// Start one continuable background child: reserve its durable identity,
    /// persist the descriptor, materialize the child agent, and submit the
    /// initial prompt. Returns at inbox acceptance; [`Self::run_pending`]
    /// later drives the accepted turn.
    pub fn start_continuable(
        &self,
        provider: &str,
        label: &str,
        prompt: Vec<ContentBlock>,
        parent: &Arc<dyn Agent>,
    ) -> std::result::Result<ContinuableStart, String> {
        let registered = self
            .get_provider(provider)
            .ok_or_else(|| SubagentError::NoProvider(provider.into()).to_string())?;
        if !registered.supports_continuable() {
            return Err(format!(
                "tool-subagent: provider \"{provider}\" does not support `backgroundMode: continuable`"
            ));
        }
        // Snapshot before any child session exists: a later parent switch
        // belongs to the parent's future, not to this child.
        let inherited = {
            let ctx = self
                .ctx
                .lock()
                .expect("subagents ctx")
                .clone()
                .ok_or_else(|| {
                    "continuable subagents require an installed subagent service".to_string()
                })?;
            capture_delegated_policy_overrides(&ctx, Some(parent.session().as_ref()))
        };
        let (sessions, agents) = self.host()?;
        let parent_header = parent.session().header().clone();
        let header = SessionHeader::for_subagent_child(Some(&parent_header), parent.id().clone());
        let child_id = header.id.clone();
        let child = sessions.publish(Session::with_header(header));
        child
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": SUBAGENT_DESCRIPTOR_VERSION,
                        "mode": "continuable",
                        "provider": provider,
                        "label": label,
                    }),
                },
                None,
            )
            .map_err(|error| error.to_string())?;
        append_delegated_policy_overrides(child.as_ref(), &inherited)
            .map_err(|error| error.to_string())?;
        let handle = agents
            .create(Arc::clone(&child))
            .map_err(|error| error.to_string())?;
        let message = UserMessage::from_parts(prompt, MessageSource::User);
        let message_id = message.id.clone();
        handle.agent.followup(message);
        self.activations
            .lock()
            .expect("activations")
            .push(Activation {
                child_id: child_id.clone(),
                parent_id: parent.id().clone(),
                handle,
            });
        Ok(ContinuableStart {
            child_id,
            message_id,
        })
    }

    /// Deliver one later message to a known continuable child as its next
    /// turn. A resident child is woken directly; an absent one is resumed
    /// from the session catalog after its descriptor authorizes continuation.
    pub fn followup(
        &self,
        parent: &Arc<dyn Agent>,
        child_id: &SessionId,
        content: Vec<ContentBlock>,
        source: MessageSource,
    ) -> std::result::Result<String, String> {
        let (sessions, agents) = self.host()?;
        if !agents
            .get(parent.id())
            .is_some_and(|live| Arc::ptr_eq(&live, parent))
        {
            return Err(format!(
                "subagent \"{child_id}\" delivery requires the exact live parent agent"
            ));
        }
        let Some(child) = sessions.get(child_id) else {
            return Err(format!("subagent \"{child_id}\" is unavailable"));
        };
        if child.header().parent_session.as_ref() != Some(parent.id()) {
            return Err(format!(
                "subagent \"{child_id}\" belongs to another parent session"
            ));
        }
        if fold_descriptor(&child).map(|descriptor| descriptor.mode) != Some("continuable".into()) {
            return Err(format!(
                "subagent \"{child_id}\" has no supported continuation state and cannot be resumed; \
do not retry send_message with this id"
            ));
        }
        let message = UserMessage::from_parts(content, source);
        let message_id = message.id.clone();
        let resident = {
            let activations = self.activations.lock().expect("activations");
            activations
                .iter()
                .find(|activation| &activation.child_id == child_id)
                .map(|activation| Arc::clone(&activation.handle.agent))
        };
        match resident {
            Some(agent) => agent.followup(message),
            None => {
                let handle = agents
                    .resume(Arc::clone(&child))
                    .map_err(|error| error.to_string())?;
                handle.agent.followup(message);
                self.activations
                    .lock()
                    .expect("activations")
                    .push(Activation {
                        child_id: child_id.clone(),
                        parent_id: parent.id().clone(),
                        handle,
                    });
            }
        }
        Ok(message_id)
    }

    /// Interrupt one live continuable child's current turn. Authorization
    /// walks the target's durable parent chain to the caller; an absent
    /// target is an accepted no-op.
    pub fn interrupt(
        &self,
        target: &SessionId,
        caller: &Arc<dyn Agent>,
    ) -> std::result::Result<(), String> {
        let (sessions, agents) = self.host()?;
        if !agents
            .get(caller.id())
            .is_some_and(|live| Arc::ptr_eq(&live, caller))
        {
            return Err(format!(
                "interrupting \"{target}\" requires the exact live ancestor agent"
            ));
        }
        if caller.id() == target {
            return Err(format!("agent \"{}\" cannot interrupt itself", caller.id()));
        }
        let resident = {
            let activations = self.activations.lock().expect("activations");
            activations
                .iter()
                .find(|activation| &activation.child_id == target)
                .map(|activation| Arc::clone(&activation.handle.agent))
        };
        let Some(agent) = resident else {
            return Ok(());
        };
        if !is_descendant_of(&sessions, target, caller.id()) {
            return Err(format!(
                "subagent \"{target}\" is not a live descendant of agent \"{}\"",
                caller.id()
            ));
        }
        agent.cancel(dsh_agent::AgentCancelCause {
            kind: "parent".into(),
        });
        Ok(())
    }

    /// Deliver explicitly selected content from one resident continuable
    /// child to its durable direct parent. `next-step` steers the parent;
    /// `quiet` injects without waking.
    pub fn report_from(
        &self,
        child: &Arc<dyn Agent>,
        content: Vec<ContentBlock>,
        delivery: &str,
    ) -> std::result::Result<String, String> {
        let (_sessions, agents) = self.host()?;
        let resident = {
            let activations = self.activations.lock().expect("activations");
            activations
                .iter()
                .find(|activation| &activation.child_id == child.id())
                .map(|activation| Arc::clone(&activation.handle.agent))
        };
        if !resident.is_some_and(|agent| Arc::ptr_eq(&agent, child)) {
            return Err(format!(
                "agent \"{}\" is not a live continuable subagent and cannot report",
                child.id()
            ));
        }
        let parent = child
            .session()
            .header()
            .parent_session
            .as_ref()
            .and_then(|parent_id| agents.get(parent_id))
            .ok_or_else(|| "direct parent is not live; report was not delivered".to_string())?;
        let mut framed = vec![ContentBlock::text(format!(
            "Background subagent {} reported:",
            child.id()
        ))];
        framed.extend(content);
        let message =
            UserMessage::from_parts(framed, MessageSource::subagent_report(child.id().as_str()));
        let message_id = message.id.clone();
        if delivery == "next-step" {
            parent.steer(message);
        } else {
            parent.inject(message);
        }
        Ok(message_id)
    }

    /// Whether `agent_id` is a resident continuable child. The child-scoped
    /// `report` tool keys its visibility on this.
    pub fn is_resident_continuable(&self, agent_id: &str) -> bool {
        self.activations
            .lock()
            .expect("activations")
            .iter()
            .any(|activation| activation.child_id.as_str() == agent_id)
    }

    /// Direct children of `parent_id` from the durable session catalog, in
    /// creation order, each carrying its descriptor mode and label.
    ///
    /// # Errors
    /// `sessionProjections` is not mounted.
    pub fn list_children(
        &self,
        parent_id: &SessionId,
    ) -> std::result::Result<Vec<SubagentListEntry>, String> {
        let (sessions, _agents) = self.host().map_err(|error| error.to_string())?;
        children_of(self, &sessions, parent_id, 1)
    }

    /// The complete tree below `parent_id` in stable pre-order, each entry
    /// annotated with its durable direct parent and depth.
    ///
    /// # Errors
    /// `sessionProjections` is not mounted.
    pub fn list_descendants(
        &self,
        parent_id: &SessionId,
    ) -> std::result::Result<Vec<SubagentListEntry>, String> {
        let (sessions, _agents) = self.host().map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        collect_descendants(self, &sessions, parent_id, 1, &mut entries)?;
        Ok(entries)
    }

    /// Live status of one durable child: `running` for an active driver,
    /// `idle` for a resident agent between turns, `ready` when only the
    /// session remains.
    pub fn status_of(&self, child_id: &SessionId) -> &'static str {
        let Ok((_sessions, agents)) = self.host() else {
            return "ready";
        };
        match agents.get(child_id) {
            None => "ready",
            Some(agent) => {
                if agent.status() == AgentStatus::Running {
                    "running"
                } else {
                    "idle"
                }
            }
        }
    }

    /// Drive resident continuable children until none has pending inbox work,
    /// settling each quiescent childless activation with a settlement notice
    /// to its parent. Returns whether any child turn ran.
    pub async fn run_pending(&self) -> bool {
        let mut ran = false;
        loop {
            let next = {
                let activations = self.activations.lock().expect("activations");
                activations
                    .iter()
                    .find(|activation| activation.handle.agent.inbox().has_pending())
                    .map(|activation| Arc::clone(&activation.handle.agent))
            };
            if let Some(agent) = next {
                ran = true;
                let _ = agent.run().await;
                continue;
            }
            if !self.settle_quiescent() {
                return ran;
            }
        }
    }

    /// Dispose every quiescent childless activation and deliver its
    /// settlement notice. Returns whether any activation settled.
    fn settle_quiescent(&self) -> bool {
        let Ok((_sessions, agents)) = self.host() else {
            return false;
        };
        let settled = {
            let mut activations = self.activations.lock().expect("activations");
            let owners: Vec<SessionId> = activations
                .iter()
                .map(|activation| activation.parent_id.clone())
                .collect();
            let mut settled = Vec::new();
            let mut index = 0;
            while index < activations.len() {
                let activation = &activations[index];
                let quiescent = activation.handle.agent.status() != AgentStatus::Running
                    && !activation.handle.agent.inbox().has_pending();
                let childless = !owners.iter().any(|owner| owner == &activation.child_id);
                if quiescent && childless {
                    settled.push(activations.remove(index));
                } else {
                    index += 1;
                }
            }
            settled
        };
        let any = !settled.is_empty();
        for activation in settled {
            let notice = settlement_message(&activation);
            let session = activation.handle.agent.session();
            let reason = session
                .events()
                .into_iter()
                .rev()
                .find_map(|event| match event.data {
                    SessionEventData::TurnEnd { reason, .. } => Some(reason),
                    _ => None,
                })
                .unwrap_or(TurnEndReason::Completed);
            let provider = fold_descriptor(session.as_ref())
                .map(|descriptor| descriptor.provider)
                .unwrap_or_else(|| "spawn".into());
            let output = session.last_assistant_text().unwrap_or_default();
            self.emit_end(
                &provider,
                &SubagentResult {
                    output,
                    id: activation.child_id.clone(),
                    stop_reason: stop_reason_name(&reason).into(),
                },
                &activation.parent_id,
                true,
            );
            activation.handle.dispose();
            if let Some(parent) = agents.get(&activation.parent_id) {
                if parent.status() == AgentStatus::Idle {
                    parent.followup(notice);
                } else {
                    parent.steer(notice);
                }
            }
        }
        any
    }
}

/// Parsed `subagent/descriptor` fields continuation consults.
struct Descriptor {
    mode: String,
    #[allow(dead_code)]
    label: String,
    provider: String,
}

/// Fold a child log to its first `subagent/descriptor` payload, requiring the
/// current [`SUBAGENT_DESCRIPTOR_VERSION`].
fn fold_descriptor(session: &Session) -> Option<Descriptor> {
    session.events().into_iter().find_map(|event| {
        let SessionEventData::Extension { type_name, data } = &event.data else {
            return None;
        };
        if type_name != "subagent/descriptor" {
            return None;
        }
        if data.get("version").and_then(Value::as_u64) != Some(SUBAGENT_DESCRIPTOR_VERSION) {
            return None;
        }
        Some(Descriptor {
            mode: data
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            label: data
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            provider: data
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    })
}

const PROJECTIONS_UNAVAILABLE: &str = "listing subagents requires the sessionProjections registry (load @deepseek-ai/dsh-session-projection)";

/// Direct catalog children of `parent_id`, sorted by creation time then id.
fn children_of(
    runtime: &SubagentRuntime,
    sessions: &SessionStore,
    parent_id: &SessionId,
    depth: u32,
) -> std::result::Result<Vec<SubagentListEntry>, String> {
    let projections = runtime
        .ctx
        .lock()
        .expect("subagents ctx")
        .as_ref()
        .and_then(|ctx| ctx.get::<SessionProjectionRegistry>())
        .ok_or_else(|| PROJECTIONS_UNAVAILABLE.to_string())?;
    let mut children: Vec<Arc<Session>> = sessions
        .live()
        .into_iter()
        .filter(|session| session.header().parent_session.as_ref() == Some(parent_id))
        .collect();
    children.sort_by(|a, b| {
        (a.header().created_at, a.id().as_str()).cmp(&(b.header().created_at, b.id().as_str()))
    });
    Ok(children
        .into_iter()
        .filter_map(|session| {
            let identity = projections
                .snapshot(&session)
                .values
                .get("subagent")
                .cloned()?;
            if identity.is_null() {
                return None;
            }
            Some(SubagentListEntry {
                id: session.id().clone(),
                label: identity
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                mode: identity
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parent: parent_id.clone(),
                depth,
            })
        })
        .collect())
}

/// Append the subtree below `parent_id` in stable pre-order.
fn collect_descendants(
    runtime: &SubagentRuntime,
    sessions: &SessionStore,
    parent_id: &SessionId,
    depth: u32,
    entries: &mut Vec<SubagentListEntry>,
) -> std::result::Result<(), String> {
    for child in children_of(runtime, sessions, parent_id, depth)? {
        let child_id = child.id.clone();
        entries.push(child);
        collect_descendants(runtime, sessions, &child_id, depth + 1, entries)?;
    }
    Ok(())
}

/// Whether `target`'s durable parent chain contains `ancestor`.
fn is_descendant_of(sessions: &SessionStore, target: &SessionId, ancestor: &SessionId) -> bool {
    let mut current = sessions
        .get(target)
        .and_then(|session| session.header().parent_session.clone());
    while let Some(parent) = current {
        if &parent == ancestor {
            return true;
        }
        current = sessions
            .get(&parent)
            .and_then(|session| session.header().parent_session.clone());
    }
    false
}

/// TypeScript `stopReason` string for one turn-end reason.
fn stop_reason_name(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::Completed => "completed",
        TurnEndReason::MaxTokens => "max-tokens",
        TurnEndReason::Blocked => "blocked",
        TurnEndReason::Aborted { .. } => "aborted",
        TurnEndReason::Error { .. } => "error",
        TurnEndReason::Interrupted => "interrupted",
    }
}

/// One line telling a parent that a background child is finished and why.
fn settlement_summary(child_id: &SessionId, reason: &TurnEndReason) -> String {
    let subject = format!("Background subagent {child_id}");
    match reason {
        TurnEndReason::Completed => {
            format!("{subject} finished and will do no further work unless you send it more.")
        }
        TurnEndReason::Aborted { .. } => format!("{subject} was stopped before it finished."),
        TurnEndReason::MaxTokens => format!("{subject} ran out of room before it finished."),
        // A pre-step rejection discarded input the child had claimed, so the
        // parent must not treat the task as done.
        TurnEndReason::Blocked => format!("{subject} declined the task."),
        TurnEndReason::Error { .. } => format!("{subject} failed before it finished."),
        TurnEndReason::Interrupted => {
            format!("{subject} ended abnormally (interrupted) before it finished.")
        }
    }
}

/// Build the settlement notice from the child's final turn and closing text.
fn settlement_message(activation: &Activation) -> UserMessage {
    let session = activation.handle.agent.session();
    let reason = session
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match event.data {
            SessionEventData::TurnEnd { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or(TurnEndReason::Completed);
    let summary = settlement_summary(&activation.child_id, &reason);
    let mut content = vec![ContentBlock::text(summary.clone())];
    match session
        .last_assistant_text()
        .filter(|text| !text.is_empty())
    {
        None => content.push(ContentBlock::text("It left no closing message.")),
        Some(text) => {
            content.push(ContentBlock::text("Its closing message:"));
            content.push(ContentBlock::text(text));
        }
    }
    UserMessage::from_parts(
        content,
        MessageSource::subagent_settled(summary, activation.child_id.as_str()),
    )
}

impl Service for SubagentRuntime {
    const KEY: &'static str = "subagents";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentStatus, Inbox, InboxTarget,
    };
    use dsh_sandbox::SandboxMode;
    use dsh_sandbox_policy::set_sandbox_mode;
    use dsh_session::session_id;
    use dsh_user_approval::{effective_approval_policy, ApprovalPolicy};

    struct Fake;

    #[async_trait]
    impl SubagentProvider for Fake {
        fn name(&self) -> &str {
            "spawn"
        }
        fn inherits_parent_context(&self) -> bool {
            false
        }
        async fn start(
            &self,
            request: SubagentStartRequest,
        ) -> std::result::Result<SubagentResult, SubagentError> {
            Ok(SubagentResult {
                output: request.prompt,
                id: session_id("child"),
                stop_reason: "completed".into(),
            })
        }
    }

    struct ContinuableFake;

    #[async_trait]
    impl SubagentProvider for ContinuableFake {
        fn name(&self) -> &str {
            "spawn"
        }
        fn inherits_parent_context(&self) -> bool {
            false
        }
        fn supports_continuable(&self) -> bool {
            true
        }
        async fn start(
            &self,
            request: SubagentStartRequest,
        ) -> std::result::Result<SubagentResult, SubagentError> {
            Ok(SubagentResult {
                output: request.prompt,
                id: session_id("child"),
                stop_reason: "completed".into(),
            })
        }
    }

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn id(&self) -> &SessionId {
            self.session.id()
        }
        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.session)
        }
        fn inbox(&self) -> Arc<Inbox> {
            Arc::clone(&self.inbox)
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn send(&self, _: UserMessage, _: InboxTarget, _: bool) {}
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> std::result::Result<(), AgentError> {
            Ok(())
        }
    }

    struct StubFactory;

    impl AgentFactory for StubFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(StubAgent {
                inbox: Arc::new(Inbox::for_session(Arc::clone(&session))),
                session,
            })
        }
    }

    fn continuable_host(
        with_approval: bool,
    ) -> (
        Context,
        Arc<SubagentRuntime>,
        dsh_agent::AgentHandle,
        Arc<SessionStore>,
    ) {
        let ctx = Context::new();
        let store = Arc::new(SessionStore::new());
        ctx.provide(Arc::clone(&store)).unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        ctx.provide(Arc::new(agents)).unwrap();
        dsh_sandbox_policy::install(
            &ctx,
            Some(&serde_json::json!({
                "mode": "workspace-write",
                "workspaceRoot": std::env::temp_dir().to_string_lossy()
            })),
        )
        .unwrap();
        if with_approval {
            dsh_user_approval::install(&ctx, None).unwrap();
        }
        let runtime = SubagentRuntime::install(&ctx).unwrap();
        runtime
            .register_provider(Arc::new(ContinuableFake))
            .unwrap();
        let parent_session = store.create(session_id("parent"));
        let parent = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(parent_session)
            .unwrap();
        (ctx, runtime, parent, store)
    }

    #[tokio::test]
    async fn start_records_result() {
        let runtime = SubagentRuntime::new();
        runtime.register_provider(Arc::new(Fake)).unwrap();
        let result = runtime
            .start(
                "spawn",
                SubagentStartRequest {
                    label: "t".into(),
                    prompt: "ping".into(),
                    parent_id: session_id("parent"),
                    seed: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.output, "ping");
        assert_eq!(runtime.results(), vec!["ping".to_string()]);
    }

    #[test]
    fn start_continuable_seeds_parent_sandbox_and_pins_approval() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::DangerFullAccess)
            .unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
            )
            .unwrap();
        let child = store.get(&started.child_id).unwrap();
        let events = child.events();
        let policy: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.data,
                    SessionEventData::SandboxMode { .. } | SessionEventData::ApprovalPolicy { .. }
                )
            })
            .collect();
        assert_eq!(policy.len(), 2);
        match &policy[0].data {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "danger-full-access");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match &policy[1].data {
            SessionEventData::ApprovalPolicy { policy, source } => {
                assert_eq!(policy, "never");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            effective_approval_policy(&events),
            Some(ApprovalPolicy::Never)
        );
    }

    #[test]
    fn start_continuable_skips_unswitched_sandbox_and_still_pins_approval() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
            )
            .unwrap();
        let child = store.get(&started.child_id).unwrap();
        let events = child.events();
        assert!(events
            .iter()
            .all(|event| !matches!(event.data, SessionEventData::SandboxMode { .. })));
        match &events
            .iter()
            .find(|event| matches!(event.data, SessionEventData::ApprovalPolicy { .. }))
            .unwrap()
            .data
        {
            SessionEventData::ApprovalPolicy { policy, source } => {
                assert_eq!(policy, "never");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn start_continuable_omits_approval_when_the_service_is_absent() {
        let (_ctx, runtime, parent, store) = continuable_host(false);
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
            )
            .unwrap();
        let child = store.get(&started.child_id).unwrap();
        assert!(child.events().iter().all(|event| {
            !matches!(
                event.data,
                SessionEventData::SandboxMode { .. } | SessionEventData::ApprovalPolicy { .. }
            )
        }));
    }

    #[test]
    fn start_continuable_keeps_the_captured_mode_after_a_later_parent_switch() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::ReadOnly).unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
            )
            .unwrap();
        set_sandbox_mode(
            parent.agent.session().as_ref(),
            SandboxMode::DangerFullAccess,
        )
        .unwrap();
        let child = store.get(&started.child_id).unwrap();
        assert_eq!(
            dsh_sandbox_policy::effective_sandbox_mode(&child.events()),
            Some(SandboxMode::ReadOnly)
        );
        assert_eq!(
            dsh_sandbox_policy::effective_sandbox_mode(&parent.agent.session().events()),
            Some(SandboxMode::DangerFullAccess)
        );
    }

    #[tokio::test]
    async fn followup_resume_does_not_reseed_delegation_policy() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::ReadOnly).unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
            )
            .unwrap();
        runtime.run_pending().await;
        runtime
            .followup(
                &parent.agent,
                &started.child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .unwrap();
        let child = store.get(&started.child_id).unwrap();
        let sandbox: Vec<_> = child
            .events()
            .into_iter()
            .filter(|event| matches!(event.data, SessionEventData::SandboxMode { .. }))
            .collect();
        let approval: Vec<_> = child
            .events()
            .into_iter()
            .filter(|event| matches!(event.data, SessionEventData::ApprovalPolicy { .. }))
            .collect();
        assert_eq!(sandbox.len(), 1);
        assert_eq!(approval.len(), 1);
        match &sandbox[0].data {
            SessionEventData::SandboxMode { source, .. } => {
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
