//! Approval capability (`ctx.approval`).
//!
//! Session policy is decided before any answerer: `never` rejects without
//! dispatch; `ask` runs `approval/request` and fails closed as `unavailable`
//! when no answerer claims the question. The `approval/asked` +
//! `approval/decided` audit pair must sit inside an open turn.

use dsh_agent::Agent;
use dsh_cordis::{Context, Result, Service};
use dsh_llm::{ContentBlock, MessageSource, UserMessage};
use dsh_session::{Session, SessionEvent, SessionEventData};
use dsh_system_prompt::{PromptContext, PromptContextText, SystemPrompt};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "dsh-user-approval"
}

/// Model-facing statement for the deterministic `'never'` policy.
pub const NEVER_SENTENCE: &str = "Approval prompts are disabled in this session: actions that require approval are rejected automatically — do not request sandbox escalation (do not set `sandbox_permissions`).";

/// Model-facing statement for an interactive policy that may still fail closed.
pub const ASK_SENTENCE: &str = "Approval policy: ask. Operations that require approval may ask through the configured answerers; without an available answerer, the request fails closed.";

/// Exact sentence thrown when [`ApprovalService::request`] runs between turns.
pub const OUTSIDE_TURN: &str = "approval.request() outside an open turn: the approval/asked + approval/decided audit pair must be turn-enclosed (a bare event between turns is crash-tail garbage on reload). Ask from inside the turn that needs the decision.";

/// Session approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Delegate to composed answerers; missing answerers fail closed.
    Ask,
    /// Reject every ask without prompting.
    Never,
}

impl ApprovalPolicy {
    /// Parse a TypeScript policy string.
    pub fn parse(policy: &str) -> Option<Self> {
        match policy {
            "ask" => Some(Self::Ask),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// TypeScript policy string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

/// Closed approval outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// One-shot grant for the requested action only.
    AllowedOnce,
    /// Explicit rejection, including the `never` policy.
    Rejected,
    /// Withdrawn request.
    Cancelled,
    /// Missing or rogue answerer; callers fail closed.
    Unavailable,
}

impl ApprovalOutcome {
    /// Parse a TypeScript outcome string.
    pub fn parse(outcome: &str) -> Option<Self> {
        match outcome {
            "allowed-once" => Some(Self::AllowedOnce),
            "rejected" => Some(Self::Rejected),
            "cancelled" => Some(Self::Cancelled),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// TypeScript outcome string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Readonly same-process permission question.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// Tool the question is about.
    pub tool_name: String,
    /// Exact tool call when the asker had one.
    pub call_id: Option<String>,
    /// Asker's human-readable explanation.
    pub reason: Option<String>,
}

/// Plugin config. Omitted `policy` is `ask`.
#[derive(Debug, Clone)]
pub struct Config {
    /// Deployment default for sessions without an `approval/policy` override.
    pub policy: ApprovalPolicy,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// A policy other than `ask` / `never`.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let policy = match config.and_then(|value| value.get("policy")) {
            None => ApprovalPolicy::Ask,
            Some(value) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| "user-approval: policy must be a string".to_string())?;
                ApprovalPolicy::parse(text).ok_or_else(|| {
                    format!("user-approval: policy must be one of \"ask\" or \"never\"")
                })?
            }
        };
        Ok(Self { policy })
    }
}

/// `ctx.approval`.
pub struct ApprovalService {
    default_policy: ApprovalPolicy,
}

impl Service for ApprovalService {
    const KEY: &'static str = "approval";
}

impl ApprovalService {
    /// Deployment default policy beneath a session override.
    pub fn default_policy(&self) -> ApprovalPolicy {
        self.default_policy
    }

    /// Session fold, else the configured default.
    pub fn effective_policy(&self, session: &Session) -> ApprovalPolicy {
        self.override_of(session).unwrap_or(self.default_policy)
    }

    /// Last `approval/policy` in the log, without the configured default.
    pub fn override_of(&self, session: &Session) -> Option<ApprovalPolicy> {
        effective_approval_policy(&session.events())
    }

    /// Switch one live agent's policy and inject the transition for its next step.
    ///
    /// Session initialization uses [`set_approval_policy`] because there is no
    /// previously visible policy to change.
    ///
    /// # Errors
    /// A refused session append.
    pub fn set_policy(
        &self,
        agent: &dyn Agent,
        policy: ApprovalPolicy,
    ) -> std::result::Result<(), String> {
        let session = agent.session();
        let previous = self.effective_policy(session.as_ref());
        if previous == policy {
            return Ok(());
        }
        set_approval_policy(session.as_ref(), policy)?;
        agent.inject(UserMessage::from_parts(
            vec![ContentBlock::text(format!(
                "The approval policy changed from \"{}\" to \"{}\" (changed by the user).",
                previous.as_str(),
                policy.as_str()
            ))],
            MessageSource::plugin("user-approval"),
        ));
        Ok(())
    }

    /// Ask the composed answerers to decide one request.
    ///
    /// # Errors
    /// No turn is open, or either audit append fails before commit.
    pub fn request(
        &self,
        ctx: &Context,
        session: &Session,
        req: ApprovalRequest,
    ) -> std::result::Result<ApprovalOutcome, String> {
        if !has_open_turn(&session.events()) {
            return Err(OUTSIDE_TURN.into());
        }
        let id = Uuid::new_v4().to_string();
        session
            .append(
                SessionEventData::ApprovalAsked {
                    id: id.clone(),
                    tool_name: req.tool_name.clone(),
                    call_id: req.call_id.clone(),
                    reason: req.reason.clone(),
                },
                None,
            )
            .map_err(|error| error.to_string())?;
        let outcome = self.decide(ctx, session, &req);
        session
            .append(
                SessionEventData::ApprovalDecided {
                    id,
                    outcome: outcome.as_str().to_string(),
                },
                None,
            )
            .map_err(|error| error.to_string())?;
        Ok(outcome)
    }

    fn decide(&self, ctx: &Context, session: &Session, req: &ApprovalRequest) -> ApprovalOutcome {
        if self.effective_policy(session) == ApprovalPolicy::Never {
            return ApprovalOutcome::Rejected;
        }
        let payload = json!({
            "toolName": req.tool_name,
            "callId": req.call_id,
            "reason": req.reason,
        });
        match ctx.waterfall("approval/request", payload, |_| json!("unavailable")) {
            Ok(Value::String(outcome)) => {
                ApprovalOutcome::parse(&outcome).unwrap_or(ApprovalOutcome::Unavailable)
            }
            _ => ApprovalOutcome::Unavailable,
        }
    }
}

/// Last `approval/policy` event in log order, or `None` when the session never switched.
pub fn effective_approval_policy(events: &[SessionEvent]) -> Option<ApprovalPolicy> {
    for event in events.iter().rev() {
        if let SessionEventData::ApprovalPolicy { policy } = &event.data {
            return ApprovalPolicy::parse(policy);
        }
    }
    None
}

/// Append one `approval/policy` override.
///
/// # Errors
/// A refused session append.
pub fn set_approval_policy(
    session: &Session,
    policy: ApprovalPolicy,
) -> std::result::Result<(), String> {
    session
        .append(
            SessionEventData::ApprovalPolicy {
                policy: policy.as_str().to_string(),
            },
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn has_open_turn(events: &[SessionEvent]) -> bool {
    for event in events.iter().rev() {
        match dsh_session::event_type_name(&event.data) {
            "turn/start" => return true,
            "turn/end" => return false,
            _ => {}
        }
    }
    false
}

/// Provide `ctx.approval` and bind the runtime-context contribution when possible.
///
/// # Errors
/// Invalid config, or a duplicate service registration.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<Arc<ApprovalService>> {
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let service = Arc::new(ApprovalService {
        default_policy: resolved.policy,
    });
    ctx.provide(Arc::clone(&service))?;
    bind_prompt(ctx)?;
    Ok(service)
}

/// Register `approval:policy` (order 115) when both services are present.
///
/// # Errors
/// Prompt registration does not fail; this returns `Ok` when either service is absent.
pub fn bind_prompt(ctx: &Context) -> Result<()> {
    let Some(prompt) = ctx.get::<SystemPrompt>() else {
        return Ok(());
    };
    let Some(service) = ctx.get::<ApprovalService>() else {
        return Ok(());
    };
    prompt.register_context(PromptContext {
        name: "approval:policy".into(),
        order: 115,
        text: PromptContextText::Dynamic(Arc::new(move |session| {
            let Some(session) = session else {
                return String::new();
            };
            match service.effective_policy(session) {
                ApprovalPolicy::Never => NEVER_SENTENCE.to_string(),
                ApprovalPolicy::Ask => ASK_SENTENCE.to_string(),
            }
        })),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionStore};

    fn session_with_turn() -> (SessionStore, Arc<Session>) {
        let store = SessionStore::new();
        let session = store.create(session_id("a"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        (store, session)
    }

    #[test]
    fn omitted_policy_is_ask() {
        assert_eq!(Config::resolve(None).unwrap().policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn request_outside_turn_fails_before_audit() {
        let ctx = Context::new();
        install(&ctx, Some(&serde_json::json!({ "policy": "never" }))).unwrap();
        let session = SessionStore::new().create(session_id("idle"));
        let service = ctx.service::<ApprovalService>().unwrap();
        let error = service
            .request(
                &ctx,
                session.as_ref(),
                ApprovalRequest {
                    tool_name: "bash".into(),
                    call_id: None,
                    reason: None,
                },
            )
            .unwrap_err();
        assert_eq!(error, OUTSIDE_TURN);
        assert!(session.events().is_empty());
    }

    #[test]
    fn never_rejects_without_answerer_and_writes_the_pair() {
        let ctx = Context::new();
        install(&ctx, Some(&serde_json::json!({ "policy": "never" }))).unwrap();
        let (_store, session) = session_with_turn();
        let service = ctx.service::<ApprovalService>().unwrap();
        let outcome = service
            .request(
                &ctx,
                session.as_ref(),
                ApprovalRequest {
                    tool_name: "bash".into(),
                    call_id: Some("c1".into()),
                    reason: Some("escalate".into()),
                },
            )
            .unwrap();
        assert_eq!(outcome, ApprovalOutcome::Rejected);
        let events = session.events();
        let types: Vec<_> = events
            .iter()
            .map(|event| dsh_session::event_type_name(&event.data))
            .collect();
        assert_eq!(
            types,
            ["turn/start", "approval/asked", "approval/decided"]
        );
    }

    #[test]
    fn ask_without_answerer_is_unavailable() {
        let ctx = Context::new();
        install(&ctx, Some(&serde_json::json!({ "policy": "ask" }))).unwrap();
        let (_store, session) = session_with_turn();
        let service = ctx.service::<ApprovalService>().unwrap();
        let outcome = service
            .request(
                &ctx,
                session.as_ref(),
                ApprovalRequest {
                    tool_name: "write".into(),
                    call_id: None,
                    reason: None,
                },
            )
            .unwrap();
        assert_eq!(outcome, ApprovalOutcome::Unavailable);
    }

    #[test]
    fn ask_sentence_is_contributed_with_a_session() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        install(&ctx, Some(&serde_json::json!({ "policy": "ask" }))).unwrap();
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        assert!(prompt.context_sections(None).is_empty());
        let session = SessionStore::new().create(session_id("p"));
        let sections = prompt.context_sections(Some(session.as_ref()));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].text, ASK_SENTENCE);
    }
}
