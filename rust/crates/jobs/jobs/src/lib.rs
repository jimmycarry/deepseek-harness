//! Jobs seam (ctx.jobs).
use dsh_cordis::Service;

/// Runtime placeholder for `jobs`.
#[derive(Default)]
pub struct Runtime;

impl Runtime {
    /// Create the service.
    pub fn new() -> Self { Self }
}

impl Service for Runtime {
    const KEY: &'static str = "jobs";
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
        assert!(ctx.has_service("jobs"));
        ctx.dispose();
        assert!(!ctx.has_service("jobs"));
    }
}
