//! Model-facing `workflow` tool.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result};
use dsh_session::{session_id, SessionEventData};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use dsh_workflow::{validate_meta, WorkflowEngine, WorkflowRuntime, WorkflowStartRequest};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-workflow"
}

/// Consumer config.
#[derive(Debug, Clone)]
pub struct Config {
    /// Model-facing tool name.
    pub tool_name: String,
    /// Truncate rendered result after this many characters.
    pub max_result_chars: usize,
}

impl Config {
    /// Resolve plugin config.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let tool_name = value
            .and_then(|value| value.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("workflow")
            .to_string();
        let max_result_chars = match value.and_then(|value| value.get("maxResultChars")) {
            None => 50_000,
            Some(item) => {
                let number = item.as_u64().ok_or_else(|| {
                    "tool-workflow: maxResultChars must be a positive integer".to_string()
                })?;
                if number < 1 {
                    return Err("tool-workflow: maxResultChars must be a positive integer".into());
                }
                number as usize
            }
        };
        Ok(Self {
            tool_name,
            max_result_chars,
        })
    }
}

/// Render the success text.
pub fn render_result(name: &str, agent_count: u32, value: &Value, max_chars: usize) -> String {
    let noun = if agent_count == 1 { "agent" } else { "agents" };
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut text = format!("workflow \"{name}\" completed ({agent_count} {noun}).\nReturn value:\n{pretty}");
    if text.len() > max_chars {
        let omitted = text.len() - max_chars;
        text.truncate(max_chars);
        text.push_str(&format!("… [truncated: {omitted} more characters]"));
    }
    text
}

/// Register the workflow tool.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let engine = ctx.service::<WorkflowRuntime>()?;
    let agents = ctx.get::<AgentRegistry>();
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: "tool:workflow".into(),
            text: "Use the workflow tool ONLY when the user explicitly asks for a workflow or for large multi-agent orchestration. Pass identity in the meta argument, not in the script.".into(),
            order: 118,
        });
    }
    tools.insert(Arc::new(RunWorkflowTool {
        engine,
        agents,
        config,
    }));
    Ok(())
}

struct RunWorkflowTool {
    engine: Arc<WorkflowRuntime>,
    agents: Option<Arc<AgentRegistry>>,
    config: Config,
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        &self.config.tool_name
    }

    fn description(&self) -> &str {
        "Run a workflow script and return its result. Script is a top-level `return <json>` body; identity rides the meta argument."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": { "type": "string" },
                "meta": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "whenToUse": { "type": "string" }
                    },
                    "required": ["name", "description"]
                },
                "args": { "type": "object" }
            },
            "required": ["script", "meta"]
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
        let script = call
            .args
            .get("script")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("script required".into()))?;
        let meta_value = call
            .args
            .get("meta")
            .ok_or_else(|| ToolError::Body("meta required".into()))?;
        let meta = validate_meta(meta_value).map_err(|error| ToolError::Body(error.to_string()))?;
        let args = call.args.get("args").cloned();
        let run_id = Uuid::new_v4().to_string();
        if let Some(session) = self
            .agents
            .as_ref()
            .and_then(|agents| {
                call.agent_id
                    .as_deref()
                    .and_then(|id| agents.get(&session_id(id)))
            })
            .map(|agent| agent.session())
        {
            let _ = session.append(
                SessionEventData::Extension {
                    type_name: "tool-workflow/run-start".into(),
                    data: json!({ "runId": run_id, "name": meta.name }),
                },
                None,
            );
        }
        match self
            .engine
            .start(WorkflowStartRequest {
                script: script.to_string(),
                meta: meta.clone(),
                args,
            })
            .await
        {
            Ok(result) => {
                if let Some(session) = self
                    .agents
                    .as_ref()
                    .and_then(|agents| {
                        call.agent_id
                            .as_deref()
                            .and_then(|id| agents.get(&session_id(id)))
                    })
                    .map(|agent| agent.session())
                {
                    let _ = session.append(
                        SessionEventData::Extension {
                            type_name: "tool-workflow/run-end".into(),
                            data: json!({ "runId": run_id, "stopReason": result.stop_reason }),
                        },
                        None,
                    );
                }
                Ok(ToolOutcome::text(render_result(
                    &meta.name,
                    result.agent_count,
                    &result.value,
                    self.config.max_result_chars,
                )))
            }
            Err(error) => Ok(ToolOutcome::error(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_workflow::WorkflowRuntime;

    #[tokio::test]
    async fn runs_return_json() {
        let engine = Arc::new(WorkflowRuntime::new("in-process"));
        let tool = RunWorkflowTool {
            engine,
            agents: None,
            config: Config::resolve(None).unwrap(),
        };
        let outcome = tool
            .execute(json!({
                "script": "return {\"ok\":true}",
                "meta": { "name": "snapshot-flow", "description": "test" }
            }))
            .await
            .unwrap();
        let text = match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text,
            _ => panic!("text"),
        };
        assert!(text.contains("workflow \"snapshot-flow\" completed (0 agents)."));
        assert!(text.contains("\"ok\": true"));
    }
}
