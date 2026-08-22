//! The globally named `send_message`, `interrupt_agent`, and `list_agents`
//! tools: thin model-facing adapters over the continuable-subagent service.
//! They perform no lifecycle routing of their own and live apart from the
//! provider-bound delegation tools so multiple delegation tools share one
//! control API. `list_agents` installs separately so a deployment can register
//! continuation delivery without exposing discovery.

use async_trait::async_trait;
use dsh_agent::{Agent, AgentRegistry};
use dsh_cordis::{Context, Result};
use dsh_llm::{ContentBlock, MessageSource};
use dsh_session::session_id;
use dsh_subagent::SubagentRuntime;
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name for the control pair.
pub fn name() -> &'static str {
    "dsh-tool-subagent-control"
}

/// Register the `send_message` and `interrupt_agent` tools.
pub fn install(ctx: &Context) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    tools.insert(Arc::new(SendMessageTool {
        subagents: Arc::clone(&subagents),
        agents: Arc::clone(&agents),
    }));
    tools.insert(Arc::new(InterruptAgentTool { subagents, agents }));
    Ok(())
}

/// Register the `list_agents` tool.
pub fn install_list_agents(ctx: &Context) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    tools.insert(Arc::new(ListAgentsTool { subagents, agents }));
    Ok(())
}

/// Resolve the exact live calling agent, or the TS-parity failure text.
fn calling_agent(
    agents: &AgentRegistry,
    call: &ToolCall,
    tool: &str,
) -> std::result::Result<Arc<dyn Agent>, ToolError> {
    call.agent_id
        .as_deref()
        .and_then(|id| agents.get(&session_id(id)))
        .ok_or_else(|| {
            ToolError::Body(format!(
                "{tool} requires a calling agent (exec.agent was undefined)"
            ))
        })
}

struct SendMessageTool {
    subagents: Arc<SubagentRuntime>,
    agents: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to a background subagent by its subagent id, continuing the same conversation. It \
becomes the subagent's next turn: if it is still working, the message waits until its current turn \
finishes, so it cannot redirect work already underway. This call returns no answer from the \
subagent — only confirmation that the message was delivered — so use it to give it more work. A \
failure means the message was NOT delivered."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent_id": {
                    "type": "string",
                    "description": "The subagent id returned when the background subagent was started.",
                },
                "message": {
                    "type": "string",
                    "description": "The message to deliver to the subagent.",
                },
            },
            "required": ["subagent_id", "message"]
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
        let parent = calling_agent(&self.agents, call, "send_message")?;
        let subagent_id = call
            .args
            .get("subagent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("subagent_id required".into()))?;
        let message = call
            .args
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("message required".into()))?;
        match self.subagents.followup(
            &parent,
            &session_id(subagent_id),
            vec![ContentBlock::text(message)],
            MessageSource::coordinator(parent.id().as_str()),
        ) {
            Ok(_message_id) => Ok(ToolOutcome::text(format!(
                "message queued as the next turn for subagent {subagent_id}"
            ))),
            Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
        }
    }
}

struct InterruptAgentTool {
    subagents: Arc<SubagentRuntime>,
    agents: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for InterruptAgentTool {
    fn name(&self) -> &str {
        "interrupt_agent"
    }

    fn description(&self) -> &str {
        "Request cancellation of a background agent's current turn by its agent id. The target may be your \
direct child or a deeper agent created under you. Only the current turn stops: messages already \
queued for the agent stay parked until a later send_message, agents it started keep running, and \
the agent itself stays available for follow-ups. This call returns as soon as the stop request is \
accepted, so the target may keep running briefly; interrupting an agent that already finished is \
an accepted no-op."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The agent id of the running agent to interrupt.",
                },
            },
            "required": ["agent_id"]
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
        let caller = calling_agent(&self.agents, call, "interrupt_agent")?;
        let agent_id = call
            .args
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("agent_id required".into()))?;
        match self.subagents.interrupt(&session_id(agent_id), &caller) {
            Ok(()) => Ok(ToolOutcome::text(format!(
                "interrupt requested for agent {agent_id}"
            ))),
            Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
        }
    }
}

struct ListAgentsTool {
    subagents: Arc<SubagentRuntime>,
    agents: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> &str {
        "List your continuable background subagents by durable id and label. Use it to recall which ones \
you started, not to poll for completion — you are told when one finishes. Status comes from the live \
registry: running means the agent is working right now, idle means it is loaded but between turns \
(it may be waiting on agents it started), and ready means it exists only in storage — resumable, not \
terminal, and not a result waiting to be collected; a `send_message` starts a new turn on the same \
conversation, and a direct child remains a `send_message` candidate in every status. The snapshot is not a delivery \
promise — `send_message` performs the authoritative check and may still fail. Children that could \
not be read are reported as diagnostics instead of being silently dropped. Scope `descendants` \
walks the whole tree below you in stable pre-order, annotating each entry with its durable direct-parent \
session id and depth. You may use `send_message` only for depth-1 entries; deeper entries are \
candidates for `interrupt_agent` only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["children", "descendants"],
                    "description": "children (default) lists direct children only; descendants walks the complete tree below you.",
                },
            },
            "required": []
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
        let parent = calling_agent(&self.agents, call, "list_agents")?;
        let scope = call
            .args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("children");
        let entries = match scope {
            "descendants" => self.subagents.list_descendants(parent.id()),
            _ => self.subagents.list_children(parent.id()),
        };
        // One-shot children cannot be continued by send_message, so the model
        // never selects them; discovery still traversed them for descendants.
        let lines: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.mode == "continuable")
            .map(|entry| {
                let at = if scope == "descendants" {
                    format!(" parent={} depth={}", entry.parent, entry.depth)
                } else {
                    String::new()
                };
                format!(
                    "{} [{}]{} — {}",
                    entry.id,
                    self.subagents.status_of(&entry.id),
                    at,
                    entry.label
                )
            })
            .collect();
        Ok(ToolOutcome::text(if lines.is_empty() {
            "(no subagents)".to_string()
        } else {
            lines.join("\n")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_message_without_agent_fails() {
        let subagents = Arc::new(SubagentRuntime::new());
        let agents = Arc::new(AgentRegistry::new());
        let tool = SendMessageTool { subagents, agents };
        let error = tool
            .execute(json!({ "subagent_id": "x", "message": "hi" }))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "send_message requires a calling agent (exec.agent was undefined)"
        );
    }
}
