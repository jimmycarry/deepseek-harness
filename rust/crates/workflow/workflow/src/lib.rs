//! Workflow engine (`ctx.workflowEngine`).

use dsh_cordis::Service;

/// Deployment-varying workflow execution choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConfig {
    /// Isolation realm for one run (`in-process`, `worker-thread`).
    pub isolation: String,
}

/// `ctx.workflowEngine`.
pub struct WorkflowRuntime {
    config: WorkflowConfig,
}

impl WorkflowRuntime {
    /// Bind a config. Isolation is never hardcoded in `run`.
    pub fn new(config: WorkflowConfig) -> Self {
        Self { config }
    }

    /// Config this engine was constructed with.
    pub fn config(&self) -> &WorkflowConfig {
        &self.config
    }

    /// Run one script in the configured isolation realm.
    pub async fn run(&self, script: &str) -> String {
        format!("[{}] {script}", self.config.isolation)
    }
}

impl Service for WorkflowRuntime {
    const KEY: &'static str = "workflowEngine";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[tokio::test]
    async fn run_uses_config_isolation() {
        let engine = WorkflowRuntime::new(WorkflowConfig {
            isolation: "worker-thread".into(),
        });
        assert_eq!(engine.run("return 1").await, "[worker-thread] return 1");
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(WorkflowRuntime::new(WorkflowConfig {
            isolation: "in-process".into(),
        })))
        .unwrap();
        assert!(ctx.has_service("workflowEngine"));
        ctx.dispose();
        assert!(!ctx.has_service("workflowEngine"));
    }
}
