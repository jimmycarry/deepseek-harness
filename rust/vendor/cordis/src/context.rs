use crate::events::{EventBus, Listener, SerialFn, WaterfallFn};
use crate::fiber::{Fiber, FiberHandle, FiberState};
use crate::service::{Service, ServiceSlot};
use crate::{CordisError, Plugin, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct ContextInner {
    fiber: Arc<Fiber>,
    services: Arc<Mutex<HashMap<String, ServiceSlot>>>,
    events: Arc<EventBus>,
    children: Mutex<Vec<Arc<Fiber>>>,
}

/// Scoped handle to the shared plugin context.
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Create a root context with an Active root fiber.
    pub fn new() -> Self {
        let fiber = Fiber::new("root");
        fiber.set_state(FiberState::Active);
        Self {
            inner: Arc::new(ContextInner {
                fiber,
                services: Arc::new(Mutex::new(HashMap::new())),
                events: EventBus::new(),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The fiber that owns this context.
    pub fn fiber(&self) -> &Fiber {
        &self.inner.fiber
    }

    /// Register a service. The registration is an effect: unload removes it.
    pub fn provide<S: Service>(&self, service: Arc<S>) -> Result<()> {
        let key = S::KEY;
        let slot = ServiceSlot::new(service);
        self.inner.fiber.effect(&format!("ctx.provide({key})"), || {
            self.inner
                .services
                .lock()
                .expect("services")
                .insert(key.to_string(), slot);
            let services = Arc::clone(&self.inner.services);
            let key = key.to_string();
            move || {
                services.lock().expect("services").remove(&key);
            }
        })
    }

    /// Look up a required service.
    pub fn service<S: Service>(&self) -> Result<Arc<S>> {
        self.inner
            .services
            .lock()
            .expect("services")
            .get(S::KEY)
            .and_then(ServiceSlot::downcast::<S>)
            .ok_or_else(|| CordisError::MissingService(S::KEY.into()))
    }

    /// Optional lookup; does not fail when the service is absent.
    pub fn get<S: Service>(&self) -> Option<Arc<S>> {
        self.service::<S>().ok()
    }

    /// Whether a named service is currently provided.
    pub fn has_service(&self, key: &str) -> bool {
        self.inner
            .services
            .lock()
            .expect("services")
            .contains_key(key)
    }

    /// Register a cleanup-aware side effect on the owning fiber.
    pub fn effect<F, D>(&self, label: &str, setup: F) -> Result<()>
    where
        F: FnOnce() -> D,
        D: FnOnce() + Send + 'static,
    {
        self.inner.fiber.effect(label, setup)
    }

    /// Mount a plugin. The child fiber stays Pending until `inject` is satisfied.
    pub fn plugin<P: Plugin>(&self, plugin: P) -> Result<FiberHandle> {
        let name = plugin.name();
        let inject = plugin.inject();
        let child = Fiber::new(name);
        for key in inject {
            if !self.has_service(key) {
                child.set_state(FiberState::Pending);
                return Err(CordisError::MissingService((*key).into()));
            }
        }
        child.set_state(FiberState::Loading);
        self.inner
            .children
            .lock()
            .expect("children")
            .push(Arc::clone(&child));
        let child_ctx = self.with_fiber(Arc::clone(&child));
        match plugin.apply(&child_ctx) {
            Ok(()) => {
                child.set_state(FiberState::Active);
                let parent = Arc::clone(&self.inner.fiber);
                let disposed = Arc::clone(&child);
                parent
                    .effect(&format!("plugin({name})"), || {
                        move || disposed.dispose()
                    })
                    .ok();
                Ok(FiberHandle::new(child))
            }
            Err(error) => {
                child.set_state(FiberState::Failed);
                child.dispose();
                Err(error)
            }
        }
    }

    /// Listen for an emit event. The registration is an effect.
    pub fn on<F>(&self, name: &'static str, listener: F) -> Result<()>
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        let listener: Listener = Arc::new(listener);
        let disposer = self.inner.events.on_emit(name, listener);
        self.effect(&format!("ctx.on({name})"), || disposer)
    }

    /// Listen for a serial event.
    pub fn on_serial<F>(&self, name: &'static str, listener: F) -> Result<()>
    where
        F: Fn(Value) -> Option<Value> + Send + Sync + 'static,
    {
        let listener: SerialFn = Arc::new(listener);
        let disposer = self.inner.events.on_serial(name, listener);
        self.effect(&format!("ctx.on_serial({name})"), || disposer)
    }

    /// Listen for a waterfall event. The listener must call `next` to delegate.
    pub fn on_waterfall<F>(&self, name: &'static str, listener: F) -> Result<()>
    where
        F: Fn(Value, &mut crate::Next<'_>) -> Value + Send + Sync + 'static,
    {
        let listener: WaterfallFn = Arc::new(listener);
        let disposer = self.inner.events.on_waterfall(name, listener);
        self.effect(&format!("ctx.on_waterfall({name})"), || disposer)
    }

    /// Emit a named event.
    pub fn emit(&self, name: &str, payload: Value) {
        self.inner.events.emit(name, payload);
    }

    /// Serial-dispatch a named event.
    pub fn serial(&self, name: &str, payload: Value) -> Option<Value> {
        self.inner.events.serial(name, payload)
    }

    /// Waterfall-dispatch a named event.
    pub fn waterfall(
        &self,
        name: &str,
        payload: Value,
        inner: impl FnOnce(Value) -> Value + Send + 'static,
    ) -> Result<Value> {
        self.inner.events.waterfall(name, payload, inner)
    }

    /// Dispose the root fiber and every child.
    pub fn dispose(&self) {
        self.inner.fiber.dispose();
    }

    fn with_fiber(&self, fiber: Arc<Fiber>) -> Self {
        Self {
            inner: Arc::new(ContextInner {
                fiber,
                services: Arc::clone(&self.inner.services),
                events: Arc::clone(&self.inner.events),
                children: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FnPlugin;
    use serde_json::json;

    struct Ping;
    impl Service for Ping {
        const KEY: &'static str = "ping";
    }

    #[test]
    fn missing_inject_keeps_plugin_from_loading() {
        let ctx = Context::new();
        let result = ctx.plugin(FnPlugin::new("consumer", |_| Ok(())).with_inject(&["ping"]));
        assert!(matches!(result, Err(CordisError::MissingService(key)) if key == "ping"));
    }

    #[test]
    fn provide_then_inject_loads() {
        let ctx = Context::new();
        ctx.provide(Arc::new(Ping)).unwrap();
        let handle = ctx
            .plugin(FnPlugin::new("consumer", |child| {
                assert!(child.has_service("ping"));
                Ok(())
            }))
            .unwrap();
        assert_eq!(handle.fiber().state(), FiberState::Active);
    }

    #[test]
    fn dispose_unwinds_service() {
        let ctx = Context::new();
        ctx.provide(Arc::new(Ping)).unwrap();
        assert!(ctx.has_service("ping"));
        ctx.dispose();
        assert!(!ctx.has_service("ping"));
        assert_eq!(ctx.fiber().state(), FiberState::Disposed);
    }

    #[test]
    fn waterfall_must_call_next_or_short_circuit() {
        let ctx = Context::new();
        ctx.on_waterfall("demo", |payload, next| next.call(payload))
            .unwrap();
        ctx.on_waterfall("demo", |payload, _next| {
            // Policy listener owns the decision and skips next().
            json!({ "veto": payload })
        })
        .unwrap();
        let result = ctx
            .waterfall("demo", json!("inner"), |payload| json!({ "inner": payload }))
            .unwrap();
        assert_eq!(result, json!({ "veto": "inner" }));
    }

    #[test]
    fn waterfall_delegates_when_next_is_called() {
        let ctx = Context::new();
        ctx.on_waterfall("demo", |payload, next| {
            let mut value = next.call(payload);
            if let Value::Object(map) = &mut value {
                map.insert("wrapped".into(), json!(true));
            }
            value
        })
        .unwrap();
        let result = ctx
            .waterfall("demo", json!({}), |payload| payload)
            .unwrap();
        assert_eq!(result["wrapped"], json!(true));
    }

    #[test]
    fn emit_listener_is_removed_on_dispose() {
        let ctx = Context::new();
        let seen = Arc::new(Mutex::new(0u32));
        let flag = Arc::clone(&seen);
        ctx.on("tick", move |_| {
            *flag.lock().expect("seen") += 1;
        })
        .unwrap();
        ctx.emit("tick", json!(null));
        ctx.dispose();
        ctx.emit("tick", json!(null));
        assert_eq!(*seen.lock().expect("seen"), 1);
    }

    #[test]
    fn unloading_rejects_new_effects() {
        let ctx = Context::new();
        ctx.dispose();
        let err = ctx.effect("late", || || {}).unwrap_err();
        assert!(matches!(err, CordisError::InactiveEffect(_)));
    }
}
