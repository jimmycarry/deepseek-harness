//! Model-facing skill tool.

use async_trait::async_trait;
use dsh_skill::SkillRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `skill` loader over [`SkillRuntime`].
pub struct LoadSkillTool {
    skills: Arc<SkillRuntime>,
}

impl LoadSkillTool {
    /// Bind to `ctx.skills`.
    pub fn new(skills: Arc<SkillRuntime>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a registered skill body by name."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("name required".into()))?;
        match self.skills.get(name) {
            Some(skill) => Ok(ToolOutcome::text(skill.body)),
            None => Ok(ToolOutcome::error(format!("unknown skill `{name}`"))),
        }
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-skill"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_skill::Skill;

    #[tokio::test]
    async fn loads_registered_skill() {
        let skills = Arc::new(SkillRuntime::new());
        skills.register(Skill {
            name: "review".into(),
            body: "do a review".into(),
        });
        let tool = LoadSkillTool::new(skills);
        let outcome = tool
            .execute(serde_json::json!({ "name": "review" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
    }
}
