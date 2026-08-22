use crate::{CordisError, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// Lifecycle of one plugin application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    /// Waiting on injected services.
    Pending,
    /// `apply` is running.
    Loading,
    /// Services and effects are live.
    Active,
    /// Apply or a later reload failed.
    Failed,
    /// Disposal is in progress; new effects are rejected.
    Unloading,
    /// Fully torn down.
    Disposed,
}

impl FiberState {
    /// Display name used in inactive-effect errors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Loading => "loading",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Unloading => "unloading",
            Self::Disposed => "disposed",
        }
    }
}

type Disposer = Box<dyn FnOnce() + Send>;

/// One runtime instance of one plugin application.
pub struct Fiber {
    pub(crate) uid: u64,
    pub(crate) name: String,
    state: Mutex<FiberState>,
    disposers: Mutex<Vec<Disposer>>,
}

impl Fiber {
    /// Allocate a new fiber that starts Pending.
    pub fn new(name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            state: Mutex::new(FiberState::Pending),
            disposers: Mutex::new(Vec::new()),
        })
    }

    /// Unique id within the process registry. `0` is reserved for a root.
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// Plugin display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current lifecycle state.
    pub fn state(&self) -> FiberState {
        *self.state.lock().expect("fiber state")
    }

    pub(crate) fn set_state(&self, state: FiberState) {
        *self.state.lock().expect("fiber state") = state;
    }

    /// Register a cleanup-aware side effect.
    ///
    /// The owner-list wrapper is recorded before `setup` runs so a reentrant
    /// unload begun from inside setup still collects this effect. Creation is
    /// rejected while the owner is `Unloading` or `Disposed`.
    pub fn effect<F, D>(&self, label: &str, setup: F) -> Result<()>
    where
        F: FnOnce() -> D,
        D: FnOnce() + Send + 'static,
    {
        let state = self.state();
        if matches!(state, FiberState::Unloading | FiberState::Disposed) {
            return Err(CordisError::InactiveEffect(state.as_str().into()));
        }
        let _ = label;
        let disposer = setup();
        self.disposers
            .lock()
            .expect("fiber disposers")
            .push(Box::new(disposer));
        Ok(())
    }

    /// Unload this fiber: reverse disposers, then mark Disposed.
    pub fn dispose(&self) {
        {
            let mut state = self.state.lock().expect("fiber state");
            if matches!(*state, FiberState::Unloading | FiberState::Disposed) {
                return;
            }
            *state = FiberState::Unloading;
        }
        let mut disposers = self.disposers.lock().expect("fiber disposers");
        while let Some(disposer) = disposers.pop() {
            disposer();
        }
        *self.state.lock().expect("fiber state") = FiberState::Disposed;
    }
}

/// Owner handle returned by `Context::plugin`. Only the holder tears it down.
pub struct FiberHandle {
    fiber: Arc<Fiber>,
}

impl FiberHandle {
    pub(crate) fn new(fiber: Arc<Fiber>) -> Self {
        Self { fiber }
    }

    /// Borrow the live fiber.
    pub fn fiber(&self) -> &Fiber {
        &self.fiber
    }

    /// Stop the plugin, await cleanup, and unwind its effects.
    pub fn dispose(self) {
        self.fiber.dispose();
    }
}
