//! Process-confinement seam (`ctx.sandbox`).
//!
//! Backends enforce a [`SandboxPolicy`] per call. The runtime is the named
//! service; containment decisions stay on the policy object.

use dsh_cordis::Service;
use std::sync::Arc;

/// File-path policy for one confined execution.
pub trait SandboxPolicy: Send + Sync {
    /// Whether `path` may be read or written under this policy.
    fn allow_path(&self, path: &str) -> bool;
}

/// `ctx.sandbox`.
pub struct SandboxRuntime {
    policy: Arc<dyn SandboxPolicy>,
}

impl SandboxRuntime {
    /// Wrap a policy as the process-confinement service.
    pub fn new(policy: Arc<dyn SandboxPolicy>) -> Self {
        Self { policy }
    }

    /// Borrow the active policy.
    pub fn policy(&self) -> Arc<dyn SandboxPolicy> {
        Arc::clone(&self.policy)
    }

    /// Delegate a path check to the active policy.
    pub fn allow_path(&self, path: &str) -> bool {
        self.policy.allow_path(path)
    }
}

impl Service for SandboxRuntime {
    const KEY: &'static str = "sandbox";
}

impl SandboxPolicy for SandboxRuntime {
    fn allow_path(&self, path: &str) -> bool {
        self.policy.allow_path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    struct AllowAll;

    impl SandboxPolicy for AllowAll {
        fn allow_path(&self, _: &str) -> bool {
            true
        }
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SandboxRuntime::new(Arc::new(AllowAll))))
            .unwrap();
        assert!(ctx.has_service("sandbox"));
        ctx.dispose();
        assert!(!ctx.has_service("sandbox"));
    }
}
