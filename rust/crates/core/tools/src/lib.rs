//! Scoped tool registry and guarded execution pipeline (`ctx.tools`).

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use dsh_llm::{ContentBlock, ToolSchema, UserMessage};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// One scheduled tool call, including the live agent when the loop invoked it.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Advertised tool name.
    pub name: String,
    /// Parsed arguments.
    pub args: Value,
    /// Calling agent's session id, when the agent loop invoked the tool.
    pub agent_id: Option<String>,
}

/// Outcome of one pipeline run, including post-execute contexts.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    /// Tool body or pipeline failure rendered as a result.
    pub outcome: ToolOutcome,
    /// Model-visible notices prepended by `tools/post-execute` listeners.
    pub additional_contexts: Vec<UserMessage>,
}

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

    /// Execute with the calling agent when the loop supplied one.
    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        self.execute(call.args.clone()).await
    }

    /// Registration-declared body deadline in milliseconds. `None` means the
    /// tool declares no deadline; the timeout policy then leaves it untouched.
    fn timeout_ms(&self) -> Option<u64> {
        None
    }
}

/// Cordis service key checked before enforcing a tool's declared deadline.
pub const TOOL_TIMEOUT_POLICY_KEY: &str = "toolCallTimeoutPolicy";

/// Stable failure code for a policy-enforced tool deadline.
pub const TOOL_TIMEOUT: &str = "TOOL_TIMEOUT";

/// Run one tool body, enforcing its declared deadline when the timeout policy
/// service is mounted. A deadline hit replaces the result with the
/// model-visible `TOOL_TIMEOUT` failure.
async fn run_body(
    ctx: &Context,
    tool: &Arc<dyn Tool>,
    call: &ToolCall,
) -> Result<ToolOutcome, ToolError> {
    let deadline = if ctx.has_service(TOOL_TIMEOUT_POLICY_KEY) {
        tool.timeout_ms()
    } else {
        None
    };
    match deadline {
        None => tool.execute_call(call).await,
        Some(timeout_ms) => {
            match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                tool.execute_call(call),
            )
            .await
            {
                Ok(result) => result,
                Err(_elapsed) => Ok(ToolOutcome::error(format!(
                    "Error: tool call timed out after {timeout_ms}ms"
                ))),
            }
        }
    }
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
        let result = self.execute_for(ctx, name, args, None).await?;
        if result.outcome.is_error {
            let text = outcome_text(&result.outcome);
            if text.contains("denied by pre-execute") {
                return Err(ToolError::Denied(name.into()));
            }
            if text.starts_with("unknown tool") {
                return Err(ToolError::Unknown(name.into()));
            }
        }
        Ok(result.outcome)
    }

    /// Run the pipeline for one call and return post-execute contexts.
    ///
    /// Denied and unknown names still run `tools/post-execute` so a repeat
    /// detector can count a hammered denial. The body outcome is then an
    /// error; contexts still ride to the next step.
    pub async fn execute_for(
        &self,
        ctx: &Context,
        name: &str,
        args: Value,
        agent_id: Option<&str>,
    ) -> Result<ToolExecutionResult, ToolError> {
        let pre = ctx.waterfall(
            "tools/pre-execute",
            serde_json::json!({ "name": name, "args": args }),
            |payload| payload,
        );
        let denied = matches!(
            pre,
            Ok(ref value) if value.get("deny").and_then(Value::as_bool) == Some(true)
        );
        if denied {
            let outcome = ToolOutcome::error(ToolError::Denied(name.into()).to_string());
            return Ok(post_execute(ctx, name, &args, agent_id, outcome));
        }
        let Some(tool) = self.get(name) else {
            let outcome = ToolOutcome::error(ToolError::Unknown(name.into()).to_string());
            return Ok(post_execute(ctx, name, &args, agent_id, outcome));
        };
        let call = ToolCall {
            name: name.to_string(),
            args: args.clone(),
            agent_id: agent_id.map(str::to_string),
        };
        let outcome = run_body(ctx, &tool, &call).await?;
        Ok(post_execute(ctx, name, &args, agent_id, outcome))
    }

    /// Run many calls: pre-execute stays model-ordered, bodies overlap up to
    /// [`Self::max_parallel`], and post-execute commits in call order.
    pub async fn execute_many(
        &self,
        ctx: &Context,
        calls: Vec<(String, Value)>,
    ) -> Vec<Result<ToolOutcome, ToolError>> {
        self.execute_many_for(ctx, calls, None)
            .await
            .into_iter()
            .map(|result| match result {
                Ok(exec) if !exec.outcome.is_error => Ok(exec.outcome),
                Ok(exec) => {
                    let text = outcome_text(&exec.outcome);
                    if text.contains("denied by pre-execute") {
                        Err(ToolError::Denied(
                            exec.outcome
                                .content
                                .first()
                                .and_then(|_| Some(String::new()))
                                .unwrap_or_default(),
                        ))
                    } else {
                        Ok(exec.outcome)
                    }
                }
                Err(error) => Err(error),
            })
            .collect()
    }

    /// Run many calls and keep post-execute contexts for the next step.
    pub async fn execute_many_for(
        &self,
        ctx: &Context,
        calls: Vec<(String, Value)>,
        agent_id: Option<&str>,
    ) -> Vec<Result<ToolExecutionResult, ToolError>> {
        enum Prepared {
            Denied {
                name: String,
                args: Value,
            },
            Unknown {
                name: String,
                args: Value,
            },
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
                    prepared.push(Prepared::Denied { name, args });
                    continue;
                }
            }
            match self.get(&name) {
                Some(tool) => prepared.push(Prepared::Ready { name, tool, args }),
                None => prepared.push(Prepared::Unknown { name, args }),
            }
        }

        let mut outcomes: Vec<Option<Result<ToolOutcome, ToolError>>> =
            (0..prepared.len()).map(|_| None).collect();
        for (index, item) in prepared.iter().enumerate() {
            match item {
                Prepared::Denied { name, .. } => {
                    outcomes[index] = Some(Ok(ToolOutcome::error(
                        ToolError::Denied(name.clone()).to_string(),
                    )));
                }
                Prepared::Unknown { name, .. } => {
                    outcomes[index] = Some(Ok(ToolOutcome::error(
                        ToolError::Unknown(name.clone()).to_string(),
                    )));
                }
                Prepared::Ready { .. } => {}
            }
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_parallel()));
        let mut set = tokio::task::JoinSet::new();
        for (index, item) in prepared.iter().enumerate() {
            if let Prepared::Ready { name, tool, args } = item {
                let tool = Arc::clone(tool);
                let call = ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                    agent_id: agent_id.map(str::to_string),
                };
                let semaphore = Arc::clone(&semaphore);
                let body_ctx = ctx.clone();
                set.spawn(async move {
                    let _permit = semaphore.acquire().await.expect("tools semaphore");
                    (index, run_body(&body_ctx, &tool, &call).await)
                });
            }
        }
        while let Some(joined) = set.join_next().await {
            let (index, result) = joined.expect("tool body");
            outcomes[index] = Some(result);
        }

        let mut results = Vec::with_capacity(outcomes.len());
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let outcome = outcome.expect("every call is filled");
            let (name, args) = match &prepared[index] {
                Prepared::Ready { name, args, .. }
                | Prepared::Denied { name, args }
                | Prepared::Unknown { name, args } => (name.as_str(), args),
            };
            results.push(outcome.map(|outcome| post_execute(ctx, name, args, agent_id, outcome)));
        }
        results
    }
}

fn post_execute(
    ctx: &Context,
    name: &str,
    args: &Value,
    agent_id: Option<&str>,
    outcome: ToolOutcome,
) -> ToolExecutionResult {
    let payload = serde_json::json!({
        "name": name,
        "args": args,
        "agentId": agent_id,
        "isError": outcome.is_error,
        "content": outcome.content,
        "additionalContexts": []
    });
    match ctx.waterfall("tools/post-execute", payload, |payload| payload) {
        Ok(value) => {
            let additional_contexts = value
                .get("additionalContexts")
                .cloned()
                .and_then(|item| serde_json::from_value(item).ok())
                .unwrap_or_default();
            let content = value
                .get("content")
                .cloned()
                .and_then(|item| serde_json::from_value(item).ok())
                .unwrap_or(outcome.content);
            ToolExecutionResult {
                outcome: ToolOutcome {
                    content,
                    is_error: outcome.is_error,
                },
                additional_contexts,
            }
        }
        Err(_) => ToolExecutionResult {
            outcome,
            additional_contexts: Vec::new(),
        },
    }
}

fn outcome_text(outcome: &ToolOutcome) -> String {
    outcome
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
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
        ctx.on_waterfall(
            "tools/pre-execute",
            |_payload, _next| serde_json::json!({ "deny": true }),
        )
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
    async fn post_execute_can_replace_content() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("echo", "echo", |_| {
            ToolOutcome::text("huge")
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        ctx.on_waterfall("tools/post-execute", |mut payload, next| {
            let mut downstream = next.call(payload);
            if let Value::Object(map) = &mut downstream {
                map.insert(
                    "content".into(),
                    serde_json::json!([{ "type": "text", "text": "preview" }]),
                );
            }
            downstream
        })
        .unwrap();
        let result = tools
            .execute_for(&ctx, "echo", serde_json::json!({}), Some("sess"))
            .await
            .unwrap();
        assert_eq!(result.outcome.content[0], ContentBlock::text("preview"));
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
