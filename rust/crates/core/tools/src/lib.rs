//! Scoped tool registry and guarded execution pipeline (`ctx.tools`).

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use dsh_llm::{ContentBlock, ToolSchema};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// A model-facing tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name advertised to the model.
    fn name(&self) -> &str;
    /// Human description.
    fn description(&self) -> &str;
    /// JSON Schema parameters.
    fn parameters(&self) -> Value;
    /// Execute with parsed arguments.
    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError>;
}

/// Successful or failed tool body.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Model-visible content.
    pub content: Vec<ContentBlock>,
    /// Whether the tool reported a failure.
    pub is_error: bool,
}

impl ToolOutcome {
    /// Text success.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: false,
        }
    }

    /// Text failure.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: true,
        }
    }
}

/// Tool pipeline failures.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Unknown tool name.
    #[error("unknown tool `{0}`")]
    Unknown(String),
    /// `tools/pre-execute` rejected the call.
    #[error("tool `{0}` denied by pre-execute")]
    Denied(String),
    /// Body failed.
    #[error("{0}")]
    Body(String),
}

/// `ctx.tools`.
pub struct ToolRuntime {
    tools: Arc<Mutex<HashMap<String, Arc<dyn Tool>>>>,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self {
            tools: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl ToolRuntime {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. The registration is an effect on `ctx`.
    pub fn register(&self, ctx: &Context, tool: Arc<dyn Tool>) -> dsh_cordis::Result<()> {
        let name = tool.name().to_string();
        let map = Arc::clone(&self.tools);
        ctx.effect(&format!("tools.register({name})"), || {
            map.lock().expect("tools").insert(name.clone(), tool);
            let map = Arc::clone(&map);
            let name = name.clone();
            move || {
                map.lock().expect("tools").remove(&name);
            }
        })
    }

    /// Register a tool without an effect (tests / static composition).
    pub fn insert(&self, tool: Arc<dyn Tool>) {
        self.tools
            .lock()
            .expect("tools")
            .insert(tool.name().to_string(), tool);
    }

    /// Schemas in registration order of names (sorted for determinism).
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let mut tools: Vec<_> = self
            .tools
            .lock()
            .expect("tools")
            .values()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters(),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Look up a tool.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().expect("tools").get(name).cloned()
    }

    /// Run pre / execute / post. Waterfall listeners on `ctx` may deny.
    pub async fn execute(
        &self,
        ctx: &Context,
        name: &str,
        args: Value,
    ) -> Result<ToolOutcome, ToolError> {
        let pre = ctx.waterfall(
            "tools/pre-execute",
            serde_json::json!({ "name": name, "args": args }),
            |payload| payload,
        );
        if let Ok(value) = pre {
            if value.get("deny").and_then(Value::as_bool) == Some(true) {
                return Err(ToolError::Denied(name.into()));
            }
        }
        let tool = self.get(name).ok_or_else(|| ToolError::Unknown(name.into()))?;
        let outcome = tool.execute(args).await?;
        let _ = ctx.waterfall(
            "tools/post-execute",
            serde_json::json!({ "name": name }),
            |payload| payload,
        );
        Ok(outcome)
    }
}

impl Service for ToolRuntime {
    const KEY: &'static str = "tools";
}

/// A scripted tool for tests.
pub struct ScriptTool {
    name: String,
    description: String,
    body: Box<dyn Fn(Value) -> ToolOutcome + Send + Sync>,
}

impl ScriptTool {
    /// Build a scripted tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Fn(Value) -> ToolOutcome + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            body: Box::new(body),
        }
    }
}

#[async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        Ok((self.body)(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[tokio::test]
    async fn pre_execute_can_deny() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("echo", "echo", |_| {
            ToolOutcome::text("ok")
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        ctx.on_waterfall("tools/pre-execute", |_payload, _next| {
            serde_json::json!({ "deny": true })
        })
        .unwrap();
        let err = tools
            .execute(&ctx, "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(name) if name == "echo"));
    }

    #[tokio::test]
    async fn dispose_removes_registered_tool() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        ctx.provide(Arc::clone(&tools)).unwrap();
        tools
            .register(
                &ctx,
                Arc::new(ScriptTool::new("echo", "echo", |_| ToolOutcome::text("ok"))),
            )
            .unwrap();
        assert_eq!(tools.schemas().len(), 1);
        ctx.dispose();
        assert!(tools.schemas().is_empty());
    }
}
