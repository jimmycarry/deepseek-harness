//! Combined session-history reads, traces, and full-text search (`ctx.sessionQuery`).
//!
//! Exact reads, titles, and lineage traces are backend-independent. A search
//! backend implements `search_sessions` / `search_events` on the same service.

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_session::{Session, SessionEvent, SessionEventData, SessionId, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

/// Default maximum `before`/`after` raw-event window.
pub const SESSION_QUERY_READ_WINDOW_MAX: usize = 50;

/// Closed taxonomy for session-query failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionQueryErrorCode {
    /// Full-text search is off for this deployment.
    SearchDisabled,
    /// Requested session is neither live nor persisted.
    SessionNotFound,
    /// Requested event seq is absent.
    EventNotFound,
    /// `before`/`after` is outside the configured window.
    InvalidWindow,
    /// Persistence backend failed.
    PersistenceFailed,
}

/// Typed session-query failure.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct SessionQueryError {
    /// Human message.
    pub message: String,
    /// Machine-routing code.
    pub code: SessionQueryErrorCode,
}

impl SessionQueryError {
    /// Construct a typed failure.
    pub fn new(message: impl Into<String>, code: SessionQueryErrorCode) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    /// Wire code string matching TypeScript.
    pub fn code_str(&self) -> &'static str {
        match self.code {
            SessionQueryErrorCode::SearchDisabled => "SESSION_QUERY_SEARCH_DISABLED",
            SessionQueryErrorCode::SessionNotFound => "SESSION_QUERY_SESSION_NOT_FOUND",
            SessionQueryErrorCode::EventNotFound => "SESSION_QUERY_EVENT_NOT_FOUND",
            SessionQueryErrorCode::InvalidWindow => "SESSION_QUERY_INVALID_WINDOW",
            SessionQueryErrorCode::PersistenceFailed => "SESSION_QUERY_PERSISTENCE_FAILED",
        }
    }
}

/// Lightweight listed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session id.
    pub id: String,
    /// Latest log-backed title, when the log has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Complete detached session log.
#[derive(Debug, Clone)]
pub struct SessionLogSnapshot {
    /// Session id.
    pub id: SessionId,
    /// Detached event log.
    pub events: Vec<SessionEvent>,
}

/// One event plus a bounded raw-log window.
#[derive(Debug, Clone)]
pub struct SessionEventWindow {
    /// Target event.
    pub target: SessionEvent,
    /// Neighboring events inclusive of the target.
    pub events: Vec<SessionEvent>,
    /// First seq in `events`.
    pub start_seq: u64,
    /// Last seq in `events`.
    pub end_seq: u64,
}

/// Known ancestry from one corpus observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLineageTrace {
    /// Requested session id.
    pub target: String,
    /// Parent ids walking toward the root.
    pub ancestors: Vec<String>,
}

/// One full-text hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchHit {
    /// Session id.
    pub session_id: String,
    /// Matching event seq.
    pub seq: u64,
    /// Bounded snippet.
    pub snippet: String,
}

/// Full-text search backend. `openAt: never` uses [`DisabledSearch`].
#[async_trait]
pub trait SessionSearch: Send + Sync {
    /// Search the live-preferred logical corpus.
    async fn search_sessions(
        &self,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError>;
    /// Search events within one session.
    async fn search_events(
        &self,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError>;
}

/// Search backend that fails before any index work.
pub struct DisabledSearch;

#[async_trait]
impl SessionSearch for DisabledSearch {
    async fn search_sessions(
        &self,
        _query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError> {
        Err(search_disabled())
    }

    async fn search_events(
        &self,
        _session_id: &str,
        _query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError> {
        Err(search_disabled())
    }
}

/// TypeScript `SESSION_QUERY_SEARCH_DISABLED` sentence.
pub fn search_disabled() -> SessionQueryError {
    SessionQueryError::new(
        "session search is disabled: this deployment configures the session-query index with openAt \"never\"",
        SessionQueryErrorCode::SearchDisabled,
    )
}

/// Fold the latest `session/title` from one log.
pub fn fold_session_title(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.data {
        SessionEventData::SessionTitle { title, .. } => Some(title.clone()),
        _ => None,
    })
}

/// `ctx.sessionQuery`.
pub struct SessionQueryEngine {
    sessions: Arc<SessionStore>,
    persistence: Option<Arc<PersistenceRuntime>>,
    read_window_max: usize,
    search: Arc<dyn SessionSearch>,
}

impl SessionQueryEngine {
    /// Build the service from already-resolved collaborators.
    pub fn new(
        sessions: Arc<SessionStore>,
        persistence: Option<Arc<PersistenceRuntime>>,
        read_window_max: usize,
        search: Arc<dyn SessionSearch>,
    ) -> Self {
        Self {
            sessions,
            persistence,
            read_window_max,
            search,
        }
    }

    /// List live-preferred records, then persisted ids the live store lacks.
    ///
    /// @returns deterministic id-sorted cloned session records.
    pub async fn list_sessions(&self) -> Result<Vec<SessionRecord>, SessionQueryError> {
        let mut records = BTreeMap::new();
        for session in self.sessions.live() {
            records.insert(
                session.id().as_str().to_string(),
                SessionRecord {
                    id: session.id().as_str().to_string(),
                    title: fold_session_title(&session.events()),
                },
            );
        }
        if let Some(persistence) = &self.persistence {
            let ids = persistence
                .list_ids()
                .await
                .map_err(|error| {
                    SessionQueryError::new(error.to_string(), SessionQueryErrorCode::PersistenceFailed)
                })?;
            for id in ids {
                records.entry(id.as_str().to_string()).or_insert(SessionRecord {
                    id: id.as_str().to_string(),
                    title: None,
                });
            }
        }
        Ok(records.into_values().collect())
    }

    /// Read and replay-validate one complete logical session log.
    ///
    /// @param id - live or persisted session id.
    /// @returns cloned event log from one observation.
    pub async fn read_session(&self, id: &SessionId) -> Result<SessionLogSnapshot, SessionQueryError> {
        let session = self.load(id).await?;
        Ok(SessionLogSnapshot {
            id: id.clone(),
            events: session.events(),
        })
    }

    /// Fold the latest log-backed title from one live-preferred session.
    ///
    /// @param id - live or persisted session id.
    /// @returns latest title, or `None` when the log has no title event.
    pub async fn read_title(&self, id: &SessionId) -> Result<Option<String>, SessionQueryError> {
        Ok(fold_session_title(&self.load(id).await?.events()))
    }

    /// Read one full event plus a bounded raw-log context window.
    ///
    /// @param id - live or persisted session id.
    /// @param seq - target event seq.
    /// @param before - raw events before the target; defaults to 0.
    /// @param after - raw events after the target; defaults to 0.
    pub async fn read_event(
        &self,
        id: &SessionId,
        seq: u64,
        before: Option<usize>,
        after: Option<usize>,
    ) -> Result<SessionEventWindow, SessionQueryError> {
        let before = self.read_window("before", before)?;
        let after = self.read_window("after", after)?;
        let events = self.load(id).await?.events();
        let target = events
            .iter()
            .find(|event| event.seq == seq)
            .cloned()
            .ok_or_else(|| {
                SessionQueryError::new(
                    format!("session \"{}\" has no event at seq {seq}", id.as_str()),
                    SessionQueryErrorCode::EventNotFound,
                )
            })?;
        let start = seq.saturating_sub(before as u64);
        let end = (seq + after as u64).min(events.last().map(|event| event.seq).unwrap_or(seq));
        let window = events
            .into_iter()
            .filter(|event| event.seq >= start && event.seq <= end)
            .collect::<Vec<_>>();
        Ok(SessionEventWindow {
            target,
            events: window,
            start_seq: start,
            end_seq: end,
        })
    }

    /// Trace known ancestry. Rust sessions do not yet store a parent header,
    /// so a found session returns an empty ancestor list.
    ///
    /// @param id - logical session id to trace.
    pub async fn trace_session(&self, id: &SessionId) -> Result<SessionLineageTrace, SessionQueryError> {
        self.load(id).await?;
        Ok(SessionLineageTrace {
            target: id.as_str().to_string(),
            ancestors: Vec::new(),
        })
    }

    /// Search the live-preferred logical corpus.
    ///
    /// @param query - trimmed literal phrase.
    pub async fn search_sessions(
        &self,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError> {
        self.search.search_sessions(query).await
    }

    /// Search events within one session.
    ///
    /// @param session_id - target session.
    /// @param query - trimmed literal phrase.
    pub async fn search_events(
        &self,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<SessionSearchHit>, SessionQueryError> {
        self.search.search_events(session_id, query).await
    }

    async fn load(&self, id: &SessionId) -> Result<Arc<Session>, SessionQueryError> {
        if let Some(live) = self.sessions.get(id) {
            return Ok(live);
        }
        if let Some(persistence) = &self.persistence {
            return persistence
                .load(id)
                .await
                .map(Arc::new)
                .map_err(|error| {
                    SessionQueryError::new(error.to_string(), SessionQueryErrorCode::SessionNotFound)
                });
        }
        Err(SessionQueryError::new(
            format!("session \"{}\" was not found", id.as_str()),
            SessionQueryErrorCode::SessionNotFound,
        ))
    }

    fn read_window(
        &self,
        name: &str,
        value: Option<usize>,
    ) -> Result<usize, SessionQueryError> {
        let Some(value) = value else {
            return Ok(0);
        };
        if value > self.read_window_max {
            return Err(SessionQueryError::new(
                format!("{name} must be an integer between 0 and {}", self.read_window_max),
                SessionQueryErrorCode::InvalidWindow,
            ));
        }
        Ok(value)
    }
}

impl Service for SessionQueryEngine {
    const KEY: &'static str = "sessionQuery";
}

/// Convenience constructor used by tests that only need exact reads.
pub fn disabled_engine(sessions: Arc<SessionStore>) -> SessionQueryEngine {
    SessionQueryEngine::new(
        sessions,
        None,
        SESSION_QUERY_READ_WINDOW_MAX,
        Arc::new(DisabledSearch),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData};

    #[tokio::test]
    async fn exact_reads_and_disabled_search() {
        let store = Arc::new(SessionStore::new());
        let session = store.create(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::SessionTitle {
                    title: "hello".into(),
                    message_seqs: vec![],
                    source: serde_json::json!("fallback"),
                },
                None,
            )
            .unwrap();
        let engine = disabled_engine(Arc::clone(&store));
        let listed = engine.list_sessions().await.unwrap();
        assert_eq!(listed[0].id, "s");
        assert_eq!(listed[0].title.as_deref(), Some("hello"));
        assert_eq!(
            engine.read_title(&session_id("s")).await.unwrap().as_deref(),
            Some("hello")
        );
        let window = engine
            .read_event(&session_id("s"), 0, Some(0), Some(1))
            .await
            .unwrap();
        assert_eq!(window.target.seq, 0);
        assert_eq!(window.events.len(), 2);
        let lineage = engine.trace_session(&session_id("s")).await.unwrap();
        assert!(lineage.ancestors.is_empty());
        let err = engine.search_sessions("hello").await.unwrap_err();
        assert_eq!(err.code, SessionQueryErrorCode::SearchDisabled);
        assert_eq!(err.code_str(), "SESSION_QUERY_SEARCH_DISABLED");
        let missing = engine
            .read_session(&session_id("missing"))
            .await
            .unwrap_err();
        assert_eq!(missing.code, SessionQueryErrorCode::SessionNotFound);
    }
}
