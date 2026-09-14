//! Combined session-history reads, traces, filters, and full-text search (`ctx.sessionQuery`).
//!
//! Exact reads, titles, newest-first listing, lineage traces, surface traces,
//! and provider-independent filters are backend-independent. A search backend
//! implements `search_sessions` / `search_events` on the same service.

mod documents;
mod extraction;
mod filters;
mod tracing;

use async_trait::async_trait;
use dsh_cordis::Service;
use dsh_session::{
    Session, SessionEvent, SessionEventData, SessionHeader, SessionId, SessionStore,
};
use dsh_session_persistence::PersistenceRuntime;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;

pub use documents::{
    build_session_event_records, build_session_event_search_documents, SessionEventSearchDocument,
};
pub use extraction::extract_session_event_text;
pub use filters::{
    compile_session_text_filter, filter_session_event_documents, filter_session_results,
    materialize_session_event_result_filters, materialize_session_result_filters,
    unknown_filter_kind, SessionEventResultFilter, SessionResultFilter, SessionTextFilter,
};
pub use tracing::{current_surface_events, event_records, trace_event};

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
    /// Live and persisted headers name the same id but disagree on immutable fields.
    SourceConflict,
    /// A parent chain connected to the target contains a cycle.
    InvalidLineage,
    /// A loaded log failed the canonical surface fold.
    InvalidSurface,
    /// A filter clause failed validation.
    InvalidFilter,
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
            SessionQueryErrorCode::SourceConflict => "SESSION_QUERY_SOURCE_CONFLICT",
            SessionQueryErrorCode::InvalidLineage => "SESSION_QUERY_INVALID_LINEAGE",
            SessionQueryErrorCode::InvalidSurface => "SESSION_QUERY_INVALID_SURFACE",
            SessionQueryErrorCode::InvalidFilter => "SESSION_QUERY_INVALID_FILTER",
        }
    }
}

/// Lightweight identity and source availability for one logical session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Cloned session header selected from the live-preferred corpus.
    pub header: SessionHeader,
    /// Whether the id currently exists in `ctx.sessions`.
    pub live: bool,
    /// Whether the active persistence backend currently materializes the id.
    pub persisted: bool,
}

/// Source availability predicates understood by logical-session filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAvailability {
    /// The id currently exists in `ctx.sessions`.
    Live,
    /// The active persistence backend currently materializes the id.
    Persisted,
}

/// Whether an event is current model context, replaced context, or raw-log-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionEventSurface {
    /// Present on the folded current surface.
    Current,
    /// Removed from the surface by a later replacement.
    Shadowed,
    /// Never a surface node.
    #[serde(rename = "log-only")]
    LogOnly,
}

/// Inclusive numeric interval used by time and sequence filters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionResultRange {
    /// Inclusive lower bound.
    pub from: Option<f64>,
    /// Inclusive upper bound.
    pub to: Option<f64>,
}

/// Detached source selected for one exact read.
#[derive(Debug, Clone)]
pub struct LogicalSession {
    /// Cloned source header.
    pub header: SessionHeader,
    /// Cloned raw event log.
    pub events: Vec<SessionEvent>,
}

/// One atomic live-preferred observation of a session's current model surface.
#[derive(Debug, Clone)]
pub struct SessionSurfaceSnapshot {
    /// Cloned session header selected from the same corpus observation as `events`.
    pub session: SessionHeader,
    /// Highest raw-log seq included in the observation, or `None` for an empty log.
    pub captured_through_seq: Option<u64>,
    /// Cloned current surface events in model-history order.
    pub events: Vec<SessionEvent>,
}

/// Lightweight metadata for one event within a logical session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEventRecord {
    /// Session that owns the event.
    pub session_id: SessionId,
    /// Monotonic event seq within the session.
    pub seq: u64,
    /// Discriminant of the session event.
    pub type_name: String,
    /// Event timestamp in Unix epoch milliseconds.
    pub time: u64,
    /// Event placement in the folded session surface.
    pub surface: SessionEventSurface,
}

/// Direct surface replacements and relationships to cited source events.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEventTrace {
    /// Lightweight target record.
    pub target: SessionEventRecord,
    /// Immediate positional replacement event, when the target was shadowed.
    pub replaced_by: Option<u64>,
    /// Positional replacers from the immediate replacement to the final replacement.
    pub replacement_chain: Vec<u64>,
    /// Surface nodes directly removed when the target itself performed a replacement.
    pub replaced_event_seqs: Vec<u64>,
    /// Earlier events cited directly as sources, in their recorded order.
    pub source_event_seqs: Vec<u64>,
    /// Later events that directly cite the target as a source, in log order.
    pub derived_event_seqs: Vec<u64>,
}

/// Event relationships bound to the same session-header observation.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEventTraceObservation {
    /// Cloned header selected with the event log used for the trace.
    pub session: SessionHeader,
    /// Direct surface replacements and cited source-event links.
    pub trace: SessionEventTrace,
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

/// Recursive descendant node in a session-lineage trace.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionLineageNode {
    /// Detached logical-corpus record for this descendant.
    pub session: SessionRecord,
    /// Direct children, each carrying its own recursive descendants.
    pub descendants: Vec<SessionLineageNode>,
}

/// Known ancestry and descendants for one logical session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionLineageTrace {
    /// The complete parent chain is present in the logical corpus.
    Complete {
        /// Detached record for the session that was traced.
        target: SessionRecord,
        /// Known parents from the immediate parent outward.
        ancestors: Vec<SessionRecord>,
        /// Complete known descendant trees rooted at the target's direct children.
        descendants: Vec<SessionLineageNode>,
        /// Detached record at the top of the complete lineage.
        root: SessionRecord,
    },
    /// The parent chain leaves the visible logical corpus.
    Incomplete {
        /// Detached record for the session that was traced.
        target: SessionRecord,
        /// Known parents from the immediate parent outward.
        ancestors: Vec<SessionRecord>,
        /// Complete known descendant trees rooted at the target's direct children.
        descendants: Vec<SessionLineageNode>,
        /// First parent id that is not present in the logical corpus.
        unresolved_parent_id: SessionId,
    },
}

impl SessionLineageTrace {
    /// Detached record for the session that was traced.
    pub fn target(&self) -> &SessionRecord {
        match self {
            Self::Complete { target, .. } | Self::Incomplete { target, .. } => target,
        }
    }

    /// Known parents from the immediate parent outward.
    pub fn ancestors(&self) -> &[SessionRecord] {
        match self {
            Self::Complete { ancestors, .. } | Self::Incomplete { ancestors, .. } => ancestors,
        }
    }

    /// Complete known descendant trees rooted at the target's direct children.
    pub fn descendants(&self) -> &[SessionLineageNode] {
        match self {
            Self::Complete { descendants, .. } | Self::Incomplete { descendants, .. } => {
                descendants
            }
        }
    }

    /// Whether the complete parent chain is present in the logical corpus.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete { .. })
    }
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

/// Reject incompatible observations of one logical session source.
///
/// Compared fields are `version`, `id`, `createdAt`, `cwd`, `parentSession`,
/// `seedLength`, and `delegationDepth`. `origin` is not compared.
///
/// @param left - first live, listed, or loaded header observation.
/// @param right - second header observation expected to identify the same source.
/// @returns `Ok(())` when the immutable identity fields match.
pub fn assert_session_headers_compatible(
    left: &SessionHeader,
    right: &SessionHeader,
) -> Result<(), SessionQueryError> {
    if left.version != right.version
        || left.id != right.id
        || left.created_at != right.created_at
        || left.cwd != right.cwd
        || left.parent_session != right.parent_session
        || left.seed_length != right.seed_length
        || left.delegation_depth != right.delegation_depth
    {
        return Err(SessionQueryError::new(
            format!(
                "session source headers conflict for session \"{}\"",
                left.id.as_str()
            ),
            SessionQueryErrorCode::SourceConflict,
        ));
    }
    Ok(())
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

    /// List the complete logical corpus with live precedence and cloned headers.
    ///
    /// @returns records in deterministic newest-first order.
    pub async fn list_sessions(&self) -> Result<Vec<SessionRecord>, SessionQueryError> {
        self.observe_corpus().await
    }

    /// Read and replay-validate one complete logical session log.
    ///
    /// @param id - live or persisted session id.
    /// @returns cloned event log from one observation.
    pub async fn read_session(
        &self,
        id: &SessionId,
    ) -> Result<SessionLogSnapshot, SessionQueryError> {
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

    /// Trace known ancestry and descendants from one corpus observation.
    ///
    /// @param id - logical session id to trace.
    /// @returns a complete lineage or the first parent that could not be resolved.
    pub async fn trace_session(
        &self,
        id: &SessionId,
    ) -> Result<SessionLineageTrace, SessionQueryError> {
        let records = self.observe_corpus().await?;
        trace_session(&records, id)
    }

    /// Filter the complete logical corpus with provider-independent predicates.
    ///
    /// @param filters - ANDed session metadata and availability clauses.
    /// @returns matching cloned records in deterministic newest-first order.
    pub async fn filter_sessions(
        &self,
        filters: &[SessionResultFilter],
    ) -> Result<Vec<SessionRecord>, SessionQueryError> {
        let filters = materialize_session_result_filters(filters)?;
        filter_session_results(&self.observe_corpus().await?, &filters)
    }

    /// List lightweight raw-log event records for one logical session.
    ///
    /// @param id - live-preferred session id to read.
    /// @returns event records in ascending seq order.
    pub async fn list_events(
        &self,
        id: &SessionId,
    ) -> Result<Vec<SessionEventRecord>, SessionQueryError> {
        let loaded = self.load_logical(id).await?;
        crate::event_records(id, &loaded.events)
    }

    /// Scan first-party semantic event documents with provider-independent filters.
    ///
    /// @param id - live-preferred session id to scan.
    /// @param filters - ANDed metadata and literal-text predicates.
    /// @returns matching semantic documents in ascending seq order.
    pub async fn filter_events(
        &self,
        id: &SessionId,
        filters: &[SessionEventResultFilter],
    ) -> Result<Vec<SessionEventSearchDocument>, SessionQueryError> {
        let filters = materialize_session_event_result_filters(filters)?;
        let loaded = self.load_logical(id).await?;
        let documents = build_session_event_search_documents(id, &loaded.events)?;
        filter_session_event_documents(&documents, &filters)
    }

    /// Read one session's complete current model surface from one corpus observation.
    ///
    /// @param id - live-preferred session id to read.
    /// @returns cloned header, current surface, and the last sequence number included in the raw-log capture.
    pub async fn read_surface(
        &self,
        id: &SessionId,
    ) -> Result<SessionSurfaceSnapshot, SessionQueryError> {
        let loaded = self.load_logical(id).await?;
        Ok(SessionSurfaceSnapshot {
            session: loaded.header,
            captured_through_seq: loaded.events.last().map(|event| event.seq),
            events: crate::current_surface_events(id, &loaded.events)?,
        })
    }

    /// Trace one event's direct positional replacements and cited source events.
    ///
    /// @param id - target session id.
    /// @param seq - target event seq.
    /// @returns source header, direct links, and the target's positional replacement chain.
    pub async fn trace_event(
        &self,
        id: &SessionId,
        seq: u64,
    ) -> Result<SessionEventTraceObservation, SessionQueryError> {
        let loaded = self.load_logical(id).await?;
        Ok(SessionEventTraceObservation {
            session: loaded.header,
            trace: crate::tracing::trace_event(id, &loaded.events, seq)?,
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

    async fn observe_corpus(&self) -> Result<Vec<SessionRecord>, SessionQueryError> {
        let mut records = HashMap::new();
        if let Some(persistence) = &self.persistence {
            let headers = persistence.list_headers().await.map_err(listing_failed)?;
            for header in headers {
                records.insert(
                    header.id.as_str().to_string(),
                    SessionRecord {
                        header,
                        live: false,
                        persisted: true,
                    },
                );
            }
        }
        for session in self.sessions.live() {
            let header = session.header().clone();
            let id = header.id.as_str().to_string();
            if let Some(durable) = records.get(&id) {
                assert_session_headers_compatible(&header, &durable.header)?;
            }
            let persisted = records.contains_key(&id);
            records.insert(
                id,
                SessionRecord {
                    header,
                    live: true,
                    persisted,
                },
            );
        }
        let mut listed: Vec<_> = records.into_values().collect();
        listed.sort_by(compare_sessions);
        Ok(listed)
    }

    async fn load(&self, id: &SessionId) -> Result<Arc<Session>, SessionQueryError> {
        if let Some(live) = self.sessions.get(id) {
            return Ok(live);
        }
        if let Some(persistence) = &self.persistence {
            return persistence.load(id).await.map(Arc::new).map_err(|error| {
                SessionQueryError::new(error.to_string(), SessionQueryErrorCode::SessionNotFound)
            });
        }
        Err(not_found(id))
    }

    async fn load_logical(&self, id: &SessionId) -> Result<LogicalSession, SessionQueryError> {
        if let Some(live) = self.sessions.get(id) {
            return Ok(snapshot_live(&live));
        }
        let Some(persistence) = &self.persistence else {
            return Err(not_found(id));
        };
        let listed = persistence.list_headers().await.map_err(listing_failed)?;
        let listed = listed
            .into_iter()
            .find(|header| header.id.as_str() == id.as_str())
            .ok_or_else(|| not_found(id))?;
        let loaded = persistence.inspect(id).await.map_err(|error| {
            SessionQueryError::new(
                format!("failed to inspect session \"{}\": {error}", id.as_str()),
                SessionQueryErrorCode::PersistenceFailed,
            )
        })?;
        if let Some(live) = self.sessions.get(id) {
            return Ok(snapshot_live(&live));
        }
        assert_session_headers_compatible(&loaded.meta, &listed)?;
        Ok(LogicalSession {
            header: loaded.meta,
            events: loaded.events,
        })
    }

    fn read_window(&self, name: &str, value: Option<usize>) -> Result<usize, SessionQueryError> {
        let Some(value) = value else {
            return Ok(0);
        };
        if value > self.read_window_max {
            return Err(SessionQueryError::new(
                format!(
                    "{name} must be an integer between 0 and {}",
                    self.read_window_max
                ),
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

fn snapshot_live(session: &Session) -> LogicalSession {
    LogicalSession {
        header: session.header().clone(),
        events: session.events(),
    }
}

fn listing_failed(error: impl std::fmt::Display) -> SessionQueryError {
    SessionQueryError::new(
        format!("session persistence listing failed: {error}"),
        SessionQueryErrorCode::PersistenceFailed,
    )
}

fn not_found(id: &SessionId) -> SessionQueryError {
    SessionQueryError::new(
        format!("session \"{}\" not found", id.as_str()),
        SessionQueryErrorCode::SessionNotFound,
    )
}

fn compare_sessions(left: &SessionRecord, right: &SessionRecord) -> std::cmp::Ordering {
    right
        .header
        .created_at
        .cmp(&left.header.created_at)
        .then_with(|| left.header.id.as_str().cmp(right.header.id.as_str()))
}

fn compare_children(left: &SessionRecord, right: &SessionRecord) -> std::cmp::Ordering {
    left.header
        .created_at
        .cmp(&right.header.created_at)
        .then_with(|| left.header.id.as_str().cmp(right.header.id.as_str()))
}

fn trace_session(
    records: &[SessionRecord],
    session_id: &SessionId,
) -> Result<SessionLineageTrace, SessionQueryError> {
    let by_id: HashMap<&str, &SessionRecord> = records
        .iter()
        .map(|record| (record.header.id.as_str(), record))
        .collect();
    let target = by_id.get(session_id.as_str()).copied().ok_or_else(|| {
        SessionQueryError::new(
            format!("session \"{}\" not found", session_id.as_str()),
            SessionQueryErrorCode::SessionNotFound,
        )
    })?;

    let mut ancestors = Vec::new();
    let mut ancestry_seen = HashSet::new();
    ancestry_seen.insert(session_id.as_str().to_string());
    let mut unresolved_parent_id = None;
    let mut parent_id = target.header.parent_session.clone();
    while let Some(current) = parent_id {
        if !ancestry_seen.insert(current.as_str().to_string()) {
            return Err(SessionQueryError::new(
                format!(
                    "session lineage contains a cycle at \"{}\"",
                    current.as_str()
                ),
                SessionQueryErrorCode::InvalidLineage,
            ));
        }
        match by_id.get(current.as_str()) {
            Some(parent) => {
                ancestors.push((*parent).clone());
                parent_id = parent.header.parent_session.clone();
            }
            None => {
                unresolved_parent_id = Some(current);
                break;
            }
        }
    }

    let descendants = build_descendants(records, session_id);
    let target = target.clone();
    match unresolved_parent_id {
        Some(unresolved_parent_id) => Ok(SessionLineageTrace::Incomplete {
            target,
            ancestors,
            descendants,
            unresolved_parent_id,
        }),
        None => {
            let root = ancestors.last().cloned().unwrap_or_else(|| target.clone());
            Ok(SessionLineageTrace::Complete {
                target,
                ancestors,
                descendants,
                root,
            })
        }
    }
}

fn build_descendants(records: &[SessionRecord], session_id: &SessionId) -> Vec<SessionLineageNode> {
    let mut children_by_parent: HashMap<&str, Vec<SessionRecord>> = HashMap::new();
    for record in records {
        if let Some(parent) = &record.header.parent_session {
            children_by_parent
                .entry(parent.as_str())
                .or_default()
                .push(record.clone());
        }
    }
    for children in children_by_parent.values_mut() {
        children.sort_by(compare_children);
    }

    struct Draft {
        session: SessionRecord,
        child_indexes: Vec<usize>,
    }

    let mut drafts = Vec::new();
    let mut pending = Vec::new();
    let Some(roots) = children_by_parent.get(session_id.as_str()) else {
        return Vec::new();
    };
    for child in roots {
        let index = drafts.len();
        drafts.push(Draft {
            session: child.clone(),
            child_indexes: Vec::new(),
        });
        pending.push(index);
    }
    let root_count = pending.len();

    let mut cursor = 0;
    while cursor < pending.len() {
        let index = pending[cursor];
        cursor += 1;
        let child_id = drafts[index].session.header.id.as_str().to_string();
        let Some(children) = children_by_parent.get(child_id.as_str()) else {
            continue;
        };
        let mut child_indexes = Vec::with_capacity(children.len());
        for child in children {
            let child_index = drafts.len();
            drafts.push(Draft {
                session: child.clone(),
                child_indexes: Vec::new(),
            });
            child_indexes.push(child_index);
            pending.push(child_index);
        }
        drafts[index].child_indexes = child_indexes;
    }

    let mut materialized = vec![None; drafts.len()];
    for index in (0..drafts.len()).rev() {
        let descendants = drafts[index]
            .child_indexes
            .iter()
            .map(|&child| {
                materialized[child]
                    .take()
                    .expect("child nodes are appended after their parent")
            })
            .collect();
        materialized[index] = Some(SessionLineageNode {
            session: drafts[index].session.clone(),
            descendants,
        });
    }
    (0..root_count)
        .map(|index| {
            materialized[index]
                .take()
                .expect("root nodes occupy the first draft slots")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_llm::{AssistantMessage, ContentBlock, StreamChunk, UserMessage};
    use dsh_session::{
        session_id, SessionEvent, SessionEventData, SurfaceOp, SESSION_FORMAT_VERSION,
    };
    use dsh_session_persistence::{
        PersistenceError, PersistenceRuntime, SessionInspection, SessionStoreBackend,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemoryEntry {
        header: SessionHeader,
        events: Vec<SessionEvent>,
    }

    #[derive(Default)]
    struct MemoryBackend {
        entries: Mutex<HashMap<String, MemoryEntry>>,
        list_error: Mutex<Option<String>>,
        inspect_error: Mutex<Option<String>>,
        inspect_meta: Mutex<Option<SessionHeader>>,
    }

    impl MemoryBackend {
        fn insert(&self, header: SessionHeader) {
            self.insert_raw(header, Vec::new());
        }

        fn insert_raw(&self, header: SessionHeader, events: Vec<SessionEvent>) {
            self.entries.lock().expect("memory entries").insert(
                header.id.as_str().to_string(),
                MemoryEntry { header, events },
            );
        }

        fn fail_list(&self, message: impl Into<String>) {
            *self.list_error.lock().expect("list error") = Some(message.into());
        }

        fn fail_inspect(&self, message: impl Into<String>) {
            *self.inspect_error.lock().expect("inspect error") = Some(message.into());
        }

        fn patch_inspect_meta(&self, header: SessionHeader) {
            *self.inspect_meta.lock().expect("inspect meta") = Some(header);
        }
    }

    #[async_trait]
    impl SessionStoreBackend for MemoryBackend {
        async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
            self.insert_raw(session.header().clone(), session.events());
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
            let inspection = self.inspect(id).await?;
            if inspection.events.is_empty() {
                return Ok(Session::with_header(inspection.meta));
            }
            inspection.into_session()
        }

        async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
            if let Some(message) = self.inspect_error.lock().expect("inspect error").clone() {
                return Err(PersistenceError::Format(message));
            }
            let entries = self.entries.lock().expect("memory entries");
            let entry = entries
                .get(id.as_str())
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))?;
            let mut meta = entry.header.clone();
            if let Some(patch) = self.inspect_meta.lock().expect("inspect meta").clone() {
                if patch.id.as_str() == id.as_str() {
                    meta = patch;
                }
            }
            Ok(SessionInspection {
                meta,
                events: entry.events.clone(),
            })
        }

        async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
            if let Some(message) = self.list_error.lock().expect("list error").clone() {
                return Err(PersistenceError::Format(message));
            }
            Ok(self
                .entries
                .lock()
                .expect("memory entries")
                .keys()
                .map(|id| session_id(id.clone()))
                .collect())
        }

        async fn list_headers(&self) -> Result<Vec<SessionHeader>, PersistenceError> {
            if let Some(message) = self.list_error.lock().expect("list error").clone() {
                return Err(PersistenceError::Format(message));
            }
            Ok(self
                .entries
                .lock()
                .expect("memory entries")
                .values()
                .map(|entry| entry.header.clone())
                .collect())
        }
    }

    fn header(id: &str, created_at: u64, parent: Option<&str>, cwd: Option<&str>) -> SessionHeader {
        SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: session_id(id),
            created_at,
            cwd: cwd.map(str::to_string),
            parent_session: parent.map(session_id),
            seed_length: None,
            origin: None,
            delegation_depth: 0,
        }
    }

    fn engine_with(
        store: Arc<SessionStore>,
        persistence: Option<Arc<PersistenceRuntime>>,
    ) -> SessionQueryEngine {
        SessionQueryEngine::new(
            store,
            persistence,
            SESSION_QUERY_READ_WINDOW_MAX,
            Arc::new(DisabledSearch),
        )
    }

    fn raw_user(seq: u64, sources: Option<Vec<u64>>) -> SessionEvent {
        SessionEvent {
            seq,
            time: seq + 1,
            data: SessionEventData::UserMessage(UserMessage::text(format!("event {seq}"))),
            source_event_seqs: sources,
            surface_op: Some(SurfaceOp::Append),
            ignorable: false,
        }
    }

    fn append_trace_events(session: &Session) {
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(SessionEventData::StepStart { turn: 1, step: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::TextDelta {
                        index: 0,
                        text: "draft".into(),
                    },
                },
                None,
            )
            .unwrap();
        session
            .append_cited(
                SessionEventData::UserMessage(UserMessage::text("original")),
                SurfaceOp::Append,
                vec![2],
            )
            .unwrap();
        session
            .append_cited(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::text("summary one")],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                SurfaceOp::Replace { start: 3, end: 3 },
                vec![3, 2],
            )
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::notice("test", "context", "context")),
                Some(SurfaceOp::Append),
            )
            .unwrap();
        session
            .append(SessionEventData::StepEnd { turn: 1, step: 1 }, None)
            .unwrap();
        session
            .append(SessionEventData::StepStart { turn: 1, step: 2 }, None)
            .unwrap();
        session
            .append_cited(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 2,
                    message: AssistantMessage::model(
                        vec![ContentBlock::text("summary two")],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                SurfaceOp::Replace { start: 4, end: 4 },
                vec![2, 4],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn exact_reads_and_disabled_search() {
        let store = Arc::new(SessionStore::new());
        let session = store.publish(Session::with_header(header("s", 1, None, None)));
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
        assert_eq!(listed[0].header.id.as_str(), "s");
        assert!(listed[0].live);
        assert!(!listed[0].persisted);
        assert_eq!(
            engine
                .read_title(&session_id("s"))
                .await
                .unwrap()
                .as_deref(),
            Some("hello")
        );
        let window = engine
            .read_event(&session_id("s"), 0, Some(0), Some(1))
            .await
            .unwrap();
        assert_eq!(window.target.seq, 0);
        assert_eq!(window.events.len(), 2);
        let lineage = engine.trace_session(&session_id("s")).await.unwrap();
        assert!(lineage.ancestors().is_empty());
        assert!(lineage.is_complete());
        let err = engine.search_sessions("hello").await.unwrap_err();
        assert_eq!(err.code, SessionQueryErrorCode::SearchDisabled);
        assert_eq!(err.code_str(), "SESSION_QUERY_SEARCH_DISABLED");
        let missing = engine
            .read_session(&session_id("missing"))
            .await
            .unwrap_err();
        assert_eq!(missing.code, SessionQueryErrorCode::SessionNotFound);
        assert_eq!(missing.message, "session \"missing\" not found");
    }

    #[tokio::test]
    async fn lists_newest_first_with_live_and_persisted_bits() {
        let store = Arc::new(SessionStore::new());
        let backend = Arc::new(MemoryBackend::default());
        let shared = header("shared", 3, None, Some("/same"));
        let durable = header("durable", 2, None, None);
        backend.insert(shared.clone());
        backend.insert(durable.clone());
        store.publish(Session::with_header(shared.clone()));
        store.publish(Session::with_header(header("live-only", 1, None, None)));
        let persistence = Arc::new(PersistenceRuntime::new(backend));
        let engine = engine_with(store, Some(persistence));

        let listed = engine.list_sessions().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| (
                    record.header.id.as_str().to_string(),
                    record.live,
                    record.persisted
                ))
                .collect::<Vec<_>>(),
            vec![
                ("shared".into(), true, true),
                ("durable".into(), false, true),
                ("live-only".into(), true, false),
            ]
        );
    }

    #[tokio::test]
    async fn newest_first_breaks_ties_by_id() {
        let store = Arc::new(SessionStore::new());
        store.publish(Session::with_header(header("b", 5, None, None)));
        store.publish(Session::with_header(header("a", 5, None, None)));
        let listed = disabled_engine(store).list_sessions().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| record.header.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn rejects_conflicting_live_and_persisted_headers() {
        let store = Arc::new(SessionStore::new());
        let backend = Arc::new(MemoryBackend::default());
        backend.insert(header("shared", 3, None, Some("/same")));
        store.publish(Session::with_header(header(
            "shared",
            3,
            None,
            Some("/conflict"),
        )));
        let persistence = Arc::new(PersistenceRuntime::new(backend));
        let error = engine_with(store, Some(persistence))
            .list_sessions()
            .await
            .unwrap_err();
        assert_eq!(error.code, SessionQueryErrorCode::SourceConflict);
        assert_eq!(error.code_str(), "SESSION_QUERY_SOURCE_CONFLICT");
        assert_eq!(
            error.message,
            "session source headers conflict for session \"shared\""
        );
    }

    #[tokio::test]
    async fn header_compatibility_ignores_origin() {
        let mut persisted = header("shared", 1, None, Some("/work"));
        persisted.origin = Some("disk".into());
        let mut live = header("shared", 1, None, Some("/work"));
        live.origin = Some("memory".into());
        assert_session_headers_compatible(&live, &persisted).unwrap();

        live.delegation_depth = 1;
        let error = assert_session_headers_compatible(&live, &persisted).unwrap_err();
        assert_eq!(error.code, SessionQueryErrorCode::SourceConflict);
    }

    #[tokio::test]
    async fn traces_complete_ancestry_and_sorted_descendants() {
        let store = Arc::new(SessionStore::new());
        store.publish(Session::with_header(header("root", 0, None, None)));
        store.publish(Session::with_header(header(
            "parent",
            1,
            Some("root"),
            None,
        )));
        store.publish(Session::with_header(header(
            "target",
            2,
            Some("parent"),
            None,
        )));
        store.publish(Session::with_header(header("b", 4, Some("target"), None)));
        store.publish(Session::with_header(header("a", 4, Some("target"), None)));
        store.publish(Session::with_header(header(
            "older",
            3,
            Some("target"),
            None,
        )));
        store.publish(Session::with_header(header(
            "grandchild",
            5,
            Some("a"),
            None,
        )));
        let engine = disabled_engine(store);
        let SessionLineageTrace::Complete {
            target,
            ancestors,
            descendants,
            root,
        } = engine.trace_session(&session_id("target")).await.unwrap()
        else {
            panic!("expected complete lineage");
        };
        assert_eq!(target.header.id.as_str(), "target");
        assert_eq!(
            ancestors
                .iter()
                .map(|record| record.header.id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "root"]
        );
        assert_eq!(root.header.id.as_str(), "root");
        assert_eq!(
            descendants
                .iter()
                .map(|node| node.session.header.id.as_str())
                .collect::<Vec<_>>(),
            vec!["older", "a", "b"]
        );
        assert_eq!(
            descendants[1]
                .descendants
                .iter()
                .map(|node| node.session.header.id.as_str())
                .collect::<Vec<_>>(),
            vec!["grandchild"]
        );
    }

    #[tokio::test]
    async fn traces_root_and_unresolved_parent() {
        let store = Arc::new(SessionStore::new());
        store.publish(Session::with_header(header("root", 1, None, None)));
        store.publish(Session::with_header(header(
            "partial",
            2,
            Some("outside"),
            None,
        )));
        let engine = disabled_engine(store);

        let root = engine.trace_session(&session_id("root")).await.unwrap();
        assert!(root.is_complete());
        assert!(root.ancestors().is_empty());
        match &root {
            SessionLineageTrace::Complete { root, .. } => {
                assert_eq!(root.header.id.as_str(), "root");
            }
            SessionLineageTrace::Incomplete { .. } => panic!("root should be complete"),
        }

        match engine.trace_session(&session_id("partial")).await.unwrap() {
            SessionLineageTrace::Incomplete {
                unresolved_parent_id,
                ancestors,
                ..
            } => {
                assert_eq!(unresolved_parent_id.as_str(), "outside");
                assert!(ancestors.is_empty());
            }
            SessionLineageTrace::Complete { .. } => panic!("expected incomplete lineage"),
        }
    }

    #[tokio::test]
    async fn rejects_cycles_and_missing_targets() {
        let store = Arc::new(SessionStore::new());
        store.publish(Session::with_header(header("a", 1, Some("b"), None)));
        store.publish(Session::with_header(header("b", 2, Some("a"), None)));
        let engine = disabled_engine(store);

        let cycle = engine.trace_session(&session_id("a")).await.unwrap_err();
        assert_eq!(cycle.code, SessionQueryErrorCode::InvalidLineage);
        assert_eq!(cycle.code_str(), "SESSION_QUERY_INVALID_LINEAGE");
        assert_eq!(cycle.message, "session lineage contains a cycle at \"a\"");

        let missing = engine
            .trace_session(&session_id("missing"))
            .await
            .unwrap_err();
        assert_eq!(missing.code, SessionQueryErrorCode::SessionNotFound);
        assert_eq!(missing.message, "session \"missing\" not found");
    }

    #[tokio::test]
    async fn lineage_uses_one_corpus_listing_and_preserves_persistence_failure() {
        let backend = Arc::new(MemoryBackend::default());
        backend.insert(header("durable", 1, None, None));
        let persistence = Arc::new(PersistenceRuntime::new(
            Arc::clone(&backend) as Arc<dyn SessionStoreBackend>
        ));
        let engine = engine_with(
            Arc::new(SessionStore::new()),
            Some(Arc::clone(&persistence)),
        );

        match engine.trace_session(&session_id("durable")).await.unwrap() {
            SessionLineageTrace::Complete { target, .. } => {
                assert!(!target.live);
                assert!(target.persisted);
            }
            SessionLineageTrace::Incomplete { .. } => panic!("durable root should be complete"),
        }

        backend.fail_list("unavailable");
        let error = engine
            .trace_session(&session_id("durable"))
            .await
            .unwrap_err();
        assert_eq!(error.code, SessionQueryErrorCode::PersistenceFailed);
        assert_eq!(
            error.message,
            "session persistence listing failed: unavailable"
        );
        let listed = engine.list_sessions().await.unwrap_err();
        assert_eq!(listed.code, SessionQueryErrorCode::PersistenceFailed);
    }

    #[tokio::test]
    async fn descendant_trees_are_iterative() {
        let store = Arc::new(SessionStore::new());
        store.publish(Session::with_header(header("deep-0", 0, None, None)));
        let mut parent = "deep-0".to_string();
        for depth in 1..256 {
            let id = format!("deep-{depth}");
            store.publish(Session::with_header(header(
                &id,
                depth as u64,
                Some(&parent),
                None,
            )));
            parent = id;
        }
        let trace = disabled_engine(store)
            .trace_session(&session_id("deep-0"))
            .await
            .unwrap();
        assert!(trace.is_complete());
        let mut node = trace.descendants().first();
        for depth in 1..256 {
            let current = node.expect("lineage ended early");
            if depth == 255 {
                assert_eq!(current.session.header.id.as_str(), "deep-255");
            }
            node = current.descendants.first();
        }
        assert!(node.is_none());
    }

    #[tokio::test]
    async fn classifies_current_shadowed_and_log_only_events() {
        let store = Arc::new(SessionStore::new());
        let session = store.publish(Session::with_header(header("surface", 1, None, None)));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(SessionEventData::StepStart { turn: 1, step: 1 }, None)
            .unwrap();
        let first = session
            .append(
                SessionEventData::UserMessage(UserMessage::text("first")),
                Some(SurfaceOp::Append),
            )
            .unwrap();
        session
            .append(
                SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::TextDelta {
                        index: 0,
                        text: "draft".into(),
                    },
                },
                None,
            )
            .unwrap();
        session
            .append_cited(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::text("replacement")],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                SurfaceOp::Replace {
                    start: first.seq,
                    end: first.seq,
                },
                vec![first.seq],
            )
            .unwrap();
        let records = disabled_engine(store)
            .list_events(&session_id("surface"))
            .await
            .unwrap();
        assert_eq!(
            records
                .iter()
                .skip(2)
                .map(|record| record.surface)
                .collect::<Vec<_>>(),
            vec![
                SessionEventSurface::Shadowed,
                SessionEventSurface::LogOnly,
                SessionEventSurface::Current,
            ]
        );
    }

    #[tokio::test]
    async fn reads_current_surface_and_empty_capture_boundary() {
        let store = Arc::new(SessionStore::new());
        let session = store.publish(Session::with_header(header(
            "surface-snapshot",
            1,
            None,
            Some("/work"),
        )));
        let first = session
            .append(
                SessionEventData::UserMessage(UserMessage::text("old")),
                Some(SurfaceOp::Append),
            )
            .unwrap();
        session
            .append(
                SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::TextDelta {
                        index: 0,
                        text: "draft".into(),
                    },
                },
                None,
            )
            .unwrap();
        session
            .append_cited(
                SessionEventData::UserMessage(UserMessage::notice(
                    "compact",
                    "checkpoint",
                    "checkpoint",
                )),
                SurfaceOp::Replace {
                    start: first.seq,
                    end: first.seq,
                },
                vec![first.seq],
            )
            .unwrap();
        let retained = session
            .append(
                SessionEventData::UserMessage(UserMessage::text("retained tail")),
                Some(SurfaceOp::Append),
            )
            .unwrap();
        session
            .append_cited(
                SessionEventData::UserMessage(UserMessage::notice(
                    "compact",
                    "latest checkpoint",
                    "latest checkpoint",
                )),
                SurfaceOp::Replace {
                    start: 2,
                    end: retained.seq,
                },
                vec![2, retained.seq],
            )
            .unwrap();
        session
            .append(
                SessionEventData::AssistantMessage {
                    turn: 2,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::text("latest answer")],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                Some(SurfaceOp::Append),
            )
            .unwrap();
        let engine = disabled_engine(Arc::clone(&store));
        let snapshot = engine
            .read_surface(&session_id("surface-snapshot"))
            .await
            .unwrap();
        assert_eq!(snapshot.session.cwd.as_deref(), Some("/work"));
        assert_eq!(snapshot.captured_through_seq, Some(5));
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );

        let empty = store.publish(Session::with_header(header("empty-surface", 2, None, None)));
        let empty_snapshot = engine.read_surface(empty.id()).await.unwrap();
        assert_eq!(empty_snapshot.captured_through_seq, None);
        assert!(empty_snapshot.events.is_empty());
    }

    #[tokio::test]
    async fn traces_replacement_chain_and_cited_sources() {
        let store = Arc::new(SessionStore::new());
        let session = store.publish(Session::with_header(header("trace", 1, None, None)));
        append_trace_events(&session);
        let engine = disabled_engine(store);

        let original = engine.trace_event(&session_id("trace"), 3).await.unwrap();
        assert_eq!(original.trace.target.seq, 3);
        assert_eq!(original.trace.target.surface, SessionEventSurface::Shadowed);
        assert_eq!(original.trace.replaced_by, Some(4));
        assert_eq!(original.trace.replacement_chain, vec![4, 8]);
        assert!(original.trace.replaced_event_seqs.is_empty());
        assert_eq!(original.trace.source_event_seqs, vec![2]);
        assert_eq!(original.trace.derived_event_seqs, vec![4]);

        let mid = engine.trace_event(&session_id("trace"), 4).await.unwrap();
        assert_eq!(mid.trace.replaced_by, Some(8));
        assert_eq!(mid.trace.replacement_chain, vec![8]);
        assert_eq!(mid.trace.replaced_event_seqs, vec![3]);
        assert_eq!(mid.trace.source_event_seqs, vec![3, 2]);
        assert_eq!(mid.trace.derived_event_seqs, vec![8]);

        let chunk = engine.trace_event(&session_id("trace"), 2).await.unwrap();
        assert_eq!(chunk.trace.target.surface, SessionEventSurface::LogOnly);
        assert!(chunk.trace.replacement_chain.is_empty());
        assert!(chunk.trace.source_event_seqs.is_empty());
        assert_eq!(chunk.trace.derived_event_seqs, vec![3, 4, 8]);

        let latest = engine.trace_event(&session_id("trace"), 8).await.unwrap();
        assert!(latest.trace.replacement_chain.is_empty());
        assert_eq!(latest.trace.replaced_event_seqs, vec![4]);
        assert_eq!(latest.trace.source_event_seqs, vec![2, 4]);
        assert!(latest.trace.derived_event_seqs.is_empty());
    }

    #[tokio::test]
    async fn load_logical_prefers_live_and_preserves_list_inspect_failures() {
        let durable = header("shared", 1, None, Some("/same"));
        let backend = Arc::new(MemoryBackend::default());
        backend.insert_raw(durable.clone(), vec![raw_user(0, None)]);
        let persistence = Arc::new(PersistenceRuntime::new(
            Arc::clone(&backend) as Arc<dyn SessionStoreBackend>
        ));
        let store = Arc::new(SessionStore::new());
        let engine = engine_with(Arc::clone(&store), Some(Arc::clone(&persistence)));

        let persisted = engine.trace_event(&session_id("shared"), 0).await.unwrap();
        assert_eq!(persisted.trace.target.surface, SessionEventSurface::Current);

        let live = store.publish(Session::with_header(durable.clone()));
        live.append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        live.append(
            SessionEventData::UserMessage(UserMessage::notice("test", "live", "live")),
            Some(SurfaceOp::Append),
        )
        .unwrap();
        backend.fail_list("list unavailable");
        backend.fail_inspect("inspect unavailable");
        let live_trace = engine.trace_event(&session_id("shared"), 1).await.unwrap();
        assert_eq!(live_trace.trace.target.type_name, "user/message");

        let failed_store = Arc::new(SessionStore::new());
        let failed_backend = Arc::new(MemoryBackend::default());
        failed_backend.insert_raw(durable.clone(), vec![raw_user(0, None)]);
        let failed_engine = engine_with(
            failed_store,
            Some(Arc::new(PersistenceRuntime::new(
                Arc::clone(&failed_backend) as Arc<dyn SessionStoreBackend>,
            ))),
        );
        failed_backend.fail_list("list unavailable");
        let list_error = failed_engine
            .trace_event(&session_id("shared"), 0)
            .await
            .unwrap_err();
        assert_eq!(list_error.code, SessionQueryErrorCode::PersistenceFailed);
        assert_eq!(
            list_error.message,
            "session persistence listing failed: list unavailable"
        );
        *failed_backend.list_error.lock().expect("list error") = None;
        failed_backend.fail_inspect("inspect unavailable");
        let inspect_error = failed_engine
            .trace_event(&session_id("shared"), 0)
            .await
            .unwrap_err();
        assert_eq!(inspect_error.code, SessionQueryErrorCode::PersistenceFailed);
        assert_eq!(
            inspect_error.message,
            "failed to inspect session \"shared\": inspect unavailable"
        );
        *failed_backend.inspect_error.lock().expect("inspect error") = None;
        let mut changed = durable.clone();
        changed.cwd = Some("/changed".into());
        failed_backend.patch_inspect_meta(changed);
        let conflict = failed_engine
            .trace_event(&session_id("shared"), 0)
            .await
            .unwrap_err();
        assert_eq!(conflict.code, SessionQueryErrorCode::SourceConflict);
    }

    #[tokio::test]
    async fn checks_target_existence_before_surface_analysis() {
        let backend = Arc::new(MemoryBackend::default());
        backend.insert_raw(
            header("bad-target", 1, None, None),
            vec![
                raw_user(0, None),
                SessionEvent {
                    seq: 1,
                    time: 2,
                    data: SessionEventData::AssistantMessage {
                        turn: 1,
                        step: 1,
                        message: AssistantMessage::model(vec![], "mock", "mock"),
                        usage: None,
                    },
                    source_event_seqs: Some(vec![]),
                    surface_op: Some(SurfaceOp::Replace { start: 9, end: 9 }),
                    ignorable: false,
                },
            ],
        );
        let engine = engine_with(
            Arc::new(SessionStore::new()),
            Some(Arc::new(PersistenceRuntime::new(
                backend as Arc<dyn SessionStoreBackend>,
            ))),
        );
        let missing = engine
            .trace_event(&session_id("bad-target"), 9)
            .await
            .unwrap_err();
        assert_eq!(missing.code, SessionQueryErrorCode::EventNotFound);
        assert_eq!(
            missing.message,
            "session \"bad-target\" has no event at seq 9"
        );
        let invalid = engine
            .trace_event(&session_id("bad-target"), 0)
            .await
            .unwrap_err();
        assert_eq!(invalid.code, SessionQueryErrorCode::InvalidSurface);
        assert!(invalid.message.starts_with("invalid session surface:"));
        assert_eq!(invalid.code_str(), "SESSION_QUERY_INVALID_SURFACE");
    }

    #[tokio::test]
    async fn rejects_invalid_persisted_surfaces_on_list_and_trace() {
        let cases: Vec<(&str, Vec<SessionEvent>)> = vec![
            (
                "non-surface sources",
                vec![SessionEvent {
                    seq: 0,
                    time: 1,
                    data: SessionEventData::TurnStart { turn: 1 },
                    source_event_seqs: Some(vec![0]),
                    surface_op: None,
                    ignorable: false,
                }],
            ),
            ("empty sources", vec![raw_user(0, Some(vec![]))]),
            (
                "duplicate sources",
                vec![raw_user(0, None), raw_user(1, Some(vec![0, 0]))],
            ),
            (
                "future source",
                vec![raw_user(0, Some(vec![1])), raw_user(1, None)],
            ),
            (
                "replacement without sources",
                vec![
                    raw_user(0, None),
                    SessionEvent {
                        seq: 1,
                        time: 2,
                        data: SessionEventData::UserMessage(UserMessage::text("next")),
                        source_event_seqs: None,
                        surface_op: Some(SurfaceOp::Replace { start: 0, end: 0 }),
                        ignorable: false,
                    },
                ],
            ),
            (
                "surfaceOp on non-surface",
                vec![SessionEvent {
                    seq: 0,
                    time: 1,
                    data: SessionEventData::TurnStart { turn: 1 },
                    source_event_seqs: None,
                    surface_op: Some(SurfaceOp::Append),
                    ignorable: false,
                }],
            ),
        ];
        for (name, events) in cases {
            let backend = Arc::new(MemoryBackend::default());
            backend.insert_raw(header(name, 1, None, None), events);
            let engine = engine_with(
                Arc::new(SessionStore::new()),
                Some(Arc::new(PersistenceRuntime::new(
                    backend as Arc<dyn SessionStoreBackend>,
                ))),
            );
            let id = session_id(name);
            let traced = engine.trace_event(&id, 0).await.unwrap_err();
            assert_eq!(traced.code, SessionQueryErrorCode::InvalidSurface, "{name}");
            let listed = engine.list_events(&id).await.unwrap_err();
            assert_eq!(listed.code, SessionQueryErrorCode::InvalidSurface, "{name}");
        }
    }

    #[tokio::test]
    async fn filters_sessions_and_events() {
        let durable = header("durable-filter", 1, None, None);
        let backend = Arc::new(MemoryBackend::default());
        backend.insert_raw(durable.clone(), vec![raw_user(0, None)]);
        let store = Arc::new(SessionStore::new());
        let live = store.publish(Session::with_header(header("live-filter", 2, None, None)));
        live.append(
            SessionEventData::UserMessage(UserMessage::text("live")),
            Some(SurfaceOp::Append),
        )
        .unwrap();
        let engine = engine_with(
            store,
            Some(Arc::new(PersistenceRuntime::new(
                backend as Arc<dyn SessionStoreBackend>,
            ))),
        );
        let filtered = engine
            .filter_sessions(&[SessionResultFilter::Id {
                values: vec!["durable-filter".into()],
            }])
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].header.id.as_str(), "durable-filter");
        assert!(!filtered[0].live);
        assert!(filtered[0].persisted);

        let events = engine
            .filter_events(
                &session_id("live-filter"),
                &[SessionEventResultFilter::Surface {
                    values: vec![SessionEventSurface::Current],
                }],
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "live");
        assert_eq!(events[0].surface, SessionEventSurface::Current);

        let text = engine
            .filter_events(
                &session_id("live-filter"),
                &[SessionEventResultFilter::Text {
                    text: "live".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(text[0].seq, 0);

        let unknown = unknown_filter_kind("future");
        assert_eq!(unknown.code, SessionQueryErrorCode::InvalidFilter);
        assert_eq!(unknown.code_str(), "SESSION_QUERY_INVALID_FILTER");
        assert_eq!(unknown.message, "session unknown filter kind \"future\"");
    }
}
