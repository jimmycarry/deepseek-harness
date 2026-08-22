//! The child-scoped `report` tool for continuable subagents. Registered once
//! globally but scope-locked through [`dsh_tools::Tool::enabled_for`]: only a
//! resident continuable child sees it in schemas or may execute it, so roots,
//! one-shot children, and agentless executions never observe the registration.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result};
use dsh_llm::ContentBlock;
use dsh_session::session_id;
use dsh_subagent::SubagentRuntime;
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-subagent-report"
}

/// Deployment scheduling policy for accepted reports.
#[derive(Debug, Clone)]
pub struct Config {
    /// `next-step` wakes the parent at its nearest step boundary; `quiet`
    /// adds the same context without waking.
    pub report_delivery: String,
}

impl Config {
    /// Resolve plugin config; `reportDelivery` defaults to `next-step`.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let report_delivery = value
            .and_then(|value| value.get("reportDelivery"))
            .and_then(Value::as_str)
            .unwrap_or("next-step")
            .to_string();
        if report_delivery != "next-step" && report_delivery != "quiet" {
            return Err("tool-subagent-report: reportDelivery must be next-step or quiet".into());
        }
        Ok(Self { report_delivery })
    }
}

/// Register the scope-locked `report` tool.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    tools.insert(Arc::new(ReportTool {
        subagents,
        agents,
        delivery: config.report_delivery,
    }));
    Ok(())
}

struct ReportTool {
    subagents: Arc<SubagentRuntime>,
    agents: Arc<AgentRegistry>,
    delivery: String,
}

#[async_trait]
impl Tool for ReportTool {
    fn name(&self) -> &str {
        "report"
    }

    fn description(&self) -> &str {
        "Report selected content to the agent that started you. Call this once before you finish, with a \
self-contained final result, and earlier for progress or findings that change what that agent does \
next. That agent shares your workspace but does not automatically receive your transcript, tool \
output, or reasoning, so finishing your work is not itself a result. Reporting does not end your \
turn or finish your work, and only your direct parent receives it. A failed call may still have \
arrived, so do not blindly repeat it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "output": {
                    "type": "string",
                    "description": "Actionable content for your parent; summarize conclusions and reference relevant shared paths.",
                },
            },
            "required": ["output"]
        })
    }

    fn enabled_for(&self, agent_id: Option<&str>) -> bool {
        agent_id.is_some_and(|id| self.subagents.is_resident_continuable(id))
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
        let output = call
            .args
            .get("output")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("output required".into()))?;
        let agent = call
            .agent_id
            .as_deref()
            .and_then(|id| self.agents.get(&session_id(id)))
            .ok_or_else(|| {
                ToolError::Body("report requires a calling agent (exec.agent was undefined)".into())
            })?;
        match self
            .subagents
            .report_from(&agent, vec![ContentBlock::text(output)], &self.delivery)
        {
            Ok(message_id) => Ok(ToolOutcome::text(format!(
                "report accepted by the agent that started you as message {message_id}"
            ))),
            Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_unknown_delivery() {
        let error = Config::resolve(Some(&json!({ "reportDelivery": "loud" }))).unwrap_err();
        assert!(error.contains("reportDelivery"));
        let config = Config::resolve(None).unwrap();
        assert_eq!(config.report_delivery, "next-step");
    }
}
