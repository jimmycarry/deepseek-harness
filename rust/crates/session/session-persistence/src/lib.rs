//! Persistence seam (`ctx.sessionPersistence`).
//!
//! [`PersistenceRuntime::inspect`] is the read-only logical view: validated
//! header and events, in-memory crash-repair closers, no durable rewrite, and
//! no Session publication. Concurrent inspects of the same id share one
//! backend read; a ready LRU of [`DEFAULT_PREPARED_SESSION_CACHE_SIZE`]
//! reuses unpublished views; a changed [`SessionPersistenceRevision`]
//! discards the ready entry and reloads. [`PersistenceRuntime::load`]
//! reconstructs a Session for resume and other publishable paths and does
//! not publish from that LRU. [`PersistenceRuntime::list_snapshots`] and
//! [`PersistenceRuntime::read_stored_revision`] read the backend directly.

mod preparations;

pub use preparations::{
    DiscardReady, PreparedSessionSource, SessionPreparations, DEFAULT_PREPARED_SESSION_CACHE_SIZE,
};

use async_trait::async_trait;
use dsh_brand::Branded;
use dsh_cordis::Service;
use dsh_session::{Session, SessionError, SessionEvent, SessionHeader, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Absolute per-session artifact target, when the backend has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLocation {
    /// JSONL transcript path. The file may not exist yet.
    Jsonl {
        /// Absolute target path.
        path: PathBuf,
    },
}

/// Failures from a session-store backend.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// Underlying IO failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Header, schema, or JSON that this build will not interpret.
    #[error("{0}")]
    Format(String),
    /// No durable artifact exists for this session id.
    #[error("session \"{0}\" not found")]
    NotFound(String),
    /// Session append or required-on-read refusal.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Read-only logical session: validated header and event log, without
/// publishing a Session or committing crash recovery.
#[derive(Debug, Clone)]
pub struct SessionInspection {
    /// Validated session header.
    pub meta: SessionHeader,
    /// Validated contiguous event log. An interrupted trailing turn may
    /// include an in-memory `turn/end { interrupted }` closer that is not
    /// written back.
    pub events: Vec<SessionEvent>,
}

/// Brand token for a persisted-session revision.
pub struct SessionPersistenceRevisionBrand;

/// Opaque backend-owned revision of one persisted session log.
pub type SessionPersistenceRevision = Branded<SessionPersistenceRevisionBrand>;

/// Brand a backend revision token.
pub fn session_persistence_revision(value: impl Into<String>) -> SessionPersistenceRevision {
    SessionPersistenceRevision::new(value)
}

/// One stored session's header plus a cheap revision token.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPersistenceSnapshot {
    /// Detached metadata for one materialized session.
    pub header: SessionHeader,
    /// Opaque source-qualified token that changes when this stored log changes.
    pub revision: SessionPersistenceRevision,
}

impl SessionInspection {
    /// Build an unpublished Session from this view. Does not insert it into
    /// a [`dsh_session::SessionStore`].
    ///
    /// # Errors
    /// Session append refusal while replaying the inspected events.
    pub fn into_session(self) -> Result<Session, PersistenceError> {
        let session = Session::with_header(self.meta);
        for event in self.events {
            session.append_logged(event)?;
        }
        Ok(session)
    }
}

/// Durable save/load for one session log.
#[async_trait]
pub trait SessionStoreBackend: Send + Sync {
    /// Persist the current log.
    async fn save(&self, session: &Session) -> Result<(), PersistenceError>;
    /// Reconstruct a session from durable storage.
    async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError>;
    /// Read-only logical view: parse, validate, and apply in-memory
    /// crash-repair closers without writing them and without publishing a
    /// Session. The default reconstructs via [`Self::load`].
    async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
        let session = self.load(id).await?;
        Ok(SessionInspection {
            meta: session.header().clone(),
            events: session.events(),
        })
    }
    /// Session ids currently stored by this backend.
    async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError>;
    /// Stored headers without requiring a full event-log parse when the
    /// backend can supply metadata alone. The default inspects each id.
    async fn list_headers(&self) -> Result<Vec<SessionHeader>, PersistenceError> {
        let mut headers = Vec::new();
        for id in self.list_ids().await? {
            headers.push(self.inspect(&id).await?.meta);
        }
        Ok(headers)
    }
    /// Resolve an absolute per-session artifact without I/O. Backends without
    /// an independent local artifact return `None`.
    fn locate(&self, id: &SessionId) -> Option<SessionLocation> {
        let _ = id;
        None
    }
    /// Current revision for one id without loading events. `Ok(None)` if absent.
    async fn read_stored_revision(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionPersistenceRevision>, PersistenceError> {
        let _ = id;
        Ok(None)
    }
    /// All materialized sessions with cheap revision tokens.
    ///
    /// The default pairs [`Self::list_headers`] with [`Self::read_stored_revision`]
    /// and skips ids whose revision is absent.
    async fn list_snapshots(&self) -> Result<Vec<SessionPersistenceSnapshot>, PersistenceError> {
        let mut snapshots = Vec::new();
        for header in self.list_headers().await? {
            if let Some(revision) = self.read_stored_revision(&header.id).await? {
                snapshots.push(SessionPersistenceSnapshot { header, revision });
            }
        }
        Ok(snapshots)
    }
}

/// `ctx.sessionPersistence`.
pub struct PersistenceRuntime {
    backend: Arc<dyn SessionStoreBackend>,
    preparations: SessionPreparations,
}

impl PersistenceRuntime {
    /// Wrap a backend with the TypeScript default prepared-session LRU.
    pub fn new(backend: Arc<dyn SessionStoreBackend>) -> Self {
        Self::with_prepared_session_cache_size(backend, DEFAULT_PREPARED_SESSION_CACHE_SIZE)
            .expect("default preparedSessionCacheSize is valid")
    }

    /// Wrap a backend with an explicit ready-entry LRU capacity.
    ///
    /// # Errors
    /// `preparedSessionCacheSize` must be a positive integer.
    pub fn with_prepared_session_cache_size(
        backend: Arc<dyn SessionStoreBackend>,
        prepared_session_cache_size: usize,
    ) -> Result<Self, PersistenceError> {
        Ok(Self {
            backend,
            preparations: SessionPreparations::new(prepared_session_cache_size)?,
        })
    }

    /// Persist the current log and drop any cached inspection for that id.
    pub async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
        self.backend.save(session).await?;
        self.preparations.invalidate(session.id());
        Ok(())
    }

    /// Reconstruct a session from durable storage.
    pub async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        self.backend.load(id).await
    }

    /// Read-only logical view. Does not commit crash recovery or publish a Session.
    ///
    /// Shares an in-flight backend inspect for the same id, reuses a ready
    /// LRU entry when [`Self::read_stored_revision`] still matches, and
    /// reloads after a stale ready discard. A reserved hold keeps the cached
    /// view when the durable revision moves.
    pub async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
        loop {
            let source = self
                .preparations
                .inspect(id, || self.prepare_core(id))
                .await?;
            if self.is_prepared_source_current(&source).await? {
                return Ok(source.inspection.clone());
            }
            if self.preparations.discard_ready(id, &source) == DiscardReady::Retained {
                return Ok(source.inspection.clone());
            }
        }
    }

    async fn prepare_core(
        &self,
        id: &SessionId,
    ) -> Result<PreparedSessionSource, PersistenceError> {
        let inspection = self.backend.inspect(id).await?;
        let revision = self.backend.read_stored_revision(id).await?;
        Ok(PreparedSessionSource {
            inspection,
            revision,
        })
    }

    async fn is_prepared_source_current(
        &self,
        source: &PreparedSessionSource,
    ) -> Result<bool, PersistenceError> {
        Ok(self
            .backend
            .read_stored_revision(&source.inspection.meta.id)
            .await?
            == source.revision)
    }

    /// Hold a ready inspection so a later stale inspect borrows it.
    pub(crate) fn hold_prepared(&self, id: &SessionId) -> bool {
        self.preparations.hold(id)
    }

    /// Session ids currently stored by the backend.
    pub async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
        self.backend.list_ids().await
    }

    /// Stored headers. Backends may return metadata without parsing events.
    pub async fn list_headers(&self) -> Result<Vec<SessionHeader>, PersistenceError> {
        self.backend.list_headers().await
    }

    /// Resolve an absolute per-session artifact without I/O.
    pub fn locate(&self, id: &SessionId) -> Option<SessionLocation> {
        self.backend.locate(id)
    }

    /// Current revision for one id without loading events. `Ok(None)` if absent.
    pub async fn read_stored_revision(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionPersistenceRevision>, PersistenceError> {
        self.backend.read_stored_revision(id).await
    }

    /// Stored headers plus a backend-owned revision token each.
    pub async fn list_snapshots(
        &self,
    ) -> Result<Vec<SessionPersistenceSnapshot>, PersistenceError> {
        self.backend.list_snapshots().await
    }
}

impl Service for PersistenceRuntime {
    const KEY: &'static str = "sessionPersistence";
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_cordis::Context;
    use dsh_session::session_id;

    struct Stub;

    #[async_trait]
    impl SessionStoreBackend for Stub {
        async fn save(&self, _: &Session) -> Result<(), PersistenceError> {
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
            Ok(Session::new(id.clone()))
        }

        async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
            Ok(vec![session_id("s")])
        }
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(PersistenceRuntime::new(Arc::new(Stub))))
            .unwrap();
        assert!(ctx.has_service("sessionPersistence"));
        ctx.dispose();
        assert!(!ctx.has_service("sessionPersistence"));
    }

    #[tokio::test]
    async fn stub_load_returns_empty_session() {
        let runtime = PersistenceRuntime::new(Arc::new(Stub));
        let session = runtime.load(&session_id("s")).await.unwrap();
        assert_eq!(session.id().as_str(), "s");
        assert!(session.events().is_empty());
        let inspected = runtime.inspect(&session_id("s")).await.unwrap();
        assert_eq!(inspected.meta.id.as_str(), "s");
        assert!(inspected.events.is_empty());
        let headers = runtime.list_headers().await.unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id.as_str(), "s");
        assert!(runtime.list_snapshots().await.unwrap().is_empty());
        assert!(runtime
            .read_stored_revision(&session_id("s"))
            .await
            .unwrap()
            .is_none());
    }

    struct CountingBackend {
        inspects: std::sync::atomic::AtomicUsize,
        store: std::sync::Mutex<std::collections::HashMap<String, (SessionInspection, String)>>,
        gate: Option<std::sync::Arc<tokio::sync::Notify>>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                inspects: std::sync::atomic::AtomicUsize::new(0),
                store: std::sync::Mutex::new(std::collections::HashMap::new()),
                gate: None,
            }
        }

        fn put(&self, id: &str, revision: &str) {
            let session = Session::new(session_id(id));
            self.store.lock().unwrap().insert(
                id.to_string(),
                (
                    SessionInspection {
                        meta: session.header().clone(),
                        events: Vec::new(),
                    },
                    revision.to_string(),
                ),
            );
        }

        fn set_revision(&self, id: &str, revision: &str) {
            self.store.lock().unwrap().get_mut(id).expect("session").1 = revision.to_string();
        }
    }

    #[async_trait]
    impl SessionStoreBackend for CountingBackend {
        async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
            let mut store = self.store.lock().unwrap();
            let revision = store
                .get(session.id().as_str())
                .map(|(_, revision)| format!("{revision}-saved"))
                .unwrap_or_else(|| "saved".into());
            store.insert(
                session.id().as_str().to_string(),
                (
                    SessionInspection {
                        meta: session.header().clone(),
                        events: session.events(),
                    },
                    revision,
                ),
            );
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
            self.inspect(id).await?.into_session()
        }

        async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
            self.inspects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(release) = &self.gate {
                release.notified().await;
            }
            self.store
                .lock()
                .unwrap()
                .get(id.as_str())
                .map(|(inspection, _)| inspection.clone())
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))
        }

        async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
            Ok(self.store.lock().unwrap().keys().map(session_id).collect())
        }

        async fn read_stored_revision(
            &self,
            id: &SessionId,
        ) -> Result<Option<SessionPersistenceRevision>, PersistenceError> {
            Ok(self
                .store
                .lock()
                .unwrap()
                .get(id.as_str())
                .map(|(_, revision)| session_persistence_revision(revision.clone())))
        }
    }

    #[test]
    fn rejects_invalid_prepared_session_cache_size() {
        let err = match PersistenceRuntime::with_prepared_session_cache_size(Arc::new(Stub), 0) {
            Ok(_) => panic!("expected invalid preparedSessionCacheSize"),
            Err(error) => error,
        };
        assert!(err
            .to_string()
            .contains("preparedSessionCacheSize must be a positive safe integer"));
    }

    #[tokio::test]
    async fn shares_in_flight_inspects_for_the_same_id() {
        let release = Arc::new(tokio::sync::Notify::new());
        let backend = Arc::new(CountingBackend {
            inspects: std::sync::atomic::AtomicUsize::new(0),
            store: std::sync::Mutex::new(std::collections::HashMap::new()),
            gate: Some(Arc::clone(&release)),
        });
        backend.put("shared", "r1");
        let runtime = Arc::new(PersistenceRuntime::new(
            Arc::clone(&backend) as Arc<dyn SessionStoreBackend>
        ));
        let a = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move { runtime.inspect(&session_id("shared")).await })
        };
        while backend.inspects.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let b = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move { runtime.inspect(&session_id("shared")).await })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();
        let left = a.await.unwrap().unwrap();
        let right = b.await.unwrap().unwrap();
        assert_eq!(left.meta.id.as_str(), "shared");
        assert_eq!(right.meta.id.as_str(), "shared");
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn reloads_a_cached_inspection_after_the_durable_revision_changes() {
        let backend = Arc::new(CountingBackend::new());
        backend.put("fresh", "r1");
        let runtime = PersistenceRuntime::new(Arc::clone(&backend) as Arc<dyn SessionStoreBackend>);
        runtime.inspect(&session_id("fresh")).await.unwrap();
        runtime.inspect(&session_id("fresh")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        backend.set_revision("fresh", "r2");
        runtime.inspect(&session_id("fresh")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[tokio::test]
    async fn evicts_only_ready_preparations_by_lru_capacity() {
        let backend = Arc::new(CountingBackend::new());
        backend.put("a", "1");
        backend.put("b", "1");
        let runtime = PersistenceRuntime::with_prepared_session_cache_size(
            Arc::clone(&backend) as Arc<dyn SessionStoreBackend>,
            1,
        )
        .unwrap();
        runtime.inspect(&session_id("a")).await.unwrap();
        runtime.inspect(&session_id("b")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        runtime.inspect(&session_id("b")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        runtime.inspect(&session_id("a")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
    }

    #[tokio::test]
    async fn save_invalidates_a_ready_inspection() {
        let backend = Arc::new(CountingBackend::new());
        backend.put("saved", "r1");
        let runtime = PersistenceRuntime::new(Arc::clone(&backend) as Arc<dyn SessionStoreBackend>);
        runtime.inspect(&session_id("saved")).await.unwrap();
        runtime
            .save(&Session::new(session_id("saved")))
            .await
            .unwrap();
        runtime.inspect(&session_id("saved")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[tokio::test]
    async fn retains_a_reserved_preparation_when_inspection_observes_a_newer_revision() {
        let backend = Arc::new(CountingBackend::new());
        backend.put("held", "r1");
        let runtime = PersistenceRuntime::new(Arc::clone(&backend) as Arc<dyn SessionStoreBackend>);
        runtime.inspect(&session_id("held")).await.unwrap();
        assert!(runtime.hold_prepared(&session_id("held")));
        backend.set_revision("held", "r2");
        runtime.inspect(&session_id("held")).await.unwrap();
        assert_eq!(
            backend.inspects.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}
