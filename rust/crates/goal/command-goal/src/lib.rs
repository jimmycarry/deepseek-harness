//! Human-facing `/goal` command over `ctx.goals`.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_commands::{Command, CommandHandler, CommandRegistry};
use dsh_cordis::{Context, Result};
use dsh_goal::{GoalRef, GoalService, GoalView};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-command-goal"
}

const USAGE: &str = "Usage: /goal [<objective>|clear|edit <objective>|pause|resume]";

/// Register `/goal`. It is not model-visible.
pub fn install(ctx: &Context) -> Result<()> {
    let commands = ctx.service::<CommandRegistry>()?;
    let goals = ctx.service::<GoalService>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    commands.register(
        ctx,
        Command {
            name: "goal".into(),
            description: "set or view the goal for a long-running task".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(GoalCommand { goals, agents }),
        },
    )
}

struct GoalCommand {
    goals: Arc<GoalService>,
    agents: Arc<AgentRegistry>,
}

enum Parsed {
    Show,
    Create(String),
    Edit(String),
    InvalidEdit,
    Pause,
    Resume,
    Clear,
}

fn parse(raw: &str) -> Parsed {
    let input = raw.trim();
    if input.is_empty() {
        return Parsed::Show;
    }
    let control = input.to_ascii_lowercase();
    if control == "clear" {
        return Parsed::Clear;
    }
    if control == "pause" {
        return Parsed::Pause;
    }
    if control == "resume" {
        return Parsed::Resume;
    }
    if control == "edit" {
        return Parsed::InvalidEdit;
    }
    if let Some(rest) = input.strip_prefix("edit ") {
        return Parsed::Edit(rest.trim().to_string());
    }
    if let Some(rest) = input.strip_prefix("EDIT ") {
        return Parsed::Edit(rest.trim().to_string());
    }
    Parsed::Create(input.to_string())
}

fn render(title: &str, goal: &GoalView) -> String {
    let mut lines = vec![
        title.to_string(),
        format!("Status: {:?}", goal.snapshot.phase).replace("Active", "active")
            .replace("Paused", "paused")
            .replace("Blocked", "blocked")
            .replace("Complete", "complete"),
    ];
    if let Some(reason) = &goal.snapshot.blocked_reason {
        lines.push(format!("Blocker: {}: {}", reason.code, reason.message));
    }
    lines.push(format!("Objective: {}", goal.snapshot.objective));
    lines.push(format!(
        "Rounds: {}/{}",
        goal.rounds_started, goal.snapshot.max_goal_rounds
    ));
    lines.push(format!("Activation: {}", goal.activation.as_str()));
    lines.push(String::new());
    lines.push(format!("Commands: {USAGE}"));
    lines.join("\n")
}

fn cas(goal: &GoalView) -> GoalRef {
    GoalRef {
        id: goal.snapshot.id.clone(),
        revision: goal.snapshot.revision,
    }
}

#[async_trait]
impl CommandHandler for GoalCommand {
    async fn handle(&self, args: &str) -> std::result::Result<String, String> {
        let Some(agent) = self.agents.live().into_iter().next() else {
            return Err("no live agent".into());
        };
        let session = agent.session();
        match parse(args) {
            Parsed::Show => match self.goals.get(session.as_ref()) {
                Some(goal) => Ok(render("Current goal", &goal)),
                None => Ok(USAGE.into()),
            },
            Parsed::InvalidEdit => Err("edit requires an objective".into()),
            Parsed::Create(objective) => {
                let goal = self
                    .goals
                    .create(session.as_ref(), &objective, None)
                    .map_err(|error| error.to_string())?;
                Ok(render("Goal created", &goal))
            }
            Parsed::Edit(objective) => {
                let current = self.goals.get(session.as_ref()).ok_or("no current goal")?;
                let goal = self
                    .goals
                    .edit(session.as_ref(), &cas(&current), Some(&objective), None)
                    .map_err(|error| error.to_string())?;
                Ok(render("Goal updated", &goal))
            }
            Parsed::Pause => {
                let current = self.goals.get(session.as_ref()).ok_or("no current goal")?;
                let goal = self
                    .goals
                    .pause(session.as_ref(), &cas(&current))
                    .map_err(|error| error.to_string())?;
                Ok(render("Goal paused", &goal))
            }
            Parsed::Resume => {
                let current = self.goals.get(session.as_ref()).ok_or("no current goal")?;
                let goal = self
                    .goals
                    .resume(session.as_ref(), &cas(&current))
                    .map_err(|error| error.to_string())?;
                Ok(render("Goal resumed", &goal))
            }
            Parsed::Clear => {
                let current = self.goals.get(session.as_ref()).ok_or("no current goal")?;
                self.goals
                    .clear(session.as_ref(), &cas(&current))
                    .map_err(|error| error.to_string())?;
                Ok("Goal cleared".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::AgentRegistry;
    use dsh_cordis::Context;
    use dsh_goal::Config;

    #[tokio::test]
    async fn command_is_not_model_visible() {
        let ctx = Context::new();
        ctx.provide(Arc::new(CommandRegistry::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        GoalService::install(&ctx, Config::resolve(None).unwrap()).unwrap();
        install(&ctx).unwrap();
        let command = ctx.service::<CommandRegistry>().unwrap().get("goal").unwrap();
        assert!(!command.model_visible);
    }
}
