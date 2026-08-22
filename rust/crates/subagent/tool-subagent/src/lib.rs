//! Model-facing `subagent` / `subagent_fork` over `ctx.subagents`.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_session::session_id;
use dsh_subagent::{SubagentRuntime, SubagentStartRequest};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-subagent"
}

/// Tool construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// `ctx.subagents` provider name.
    pub provider: String,
    /// Model-facing tool name.
    pub tool_name: String,
    /// `one-shot` or `continuable`.
    pub background_mode: String,
}

impl Config {
    /// Resolve plugin config. `provider` is required.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let provider = value
            .and_then(|value| value.get("provider"))
            .and_then(Value::as_str)
            .ok_or_else(|| "tool-subagent: provider is required".to_string())?
            .to_string();
        let tool_name = value
            .and_then(|value| value.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_string();
        let background_mode = value
            .and_then(|value| value.get("backgroundMode"))
            .and_then(Value::as_str)
            .unwrap_or("one-shot")
            .to_string();
        if background_mode != "one-shot" && background_mode != "continuable" {
            return Err(
                "tool-subagent: backgroundMode must be one-shot or continuable".into(),
            );
        }
        Ok(Self {
            provider,
            tool_name,
            background_mode,
        })
    }
}

/// Register one delegation tool.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    let inherits = subagents
        .get_provider(&config.provider)
        .map(|provider| provider.inherits_parent_context())
        .unwrap_or(false);
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: format!("tool:{}", config.tool_name),
            text: if inherits {
                "Delegate a task to a subagent that inherits this conversation's completed turns.".into()
            } else {
                "Delegate a self-contained task to a subagent. Give it a complete, standalone prompt: it does not see this conversation.".into()
            },
            order: 116,
        });
    }
    tools.insert(Arc::new(DelegateTool {
        subagents,
        config,
        inherits,
    }));
    Ok(())
}

struct DelegateTool {
    subagents: Arc<SubagentRuntime>,
    config: Config,
    inherits: bool,
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        &self.config.tool_name
    }

    fn description(&self) -> &str {
        if self.inherits {
            "Delegate a task to a subagent that inherits this conversation. It is seeded with all completed turns so far (not the current in-flight turn)."
        } else {
            "Delegate a self-contained task to a subagent. Give it a complete, standalone prompt: it does not see this conversation."
        }
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string" },
                "prompt": { "type": "string" },
                "run_in_background": { "type": "boolean" }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let description = call
            .args
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("description required".into()))?;
        let prompt = call
            .args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("prompt required".into()))?;
        let default_background = self.config.background_mode == "continuable";
        let background = call
            .args
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(default_background);
        if background {
            return Ok(ToolOutcome::error(
                "continuable background subagents are not mounted; set run_in_background to false"
                    .to_string(),
            ));
        }
        let parent_id = call
            .agent_id
            .as_deref()
            .map(session_id)
            .unwrap_or_else(|| session_id("unknown"));
        match self
            .subagents
            .start(
                &self.config.provider,
                SubagentStartRequest {
                    label: description.to_string(),
                    prompt: prompt.to_string(),
                    parent_id,
                    seed: None,
                },
            )
            .await
        {
            Ok(result) => Ok(ToolOutcome::text(result.output)),
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_requires_provider() {
        assert!(Config::resolve(None).is_err());
        let config = Config::resolve(Some(&json!({
            "provider": "spawn",
            "toolName": "subagent",
            "backgroundMode": "continuable"
        })))
        .unwrap();
        assert_eq!(config.provider, "spawn");
        assert_eq!(config.background_mode, "continuable");
    }
}
