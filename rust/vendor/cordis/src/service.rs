use std::any::Any;
use std::sync::Arc;

/// A named service exposed on `ctx`.
///
/// The `KEY` is the stable `ctx.<key>` name from the TypeScript harness
/// (`sessions`, `tools`, `agents`, …).
pub trait Service: Send + Sync + 'static {
    /// Stable context key.
    const KEY: &'static str;
}

/// Type-erased service slot stored on a context.
pub(crate) struct ServiceSlot {
    pub key: &'static str,
    pub value: Arc<dyn Any + Send + Sync>,
}

impl ServiceSlot {
    pub fn new<S: Service>(service: Arc<S>) -> Self {
        Self {
            key: S::KEY,
            value: service,
        }
    }

    pub fn downcast<S: Service>(&self) -> Option<Arc<S>> {
        self.value.clone().downcast::<S>().ok()
    }
}

/// Helpers for looking up services by key without the trait in scope.
pub trait ServiceExt {
    /// The service key this value was registered under.
    fn key(&self) -> &'static str;
}

impl<S: Service> ServiceExt for S {
    fn key(&self) -> &'static str {
        S::KEY
    }
}
