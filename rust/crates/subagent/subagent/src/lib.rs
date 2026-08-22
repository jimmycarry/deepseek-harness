//! Subagent registry (`ctx.subagents`).

use dsh_cordis::Service;
use std::sync::Mutex;

/// `ctx.subagents`.
#[derive(Default)]
pub struct SubagentRuntime {
    results: Mutex<Vec<String>>,
}

impl SubagentRuntime {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one finished child result.
    pub fn record(&self, result: impl Into<String>) {
        self.results.lock().expect("subagents").push(result.into());
    }

    /// Finished child results in record order.
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
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn records_results() {
        let runtime = SubagentRuntime::new();
        runtime.record("done");
        assert_eq!(runtime.results(), vec!["done".to_string()]);
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SubagentRuntime::new())).unwrap();
        assert!(ctx.has_service("subagents"));
        ctx.dispose();
        assert!(!ctx.has_service("subagents"));
    }
}
