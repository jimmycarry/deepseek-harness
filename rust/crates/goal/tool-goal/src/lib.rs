//! Model-facing goal tool.

use async_trait::async_trait;
use dsh_commands::{Command, CommandHandler, CommandRegistry};
use dsh_cordis::Context;
use dsh_goal::GoalRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `set_goal` over [`GoalRuntime`].
pub struct SetGoalTool {
    goals: Arc<GoalRuntime>,
}

impl SetGoalTool {
    /// Bind to `ctx.goal`.
    pub fn new(goals: Arc<GoalRuntime>) -> Self {
        Self { goals }
    }
}

#[async_trait]
impl Tool for SetGoalTool {
    fn name(&self) -> &str {
        "set_goal"
    }

    fn description(&self) -> &str {
        "Set the current same-session goal objective."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "objective": { "type": "string" } },
            "required": ["objective"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("objective required".into()))?;
        self.goals.set(objective);
        Ok(ToolOutcome::text(format!("goal set: {objective}")))
    }
}

struct GoalCommand {
    goals: Arc<GoalRuntime>,
}

#[async_trait]
impl CommandHandler for GoalCommand {
    async fn handle(&self, args: &str) -> Result<String, String> {
        let args = args.trim();
        if args.is_empty() {
            return Ok(self
                .goals
                .get()
                .unwrap_or_else(|| "Usage: /goal <objective>".into()));
        }
        self.goals.set(args);
        Ok(format!("goal set: {args}"))
    }
}

/// Register `/goal` as a human command that is not model-visible.
pub fn install_command(
    ctx: &Context,
    commands: &CommandRegistry,
    goals: Arc<GoalRuntime>,
) -> dsh_cordis::Result<()> {
    commands.register(
        ctx,
        Command {
            name: "goal".into(),
            description: "Set a same-session goal".into(),
            model_visible: false,
            handler: Arc::new(GoalCommand { goals }),
        },
    )
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-goal"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[tokio::test]
    async fn install_command_is_not_model_visible() {
        let ctx = Context::new();
        let commands = Arc::new(CommandRegistry::new());
        let goals = Arc::new(GoalRuntime::new());
        ctx.provide(Arc::clone(&commands)).unwrap();
        install_command(&ctx, &commands, Arc::clone(&goals)).unwrap();
        let goal = commands.get("goal").unwrap();
        assert!(!goal.model_visible);
        assert_eq!(
            commands.dispatch("/goal ship it").await.unwrap().unwrap(),
            "goal set: ship it"
        );
        assert_eq!(goals.get().as_deref(), Some("ship it"));
    }

    #[tokio::test]
    async fn set_goal_tool_writes_runtime() {
        let goals = Arc::new(GoalRuntime::new());
        let tool = SetGoalTool::new(Arc::clone(&goals));
        tool.execute(serde_json::json!({ "objective": "finish" }))
            .await
            .unwrap();
        assert_eq!(goals.get().as_deref(), Some("finish"));
    }
}
