//! Event-sourced same-session goals (`ctx.goals`).
//!
//! Durable mutations append `goal/change`. Activation (`armed` / `disarmed`)
//! is process-local and never persisted.

use dsh_cordis::{Context, Result, Service};
use dsh_llm::MessageSource;
use dsh_session::{Session, SessionEvent, SessionEventData, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-goal"
}

/// Default admitted-round cap when create omits one.
pub const DEFAULT_MAX_GOAL_ROUNDS: u32 = 256;

/// Durable continuation phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalPhase {
    /// Driver may continue.
    Active,
    /// Human or cancellation paused continuation.
    Paused,
    /// Terminal blocker.
    Blocked,
    /// Terminal success.
    Complete,
}

/// Process-local continuation eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalActivation {
    /// Driver may queue the next round.
    Armed,
    /// Driver must not queue.
    Disarmed,
}

impl GoalActivation {
    /// Wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Disarmed => "disarmed",
        }
    }
}

/// Machine-routable blocked explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBlockReason {
    /// Lower-kebab-case classification.
    pub code: String,
    /// Non-empty explanation.
    pub message: String,
}

/// Durable snapshot written by every non-clear mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    /// `goal-{uuid}`.
    pub id: String,
    /// Positive revision.
    pub revision: u64,
    /// Trimmed objective.
    pub objective: String,
    /// Durable phase.
    pub phase: GoalPhase,
    /// Admitted-round cap.
    #[serde(rename = "maxGoalRounds")]
    pub max_goal_rounds: u32,
    /// Present exactly while `phase` is `blocked`.
    #[serde(rename = "blockedReason", skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GoalBlockReason>,
}

/// Current projection including process-local activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalView {
    /// Durable snapshot fields.
    #[serde(flatten)]
    pub snapshot: GoalSnapshot,
    /// Highest admitted round number.
    #[serde(rename = "roundsStarted")]
    pub rounds_started: u32,
    /// Epoch milliseconds of the create mutation.
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Epoch milliseconds of the latest mutation.
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
    /// Process-local continuation eligibility.
    pub activation: GoalActivation,
}

impl GoalView {
    /// Compact JSON the model-facing tools return.
    pub fn tool_json(&self) -> serde_json::Value {
        let mut goal = json!({
            "id": self.snapshot.id,
            "revision": self.snapshot.revision,
            "objective": self.snapshot.objective,
            "phase": self.snapshot.phase,
            "roundsStarted": self.rounds_started,
            "maxGoalRounds": self.snapshot.max_goal_rounds,
        });
        if let Some(reason) = &self.snapshot.blocked_reason {
            goal["blockedReason"] = json!({
                "code": reason.code,
                "message": reason.message,
            });
        }
        json!({
            "goal": goal,
            "activation": self.activation,
        })
    }
}

/// CAS identity for one exact revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRef {
    /// Goal id.
    pub id: String,
    /// Expected revision.
    pub revision: u64,
}

/// Domain failures with TypeScript `GOAL_*` codes.
#[derive(Debug, Error)]
pub enum GoalError {
    /// No current goal.
    #[error("no current goal")]
    NotFound,
    /// Create while one already exists.
    #[error("goal \"{id}\" already exists with phase \"{phase}\"")]
    AlreadyExists {
        /// Existing id.
        id: String,
        /// Existing phase token.
        phase: String,
    },
    /// CAS mismatch.
    #[error("stale goal ref \"{id}\" revision {got}; current is \"{current_id}\" revision {current}")]
    StaleRevision {
        /// Caller id.
        id: String,
        /// Caller revision.
        got: u64,
        /// Live id.
        current_id: String,
        /// Live revision.
        current: u64,
    },
    /// Invalid input or transition.
    #[error("{0}")]
    Invalid(String),
}

impl GoalError {
    /// Structured code matching the TypeScript `GoalError` codes.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "GOAL_NOT_FOUND",
            Self::AlreadyExists { .. } => "GOAL_ALREADY_EXISTS",
            Self::StaleRevision { .. } => "GOAL_STALE_REVISION",
            Self::Invalid(_) => "GOAL_INVALID",
        }
    }
}

/// Deployment-varying create default.
#[derive(Debug, Clone)]
pub struct Config {
    /// Used when create omits `max_goal_rounds`.
    pub default_max_goal_rounds: u32,
}

impl Config {
    /// Resolve plugin config. Missing cap takes 256.
    pub fn resolve(value: Option<&serde_json::Value>) -> std::result::Result<Self, String> {
        let default_max_goal_rounds = match value.and_then(|value| value.get("defaultMaxGoalRounds"))
        {
            None => DEFAULT_MAX_GOAL_ROUNDS,
            Some(item) => {
                let number = item.as_u64().ok_or_else(|| {
                    "goal: defaultMaxGoalRounds must be a positive integer".to_string()
                })?;
                if number < 1 {
                    return Err("goal: defaultMaxGoalRounds must be a positive integer".into());
                }
                number as u32
            }
        };
        Ok(Self {
            default_max_goal_rounds,
        })
    }
}

/// Folded durable state.
#[derive(Debug, Clone, Default)]
pub struct FoldedGoal {
    /// Current snapshot, if any.
    pub goal: Option<GoalSnapshot>,
    /// Highest admitted round.
    pub rounds_started: u32,
    /// Create timestamp.
    pub created_at: u64,
    /// Latest mutation timestamp.
    pub updated_at: u64,
}

/// Fold `goal/change` and admitted goal-sourced `user/message` events.
pub fn fold_goal(events: &[SessionEvent]) -> FoldedGoal {
    let mut state = FoldedGoal::default();
    for event in events {
        apply_goal_event(&mut state, event);
    }
    state
}

/// Apply one session event to a goal fold.
pub fn apply_goal_event(state: &mut FoldedGoal, event: &SessionEvent) {
    match &event.data {
        SessionEventData::Extension { type_name, data } if type_name == "goal/change" => {
            apply_change(state, data);
        }
        SessionEventData::UserMessage(message) => {
            if let MessageSource::Goal {
                goal_id,
                revision,
                round,
            } = &message.source
            {
                if let Some(goal) = &state.goal {
                    if goal.id == *goal_id
                        && goal.revision == *revision
                        && *round == state.rounds_started + 1
                        && *round <= goal.max_goal_rounds
                    {
                        state.rounds_started = *round;
                    }
                }
            }
        }
        _ => {}
    }
}

fn apply_change(state: &mut FoldedGoal, data: &serde_json::Value) {
    let operation = data.get("operation").and_then(serde_json::Value::as_str);
    if operation == Some("clear") {
        *state = FoldedGoal::default();
        return;
    }
    let Some(goal) = data
        .get("goal")
        .and_then(|value| serde_json::from_value::<GoalSnapshot>(value.clone()).ok())
    else {
        return;
    };
    state.goal = Some(goal);
    state.rounds_started = data
        .get("roundsStarted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    state.created_at = data
        .get("createdAt")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    state.updated_at = data
        .get("updatedAt")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
}

/// `ctx.goals`.
pub struct GoalService {
    ctx: Context,
    config: Config,
    activations: Mutex<HashMap<String, GoalActivation>>,
}

impl GoalService {
    /// Provide `ctx.goals`.
    pub fn install(ctx: &Context, config: Config) -> Result<std::sync::Arc<Self>> {
        let service = std::sync::Arc::new(Self {
            ctx: ctx.clone(),
            config,
            activations: Mutex::new(HashMap::new()),
        });
        ctx.provide(std::sync::Arc::clone(&service))?;
        Ok(service)
    }

    fn activation_of(&self, session_id: &SessionId) -> GoalActivation {
        self.activations
            .lock()
            .expect("goals")
            .get(session_id.as_str())
            .copied()
            .unwrap_or(GoalActivation::Disarmed)
    }

    fn set_activation(&self, session_id: &SessionId, activation: GoalActivation) {
        self.activations
            .lock()
            .expect("goals")
            .insert(session_id.as_str().to_string(), activation);
    }

    /// Current projection, if any.
    pub fn get(&self, session: &Session) -> Option<GoalView> {
        let folded = fold_goal(&session.events());
        folded.goal.map(|snapshot| GoalView {
            snapshot,
            rounds_started: folded.rounds_started,
            created_at: folded.created_at,
            updated_at: folded.updated_at,
            activation: self.activation_of(session.id()),
        })
    }

    /// Disarm without a durable write.
    pub fn disarm(&self, session: &Session) -> Option<GoalView> {
        self.set_activation(session.id(), GoalActivation::Disarmed);
        self.get(session)
    }

    /// Create and arm a goal.
    pub fn create(
        &self,
        session: &Session,
        objective: &str,
        max_goal_rounds: Option<u32>,
    ) -> std::result::Result<GoalView, GoalError> {
        if let Some(existing) = self.get(session) {
            return Err(GoalError::AlreadyExists {
                id: existing.snapshot.id,
                phase: phase_token(existing.snapshot.phase).into(),
            });
        }
        let objective = validate_objective(objective)?;
        let max_goal_rounds = max_goal_rounds.unwrap_or(self.config.default_max_goal_rounds);
        validate_rounds(max_goal_rounds)?;
        let now = now_ms();
        let snapshot = GoalSnapshot {
            id: format!("goal-{}", Uuid::new_v4()),
            revision: 1,
            objective,
            phase: GoalPhase::Active,
            max_goal_rounds,
            blocked_reason: None,
        };
        self.commit(
            session,
            "create",
            Some(&snapshot),
            0,
            now,
            now,
            GoalActivation::Armed,
        )
    }

    /// Edit objective and/or cap.
    pub fn edit(
        &self,
        session: &Session,
        reference: &GoalRef,
        objective: Option<&str>,
        max_goal_rounds: Option<u32>,
    ) -> std::result::Result<GoalView, GoalError> {
        let current = self.require(session, reference)?;
        if objective.is_none() && max_goal_rounds.is_none() {
            return Err(GoalError::Invalid(
                "edit requires objective or max_goal_rounds".into(),
            ));
        }
        let mut next = current.snapshot.clone();
        if let Some(objective) = objective {
            next.objective = validate_objective(objective)?;
        }
        if let Some(max_goal_rounds) = max_goal_rounds {
            validate_rounds(max_goal_rounds)?;
            next.max_goal_rounds = max_goal_rounds;
        }
        next.revision += 1;
        self.commit_from(session, "edit", next, &current, current.activation)
    }

    /// Pause an active goal.
    pub fn pause(
        &self,
        session: &Session,
        reference: &GoalRef,
    ) -> std::result::Result<GoalView, GoalError> {
        self.transition(session, reference, GoalPhase::Paused, None, "pause")
    }

    /// Resume a paused or blocked goal.
    pub fn resume(
        &self,
        session: &Session,
        reference: &GoalRef,
    ) -> std::result::Result<GoalView, GoalError> {
        self.transition(session, reference, GoalPhase::Active, None, "resume")
    }

    /// Mark complete.
    pub fn complete(
        &self,
        session: &Session,
        reference: &GoalRef,
    ) -> std::result::Result<GoalView, GoalError> {
        self.transition(session, reference, GoalPhase::Complete, None, "complete")
    }

    /// Block with a reason.
    pub fn block(
        &self,
        session: &Session,
        reference: &GoalRef,
        reason: GoalBlockReason,
    ) -> std::result::Result<GoalView, GoalError> {
        if reason.code.trim().is_empty() || reason.message.trim().is_empty() {
            return Err(GoalError::Invalid("blocked requires a reason".into()));
        }
        self.transition(session, reference, GoalPhase::Blocked, Some(reason), "block")
    }

    /// Clear the current goal.
    pub fn clear(
        &self,
        session: &Session,
        reference: &GoalRef,
    ) -> std::result::Result<GoalRef, GoalError> {
        let current = self.require(session, reference)?;
        let now = now_ms();
        let cleared = GoalRef {
            id: current.snapshot.id.clone(),
            revision: current.snapshot.revision + 1,
        };
        session
            .append(
                SessionEventData::Extension {
                    type_name: "goal/change".into(),
                    data: json!({
                        "kind": "goal/change",
                        "version": 1,
                        "operation": "clear",
                        "cleared": { "id": cleared.id, "revision": cleared.revision },
                        "clearedAt": now,
                    }),
                },
                None,
            )
            .map_err(|error| GoalError::Invalid(error.to_string()))?;
        self.set_activation(session.id(), GoalActivation::Disarmed);
        self.ctx.emit(
            "goal/changed",
            json!({
                "operation": "clear",
                "ref": { "id": cleared.id, "revision": cleared.revision },
                "sessionId": session.id().as_str(),
            }),
        );
        Ok(cleared)
    }

    fn transition(
        &self,
        session: &Session,
        reference: &GoalRef,
        phase: GoalPhase,
        reason: Option<GoalBlockReason>,
        operation: &str,
    ) -> std::result::Result<GoalView, GoalError> {
        let current = self.require(session, reference)?;
        let mut next = current.snapshot.clone();
        next.phase = phase;
        next.blocked_reason = if phase == GoalPhase::Blocked {
            reason
        } else {
            None
        };
        next.revision += 1;
        let activation = if phase == GoalPhase::Active {
            GoalActivation::Armed
        } else {
            GoalActivation::Disarmed
        };
        self.commit_from(session, operation, next, &current, activation)
    }

    fn require(
        &self,
        session: &Session,
        reference: &GoalRef,
    ) -> std::result::Result<GoalView, GoalError> {
        let Some(current) = self.get(session) else {
            return Err(GoalError::NotFound);
        };
        if current.snapshot.id != reference.id || current.snapshot.revision != reference.revision {
            return Err(GoalError::StaleRevision {
                id: reference.id.clone(),
                got: reference.revision,
                current_id: current.snapshot.id,
                current: current.snapshot.revision,
            });
        }
        Ok(current)
    }

    fn commit_from(
        &self,
        session: &Session,
        operation: &str,
        snapshot: GoalSnapshot,
        current: &GoalView,
        activation: GoalActivation,
    ) -> std::result::Result<GoalView, GoalError> {
        self.commit(
            session,
            operation,
            Some(&snapshot),
            current.rounds_started,
            current.created_at,
            now_ms().max(current.updated_at),
            activation,
        )
    }

    fn commit(
        &self,
        session: &Session,
        operation: &str,
        snapshot: Option<&GoalSnapshot>,
        rounds_started: u32,
        created_at: u64,
        updated_at: u64,
        activation: GoalActivation,
    ) -> std::result::Result<GoalView, GoalError> {
        let snapshot = snapshot.ok_or(GoalError::NotFound)?;
        session
            .append(
                SessionEventData::Extension {
                    type_name: "goal/change".into(),
                    data: json!({
                        "kind": "goal/change",
                        "version": 1,
                        "operation": operation,
                        "goal": snapshot,
                        "roundsStarted": rounds_started,
                        "createdAt": created_at,
                        "updatedAt": updated_at,
                    }),
                },
                None,
            )
            .map_err(|error| GoalError::Invalid(error.to_string()))?;
        self.set_activation(session.id(), activation);
        let view = GoalView {
            snapshot: snapshot.clone(),
            rounds_started,
            created_at,
            updated_at,
            activation,
        };
        self.ctx.emit(
            "goal/changed",
            json!({
                "operation": operation,
                "ref": { "id": view.snapshot.id, "revision": view.snapshot.revision },
                "sessionId": session.id().as_str(),
                "activation": view.activation.as_str(),
            }),
        );
        Ok(view)
    }
}

impl Service for GoalService {
    const KEY: &'static str = "goals";
}

fn validate_objective(objective: &str) -> std::result::Result<String, GoalError> {
    let trimmed = objective.trim();
    if trimmed.is_empty() {
        return Err(GoalError::Invalid("objective must be a non-empty string".into()));
    }
    Ok(trimmed.to_string())
}

fn validate_rounds(max_goal_rounds: u32) -> std::result::Result<(), GoalError> {
    if max_goal_rounds < 1 {
        return Err(GoalError::Invalid(
            "maxGoalRounds must be a positive integer".into(),
        ));
    }
    Ok(())
}

fn phase_token(phase: GoalPhase) -> &'static str {
    match phase {
        GoalPhase::Active => "active",
        GoalPhase::Paused => "paused",
        GoalPhase::Blocked => "blocked",
        GoalPhase::Complete => "complete",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_session::session_id;

    #[test]
    fn create_then_get_round_trips() {
        let ctx = Context::new();
        let goals = GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let session = Session::new(session_id("s"));
        let created = goals.create(&session, " ship it ", Some(2)).unwrap();
        assert_eq!(created.snapshot.objective, "ship it");
        assert_eq!(created.snapshot.phase, GoalPhase::Active);
        assert_eq!(created.activation, GoalActivation::Armed);
        assert_eq!(created.rounds_started, 0);
        assert_eq!(created.snapshot.max_goal_rounds, 2);
        let loaded = goals.get(&session).unwrap();
        assert_eq!(loaded.snapshot.id, created.snapshot.id);
        assert_eq!(
            event_types(&session),
            vec!["goal/change".to_string()]
        );
    }

    #[test]
    fn duplicate_create_fails() {
        let ctx = Context::new();
        let goals = GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let session = Session::new(session_id("s"));
        goals.create(&session, "one", None).unwrap();
        let err = goals.create(&session, "two", None).unwrap_err();
        assert_eq!(err.code(), "GOAL_ALREADY_EXISTS");
    }

    #[test]
    fn stale_revision_is_rejected() {
        let ctx = Context::new();
        let goals = GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let session = Session::new(session_id("s"));
        let created = goals.create(&session, "one", None).unwrap();
        let err = goals
            .complete(
                &session,
                &GoalRef {
                    id: created.snapshot.id,
                    revision: 99,
                },
            )
            .unwrap_err();
        assert_eq!(err.code(), "GOAL_STALE_REVISION");
    }

    #[test]
    fn admitted_goal_message_increments_rounds() {
        let ctx = Context::new();
        let goals = GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let session = Session::new(session_id("s"));
        let created = goals.create(&session, "one", Some(3)).unwrap();
        session
            .append(
                SessionEventData::UserMessage(dsh_llm::UserMessage::goal_round(
                    "<goal_round>",
                    created.snapshot.id,
                    created.snapshot.revision,
                    1,
                )),
                Some(dsh_session::SurfaceOp::append()),
            )
            .unwrap();
        let view = goals.get(&session).unwrap();
        assert_eq!(view.rounds_started, 1);
    }

    fn event_types(session: &Session) -> Vec<String> {
        session
            .events()
            .into_iter()
            .map(|event| dsh_session::event_type_name(&event.data).to_string())
            .collect()
    }
}
