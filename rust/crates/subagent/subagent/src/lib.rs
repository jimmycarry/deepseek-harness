//! Subagent registry and continuable-child lifecycle (`ctx.subagents`).

use async_trait::async_trait;
use dsh_agent::{Agent, AgentHandle, AgentRegistry, AgentStatus};
use dsh_cordis::{Context, Result, Service};
use dsh_llm::{ContentBlock, MessageSource, UserMessage};
use dsh_session::{
    Session, SessionEventData, SessionHeader, SessionId, SessionStore, TurnEndReason,
};
use dsh_session_persistence::PersistenceRuntime;
use dsh_session_projection::{subagent_identity_unit, SessionProjectionRegistry};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

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

/// One listing row: a classified child, or a per-child diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentListEntry {
    /// A child whose `subagent` projection served an identity.
    Child {
        /// Durable child session id.
        id: SessionId,
        /// Creation label from the descriptor.
        label: String,
        /// Descriptor mode (`one-shot` or `continuable`).
        mode: String,
        /// `running` when the record is live in `ctx.sessions`; `inactive` when
        /// it exists only in persistence.
        activity: String,
        /// Whether a direct descendant has durable `origin: "subagent"`.
        has_children: bool,
        /// Durable direct-parent session id.
        parent: SessionId,
        /// Depth below the listing agent (1 for a direct child).
        depth: u32,
    },
    /// A settled candidate the projection fold could not classify, or a
    /// failed cold read.
    Diagnostic {
        /// The candidate's session id.
        id: SessionId,
        /// `corrupt` for a settled fold with no identity or a lifecycle
        /// mismatch; `unavailable` for a failed cold inspect.
        reason: String,
        /// Durable direct-parent session id.
        parent: SessionId,
        /// Depth below the listing agent (1 for a direct child).
        depth: u32,
    },
}

/// One resident continuable child epoch: the retained live handle plus the
/// durable identities settlement delivery needs after disposal.
struct Activation {
    child_id: SessionId,
    parent_id: SessionId,
    handle: AgentHandle,
    run_id: String,
    provider: String,
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
        let run_id = Uuid::new_v4().to_string();
        self.emit_start(name, &result.id, &run_id, true);
        self.emit_end(name, &result, &parent_id, true, &run_id);
        self.results
            .lock()
            .expect("subagents")
            .push(result.output.clone());
        Ok(result)
    }

    /// Publish `subagent/start` after a child identity is known.
    fn emit_start(&self, provider: &str, child_id: &SessionId, run_id: &str, local: bool) {
        let Some(ctx) = self.ctx.lock().ok().and_then(|guard| guard.clone()) else {
            return;
        };
        ctx.emit(
            "subagent/start",
            serde_json::json!({
                "runId": run_id,
                "provider": provider,
                "id": child_id.as_str(),
                "local": local,
            }),
        );
    }

    /// Publish `subagent/end` for an in-process child that has settled.
    fn emit_end(
        &self,
        provider: &str,
        result: &SubagentResult,
        parent_id: &SessionId,
        local: bool,
        run_id: &str,
    ) {
        let Some(ctx) = self.ctx.lock().ok().and_then(|guard| guard.clone()) else {
            return;
        };
        let mut payload = serde_json::json!({
            "runId": run_id,
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
    ///
    /// Requires `ctx.sessionPersistence`. Absence fails with the TypeScript
    /// persistence sentence before any child session is published. A
    /// caller-reserved `child_id` is rejected when the live registries or
    /// configured persistence already own it.
    pub async fn start_continuable(
        &self,
        provider: &str,
        label: &str,
        prompt: Vec<ContentBlock>,
        parent: &Arc<dyn Agent>,
        child_id: Option<SessionId>,
    ) -> std::result::Result<ContinuableStart, String> {
        let registered = self
            .get_provider(provider)
            .ok_or_else(|| SubagentError::NoProvider(provider.into()).to_string())?;
        if !registered.supports_continuable() {
            return Err(format!(
                "tool-subagent: provider \"{provider}\" does not support `backgroundMode: continuable`"
            ));
        }
        self.require_persistence()?;
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
        let reserved = child_id.is_some();
        let parent_header = parent.session().header().clone();
        let header = match child_id {
            Some(id) => {
                SessionHeader::for_subagent_child_id(Some(&parent_header), parent.id().clone(), id)
            }
            None => SessionHeader::for_subagent_child(Some(&parent_header), parent.id().clone()),
        };
        let child_id = header.id.clone();
        assert_child_id_available(&sessions, &agents, &child_id)?;
        if reserved {
            let ids = self
                .require_persistence()?
                .list_ids()
                .await
                .map_err(|error| error.to_string())?;
            if ids.iter().any(|id| id == &child_id) {
                return Err(already_exists(&child_id));
            }
        }
        assert_child_id_available(&sessions, &agents, &child_id)?;
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
        let run_id = Uuid::new_v4().to_string();
        self.emit_start(provider, &child_id, &run_id, true);
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
                run_id,
                provider: provider.to_string(),
            });
        Ok(ContinuableStart {
            child_id,
            message_id,
        })
    }

    /// Deliver one later message to a known continuable child as its next
    /// turn. A resident child is woken directly; an absent Activation is
    /// resumed from the live catalog, or cold-loaded from
    /// `ctx.sessionPersistence` when the catalog misses.
    pub async fn followup(
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
        let child = if let Some(live) = sessions.get(child_id) {
            live
        } else {
            let loaded = self.cold_load_child(child_id).await?;
            authorize_followup(parent, child_id, loaded.header(), &loaded)?;
            sessions.publish(loaded)
        };
        authorize_followup(parent, child_id, child.header(), child.as_ref())?;
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
                    .map_err(|_| unavailable(child_id))?;
                let provider = fold_descriptor(child.as_ref())
                    .map(|descriptor| descriptor.provider)
                    .unwrap_or_else(|| "spawn".into());
                let run_id = Uuid::new_v4().to_string();
                self.emit_start(&provider, child_id, &run_id, true);
                handle.agent.followup(message);
                self.activations
                    .lock()
                    .expect("activations")
                    .push(Activation {
                        child_id: child_id.clone(),
                        parent_id: parent.id().clone(),
                        handle,
                        run_id,
                        provider,
                    });
            }
        }
        Ok(message_id)
    }

    /// Load one persisted child. Catalog misses require a persistence backend.
    async fn cold_load_child(&self, child_id: &SessionId) -> std::result::Result<Session, String> {
        let persistence = self.require_persistence()?;
        persistence
            .load(child_id)
            .await
            .map_err(|_| unavailable(child_id))
    }

    /// Resolve `ctx.sessionPersistence`, or the TypeScript persistence sentence.
    fn require_persistence(&self) -> std::result::Result<Arc<PersistenceRuntime>, String> {
        self.persistence()
            .ok_or_else(|| PERSISTENCE_REQUIRED.to_string())
    }

    /// Optional persistence captured on the install context.
    fn persistence(&self) -> Option<Arc<PersistenceRuntime>> {
        self.ctx.lock().ok().and_then(|guard| guard.as_ref()?.get())
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

    /// Direct children of `parent_id` from the live-preferred merge of the
    /// session store and optional persistence. Unreadable settled children
    /// become diagnostics; a live child without an identity yet is omitted.
    ///
    /// # Errors
    /// `sessionProjections` or `sessions` is not mounted, or persistence
    /// listing fails.
    pub async fn list_children(
        &self,
        parent_id: &SessionId,
    ) -> std::result::Result<Vec<SubagentListEntry>, String> {
        let (sessions, projections) = self.listing_host()?;
        let corpus = listing_corpus(self, &sessions).await?;
        let mut candidates: Vec<CorpusRecord> = corpus
            .values()
            .filter(|record| {
                record.header.origin.as_deref() == Some("subagent")
                    && record.header.parent_session.as_ref() == Some(parent_id)
            })
            .cloned()
            .collect();
        candidates.sort_by(compare_corpus_records);
        resolve_rows(self, &projections, &corpus, candidates, parent_id, 1).await
    }

    /// The complete tree below `parent_id` in stable pre-order. Ordinary
    /// sessions remain traversal nodes so a continuable child below one is
    /// still discovered. Each entry carries its durable direct parent and
    /// depth.
    ///
    /// # Errors
    /// `sessionProjections` or `sessions` is not mounted, or persistence
    /// listing fails.
    pub async fn list_descendants(
        &self,
        parent_id: &SessionId,
    ) -> std::result::Result<Vec<SubagentListEntry>, String> {
        let (sessions, projections) = self.listing_host()?;
        let corpus = listing_corpus(self, &sessions).await?;
        let parents = subagent_parent_ids(&corpus);
        let mut entries = Vec::new();
        for (record, parent, depth) in descendant_candidates(&corpus, parent_id) {
            let has_children = parents.contains(record.header.id.as_str());
            if let Some(row) =
                resolve_candidate(self, &projections, &record, parent, depth, has_children).await
            {
                entries.push(row);
            }
        }
        Ok(entries)
    }

    /// Session store and projection registry listing requires, or the
    /// TypeScript configuration sentences.
    fn listing_host(
        &self,
    ) -> std::result::Result<(Arc<SessionStore>, Arc<SessionProjectionRegistry>), String> {
        let ctx = self
            .ctx
            .lock()
            .expect("subagents ctx")
            .clone()
            .ok_or_else(|| PROJECTIONS_UNAVAILABLE.to_string())?;
        let projections = ctx
            .get::<SessionProjectionRegistry>()
            .ok_or_else(|| PROJECTIONS_UNAVAILABLE.to_string())?;
        let sessions = ctx
            .get::<SessionStore>()
            .ok_or_else(|| SESSION_STORE_UNAVAILABLE.to_string())?;
        Ok((sessions, projections))
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
            let provider = if activation.provider.is_empty() {
                fold_descriptor(session.as_ref())
                    .map(|descriptor| descriptor.provider)
                    .unwrap_or_else(|| "spawn".into())
            } else {
                activation.provider.clone()
            };
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
                &activation.run_id,
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

/// Fold a child log to its first own-suffix `subagent/descriptor` payload,
/// skipping the `seedLength` prefix so a fork seed's ancestor descriptor
/// cannot classify the child.
fn fold_descriptor(session: &Session) -> Option<Descriptor> {
    let skip = session.header().seed_length.unwrap_or(0) as usize;
    session.events().into_iter().skip(skip).find_map(|event| {
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
const SESSION_STORE_UNAVAILABLE: &str =
    "listing subagents requires the session store (load @deepseek-ai/dsh-session)";
const PERSISTENCE_REQUIRED: &str =
    "continuable subagents require session persistence (load a dsh-session-persistence backend)";
const DIAGNOSTIC_CORRUPT: &str = "corrupt";
const DIAGNOSTIC_UNAVAILABLE: &str = "unavailable";

fn unavailable(child_id: &SessionId) -> String {
    format!("subagent \"{child_id}\" is unavailable")
}

fn no_continuation(child_id: &SessionId) -> String {
    format!(
        "subagent \"{child_id}\" has no supported continuation state and cannot be resumed; \
do not retry send_message with this id"
    )
}

fn other_parent(child_id: &SessionId) -> String {
    format!("subagent \"{child_id}\" belongs to another parent session")
}

fn already_exists(child_id: &SessionId) -> String {
    format!("subagent \"{child_id}\" already exists")
}

/// Reject a child identity already owned by a live Agent or Session.
fn assert_child_id_available(
    sessions: &SessionStore,
    agents: &AgentRegistry,
    child_id: &SessionId,
) -> std::result::Result<(), String> {
    if agents.get(child_id).is_some() || sessions.get(child_id).is_some() {
        return Err(already_exists(child_id));
    }
    Ok(())
}

/// Authorize parent lineage and a continuable own-suffix descriptor.
fn authorize_followup(
    parent: &Arc<dyn Agent>,
    child_id: &SessionId,
    header: &SessionHeader,
    session: &Session,
) -> std::result::Result<(), String> {
    if header.parent_session.as_ref() != Some(parent.id()) {
        return Err(other_parent(child_id));
    }
    if fold_descriptor(session).map(|descriptor| descriptor.mode) != Some("continuable".into()) {
        return Err(no_continuation(child_id));
    }
    Ok(())
}

/// One live-preferred corpus record: a persisted header, overwritten by the
/// live session when the same id is in the catalog.
#[derive(Clone)]
struct CorpusRecord {
    header: SessionHeader,
    live: Option<Arc<Session>>,
}

/// Live-preferred merge of persisted headers and the live session store.
async fn listing_corpus(
    runtime: &SubagentRuntime,
    sessions: &SessionStore,
) -> std::result::Result<HashMap<String, CorpusRecord>, String> {
    let mut corpus = HashMap::new();
    if let Some(persistence) = runtime.persistence() {
        let headers = persistence
            .list_headers()
            .await
            .map_err(|error| error.to_string())?;
        for header in headers {
            corpus.insert(
                header.id.as_str().to_string(),
                CorpusRecord { header, live: None },
            );
        }
    }
    for session in sessions.live() {
        corpus.insert(
            session.id().as_str().to_string(),
            CorpusRecord {
                header: session.header().clone(),
                live: Some(session),
            },
        );
    }
    Ok(corpus)
}

/// Session ids that appear as `parentSession` of an `origin: "subagent"` record.
fn subagent_parent_ids(corpus: &HashMap<String, CorpusRecord>) -> HashSet<String> {
    corpus
        .values()
        .filter(|record| record.header.origin.as_deref() == Some("subagent"))
        .filter_map(|record| {
            record
                .header
                .parent_session
                .as_ref()
                .map(|parent| parent.as_str().to_string())
        })
        .collect()
}

/// Compare siblings by durable creation time, then id.
fn compare_corpus_records(a: &CorpusRecord, b: &CorpusRecord) -> std::cmp::Ordering {
    (a.header.created_at, a.header.id.as_str()).cmp(&(b.header.created_at, b.header.id.as_str()))
}

/// Immutable header fields that distinguish one lifecycle from another
/// under the same id.
fn same_lifecycle(meta: &SessionHeader, expected: &SessionHeader) -> bool {
    meta.version == expected.version
        && meta.id == expected.id
        && meta.created_at == expected.created_at
        && meta.cwd == expected.cwd
        && meta.parent_session == expected.parent_session
        && meta.seed_length == expected.seed_length
        && meta.delegation_depth == expected.delegation_depth
}

/// Fold every registered unit over `session`. A panic in any unit is
/// contained as a failed identity read.
fn projected_identity(
    projections: &SessionProjectionRegistry,
    session: &Session,
) -> std::result::Result<Option<Value>, ()> {
    match catch_unwind(AssertUnwindSafe(|| projections.snapshot(session))) {
        Ok(snapshot) => Ok(snapshot
            .values
            .get("subagent")
            .cloned()
            .filter(|value| !value.is_null())),
        Err(_) => Err(()),
    }
}

fn child_row(
    id: SessionId,
    identity: &Value,
    activity: &str,
    has_children: bool,
    parent: SessionId,
    depth: u32,
) -> SubagentListEntry {
    SubagentListEntry::Child {
        id,
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
        activity: activity.to_string(),
        has_children,
        parent,
        depth,
    }
}

fn diagnostic_row(id: SessionId, reason: &str, parent: SessionId, depth: u32) -> SubagentListEntry {
    SubagentListEntry::Diagnostic {
        id,
        reason: reason.to_string(),
        parent,
        depth,
    }
}

/// Classify one corpus candidate. Live null identity is omitted (creation
/// window); cold null identity, a lifecycle mismatch, or a fold panic is
/// `corrupt`; a failed cold inspect is `unavailable`.
async fn resolve_candidate(
    runtime: &SubagentRuntime,
    projections: &SessionProjectionRegistry,
    record: &CorpusRecord,
    parent: SessionId,
    depth: u32,
    has_children: bool,
) -> Option<SubagentListEntry> {
    let child_id = record.header.id.clone();
    if let Some(live) = &record.live {
        return match projected_identity(projections, live) {
            Err(()) => Some(diagnostic_row(child_id, DIAGNOSTIC_CORRUPT, parent, depth)),
            Ok(None) => None,
            Ok(Some(identity)) => Some(child_row(
                child_id,
                &identity,
                "running",
                has_children,
                parent,
                depth,
            )),
        };
    }
    let persistence = runtime.persistence()?;
    let inspected = match persistence.inspect(&child_id).await {
        Ok(view) => view,
        Err(_) => {
            return Some(diagnostic_row(
                child_id,
                DIAGNOSTIC_UNAVAILABLE,
                parent,
                depth,
            ));
        }
    };
    if !same_lifecycle(&inspected.meta, &record.header) {
        return Some(diagnostic_row(child_id, DIAGNOSTIC_CORRUPT, parent, depth));
    }
    let Ok(session) = inspected.into_session() else {
        return Some(diagnostic_row(child_id, DIAGNOSTIC_CORRUPT, parent, depth));
    };
    match projected_identity(projections, &session) {
        Err(()) | Ok(None) => Some(diagnostic_row(child_id, DIAGNOSTIC_CORRUPT, parent, depth)),
        Ok(Some(identity)) => Some(child_row(
            child_id,
            &identity,
            "inactive",
            has_children,
            parent,
            depth,
        )),
    }
}

/// Classify origin-filtered siblings of one parent, in creation order.
async fn resolve_rows(
    runtime: &SubagentRuntime,
    projections: &SessionProjectionRegistry,
    corpus: &HashMap<String, CorpusRecord>,
    candidates: Vec<CorpusRecord>,
    parent_id: &SessionId,
    depth: u32,
) -> std::result::Result<Vec<SubagentListEntry>, String> {
    let parents = subagent_parent_ids(corpus);
    let mut rows = Vec::new();
    for record in candidates {
        let has_children = parents.contains(record.header.id.as_str());
        if let Some(row) = resolve_candidate(
            runtime,
            projections,
            &record,
            parent_id.clone(),
            depth,
            has_children,
        )
        .await
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Origin-classified descendants in stable pre-order. Non-subagent sessions
/// stay traversal nodes so a child below one is still discovered.
fn descendant_candidates(
    corpus: &HashMap<String, CorpusRecord>,
    root: &SessionId,
) -> Vec<(CorpusRecord, SessionId, u32)> {
    let mut children: HashMap<String, Vec<CorpusRecord>> = HashMap::new();
    for record in corpus.values() {
        if let Some(parent) = &record.header.parent_session {
            children
                .entry(parent.as_str().to_string())
                .or_default()
                .push(record.clone());
        }
    }
    for siblings in children.values_mut() {
        siblings.sort_by(compare_corpus_records);
    }
    let mut stack: Vec<(CorpusRecord, SessionId, u32)> = children
        .get(root.as_str())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|record| (record, root.clone(), 1))
        .collect();
    let mut visited = HashSet::from([root.as_str().to_string()]);
    let mut positioned = Vec::new();
    while let Some((record, parent, depth)) = stack.pop() {
        let id = record.header.id.as_str().to_string();
        if !visited.insert(id.clone()) {
            continue;
        }
        if record.header.origin.as_deref() == Some("subagent") {
            positioned.push((record.clone(), parent, depth));
        }
        if let Some(descendants) = children.get(&id) {
            for child in descendants.iter().rev() {
                stack.push((child.clone(), record.header.id.clone(), depth + 1));
            }
        }
    }
    positioned
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
    use dsh_session::{session_id, SessionEvent};
    use dsh_session_persistence::{
        PersistenceError, PersistenceRuntime, SessionInspection, SessionStoreBackend,
    };
    use dsh_session_projection::SessionProjectionRegistry;
    use dsh_user_approval::{effective_approval_policy, ApprovalPolicy};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

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

    struct MemoryBackend {
        sessions: Mutex<HashMap<String, (SessionHeader, Vec<SessionEvent>)>>,
        fail_load: Mutex<HashSet<String>>,
    }

    impl MemoryBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sessions: Mutex::new(HashMap::new()),
                fail_load: Mutex::new(HashSet::new()),
            })
        }
    }

    #[async_trait]
    impl SessionStoreBackend for MemoryBackend {
        async fn save(&self, session: &Session) -> std::result::Result<(), PersistenceError> {
            self.sessions.lock().expect("memory persist").insert(
                session.id().as_str().to_string(),
                (session.header().clone(), session.events()),
            );
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> std::result::Result<Session, PersistenceError> {
            self.inspect(id).await?.into_session()
        }

        async fn inspect(
            &self,
            id: &SessionId,
        ) -> std::result::Result<SessionInspection, PersistenceError> {
            if self
                .fail_load
                .lock()
                .expect("memory persist fail")
                .contains(id.as_str())
            {
                return Err(PersistenceError::NotFound(id.as_str().to_string()));
            }
            let guard = self.sessions.lock().expect("memory persist");
            let (header, events) = guard
                .get(id.as_str())
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))?;
            Ok(SessionInspection {
                meta: header.clone(),
                events: events.clone(),
            })
        }

        async fn list_ids(&self) -> std::result::Result<Vec<SessionId>, PersistenceError> {
            Ok(self
                .sessions
                .lock()
                .expect("memory persist")
                .keys()
                .map(|id| session_id(id.as_str()))
                .collect())
        }

        async fn list_headers(&self) -> std::result::Result<Vec<SessionHeader>, PersistenceError> {
            Ok(self
                .sessions
                .lock()
                .expect("memory persist")
                .values()
                .map(|(header, _)| header.clone())
                .collect())
        }
    }

    fn continuable_host_persisted() -> (
        Context,
        Arc<SubagentRuntime>,
        dsh_agent::AgentHandle,
        Arc<SessionStore>,
        Arc<PersistenceRuntime>,
    ) {
        continuable_host_persisted_with(true)
    }

    fn continuable_host_persisted_with(
        with_approval: bool,
    ) -> (
        Context,
        Arc<SubagentRuntime>,
        dsh_agent::AgentHandle,
        Arc<SessionStore>,
        Arc<PersistenceRuntime>,
    ) {
        let (ctx, runtime, parent, store) = continuable_host(with_approval);
        let persistence = Arc::new(PersistenceRuntime::new(MemoryBackend::new()));
        ctx.provide(Arc::clone(&persistence)).unwrap();
        (ctx, runtime, parent, store, persistence)
    }

    async fn persist_and_evict(
        persistence: &PersistenceRuntime,
        store: &SessionStore,
        child_id: &SessionId,
    ) {
        let child = store.get(child_id).expect("child in catalog");
        persistence.save(child.as_ref()).await.unwrap();
        store.remove(child_id);
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
        SessionProjectionRegistry::install(&ctx).unwrap();
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

    #[tokio::test]
    async fn start_continuable_rejects_when_persistence_is_absent() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        let error = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error, PERSISTENCE_REQUIRED);
        assert!(store.get(&session_id("parent")).is_some());
        assert_eq!(store.live().len(), 1);
    }

    #[tokio::test]
    async fn start_continuable_uses_a_reserved_child_id() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap();
        assert_eq!(started.child_id, reserved);
        assert!(store.get(&reserved).is_some());
    }

    #[tokio::test]
    async fn start_continuable_rejects_a_duplicate_reserved_id_while_live() {
        let (_ctx, runtime, parent, _store, _persistence) = continuable_host_persisted();
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap();
        let error = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("again")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(error, already_exists(&reserved));
    }

    #[tokio::test]
    async fn start_continuable_rejects_a_reserved_id_after_settlement() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap();
        runtime.run_pending().await;
        assert!(store.get(&reserved).is_some());
        let error = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("again")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(error, already_exists(&reserved));
    }

    #[tokio::test]
    async fn start_continuable_rejects_a_reserved_id_owned_by_persistence() {
        let (_ctx, runtime, parent, store, persistence) = continuable_host_persisted();
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap();
        runtime.run_pending().await;
        persist_and_evict(&persistence, &store, &reserved).await;
        let error = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("again")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(error, already_exists(&reserved));
        assert!(store.get(&reserved).is_none());
    }

    #[tokio::test]
    async fn start_continuable_rejects_a_reserved_id_already_in_the_session_store() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        let reserved = session_id("00000000-0000-4000-8000-000000000123");
        store.create(reserved.clone());
        let error = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                Some(reserved.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(error, already_exists(&reserved));
        assert_eq!(
            store.get(&reserved).unwrap().header().origin.as_deref(),
            None
        );
    }

    #[tokio::test]
    async fn start_continuable_seeds_parent_sandbox_and_pins_approval() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        set_sandbox_mode(
            parent.agent.session().as_ref(),
            SandboxMode::DangerFullAccess,
        )
        .unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
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

    #[tokio::test]
    async fn start_continuable_skips_unswitched_sandbox_and_still_pins_approval() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
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

    #[tokio::test]
    async fn start_continuable_omits_approval_when_the_service_is_absent() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted_with(false);
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap();
        let child = store.get(&started.child_id).unwrap();
        assert!(child.events().iter().all(|event| {
            !matches!(
                event.data,
                SessionEventData::SandboxMode { .. } | SessionEventData::ApprovalPolicy { .. }
            )
        }));
    }

    #[tokio::test]
    async fn start_continuable_keeps_the_captured_mode_after_a_later_parent_switch() {
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::ReadOnly).unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
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
        let (_ctx, runtime, parent, store, _persistence) = continuable_host_persisted();
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::ReadOnly).unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap();
        runtime.run_pending().await;
        runtime
            .followup(
                &parent.agent,
                &started.child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
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

    #[tokio::test]
    async fn followup_without_persistence_is_unavailable_when_catalog_misses() {
        let (_ctx, runtime, parent, _store) = continuable_host(true);
        let error = runtime
            .followup(
                &parent.agent,
                &session_id("22222222-2222-4222-8222-222222222222"),
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap_err();
        assert_eq!(error, PERSISTENCE_REQUIRED);
    }

    #[tokio::test]
    async fn followup_cold_loads_a_persisted_child_after_catalog_eviction() {
        let (_ctx, runtime, parent, store, persistence) = continuable_host_persisted();
        set_sandbox_mode(parent.agent.session().as_ref(), SandboxMode::ReadOnly).unwrap();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap();
        runtime.run_pending().await;
        persist_and_evict(&persistence, &store, &started.child_id).await;
        runtime
            .followup(
                &parent.agent,
                &started.child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap();
        let child = store.get(&started.child_id).expect("republished");
        let sandbox: Vec<_> = child
            .events()
            .into_iter()
            .filter(|event| matches!(event.data, SessionEventData::SandboxMode { .. }))
            .collect();
        assert_eq!(sandbox.len(), 1);
        assert_eq!(runtime.status_of(&started.child_id), "idle");
    }

    #[tokio::test]
    async fn followup_maps_a_missing_persisted_child_to_unavailable() {
        let (_ctx, runtime, parent, _store, _persistence) = continuable_host_persisted();
        let error = runtime
            .followup(
                &parent.agent,
                &session_id("22222222-2222-4222-8222-222222222222"),
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "subagent \"22222222-2222-4222-8222-222222222222\" is unavailable"
        );
    }

    #[tokio::test]
    async fn followup_rejects_a_persisted_non_continuable_child() {
        let (_ctx, runtime, parent, _store, persistence) = continuable_host_persisted();
        let header = SessionHeader::for_subagent_child(
            Some(parent.agent.session().header()),
            parent.agent.id().clone(),
        );
        let child_id = header.id.clone();
        let child = Session::with_header(header);
        child
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": SUBAGENT_DESCRIPTOR_VERSION,
                        "mode": "one-shot",
                        "provider": "spawn",
                    }),
                },
                None,
            )
            .unwrap();
        persistence.save(&child).await.unwrap();
        let error = runtime
            .followup(
                &parent.agent,
                &child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap_err();
        assert_eq!(error, no_continuation(&child_id));
    }

    #[tokio::test]
    async fn followup_rejects_a_persisted_child_owned_by_another_parent() {
        let (ctx, runtime, parent, store, persistence) = continuable_host_persisted();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap();
        persist_and_evict(&persistence, &store, &started.child_id).await;
        let other = store.create(session_id("other-parent"));
        let other_handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(other)
            .unwrap();
        let error = runtime
            .followup(
                &other_handle.agent,
                &started.child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap_err();
        assert_eq!(error, other_parent(&started.child_id));
    }

    #[tokio::test]
    async fn followup_folds_only_the_own_suffix_after_seed_length() {
        let (_ctx, runtime, parent, store, persistence) = continuable_host_persisted();
        let mut header = SessionHeader::for_subagent_child(
            Some(parent.agent.session().header()),
            parent.agent.id().clone(),
        );
        header.seed_length = Some(1);
        let child_id = header.id.clone();
        let child = Session::with_header(header);
        child
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": SUBAGENT_DESCRIPTOR_VERSION,
                        "mode": "one-shot",
                        "provider": "spawn",
                        "label": "ancestor",
                    }),
                },
                None,
            )
            .unwrap();
        child
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": SUBAGENT_DESCRIPTOR_VERSION,
                        "mode": "continuable",
                        "provider": "spawn",
                        "label": "own child",
                    }),
                },
                None,
            )
            .unwrap();
        persistence.save(&child).await.unwrap();
        runtime
            .followup(
                &parent.agent,
                &child_id,
                vec![ContentBlock::text("continue")],
                MessageSource::User,
            )
            .await
            .unwrap();
        assert!(store.get(&child_id).is_some());
    }

    #[tokio::test]
    async fn list_children_includes_a_persisted_child_after_catalog_eviction() {
        let (_ctx, runtime, parent, store, persistence) = continuable_host_persisted();
        let started = runtime
            .start_continuable(
                "spawn",
                "child task",
                vec![ContentBlock::text("ping")],
                &parent.agent,
                None,
            )
            .await
            .unwrap();
        runtime.run_pending().await;
        persist_and_evict(&persistence, &store, &started.child_id).await;
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(entries.len(), 1);
        let SubagentListEntry::Child {
            id,
            label,
            mode,
            activity,
            ..
        } = &entries[0]
        else {
            panic!("expected child row, got {:?}", entries[0]);
        };
        assert_eq!(id, &started.child_id);
        assert_eq!(label, "child task");
        assert_eq!(mode, "continuable");
        assert_eq!(activity, "inactive");
        assert_eq!(runtime.status_of(&started.child_id), "ready");
    }

    #[tokio::test]
    async fn list_children_without_persistence_stays_live_only() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        let header = SessionHeader::for_subagent_child(
            Some(parent.agent.session().header()),
            parent.agent.id().clone(),
        );
        let child_id = header.id.clone();
        let child = store.publish(Session::with_header(header));
        child
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": SUBAGENT_DESCRIPTOR_VERSION,
                        "mode": "continuable",
                        "provider": "spawn",
                        "label": "live-only",
                    }),
                },
                None,
            )
            .unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(entries.len(), 1);
        let SubagentListEntry::Child { id, activity, .. } = &entries[0] else {
            panic!("expected child row, got {:?}", entries[0]);
        };
        assert_eq!(id, &child_id);
        assert_eq!(activity, "running");
        store.remove(&child_id);
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert!(entries.is_empty());
    }

    fn authored_header(
        parent: &SessionHeader,
        parent_id: SessionId,
        id: &str,
        created_at: u64,
    ) -> SessionHeader {
        let mut header = SessionHeader::for_subagent_child(Some(parent), parent_id);
        header.id = session_id(id);
        header.created_at = created_at;
        header
    }

    fn append_descriptor(session: &Session, mode: &str, label: &str, version: u64) {
        session
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": version,
                        "mode": mode,
                        "provider": "spawn",
                        "label": label,
                    }),
                },
                None,
            )
            .unwrap();
    }

    fn append_bare_turns(session: &Session) {
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Interrupted,
                },
                None,
            )
            .unwrap();
    }

    fn hostile_unit() -> dsh_session_projection::ProjectionUnit {
        dsh_session_projection::ProjectionUnit {
            key: "subagentListHostileProbe".into(),
            state_version: 1,
            init: Arc::new(|| serde_json::json!({})),
            apply: Arc::new(|state, event| {
                let SessionEventData::Extension { type_name, data } = &event.data else {
                    return state.clone();
                };
                if type_name == "subagent/descriptor"
                    && data.get("label").and_then(Value::as_str) == Some("poison me")
                {
                    return serde_json::json!({ "poisoned": true });
                }
                state.clone()
            }),
            view: Arc::new(|state| {
                if state.get("poisoned") == Some(&serde_json::json!(true)) {
                    panic!("hostile unit rejects the poisoned log");
                }
                Value::Null
            }),
        }
    }

    #[tokio::test]
    async fn list_children_requires_the_session_store() {
        let ctx = Context::new();
        SessionProjectionRegistry::install(&ctx).unwrap();
        let runtime = SubagentRuntime::install(&ctx).unwrap();
        let error = runtime
            .list_children(&session_id("parent"))
            .await
            .unwrap_err();
        assert_eq!(error, SESSION_STORE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_children_omits_a_live_child_without_a_descriptor() {
        let (_ctx, runtime, parent, store) = continuable_host(true);
        let header = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "11111111-1111-4111-8111-111111111111",
            2,
        );
        store.publish(Session::with_header(header));
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_children_reports_a_descriptorless_cold_child_as_corrupt() {
        let (_ctx, runtime, parent, _store, persistence) = continuable_host_persisted();
        let header = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "22222222-2222-4222-8222-222222222222",
            2,
        );
        let child_id = header.id.clone();
        let child = Session::with_header(header);
        append_bare_turns(&child);
        persistence.save(&child).await.unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![SubagentListEntry::Diagnostic {
                id: child_id,
                reason: DIAGNOSTIC_CORRUPT.to_string(),
                parent: parent.agent.id().clone(),
                depth: 1,
            }]
        );
    }

    #[tokio::test]
    async fn list_children_reports_an_unknown_descriptor_version_as_corrupt() {
        let (_ctx, runtime, parent, _store, persistence) = continuable_host_persisted();
        let header = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "33333333-3333-4333-8333-333333333333",
            2,
        );
        let child_id = header.id.clone();
        let child = Session::with_header(header);
        append_descriptor(&child, "continuable", "future", 99);
        persistence.save(&child).await.unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![SubagentListEntry::Diagnostic {
                id: child_id,
                reason: DIAGNOSTIC_CORRUPT.to_string(),
                parent: parent.agent.id().clone(),
                depth: 1,
            }]
        );
    }

    #[tokio::test]
    async fn list_children_maps_a_failed_cold_inspect_to_unavailable() {
        let ctx = Context::new();
        let store = Arc::new(SessionStore::new());
        ctx.provide(Arc::clone(&store)).unwrap();
        SessionProjectionRegistry::install(&ctx).unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        ctx.provide(Arc::new(agents)).unwrap();
        let backend = MemoryBackend::new();
        let header = authored_header(
            &SessionHeader::new(session_id("parent"), None),
            session_id("parent"),
            "44444444-4444-4444-8444-444444444444",
            2,
        );
        let child_id = header.id.clone();
        backend
            .sessions
            .lock()
            .expect("memory persist")
            .insert(child_id.as_str().to_string(), (header.clone(), Vec::new()));
        backend
            .fail_load
            .lock()
            .expect("memory persist fail")
            .insert(child_id.as_str().to_string());
        ctx.provide(Arc::new(PersistenceRuntime::new(backend)))
            .unwrap();
        let runtime = SubagentRuntime::install(&ctx).unwrap();
        let parent_session = store.create(session_id("parent"));
        let parent = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(parent_session)
            .unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![SubagentListEntry::Diagnostic {
                id: child_id,
                reason: DIAGNOSTIC_UNAVAILABLE.to_string(),
                parent: parent.agent.id().clone(),
                depth: 1,
            }]
        );
    }

    struct LifecycleMismatchBackend {
        listed: SessionHeader,
        loaded_header: SessionHeader,
        events: Vec<SessionEvent>,
    }

    #[async_trait]
    impl SessionStoreBackend for LifecycleMismatchBackend {
        async fn save(&self, _: &Session) -> std::result::Result<(), PersistenceError> {
            Ok(())
        }

        async fn load(&self, _: &SessionId) -> std::result::Result<Session, PersistenceError> {
            let session = Session::with_header(self.loaded_header.clone());
            for event in &self.events {
                session.append_logged(event.clone())?;
            }
            Ok(session)
        }

        async fn list_ids(&self) -> std::result::Result<Vec<SessionId>, PersistenceError> {
            Ok(vec![self.listed.id.clone()])
        }

        async fn list_headers(&self) -> std::result::Result<Vec<SessionHeader>, PersistenceError> {
            Ok(vec![self.listed.clone()])
        }
    }

    #[tokio::test]
    async fn list_children_reports_a_lifecycle_mismatch_as_corrupt() {
        let (ctx, runtime, parent, _store) = continuable_host(true);
        let listed = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "55555555-5555-4555-8555-555555555555",
            2,
        );
        let child_id = listed.id.clone();
        let mut loaded_header = listed.clone();
        loaded_header.created_at = 99;
        let loaded = Session::with_header(loaded_header.clone());
        append_descriptor(
            &loaded,
            "continuable",
            "reborn",
            SUBAGENT_DESCRIPTOR_VERSION,
        );
        ctx.provide(Arc::new(PersistenceRuntime::new(Arc::new(
            LifecycleMismatchBackend {
                listed,
                loaded_header,
                events: loaded.events(),
            },
        ))))
        .unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![SubagentListEntry::Diagnostic {
                id: child_id,
                reason: DIAGNOSTIC_CORRUPT.to_string(),
                parent: parent.agent.id().clone(),
                depth: 1,
            }]
        );
    }

    #[tokio::test]
    async fn list_children_contains_a_hostile_cold_fold_as_corrupt() {
        let (ctx, runtime, parent, _store, persistence) = continuable_host_persisted();
        ctx.service::<SessionProjectionRegistry>()
            .unwrap()
            .register(hostile_unit())
            .unwrap();
        let header = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "66666666-6666-4666-8666-666666666666",
            2,
        );
        let child_id = header.id.clone();
        let child = Session::with_header(header);
        append_descriptor(
            &child,
            "continuable",
            "poison me",
            SUBAGENT_DESCRIPTOR_VERSION,
        );
        persistence.save(&child).await.unwrap();
        let entries = runtime.list_children(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![SubagentListEntry::Diagnostic {
                id: child_id,
                reason: DIAGNOSTIC_CORRUPT.to_string(),
                parent: parent.agent.id().clone(),
                depth: 1,
            }]
        );
    }

    #[tokio::test]
    async fn list_descendants_positions_a_corrupt_intermediate() {
        let (_ctx, runtime, parent, _store, persistence) = continuable_host_persisted();
        let bare = authored_header(
            parent.agent.session().header(),
            parent.agent.id().clone(),
            "77777777-7777-4777-8777-777777777777",
            2,
        );
        let below = authored_header(
            &bare,
            bare.id.clone(),
            "88888888-8888-4888-8888-888888888888",
            3,
        );
        let bare_id = bare.id.clone();
        let below_id = below.id.clone();
        let bare_session = Session::with_header(bare);
        append_bare_turns(&bare_session);
        persistence.save(&bare_session).await.unwrap();
        let below_session = Session::with_header(below);
        append_descriptor(
            &below_session,
            "continuable",
            "below the corrupt node",
            SUBAGENT_DESCRIPTOR_VERSION,
        );
        persistence.save(&below_session).await.unwrap();
        let entries = runtime.list_descendants(parent.agent.id()).await.unwrap();
        assert_eq!(
            entries,
            vec![
                SubagentListEntry::Diagnostic {
                    id: bare_id.clone(),
                    reason: DIAGNOSTIC_CORRUPT.to_string(),
                    parent: parent.agent.id().clone(),
                    depth: 1,
                },
                SubagentListEntry::Child {
                    id: below_id,
                    label: "below the corrupt node".into(),
                    mode: "continuable".into(),
                    activity: "inactive".into(),
                    has_children: false,
                    parent: bare_id,
                    depth: 2,
                },
            ]
        );
    }
}
