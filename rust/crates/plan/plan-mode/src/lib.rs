//! Plan mode (`ctx.plan`).

use dsh_cordis::Service;
use std::sync::atomic::{AtomicBool, Ordering};

/// `ctx.plan`.
#[derive(Default)]
pub struct PlanRuntime {
    active: AtomicBool,
}

impl PlanRuntime {
    /// Create an inactive plan controller.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter plan mode.
    pub fn enter(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    /// Leave plan mode.
    pub fn leave(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Whether plan mode is currently in force.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

impl Service for PlanRuntime {
    const KEY: &'static str = "plan";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn enter_leave_toggles_active() {
        let plan = PlanRuntime::new();
        assert!(!plan.is_active());
        plan.enter();
        assert!(plan.is_active());
        plan.leave();
        assert!(!plan.is_active());
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(PlanRuntime::new())).unwrap();
        assert!(ctx.has_service("plan"));
        ctx.dispose();
        assert!(!ctx.has_service("plan"));
    }
}
