//! Persistence seam (`ctx.sessionPersistence`).

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_session::{Session, SessionError, SessionId};
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

/// Durable save/load for one session log.
#[async_trait]
pub trait SessionStoreBackend: Send + Sync {
    /// Persist the current log.
    async fn save(&self, session: &Session) -> Result<(), PersistenceError>;
    /// Reconstruct a session from durable storage.
    async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError>;
    /// Session ids currently stored by this backend.
    async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError>;
    /// Resolve an absolute per-session artifact without I/O. Backends without
    /// an independent local artifact return `None`.
    fn locate(&self, id: &SessionId) -> Option<SessionLocation> {
        let _ = id;
        None
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

    /// Session ids currently stored by the backend.
    pub async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
        self.backend.list_ids().await
    }

    /// Resolve an absolute per-session artifact without I/O.
    pub fn locate(&self, id: &SessionId) -> Option<SessionLocation> {
        self.backend.locate(id)
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
    }
}
