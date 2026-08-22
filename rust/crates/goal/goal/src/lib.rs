//! Goals (ctx.goals).
use dsh_cordis::Service;

/// Runtime placeholder for `goals`.
#[derive(Default)]
pub struct Runtime;

impl Runtime {
    /// Create the service.
    pub fn new() -> Self { Self }
}

impl Service for Runtime {
    const KEY: &'static str = "goals";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(Runtime::new())).unwrap();
        assert!(ctx.has_service("goals"));
        ctx.dispose();
        assert!(!ctx.has_service("goals"));
    }
}
