//! Subagent registry (`ctx.subagents`).

use async_trait::async_trait;
use dsh_cordis::{Context, Result, Service};
use dsh_session::SessionId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Registry failures.
#[derive(Debug, Error)]
pub enum SubagentError {
    /// Duplicate provider name.
    #[error("duplicate subagent provider \"{0}\"")]
    DuplicateProvider(String),
    /// Named provider is not registered.
    #[error("no subagent provider \"{0}\"")]
    NoProvider(String),
}

/// One finished child run.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    /// Joined assistant text.
    pub output: String,
    /// Child session id.
    pub id: SessionId,
    /// Why the child stopped.
    pub stop_reason: String,
}

/// What `start` needs from the parent.
pub struct SubagentStartRequest {
    /// Human-facing label.
    pub label: String,
    /// Child prompt.
    pub prompt: String,
    /// Parent session id.
    pub parent_id: SessionId,
    /// Optional seed events (fork).
    pub seed: Option<Vec<dsh_session::SessionEvent>>,
}

/// One registered backend.
#[async_trait]
pub trait SubagentProvider: Send + Sync {
    /// Registry name (`spawn`, `fork`).
    fn name(&self) -> &str;
    /// Whether the child inherits completed parent turns.
    fn inherits_parent_context(&self) -> bool;
    /// Run one one-shot child.
    async fn start(
        &self,
        request: SubagentStartRequest,
    ) -> std::result::Result<SubagentResult, SubagentError>;
}

/// `ctx.subagents`.
#[derive(Default)]
pub struct SubagentRuntime {
    providers: Mutex<HashMap<String, Arc<dyn SubagentProvider>>>,
    results: Mutex<Vec<String>>,
}

impl SubagentRuntime {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide `ctx.subagents`.
    pub fn install(ctx: &Context) -> Result<Arc<Self>> {
        let runtime = Arc::new(Self::new());
        ctx.provide(Arc::clone(&runtime))?;
        Ok(runtime)
    }

    /// Register a provider. Duplicate names fail loud.
    pub fn register_provider(
        &self,
        provider: Arc<dyn SubagentProvider>,
    ) -> std::result::Result<(), SubagentError> {
        let name = provider.name().to_string();
        let mut map = self.providers.lock().expect("subagents");
        if map.contains_key(&name) {
            return Err(SubagentError::DuplicateProvider(name));
        }
        map.insert(name, provider);
        Ok(())
    }

    /// Provider names in insertion order is not guaranteed; sorted for tests.
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .providers
            .lock()
            .expect("subagents")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Look up a provider.
    pub fn get_provider(&self, name: &str) -> Option<Arc<dyn SubagentProvider>> {
        self.providers
            .lock()
            .expect("subagents")
            .get(name)
            .cloned()
    }

    /// Start a one-shot child on `name`.
    pub async fn start(
        &self,
        name: &str,
        request: SubagentStartRequest,
    ) -> std::result::Result<SubagentResult, SubagentError> {
        let provider = self
            .get_provider(name)
            .ok_or_else(|| SubagentError::NoProvider(name.into()))?;
        let result = provider.start(request).await?;
        self.results
            .lock()
            .expect("subagents")
            .push(result.output.clone());
        Ok(result)
    }

    /// Finished child texts in record order.
    pub fn results(&self) -> Vec<String> {
        self.results.lock().expect("subagents").clone()
    }
}

impl Service for SubagentRuntime {
    const KEY: &'static str = "subagents";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::session_id;

    struct Fake;

    #[async_trait]
    impl SubagentProvider for Fake {
        fn name(&self) -> &str {
            "spawn"
        }
        fn inherits_parent_context(&self) -> bool {
            false
        }
        async fn start(
            &self,
            request: SubagentStartRequest,
        ) -> std::result::Result<SubagentResult, SubagentError> {
            Ok(SubagentResult {
                output: request.prompt,
                id: session_id("child"),
                stop_reason: "completed".into(),
            })
        }
    }

    #[tokio::test]
    async fn start_records_result() {
        let runtime = SubagentRuntime::new();
        runtime.register_provider(Arc::new(Fake)).unwrap();
        let result = runtime
            .start(
                "spawn",
                SubagentStartRequest {
                    label: "t".into(),
                    prompt: "ping".into(),
                    parent_id: session_id("parent"),
                    seed: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.output, "ping");
        assert_eq!(runtime.results(), vec!["ping".to_string()]);
    }
}
