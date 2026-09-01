//! Persistence seam (`ctx.sessionPersistence`).
//!
//! [`PersistenceRuntime::inspect`] is the read-only logical view: validated
//! header and events, in-memory crash-repair closers, no durable rewrite, and
//! no Session publication. [`PersistenceRuntime::load`] reconstructs a Session
//! for resume and other publishable paths.
//! [`PersistenceRuntime::list_snapshots`] and
//! [`PersistenceRuntime::read_stored_revision`] expose a backend-owned
//! revision token without the prepared-session LRU or freshness retry.

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl PersistenceRuntime {
    /// Wrap a backend.
    pub fn new(backend: Arc<dyn SessionStoreBackend>) -> Self {
        Self { backend }
    }

    /// Persist the current log.
    pub async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
        self.backend.save(session).await
    }

    /// Reconstruct a session from durable storage.
    pub async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        self.backend.load(id).await
    }

    /// Read-only logical view. Does not commit crash recovery or publish a Session.
    pub async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
        self.backend.inspect(id).await
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
}
