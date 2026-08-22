use crate::{CordisError, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Dispatch mode is part of the public event contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Fire-and-forget, registration order, return ignored.
    Emit,
    /// Await all listeners concurrently.
    Parallel,
    /// Await each listener; first bail value wins.
    Serial,
    /// Sync serial: first bail value wins.
    Bail,
    /// Around-middleware. Listeners receive `next`. Skipping `next` vetoes.
    Waterfall,
}

/// JSON payload used for named (plugin-mergeable) events.
pub type EventPayload = Value;

/// Continuation passed to a waterfall listener.
pub struct Next<'a> {
    remaining: &'a mut Vec<WaterfallFn>,
    inner: &'a mut Option<Box<dyn FnOnce(Value) -> Value + Send>>,
    called: bool,
}

impl Next<'_> {
    /// Invoke the next listener, or the innermost built-in behavior.
    pub fn call(&mut self, payload: Value) -> Value {
        self.called = true;
        if let Some(listener) = self.remaining.pop() {
            let mut next = Next {
                remaining: self.remaining,
                inner: self.inner,
                called: false,
            };
            listener(payload, &mut next)
        } else if let Some(inner) = self.inner.take() {
            inner(payload)
        } else {
            payload
        }
    }

    /// Whether this listener delegated.
    pub fn was_called(&self) -> bool {
        self.called
    }
}

/// What a serial/bail listener returns.
#[derive(Debug, Clone)]
pub enum WaterfallDecision {
    /// Continue the chain (or, for serial, keep going).
    Continue(Value),
    /// Stop the chain and return this value.
    Bail(Value),
}

pub(crate) type Listener = Arc<dyn Fn(Value) + Send + Sync>;
pub(crate) type SerialFn = Arc<dyn Fn(Value) -> Option<Value> + Send + Sync>;
pub(crate) type WaterfallFn = Arc<dyn Fn(Value, &mut Next<'_>) -> Value + Send + Sync>;

struct Tagged<T> {
    id: u64,
    value: T,
}

/// Event bus mixed onto `Context`.
pub struct EventBus {
    next_id: AtomicU64,
    emit: Mutex<HashMap<String, Vec<Tagged<Listener>>>>,
    serial: Mutex<HashMap<String, Vec<Tagged<SerialFn>>>>,
    waterfall: Mutex<HashMap<String, Vec<Tagged<WaterfallFn>>>>,
}

impl EventBus {
    /// Create an empty bus.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            emit: Mutex::new(HashMap::new()),
            serial: Mutex::new(HashMap::new()),
            waterfall: Mutex::new(HashMap::new()),
        })
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register an emit listener. Returns a disposer that removes it.
    pub fn on_emit(self: &Arc<Self>, name: &str, listener: Listener) -> impl FnOnce() + Send + 'static {
        let id = self.next_id();
        self.emit
            .lock()
            .expect("events")
            .entry(name.to_string())
            .or_default()
            .push(Tagged {
                id,
                value: listener,
            });
        remove_on_drop(Arc::downgrade(self), name, ListenerKind::Emit, id)
    }

    /// Fire-and-forget dispatch.
    pub fn emit(&self, name: &str, payload: Value) {
        let listeners = self
            .emit
            .lock()
            .expect("events")
            .get(name)
            .map(|list| list.iter().map(|item| item.value.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        for listener in listeners {
            listener(payload.clone());
        }
    }

    /// Register a serial listener. Bail = non-null, non-false value.
    pub fn on_serial(self: &Arc<Self>, name: &str, listener: SerialFn) -> impl FnOnce() + Send + 'static {
        let id = self.next_id();
        self.serial
            .lock()
            .expect("events")
            .entry(name.to_string())
            .or_default()
            .push(Tagged {
                id,
                value: listener,
            });
        remove_on_drop(Arc::downgrade(self), name, ListenerKind::Serial, id)
    }

    /// Serial dispatch. First non-null, non-false return bails.
    pub fn serial(&self, name: &str, payload: Value) -> Option<Value> {
        let listeners = self
            .serial
            .lock()
            .expect("events")
            .get(name)
            .map(|list| list.iter().map(|item| item.value.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        for listener in listeners {
            if let Some(bailed) = listener(payload.clone()) {
                if !matches!(bailed, Value::Null | Value::Bool(false)) {
                    return Some(bailed);
                }
            }
        }
        None
    }

    /// Register a waterfall listener.
    pub fn on_waterfall(
        self: &Arc<Self>,
        name: &str,
        listener: WaterfallFn,
    ) -> impl FnOnce() + Send + 'static {
        let id = self.next_id();
        self.waterfall
            .lock()
            .expect("events")
            .entry(name.to_string())
            .or_default()
            .push(Tagged {
                id,
                value: listener,
            });
        remove_on_drop(Arc::downgrade(self), name, ListenerKind::Waterfall, id)
    }

    /// Run a waterfall. Listeners are outermost-first; skipping `next` vetoes.
    pub fn waterfall(
        &self,
        name: &str,
        payload: Value,
        inner: impl FnOnce(Value) -> Value + Send + 'static,
    ) -> Result<Value> {
        let mut listeners = self
            .waterfall
            .lock()
            .expect("events")
            .get(name)
            .map(|list| list.iter().map(|item| item.value.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        listeners.reverse();
        let mut inner = Some(Box::new(inner) as Box<dyn FnOnce(Value) -> Value + Send>);
        let mut next = Next {
            remaining: &mut listeners,
            inner: &mut inner,
            called: false,
        };
        Ok(next.call(payload))
    }
}

enum ListenerKind {
    Emit,
    Serial,
    Waterfall,
}

fn remove_on_drop(
    bus: Weak<EventBus>,
    name: &str,
    kind: ListenerKind,
    id: u64,
) -> impl FnOnce() + Send + 'static {
    let name = name.to_string();
    move || {
        let Some(bus) = bus.upgrade() else {
            return;
        };
        match kind {
            ListenerKind::Emit => retain(&bus.emit, &name, id),
            ListenerKind::Serial => retain(&bus.serial, &name, id),
            ListenerKind::Waterfall => retain(&bus.waterfall, &name, id),
        }
    }
}

fn retain<T>(map: &Mutex<HashMap<String, Vec<Tagged<T>>>>, name: &str, id: u64) {
    if let Ok(mut map) = map.lock() {
        if let Some(list) = map.get_mut(name) {
            list.retain(|item| item.id != id);
        }
    }
}

/// Helper to wrap a listener failure.
pub fn event_error(event: &str, message: impl Into<String>) -> CordisError {
    CordisError::Event {
        event: event.into(),
        message: message.into(),
    }
}
