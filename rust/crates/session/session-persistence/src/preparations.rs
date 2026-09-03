//! Bounded sharing of unpublished cold inspections.
//!
//! One in-flight load per session id, a ready LRU of
//! [`DEFAULT_PREPARED_SESSION_CACHE_SIZE`], and exclusive holds so a stale
//! inspect can borrow a reserved view. `list` / `list_snapshots` do not use
//! this pool.

use crate::{PersistenceError, SessionInspection, SessionPersistenceRevision};
use dsh_session::SessionId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// TypeScript `DEFAULT_PREPARED_SESSION_CACHE_SIZE`.
pub const DEFAULT_PREPARED_SESSION_CACHE_SIZE: usize = 5;

/// Cold inspect plus the revision token observed with it.
#[derive(Debug, Clone)]
pub struct PreparedSessionSource {
    /// Read-only logical view.
    pub inspection: SessionInspection,
    /// Backend revision at load time. `None` when the backend has no token.
    pub revision: Option<SessionPersistenceRevision>,
}

/// Outcome of discarding a stale ready source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardReady {
    /// The exact ready source was removed.
    Discarded,
    /// An exclusive hold owns the entry; inspect may borrow it.
    Retained,
    /// No matching entry.
    Missing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Loading,
    Ready,
    Reserved,
}

struct SharedLoad {
    waiters: Mutex<Vec<oneshot::Sender<Result<Arc<PreparedSessionSource>, String>>>>,
}

struct Entry {
    phase: Phase,
    source: Option<Arc<PreparedSessionSource>>,
    load: Option<Arc<SharedLoad>>,
}

struct Inner {
    capacity: usize,
    order: Vec<String>,
    entries: HashMap<String, Entry>,
}

/// Per-id in-flight dedup, ready LRU, and exclusive hold.
pub struct SessionPreparations {
    inner: Mutex<Inner>,
}

impl SessionPreparations {
    /// Fail loud when `capacity` is not a positive integer.
    pub fn new(capacity: usize) -> Result<Self, PersistenceError> {
        if capacity < 1 {
            return Err(PersistenceError::Format(
                "preparedSessionCacheSize must be a positive safe integer".into(),
            ));
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                capacity,
                order: Vec::new(),
                entries: HashMap::new(),
            }),
        })
    }

    /// Observe one prepared source, sharing an in-flight read for the same id.
    pub async fn inspect<F, Fut>(
        &self,
        id: &SessionId,
        load: F,
    ) -> Result<Arc<PreparedSessionSource>, PersistenceError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<PreparedSessionSource, PersistenceError>>,
    {
        enum Start {
            Ready(Arc<PreparedSessionSource>),
            Wait(oneshot::Receiver<Result<Arc<PreparedSessionSource>, String>>),
            Load(Arc<SharedLoad>),
        }
        let start = {
            let mut inner = self.lock();
            if let Some(entry) = inner.entries.get_mut(id.as_str()) {
                match entry.phase {
                    Phase::Ready => {
                        let source = entry
                            .source
                            .clone()
                            .expect("ready preparation has a source");
                        inner.touch(id);
                        Start::Ready(source)
                    }
                    Phase::Reserved => Start::Ready(
                        entry
                            .source
                            .clone()
                            .expect("reserved preparation has a source"),
                    ),
                    Phase::Loading => {
                        let shared = entry.load.clone().expect("loading preparation waits");
                        let (tx, rx) = oneshot::channel();
                        shared.waiters.lock().expect("preparation waiters").push(tx);
                        Start::Wait(rx)
                    }
                }
            } else {
                let shared = Arc::new(SharedLoad {
                    waiters: Mutex::new(Vec::new()),
                });
                inner.order.push(id.as_str().to_string());
                inner.entries.insert(
                    id.as_str().to_string(),
                    Entry {
                        phase: Phase::Loading,
                        source: None,
                        load: Some(Arc::clone(&shared)),
                    },
                );
                Start::Load(shared)
            }
        };
        match start {
            Start::Ready(source) => Ok(source),
            Start::Wait(rx) => match rx.await {
                Ok(Ok(source)) => {
                    let mut inner = self.lock();
                    if inner
                        .entries
                        .get(id.as_str())
                        .is_some_and(|entry| entry.phase == Phase::Ready)
                    {
                        inner.touch(id);
                    }
                    Ok(source)
                }
                Ok(Err(message)) => Err(PersistenceError::Format(message)),
                Err(_) => Err(PersistenceError::Format(format!(
                    "session \"{}\" preparation load was dropped",
                    id.as_str()
                ))),
            },
            Start::Load(shared) => match load().await {
                Ok(source) => {
                    let source = Arc::new(source);
                    self.finish_load(id, shared, Ok(Arc::clone(&source)));
                    Ok(source)
                }
                Err(error) => {
                    self.finish_load(id, shared, Err(error.to_string()));
                    Err(error)
                }
            },
        }
    }

    /// Discard an exact stale ready source without disturbing an exclusive hold.
    pub fn discard_ready(
        &self,
        id: &SessionId,
        expected: &Arc<PreparedSessionSource>,
    ) -> DiscardReady {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get(id.as_str()) else {
            return DiscardReady::Missing;
        };
        let Some(source) = entry.source.as_ref() else {
            return DiscardReady::Missing;
        };
        if !Arc::ptr_eq(source, expected) {
            return DiscardReady::Missing;
        }
        if entry.phase != Phase::Ready {
            return DiscardReady::Retained;
        }
        inner.remove(id.as_str());
        DiscardReady::Discarded
    }

    /// Hold a ready source so inspect may borrow it when the durable revision moves.
    pub fn hold(&self, id: &SessionId) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get_mut(id.as_str()) else {
            return false;
        };
        if entry.phase != Phase::Ready || entry.source.is_none() {
            return false;
        }
        entry.phase = Phase::Reserved;
        true
    }

    /// Drop any cached entry for `id`. An in-flight load still settles its waiters.
    pub fn invalidate(&self, id: &SessionId) {
        self.lock().remove(id.as_str());
    }

    /// Whether this id has an in-flight, ready, or reserved preparation.
    pub fn has(&self, id: &SessionId) -> bool {
        self.lock().entries.contains_key(id.as_str())
    }

    /// Refuse appends while a resume reservation owns this id.
    ///
    /// # Errors
    /// The id is reserved for an unpublished preparation.
    pub fn assert_writable(&self, id: &SessionId) -> Result<(), PersistenceError> {
        let inner = self.lock();
        if inner
            .entries
            .get(id.as_str())
            .is_some_and(|entry| entry.phase == Phase::Reserved)
        {
            return Err(PersistenceError::Format(format!(
                "cannot append session \"{}\" while its persisted preparation is reserved",
                id.as_str()
            )));
        }
        Ok(())
    }

    fn finish_load(
        &self,
        id: &SessionId,
        shared: Arc<SharedLoad>,
        result: Result<Arc<PreparedSessionSource>, String>,
    ) {
        {
            let mut inner = self.lock();
            let still_ours = inner.entries.get(id.as_str()).is_some_and(|entry| {
                entry
                    .load
                    .as_ref()
                    .is_some_and(|load| Arc::ptr_eq(load, &shared))
            });
            if still_ours {
                match &result {
                    Ok(source) => {
                        let entry = inner.entries.get_mut(id.as_str()).expect("checked");
                        entry.source = Some(Arc::clone(source));
                        entry.phase = Phase::Ready;
                        entry.load = None;
                        inner.touch(id);
                    }
                    Err(_) => inner.remove(id.as_str()),
                }
            }
        }
        let waiters = std::mem::take(&mut *shared.waiters.lock().expect("preparation waiters"));
        for waiter in waiters {
            let _ = waiter.send(match &result {
                Ok(source) => Ok(Arc::clone(source)),
                Err(message) => Err(message.clone()),
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("session preparations")
    }
}

impl Inner {
    fn touch(&mut self, id: &SessionId) {
        self.order.retain(|existing| existing != id.as_str());
        self.order.push(id.as_str().to_string());
        let ready = self
            .entries
            .values()
            .filter(|entry| entry.phase == Phase::Ready)
            .count();
        if ready <= self.capacity {
            return;
        }
        let victim = self.order.iter().find(|candidate| {
            self.entries
                .get(candidate.as_str())
                .is_some_and(|entry| entry.phase == Phase::Ready)
        });
        if let Some(victim) = victim.cloned() {
            self.remove(&victim);
        }
    }

    fn remove(&mut self, id: &str) {
        self.entries.remove(id);
        self.order.retain(|existing| existing != id);
    }
}
