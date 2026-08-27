//! Model-facing `get_goal`, `create_goal`, and `update_goal`.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result};
use dsh_goal::{GoalActivation, GoalBlockReason, GoalError, GoalRef, GoalService, GoalView};
use dsh_llm::MessageSource;
use dsh_session::{Session, SessionEventData, session_id};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-goal"
}

/// Default minimum admitted rounds before a goal-round may self-report blocked.
pub const DEFAULT_BLOCKED_AFTER: u32 = 3;

/// Tool policy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Minimum admitted goal rounds before `blocked` is allowed from a goal round.
    pub blocked_after_consecutive_rounds: u32,
}

impl Config {
    /// Resolve plugin config.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let blocked_after_consecutive_rounds =
            match value.and_then(|value| value.get("blockedAfterConsecutiveRounds")) {
                None => DEFAULT_BLOCKED_AFTER,
                Some(item) => {
                    let number = item.as_u64().ok_or_else(|| {
                        "tool-goal: blockedAfterConsecutiveRounds must be a positive integer"
                            .to_string()
                    })?;
                    if number < 1 {
                        return Err(
                            "tool-goal: blockedAfterConsecutiveRounds must be a positive integer"
                                .into(),
                        );
                    }
                    number as u32
                }
            };
        Ok(Self {
            blocked_after_consecutive_rounds,
        })
    }
}

/// Register the three tools and the `tool:goal` prompt section.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let goals = ctx.service::<GoalService>()?;
    let tools = ctx.service::<ToolRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: "tool:goal".into(),
            text: guidance(config.blocked_after_consecutive_rounds),
            order: 114,
        });
    }
    tools.insert(Arc::new(GetGoalTool {
        goals: Arc::clone(&goals),
        agents: Arc::clone(&agents),
    }));
    tools.insert(Arc::new(CreateGoalTool {
        goals: Arc::clone(&goals),
        agents: Arc::clone(&agents),
    }));
    tools.insert(Arc::new(UpdateGoalTool {
        goals,
        agents,
        blocked_after: config.blocked_after_consecutive_rounds,
    }));
    Ok(())
}

fn guidance(blocked_after: u32) -> String {
    format!(
        "Use create_goal when the current human request is a long-running objective that should continue across autonomous goal rounds. Call get_goal before update_goal. complete and blocked require a direct human turn or the current goal round. A goal-round blocked report is rejected until at least {blocked_after} consecutive goal rounds have been admitted."
    )
}

fn compact(view: Option<&GoalView>) -> String {
    match view {
        None => json!({ "goal": null }).to_string(),
        Some(view) => view.tool_json().to_string(),
    }
}

fn map_error(error: GoalError) -> ToolError {
    ToolError::Body(format!("Error: {error}"))
}

fn require_agent<'a>(
    agents: &'a AgentRegistry,
    call: &ToolCall,
) -> std::result::Result<Arc<dyn dsh_agent::Agent>, ToolError> {
    let id = call
        .agent_id
        .as_deref()
        .ok_or_else(|| ToolError::Body("Error: goal tools require a calling agent".into()))?;
    agents
        .get(&session_id(id))
        .ok_or_else(|| ToolError::Body("Error: goal tools require a calling agent".into()))
}

fn latest_human(session: &Session) -> bool {
    session.events().iter().rev().any(|event| {
        matches!(
            &event.data,
            SessionEventData::UserMessage(message) if matches!(message.source, MessageSource::User)
        )
    })
}

fn current_goal_round(session: &Session, view: &GoalView) -> bool {
    session.events().iter().rev().any(|event| match &event.data {
        SessionEventData::UserMessage(message) => match &message.source {
            MessageSource::Goal {
                goal_id,
                revision,
                round,
            } => {
                *goal_id == view.snapshot.id
                    && *revision == view.snapshot.revision
                    && *round == view.rounds_started
                    && *round > 0
            }
            _ => false,
        },
        _ => false,
    })
}

struct GetGoalTool {
    goals: Arc<GoalService>,
    agents: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "Read the current same-session goal, including its exact id/revision, objective, phase, completed continuation rounds, round limit, blocker reason when present, and whether another continuation is armed. Call this before updating a goal."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let agent = require_agent(&self.agents, call)?;
        Ok(ToolOutcome::text(compact(self.goals.get(agent.session().as_ref()).as_ref())))
    }
}

struct CreateGoalTool {
    goals: Arc<GoalService>,
    agents: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "Create one persisted same-session completion goal when the current direct human request is a long-running objective that should continue across autonomous goal rounds. You may infer that intent without requiring the user to say \"create a goal\". Do not use this for trivial single-turn work. Execution rejects non-human and subagent authority."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string" },
                "max_goal_rounds": { "type": "number" }
            },
            "required": ["objective"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let agent = require_agent(&self.agents, call)?;
        let session = agent.session();
        if !latest_human(session.as_ref()) {
            return Err(ToolError::Body(
                "Error: this goal operation requires a direct human turn on a top-level agent".into(),
            ));
        }
        let objective = call
            .args
            .get("objective")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("Error: objective is required".into()))?;
        let max_goal_rounds = match call.args.get("max_goal_rounds") {
            None | Some(Value::Null) => None,
            Some(Value::Number(number)) if number.as_u64() == Some(0) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| ToolError::Body("Error: max_goal_rounds must be a number".into()))?
                    as u32,
            ),
        };
        match self.goals.create(session.as_ref(), objective, max_goal_rounds) {
            Ok(view) => Ok(ToolOutcome::text(compact(Some(&view)))),
            Err(error) => Err(map_error(error)),
        }
    }
}

struct UpdateGoalTool {
    goals: Arc<GoalService>,
    agents: Arc<AgentRegistry>,
    blocked_after: u32,
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "Compare-and-set update of the current same-session goal. edit, pause, and resume require a direct human turn. complete and blocked require a direct human turn or the current goal round."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string" },
                "revision": { "type": "number" },
                "action": { "type": "string", "enum": ["edit", "pause", "resume", "complete", "blocked"] },
                "objective": { "type": "string" },
                "max_goal_rounds": { "type": "number" },
                "blocked_reason": { "type": "string" }
            },
            "required": ["goal_id", "revision", "action"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().into(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let agent = require_agent(&self.agents, call)?;
        let session = agent.session();
        let goal_id = call
            .args
            .get("goal_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("Error: goal_id is required".into()))?;
        let revision = call
            .args
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::Body("Error: revision is required".into()))?;
        let action = call
            .args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("Error: action is required".into()))?;
        let reference = GoalRef {
            id: goal_id.into(),
            revision,
        };
        let current = self.goals.get(session.as_ref());
        let human = latest_human(session.as_ref());
        let goal_round = current
            .as_ref()
            .map(|view| current_goal_round(session.as_ref(), view))
            .unwrap_or(false);
        let result = match action {
            "edit" | "pause" | "resume" => {
                if !human {
                    return Err(ToolError::Body(
                        "Error: this goal operation requires a direct human turn on a top-level agent"
                            .into(),
                    ));
                }
                match action {
                    "edit" => {
                        let objective = nonempty_text(call.args.get("objective"));
                        let max_goal_rounds = match call.args.get("max_goal_rounds") {
                            None | Some(Value::Null) => None,
                            Some(Value::Number(number)) if number.as_u64() == Some(0) => None,
                            Some(value) => value.as_u64().map(|n| n as u32),
                        };
                        self.goals.edit(
                            session.as_ref(),
                            &reference,
                            objective,
                            max_goal_rounds,
                        )
                    }
                    "pause" => self.goals.pause(session.as_ref(), &reference),
                    _ => self.goals.resume(session.as_ref(), &reference),
                }
            }
            "complete" | "blocked" => {
                if !human && !goal_round {
                    return Err(ToolError::Body(
                        "Error: complete and blocked require a direct human turn or the current goal round"
                            .into(),
                    ));
                }
                if action == "blocked" {
                    if goal_round {
                        if let Some(view) = &current {
                            if view.rounds_started < self.blocked_after {
                                return Err(ToolError::Body(format!(
                                    "Error: blocked requires at least {} consecutive goal rounds; current round is {}",
                                    self.blocked_after, view.rounds_started
                                )));
                            }
                        }
                    }
                    let reason = nonempty_text(call.args.get("blocked_reason")).ok_or_else(|| {
                        ToolError::Body("Error: blocked requires blocked_reason".into())
                    })?;
                    self.goals.block(
                        session.as_ref(),
                        &reference,
                        GoalBlockReason {
                            code: "model-reported".into(),
                            message: reason.to_string(),
                        },
                    )
                } else {
                    self.goals.complete(session.as_ref(), &reference)
                }
            }
            other => {
                return Err(ToolError::Body(format!(
                    "Error: unknown update action {other}"
                )))
            }
        };
        match result {
            Ok(view) => {
                if goal_round
                    && matches!(view.activation, GoalActivation::Disarmed)
                    && matches!(action, "complete" | "blocked")
                {
                    agent.steer(dsh_llm::UserMessage::notice(
                        "tool-goal",
                        wrapup(&view, action),
                        format!("{action}: {}", view.snapshot.objective),
                    ));
                }
                Ok(ToolOutcome::text(compact(Some(&view))))
            }
            Err(error) => Err(map_error(error)),
        }
    }
}

fn nonempty_text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn wrapup(view: &GoalView, action: &str) -> String {
    if action == "blocked" {
        let reason = view
            .snapshot
            .blocked_reason
            .as_ref()
            .map(|reason| format!("{}: {}", reason.code, reason.message))
            .unwrap_or_else(|| "blocked".into());
        format!(
            "<goal_blocked>\nObjective: {}\nReason: {reason}\nThe goal is blocked. Summarize the blocker and stop. Do not call any more tools in this run.\n</goal_blocked>",
            view.snapshot.objective
        )
    } else {
        format!(
            "<goal_complete>\nObjective: {}\nThe goal is complete. Summarize what was achieved and stop. Do not call any more tools in this run.\n</goal_complete>",
            view.snapshot.objective
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::AgentRegistry;
    use dsh_cordis::Context;
    use dsh_goal::Config as GoalConfig;
    use dsh_session::SessionStore;
    use dsh_tools::ToolRuntime;

    #[test]
    fn install_registers_three_tools() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        GoalService::install(&ctx, GoalConfig::resolve(None).unwrap()).unwrap();
        install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let names: Vec<_> = ctx
            .service::<ToolRuntime>()
            .unwrap()
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        assert!(names.contains(&"get_goal".into()));
        assert!(names.contains(&"create_goal".into()));
        assert!(names.contains(&"update_goal".into()));
    }
}
