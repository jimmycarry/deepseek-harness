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

/// Deployment-varying tool pipeline policy. Passed in at construction.
#[derive(Debug, Clone)]
pub struct ToolRuntimeConfig {
    /// Maximum concurrent `tools/execute` bodies. `1` is sequential.
    pub max_parallel: usize,
}

/// `ctx.tools`.
pub struct ToolRuntime {
    tools: Arc<Mutex<HashMap<String, Arc<dyn Tool>>>>,
    config: ToolRuntimeConfig,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self {
            tools: Arc::new(Mutex::new(HashMap::new())),
            config: ToolRuntimeConfig { max_parallel: 1 },
        }
    }
}

impl ToolRuntime {
    /// Create an empty registry with sequential execute.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry with an explicit parallel cap.
    pub fn with_config(config: ToolRuntimeConfig) -> Self {
        Self {
            tools: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Configured execute parallelism.
    pub fn max_parallel(&self) -> usize {
        self.config.max_parallel.max(1)
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

    /// Run many calls: pre-execute stays model-ordered, bodies overlap up to
    /// [`Self::max_parallel`], and post-execute commits in call order.
    pub async fn execute_many(
        &self,
        ctx: &Context,
        calls: Vec<(String, Value)>,
    ) -> Vec<Result<ToolOutcome, ToolError>> {
        enum Prepared {
            Denied(String),
            Unknown(String),
            Ready {
                name: String,
                tool: Arc<dyn Tool>,
                args: Value,
            },
        }

        let mut prepared = Vec::with_capacity(calls.len());
        for (name, args) in calls {
            let pre = ctx.waterfall(
                "tools/pre-execute",
                serde_json::json!({ "name": name, "args": args }),
                |payload| payload,
            );
            if let Ok(value) = pre {
                if value.get("deny").and_then(Value::as_bool) == Some(true) {
                    prepared.push(Prepared::Denied(name));
                    continue;
                }
            }
            match self.get(&name) {
                Some(tool) => prepared.push(Prepared::Ready { name, tool, args }),
                None => prepared.push(Prepared::Unknown(name)),
            }
        }

        let mut outcomes: Vec<Option<Result<ToolOutcome, ToolError>>> =
            (0..prepared.len()).map(|_| None).collect();
        for (index, item) in prepared.iter().enumerate() {
            match item {
                Prepared::Denied(name) => {
                    outcomes[index] = Some(Err(ToolError::Denied(name.clone())));
                }
                Prepared::Unknown(name) => {
                    outcomes[index] = Some(Err(ToolError::Unknown(name.clone())));
                }
                Prepared::Ready { .. } => {}
            }
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_parallel()));
        let mut set = tokio::task::JoinSet::new();
        for (index, item) in prepared.iter().enumerate() {
            if let Prepared::Ready { tool, args, .. } = item {
                let tool = Arc::clone(tool);
                let args = args.clone();
                let semaphore = Arc::clone(&semaphore);
                set.spawn(async move {
                    let _permit = semaphore.acquire().await.expect("tools semaphore");
                    (index, tool.execute(args).await)
                });
            }
        }
        while let Some(joined) = set.join_next().await {
            let (index, result) = joined.expect("tool body");
            outcomes[index] = Some(result);
        }

        let mut results = Vec::with_capacity(outcomes.len());
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let result = outcome.expect("every call is filled");
            if matches!(&prepared[index], Prepared::Ready { .. }) {
                let name = match &prepared[index] {
                    Prepared::Ready { name, .. }
                    | Prepared::Denied(name)
                    | Prepared::Unknown(name) => name.as_str(),
                };
                let _ = ctx.waterfall(
                    "tools/post-execute",
                    serde_json::json!({ "name": name }),
                    |payload| payload,
                );
            }
            results.push(result);
        }
        results
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

    struct DelayTool {
        name: String,
        delay: std::time::Duration,
        started: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for DelayTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "delayed"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _: Value) -> Result<ToolOutcome, ToolError> {
            self.started.lock().expect("starts").push(self.name.clone());
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutcome::text(self.name.clone()))
        }
    }

    #[tokio::test]
    async fn execute_many_runs_bodies_overlapped_and_posts_in_call_order() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::with_config(ToolRuntimeConfig {
            max_parallel: 2,
        }));
        let starts = Arc::new(Mutex::new(Vec::new()));
        tools.insert(Arc::new(DelayTool {
            name: "slow".into(),
            delay: std::time::Duration::from_millis(40),
            started: Arc::clone(&starts),
        }));
        tools.insert(Arc::new(DelayTool {
            name: "fast".into(),
            delay: std::time::Duration::from_millis(0),
            started: Arc::clone(&starts),
        }));
        ctx.provide(Arc::clone(&tools)).unwrap();
        let posts = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&posts);
        ctx.on_waterfall("tools/post-execute", move |payload, next| {
            if let Some(name) = payload.get("name").and_then(Value::as_str) {
                recorded.lock().expect("posts").push(name.to_string());
            }
            next.call(payload)
        })
        .unwrap();
        let results = tools
            .execute_many(
                &ctx,
                vec![
                    ("slow".into(), serde_json::json!({})),
                    ("fast".into(), serde_json::json!({})),
                ],
            )
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().unwrap().content[0],
            ContentBlock::text("slow")
        );
        assert_eq!(
            results[1].as_ref().unwrap().content[0],
            ContentBlock::text("fast")
        );
        assert_eq!(posts.lock().expect("posts").as_slice(), ["slow", "fast"]);
        assert_eq!(starts.lock().expect("starts").len(), 2);
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
