//! Goals (`ctx.goal`).

use dsh_cordis::Service;
use std::sync::Mutex;

/// `ctx.goal`.
#[derive(Default)]
pub struct GoalRuntime {
    objective: Mutex<Option<String>>,
}

impl GoalRuntime {
    /// Create an empty goal store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the current objective.
    pub fn set(&self, objective: impl Into<String>) {
        *self.objective.lock().expect("goal") = Some(objective.into());
    }

    /// Read the current objective, if any.
    pub fn get(&self) -> Option<String> {
        self.objective.lock().expect("goal").clone()
    }
}

impl Service for GoalRuntime {
    const KEY: &'static str = "goal";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn set_then_get() {
        let goals = GoalRuntime::new();
        goals.set("ship the rust port");
        assert_eq!(goals.get().as_deref(), Some("ship the rust port"));
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(GoalRuntime::new())).unwrap();
        assert!(ctx.has_service("goal"));
        ctx.dispose();
        assert!(!ctx.has_service("goal"));
    }
}
