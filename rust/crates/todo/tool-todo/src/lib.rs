//! `todo_write` tool: the model replaces its structured todo list and the
//! complete snapshot lands as a log-only `todo/write` session event.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::Context;
use dsh_session::{session_id, SessionEventData};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::Value;
use std::sync::Arc;

/// Deployment policy; the parallel-progress rule is Config, never a default.
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether several todos may be `in_progress` at once.
    pub allow_parallel_in_progress: bool,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Missing or non-boolean `allowParallelInProgress`.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let allow = config
            .and_then(|value| value.get("allowParallelInProgress"))
            .and_then(Value::as_bool)
            .ok_or_else(|| "tool-todo: allowParallelInProgress must be a boolean".to_string())?;
        Ok(Self {
            allow_parallel_in_progress: allow,
        })
    }
}

const STATUSES: &[&str] = &["pending", "in_progress", "completed"];

/// `todo_write` bound to the calling agent through `ctx.agents`.
pub struct TodoWriteTool {
    agents: Arc<AgentRegistry>,
    allow_parallel: bool,
    description: String,
}

impl TodoWriteTool {
    /// Build with the deployment's parallel-progress rule.
    pub fn new(agents: Arc<AgentRegistry>, allow_parallel: bool) -> Self {
        let clause = if allow_parallel {
            "Mark every todo being actively worked on `in_progress` — several at once when work genuinely proceeds in parallel."
        } else {
            "Keep AT MOST ONE todo `in_progress` at a time; finish or park others first."
        };
        let description = format!(
            "Record and update a structured task list for the current work. Each call replaces the whole list. {clause} Mark todos `completed` immediately when done."
        );
        Self {
            agents,
            allow_parallel,
            description,
        }
    }
}

/// Validate one submitted todo list against the structural and progress rules.
fn validate(todos: &[Value], allow_parallel: bool) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut active = 0usize;
    for todo in todos {
        let content = todo.get("content").and_then(Value::as_str).unwrap_or("");
        if content.trim().is_empty() {
            return Err("invalid todo: `content` must be a non-empty string".into());
        }
        if seen.contains(&content) {
            return Err(format!(
                "invalid todos: duplicate content {}",
                serde_json::to_string(content).unwrap_or_default()
            ));
        }
        seen.push(content);
        let status = todo.get("status").and_then(Value::as_str).unwrap_or("");
        if !STATUSES.contains(&status) {
            return Err(format!(
                "invalid todo: `status` must be one of pending, in_progress, completed (got {status:?})"
            ));
        }
        if status == "in_progress" {
            active += 1;
        }
    }
    if !allow_parallel && active > 1 {
        return Err(format!(
            "invalid todos: at most one task may be in_progress (got {active})"
        ));
    }
    Ok(())
}

fn count(todos: &[Value], status: &str) -> usize {
    todos
        .iter()
        .filter(|todo| todo.get("status").and_then(Value::as_str) == Some(status))
        .count()
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        Err(ToolError::Body(
            "Error: todo_write requires an owning agent session".into(),
        ))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let agent = call
            .agent_id
            .as_deref()
            .and_then(|id| self.agents.get(&session_id(id)))
            .ok_or_else(|| {
                ToolError::Body("Error: todo_write requires an owning agent session".into())
            })?;
        let todos: Vec<Value> = call
            .args
            .get("todos")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                ToolError::Body("Error: invalid todos: `todos` must be an array".into())
            })?;
        validate(&todos, self.allow_parallel)
            .map_err(|message| ToolError::Body(format!("Error: {message}")))?;
        agent
            .session()
            .append(
                SessionEventData::TodoWrite {
                    todos: Value::Array(todos.clone()),
                },
                None,
            )
            .map_err(|error| ToolError::Body(error.to_string()))?;
        Ok(ToolOutcome::text(format!(
            "Updated todo list: {} pending, {} in progress, {} completed.",
            count(&todos, "pending"),
            count(&todos, "in_progress"),
            count(&todos, "completed"),
        )))
    }
}

/// Register `todo_write` on `ctx.tools`.
///
/// # Errors
/// Missing `ctx.tools` or `ctx.agents`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    tools.insert(Arc::new(TodoWriteTool::new(
        agents,
        config.allow_parallel_in_progress,
    )));
    Ok(())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-todo"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentStatus, Inbox, InboxTarget,
    };
    use dsh_session::{session_id, Session};

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn id(&self) -> &dsh_session::SessionId {
            self.session.id()
        }
        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.session)
        }
        fn inbox(&self) -> Arc<Inbox> {
            Arc::clone(&self.inbox)
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn send(&self, _: dsh_llm::UserMessage, _: InboxTarget, _: bool) {}
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    struct StubFactory;

    impl AgentFactory for StubFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(StubAgent {
                session,
                inbox: Arc::new(Inbox::default()),
            })
        }
    }

    fn agent_call(args: Value, agent_id: Option<&str>) -> ToolCall {
        ToolCall {
            name: "todo_write".into(),
            args,
            agent_id: agent_id.map(str::to_string),
        }
    }

    fn registry_with_agent(id: &str) -> Arc<AgentRegistry> {
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        agents
            .create(Arc::new(Session::new(session_id(id))))
            .unwrap();
        Arc::new(agents)
    }

    #[test]
    fn config_requires_boolean() {
        assert!(Config::resolve(None).is_err());
        assert!(Config::resolve(Some(&serde_json::json!({}))).is_err());
        let config = Config::resolve(Some(
            &serde_json::json!({ "allowParallelInProgress": true }),
        ))
        .unwrap();
        assert!(config.allow_parallel_in_progress);
    }

    #[tokio::test]
    async fn writes_snapshot_and_renders_counts() {
        let agents = registry_with_agent("todo");
        let tool = TodoWriteTool::new(Arc::clone(&agents), true);
        let outcome = tool
            .execute_call(&agent_call(
                serde_json::json!({
                    "todos": [
                        { "content": "ship", "status": "in_progress" },
                        { "content": "test", "status": "pending" }
                    ]
                }),
                Some("todo"),
            ))
            .await
            .unwrap();
        let text = match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(
            text,
            "Updated todo list: 1 pending, 1 in progress, 0 completed."
        );
        let session = agents.get(&session_id("todo")).unwrap().session();
        let event = &session.events()[0];
        assert!(!event.ignorable);
        match &event.data {
            SessionEventData::TodoWrite { todos } => {
                assert_eq!(todos.as_array().map(Vec::len), Some(2));
            }
            other => panic!("expected todo/write, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_invalid_lists_and_missing_agent() {
        let agents = registry_with_agent("todo2");
        let tool = TodoWriteTool::new(Arc::clone(&agents), false);
        let dup = tool
            .execute_call(&agent_call(
                serde_json::json!({
                    "todos": [
                        { "content": "x", "status": "pending" },
                        { "content": "x", "status": "pending" }
                    ]
                }),
                Some("todo2"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            dup,
            ToolError::Body(message) if message == "Error: invalid todos: duplicate content \"x\""
        ));
        let parallel = tool
            .execute_call(&agent_call(
                serde_json::json!({
                    "todos": [
                        { "content": "a", "status": "in_progress" },
                        { "content": "b", "status": "in_progress" }
                    ]
                }),
                Some("todo2"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            parallel,
            ToolError::Body(message)
                if message == "Error: invalid todos: at most one task may be in_progress (got 2)"
        ));
        let empty = tool
            .execute_call(&agent_call(
                serde_json::json!({ "todos": [{ "content": " ", "status": "pending" }] }),
                Some("todo2"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            empty,
            ToolError::Body(message)
                if message == "Error: invalid todo: `content` must be a non-empty string"
        ));
        let orphan = tool
            .execute_call(&agent_call(serde_json::json!({ "todos": [] }), None))
            .await
            .unwrap_err();
        assert!(matches!(
            orphan,
            ToolError::Body(message)
                if message == "Error: todo_write requires an owning agent session"
        ));
    }
}
