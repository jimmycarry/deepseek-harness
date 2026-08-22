//! Queue admitted `<goal_round>` followups for an armed active goal.

use dsh_agent::{AgentRegistry, AgentStatus};
use dsh_cordis::{Context, Result};
use dsh_goal::{GoalActivation, GoalBlockReason, GoalPhase, GoalRef, GoalService, GoalView};
use dsh_llm::UserMessage;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-goal-round-driver"
}

/// Model-visible continuation prompt for one same-session goal round.
pub fn render_goal_round_prompt(goal: &GoalView, round: u32) -> String {
    format!(
        "<goal_round>\nObjective: {}\nRound: {round}/{}\n\nContinue working toward the objective in this same session. Treat the current workspace, tool results, and durable session state as authoritative; inspect them instead of assuming earlier narration is still current. Make concrete progress and verify the result. Before claiming completion, gather evidence that the whole objective is achieved, read the current goal, and mark it complete. If work remains, leave the goal active for the next round. Follow the configured goal-tool policy before reporting a blocker.\n</goal_round>",
        serde_json::to_string(&goal.snapshot.objective).unwrap_or_else(|_| "\"\"".into()),
        goal.snapshot.max_goal_rounds
    )
}

/// Install driver listeners. On load, every existing agent is disarmed.
pub fn install(ctx: &Context) -> Result<()> {
    let goals = ctx.service::<GoalService>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    for agent in agents.live() {
        goals.disarm(agent.session().as_ref());
    }
    let queued: Arc<Mutex<HashMap<String, (String, u64, u32)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let drive_goals = Arc::clone(&goals);
    let drive_agents = Arc::clone(&agents);
    let drive_queued = Arc::clone(&queued);
    ctx.on("goal/changed", move |payload| {
        drive(
            &drive_goals,
            &drive_agents,
            &drive_queued,
            payload.get("sessionId").and_then(Value::as_str),
        );
    })?;
    Ok(())
}

fn drive(
    goals: &GoalService,
    agents: &AgentRegistry,
    queued: &Mutex<HashMap<String, (String, u64, u32)>>,
    only: Option<&str>,
) {
    let live = agents.live();
    for agent in live {
        if only.is_some() && only != Some(agent.id().as_str()) {
            continue;
        }
        if agent.status() == AgentStatus::Running {
            // Mid-turn: queue for the next turn after the current one settles.
        }
        let session = agent.session();
        let Some(goal) = goals.get(session.as_ref()) else {
            continue;
        };
        if goal.activation != GoalActivation::Armed || goal.snapshot.phase != GoalPhase::Active {
            continue;
        }
        if goal.rounds_started >= goal.snapshot.max_goal_rounds {
            let _ = goals.block(
                session.as_ref(),
                &GoalRef {
                    id: goal.snapshot.id.clone(),
                    revision: goal.snapshot.revision,
                },
                GoalBlockReason {
                    code: "round-limit".into(),
                    message: format!(
                        "Goal reached its configured limit of {} rounds.",
                        goal.snapshot.max_goal_rounds
                    ),
                },
            );
            continue;
        }
        let next_round = goal.rounds_started + 1;
        let key = (
            goal.snapshot.id.clone(),
            goal.snapshot.revision,
            next_round,
        );
        {
            let mut map = queued.lock().expect("queued");
            if map.get(agent.id().as_str()) == Some(&key) {
                continue;
            }
            map.insert(agent.id().as_str().to_string(), key);
        }
        agent.followup(UserMessage::goal_round(
            render_goal_round_prompt(&goal, next_round),
            goal.snapshot.id,
            goal.snapshot.revision,
            next_round,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_goal::{Config, GoalService};
    use dsh_session::{session_id, Session};

    #[test]
    fn prompt_quotes_objective() {
        let ctx = dsh_cordis::Context::new();
        let goals = GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let session = Session::new(session_id("s"));
        let goal = goals.create(&session, "ship", Some(2)).unwrap();
        let text = render_goal_round_prompt(&goal, 1);
        assert!(text.contains("<goal_round>"));
        assert!(text.contains("\"ship\""));
        assert!(text.contains("Round: 1/2"));
    }
}
