//! Concrete `ctx.sessionQuery` provider.
//!
//! `openAt: never` keeps exact reads available and fails search with
//! `SESSION_QUERY_SEARCH_DISABLED` without opening SQLite. `startup` and
//! `first-search` open a derived FTS5 index (`SCHEMA_VERSION` 1) over the
//! live-preferred corpus.

use async_trait::async_trait;
use dsh_cordis::{Context, CordisError, Result};
use dsh_session::{derive_event_message, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use dsh_session_query::{
    DisabledSearch, SessionQueryEngine, SessionQueryError, SessionSearch, SessionSearchHit,
    SESSION_QUERY_READ_WINDOW_MAX,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Current derived-index schema. Incompatible versions are refused.
pub const SESSION_QUERY_SQLITE_SCHEMA_VERSION: u32 = 1;
/// Application id protecting unrelated databases from a derived reset.
pub const SESSION_QUERY_SQLITE_APPLICATION_ID: i32 = 0x4453_4851;
/// Default result page size.
pub const SESSION_QUERY_SQLITE_DEFAULT_LIMIT: usize = 20;

/// SQLite module/handle opening phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAt {
    /// Open before service publication.
    Startup,
    /// Open on the first search.
    FirstSearch,
    /// Disable full-text search entirely.
    Never,
}

impl OpenAt {
    /// Parse the TypeScript config token.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "startup" => Some(Self::Startup),
            "first-search" => Some(Self::FirstSearch),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Combined session-query configuration backed by SQLite full-text search.
#[derive(Debug, Clone)]
pub struct Config {
    /// Dedicated derived-index path; `:memory:` is supported.
    pub path: String,
    /// When the SQLite handle opens.
    pub open_at: OpenAt,
    /// Maximum `before`/`after` raw-event count for inherited `read_event`.
    pub read_window_max: usize,
}

impl Config {
    /// Resolve plugin config. `path` is required; `openAt` defaults to `startup`.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let path = value
            .and_then(|value| value.get("path"))
            .and_then(Value::as_str)
            .ok_or_else(|| "session-query-sqlite: path is required".to_string())?
            .to_string();
        let open_at = match value
            .and_then(|value| value.get("openAt"))
            .and_then(Value::as_str)
        {
            None => OpenAt::Startup,
            Some(token) => OpenAt::parse(token).ok_or_else(|| {
                format!("session-query-sqlite: openAt must be startup, first-search, or never (got {token})")
            })?,
        };
        let read_window_max = match value.and_then(|value| value.get("readWindowMax")) {
            None => SESSION_QUERY_READ_WINDOW_MAX,
            Some(raw) => raw
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| {
                    "session-query-sqlite: readWindowMax must be a non-negative integer".to_string()
                })?,
        };
        Ok(Self {
            path,
            open_at,
            read_window_max,
        })
    }
}

/// Live-preferred FTS5 index opened on demand.
pub struct SqliteSearch {
    path: String,
    open_at: OpenAt,
    sessions: Arc<SessionStore>,
    db: Mutex<Option<Connection>>,
}

impl SqliteSearch {
    fn new(path: String, open_at: OpenAt, sessions: Arc<SessionStore>) -> Self {
        Self {
            path,
            open_at,
            sessions,
            db: Mutex::new(None),
        }
    }

    fn ensure_open(&self) -> std::result::Result<(), SessionQueryError> {
        if self.open_at == OpenAt::Never {
            return Err(dsh_session_query::search_disabled());
        }
        let mut guard = self.db.lock().expect("session-query sqlite");
        if guard.is_some() {
            return Ok(());
        }
        let db = open_index(&self.path).map_err(|error| {
            SessionQueryError::new(
                error,
                dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
            )
        })?;
        *guard = Some(db);
        Ok(())
    }

    fn rebuild(&self, db: &Connection) -> std::result::Result<(), SessionQueryError> {
        db.execute_batch("DELETE FROM docs;")
            .map_err(|error| {
                SessionQueryError::new(
                    error.to_string(),
                    dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
                )
            })?;
        let mut insert = db
            .prepare("INSERT INTO docs(session_id, seq, body) VALUES (?1, ?2, ?3)")
            .map_err(|error| {
                SessionQueryError::new(
                    error.to_string(),
                    dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
                )
            })?;
        for session in self.sessions.live() {
            for event in session.events() {
                let body = event_text(&event.data);
                if body.is_empty() {
                    continue;
                }
                insert
                    .execute(rusqlite::params![
                        session.id().as_str(),
                        event.seq as i64,
                        body
                    ])
                    .map_err(|error| {
                        SessionQueryError::new(
                            error.to_string(),
                            dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn query(
        &self,
        session_id: Option<&str>,
        query: &str,
    ) -> std::result::Result<Vec<SessionSearchHit>, SessionQueryError> {
        self.ensure_open()?;
        let guard = self.db.lock().expect("session-query sqlite");
        let db = guard.as_ref().expect("opened");
        self.rebuild(db)?;
        let phrase = sanitize_fts(query);
        let sql = if session_id.is_some() {
            "SELECT session_id, seq, snippet(docs, 2, '', '', '…', 16) \
             FROM docs WHERE docs MATCH ?1 AND session_id = ?2 LIMIT ?3"
        } else {
            "SELECT session_id, seq, snippet(docs, 2, '', '', '…', 16) \
             FROM docs WHERE docs MATCH ?1 LIMIT ?2"
        };
        let mut stmt = db.prepare(sql).map_err(|error| {
            SessionQueryError::new(
                error.to_string(),
                dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
            )
        })?;
        let rows = if let Some(session_id) = session_id {
            stmt.query_map(
                rusqlite::params![phrase, session_id, SESSION_QUERY_SQLITE_DEFAULT_LIMIT as i64],
                map_hit,
            )
        } else {
            stmt.query_map(
                rusqlite::params![phrase, SESSION_QUERY_SQLITE_DEFAULT_LIMIT as i64],
                map_hit,
            )
        };
        let mapped = rows.map_err(|error| {
            SessionQueryError::new(
                error.to_string(),
                dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
            )
        })?;
        mapped
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                SessionQueryError::new(
                    error.to_string(),
                    dsh_session_query::SessionQueryErrorCode::PersistenceFailed,
                )
            })
    }
}

fn map_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSearchHit> {
    Ok(SessionSearchHit {
        session_id: row.get(0)?,
        seq: row.get::<_, i64>(1)? as u64,
        snippet: row.get(2)?,
    })
}

fn event_text(data: &dsh_session::SessionEventData) -> String {
    derive_event_message(data)
        .map(|message| format!("{message:?}"))
        .unwrap_or_else(|| serde_json::to_string(data).unwrap_or_default())
}

fn sanitize_fts(query: &str) -> String {
    let trimmed = query.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("\"{}\"", trimmed.replace('"', " "))
}

fn open_index(path: &str) -> std::result::Result<Connection, String> {
    if path != ":memory:" {
        if let Some(parent) = PathBuf::from(path).parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }
    let db = Connection::open(path).map_err(|error| error.to_string())?;
    let application_id: i32 = db
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let version: i32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if application_id != 0 && application_id != SESSION_QUERY_SQLITE_APPLICATION_ID {
        return Err(format!(
            "session-search database at \"{path}\" belongs to another application"
        ));
    }
    if application_id == SESSION_QUERY_SQLITE_APPLICATION_ID
        && version as u32 != SESSION_QUERY_SQLITE_SCHEMA_VERSION
        && version != 0
    {
        return Err(format!(
            "schema version {version} is newer than supported {SESSION_QUERY_SQLITE_SCHEMA_VERSION}"
        ));
    }
    db.pragma_update(None, "application_id", SESSION_QUERY_SQLITE_APPLICATION_ID)
        .map_err(|error| error.to_string())?;
    db.pragma_update(None, "user_version", SESSION_QUERY_SQLITE_SCHEMA_VERSION)
        .map_err(|error| error.to_string())?;
    db.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(session_id, seq UNINDEXED, body);",
    )
    .map_err(|error| error.to_string())?;
    let _ = db
        .query_row("SELECT name FROM sqlite_master WHERE name = 'docs'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional();
    Ok(db)
}

#[async_trait]
impl SessionSearch for SqliteSearch {
    async fn search_sessions(
        &self,
        query: &str,
    ) -> std::result::Result<Vec<SessionSearchHit>, SessionQueryError> {
        self.query(None, query)
    }

    async fn search_events(
        &self,
        session_id: &str,
        query: &str,
    ) -> std::result::Result<Vec<SessionSearchHit>, SessionQueryError> {
        self.query(Some(session_id), query)
    }
}

/// Provide `ctx.sessionQuery`.
pub fn install(ctx: &Context, config: Config) -> Result<Arc<SessionQueryEngine>> {
    let sessions = ctx.service::<SessionStore>()?;
    let persistence = ctx.get::<PersistenceRuntime>();
    let search: Arc<dyn SessionSearch> = match config.open_at {
        OpenAt::Never => Arc::new(DisabledSearch),
        open_at => {
            let search = Arc::new(SqliteSearch::new(
                config.path.clone(),
                open_at,
                Arc::clone(&sessions),
            ));
            if open_at == OpenAt::Startup {
                search.ensure_open().map_err(|error| {
                    CordisError::Validation(error.message)
                })?;
            }
            search
        }
    };
    let engine = Arc::new(SessionQueryEngine::new(
        sessions,
        persistence,
        config.read_window_max,
        search,
    ));
    ctx.provide(Arc::clone(&engine))?;
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData, SessionStore};

    #[tokio::test]
    async fn never_disables_search_without_opening() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        let path = std::env::temp_dir().join(format!(
            "dsh-query-never-{}.sqlite",
            std::process::id()
        ));
        let engine = install(
            &ctx,
            Config {
                path: path.display().to_string(),
                open_at: OpenAt::Never,
                read_window_max: 50,
            },
        )
        .unwrap();
        let err = engine.search_sessions("hello").await.unwrap_err();
        assert_eq!(
            err.code,
            dsh_session_query::SessionQueryErrorCode::SearchDisabled
        );
        assert!(!path.exists());
        ctx.dispose();
    }

    #[tokio::test]
    async fn first_search_indexes_live_title() {
        let ctx = Context::new();
        let store = Arc::new(SessionStore::new());
        let session = store.create(session_id("s"));
        session
            .append(
                SessionEventData::SessionTitle {
                    title: "uniqueneedlexyz".into(),
                    message_seqs: vec![],
                    source: serde_json::json!("fallback"),
                },
                None,
            )
            .unwrap();
        ctx.provide(Arc::clone(&store)).unwrap();
        let engine = install(
            &ctx,
            Config {
                path: ":memory:".into(),
                open_at: OpenAt::FirstSearch,
                read_window_max: 50,
            },
        )
        .unwrap();
        let hits = engine.search_sessions("uniqueneedlexyz").await.unwrap();
        assert_eq!(hits[0].session_id, "s");
        ctx.dispose();
    }
}
