//! Local workflow provider.

use dsh_cordis::{Context, Result};
use dsh_workflow::{WorkflowConfig, WorkflowRuntime};
use std::sync::Arc;

/// Provide [`WorkflowRuntime`] with the given isolation config.
pub fn install(ctx: &Context, isolation: impl Into<String>) -> Result<Arc<WorkflowRuntime>> {
    let runtime = Arc::new(WorkflowRuntime::new(WorkflowConfig {
        isolation: isolation.into(),
    }));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-workflow-local"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn install_uses_supplied_isolation() {
        let ctx = Context::new();
        let engine = install(&ctx, "in-process").unwrap();
        assert_eq!(engine.config().isolation, "in-process");
        assert!(ctx.has_service("workflowEngine"));
    }
}
