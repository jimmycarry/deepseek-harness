//! SQLite session store. Newer `user_version` values are refused.
//!
//! Physical records are one JSON event row per seq. `append_events` inserts
//! new rows; torn-row `commit_repair` deletes from the first undecodable or
//! gapped seq. This crate's `SCHEMA_VERSION` is monotonic and starts at `1`;
//! it does not read the TypeScript packed schema.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_session::{
    interrupted_turn_closers, refuse_unknown, session_event_from_value, session_id, Session,
    SessionEvent, SessionHeader, SessionId,
};
use dsh_session_persistence::{
    session_persistence_revision, PersistenceError, PersistenceRuntime, SessionInspection,
    SessionPersistenceRevision, SessionPersistenceSnapshot, SessionStoreBackend, StoredSession,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Physical record version stamped on every database this build writes.
/// Version 2 stores the session header JSON on the `sessions` row.
pub const SCHEMA_VERSION: u32 = 2;
/// Application id reserved for DeepSeek Harness SQLite session databases.
pub const SESSION_PERSISTENCE_SQLITE_APPLICATION_ID: i32 = 0x4453_4850;

/// SQLite-backed store. One database file holds every session.
pub struct SqliteBackend {
    path: PathBuf,
    db: Mutex<Connection>,
}

impl SqliteBackend {
    /// Open or create `path` (`:memory:` is supported).
    pub fn open(path: impl Into<PathBuf>) -> std::result::Result<Self, PersistenceError> {
        let path = path.into();
        if path != PathBuf::from(":memory:") {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db = Connection::open(&path).map_err(sqlite_error)?;
        configure(&db)?;
        Ok(Self {
            path,
            db: Mutex::new(db),
        })
    }

    /// Artifact path for this store.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn configure(db: &Connection) -> std::result::Result<(), PersistenceError> {
    let application_id: i32 = db
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(sqlite_error)?;
    let version: i32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sqlite_error)?;
    if application_id != 0 && application_id != SESSION_PERSISTENCE_SQLITE_APPLICATION_ID {
        return Err(PersistenceError::Format(format!(
            "session database belongs to another application ({application_id})"
        )));
    }
    if version != 0 && version as u32 != SCHEMA_VERSION {
        return Err(PersistenceError::Format(format!(
            "schema version {version} is not supported {SCHEMA_VERSION}; no migration is provided pre-release"
        )));
    }
    db.pragma_update(
        None,
        "application_id",
        SESSION_PERSISTENCE_SQLITE_APPLICATION_ID,
    )
    .map_err(sqlite_error)?;
    db.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sqlite_error)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL,
            header TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS events (
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            payload TEXT NOT NULL,
            PRIMARY KEY (session_id, seq),
            FOREIGN KEY (session_id) REFERENCES sessions(id)
         );",
    )
    .map_err(sqlite_error)?;
    Ok(())
}

fn sqlite_error(error: rusqlite::Error) -> PersistenceError {
    PersistenceError::Format(error.to_string())
}

/// Content-addressed revision until the TypeScript incarnation counter lands.
fn sqlite_revision(store: &str, event_count: i64, header: &str) -> SessionPersistenceRevision {
    let digest = Sha256::digest(header.as_bytes());
    session_persistence_revision(format!(
        "sqlite:{store}:events:{event_count}:header:{digest:x}"
    ))
}

#[async_trait]
impl SessionStoreBackend for SqliteBackend {
    fn name(&self) -> &str {
        "sqlite"
    }

    async fn save(&self, session: &Session) -> std::result::Result<(), PersistenceError> {
        let id = session.id().as_str().to_string();
        let payloads = session
            .events()
            .into_iter()
            .map(|event| {
                serde_json::to_string(&event)
                    .map_err(|error| PersistenceError::Format(error.to_string()))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let header = serde_json::to_string(session.header())
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        let created_at = session.header().created_at as i64;
        let db = self.db.lock().expect("sqlite");
        let tx = db.unchecked_transaction().map_err(sqlite_error)?;
        tx.execute(
            "INSERT INTO sessions(id, created_at, header) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET header = excluded.header",
            rusqlite::params![id, created_at, header],
        )
        .map_err(sqlite_error)?;
        tx.execute("DELETE FROM events WHERE session_id = ?1", [&id])
            .map_err(sqlite_error)?;
        {
            let mut insert = tx
                .prepare("INSERT INTO events(session_id, seq, payload) VALUES (?1, ?2, ?3)")
                .map_err(sqlite_error)?;
            for (seq, payload) in payloads.iter().enumerate() {
                insert
                    .execute(rusqlite::params![id, seq as i64, payload])
                    .map_err(sqlite_error)?;
            }
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(())
    }

    async fn load(&self, id: &SessionId) -> std::result::Result<Session, PersistenceError> {
        self.inspect(id).await?.into_session()
    }

    async fn inspect(
        &self,
        id: &SessionId,
    ) -> std::result::Result<SessionInspection, PersistenceError> {
        let key = id.as_str().to_string();
        let (header, rows) = {
            let db = self.db.lock().expect("sqlite");
            let header: Option<String> = {
                let mut stmt = db
                    .prepare("SELECT header FROM sessions WHERE id = ?1")
                    .map_err(sqlite_error)?;
                stmt.query_row([&key], |row| row.get(0))
                    .map(Some)
                    .or_else(|error| match error {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(other),
                    })
                    .map_err(sqlite_error)?
            };
            let mut stmt = db
                .prepare("SELECT payload FROM events WHERE session_id = ?1 ORDER BY seq")
                .map_err(sqlite_error)?;
            let mapped = stmt
                .query_map([&key], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            let rows = mapped
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sqlite_error)?;
            (header, rows)
        };
        let Some(header) = header else {
            return Err(PersistenceError::NotFound(id.as_str().to_string()));
        };
        let header: SessionHeader = serde_json::from_str(&header)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        if header.id.as_str() != id.as_str() {
            return Err(PersistenceError::Format(format!(
                "stored header id {} does not match requested session {}",
                header.id.as_str(),
                id.as_str()
            )));
        }
        let mut events = Vec::new();
        for payload in rows {
            let value: Value = serde_json::from_str(&payload)
                .map_err(|error| PersistenceError::Format(error.to_string()))?;
            let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
            let ignorable = value
                .get("ignorable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            refuse_unknown(type_name, ignorable)?;
            events.push(session_event_from_value(value)?);
        }
        events.extend(interrupted_turn_closers(&events));
        Ok(SessionInspection {
            meta: header,
            events,
        })
    }

    async fn list_ids(&self) -> std::result::Result<Vec<SessionId>, PersistenceError> {
        let db = self.db.lock().expect("sqlite");
        let mut stmt = db
            .prepare("SELECT id FROM sessions ORDER BY id")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(session_id(id.map_err(sqlite_error)?));
        }
        Ok(ids)
    }

    async fn list_headers(&self) -> std::result::Result<Vec<SessionHeader>, PersistenceError> {
        let db = self.db.lock().expect("sqlite");
        let mut stmt = db
            .prepare("SELECT header FROM sessions ORDER BY id")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        let mut headers = Vec::new();
        for row in rows {
            let raw = row.map_err(sqlite_error)?;
            headers.push(
                serde_json::from_str(&raw)
                    .map_err(|error| PersistenceError::Format(error.to_string()))?,
            );
        }
        Ok(headers)
    }

    async fn read_stored_revision(
        &self,
        id: &SessionId,
    ) -> std::result::Result<Option<SessionPersistenceRevision>, PersistenceError> {
        let key = id.as_str().to_string();
        let store = self.path.to_string_lossy().into_owned();
        let row = {
            let db = self.db.lock().expect("sqlite");
            let mut stmt = db
                .prepare(
                    "SELECT header, (SELECT COUNT(*) FROM events WHERE session_id = sessions.id)
                     FROM sessions WHERE id = ?1",
                )
                .map_err(sqlite_error)?;
            stmt.query_row([&key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()
            .map_err(sqlite_error)?
        };
        Ok(row.map(|(header, count)| sqlite_revision(&store, count, &header)))
    }

    async fn list_snapshots(
        &self,
    ) -> std::result::Result<Vec<SessionPersistenceSnapshot>, PersistenceError> {
        let store = self.path.to_string_lossy().into_owned();
        let db = self.db.lock().expect("sqlite");
        let mut stmt = db
            .prepare(
                "SELECT header, (SELECT COUNT(*) FROM events WHERE session_id = sessions.id)
                 FROM sessions ORDER BY id",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut snapshots = Vec::new();
        for row in rows {
            let (raw, count) = row.map_err(sqlite_error)?;
            let header: SessionHeader = serde_json::from_str(&raw)
                .map_err(|error| PersistenceError::Format(error.to_string()))?;
            snapshots.push(SessionPersistenceSnapshot {
                header,
                revision: sqlite_revision(&store, count, &raw),
            });
        }
        Ok(snapshots)
    }

    async fn load_stored(
        &self,
        id: &SessionId,
    ) -> std::result::Result<Option<StoredSession>, PersistenceError> {
        match load_stored_sqlite(self, id) {
            Ok(stored) => Ok(Some(stored)),
            Err(PersistenceError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn append_events(
        &self,
        header: &SessionHeader,
        events: &[SessionEvent],
        materialized: bool,
    ) -> std::result::Result<(), PersistenceError> {
        let id = header.id.as_str().to_string();
        let header_json = serde_json::to_string(header)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        let payloads = events
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map_err(|error| PersistenceError::Format(error.to_string()))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let created_at = header.created_at as i64;
        let db = self.db.lock().expect("sqlite");
        let tx = db.unchecked_transaction().map_err(sqlite_error)?;
        if !materialized {
            tx.execute(
                "INSERT INTO sessions(id, created_at, header) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET header = excluded.header",
                rusqlite::params![id, created_at, header_json],
            )
            .map_err(sqlite_error)?;
        } else {
            let exists: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            if exists == 0 {
                tx.execute(
                    "INSERT INTO sessions(id, created_at, header) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, created_at, header_json],
                )
                .map_err(sqlite_error)?;
            }
        }
        {
            let mut insert = tx
                .prepare("INSERT INTO events(session_id, seq, payload) VALUES (?1, ?2, ?3)")
                .map_err(sqlite_error)?;
            for (event, payload) in events.iter().zip(payloads.iter()) {
                insert
                    .execute(rusqlite::params![id, event.seq as i64, payload])
                    .map_err(sqlite_error)?;
            }
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(())
    }

    async fn commit_repair(
        &self,
        header: &SessionHeader,
        torn_to: Option<u64>,
        closers: &[SessionEvent],
    ) -> std::result::Result<(), PersistenceError> {
        let id = header.id.as_str().to_string();
        let closer_payloads = closers
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map_err(|error| PersistenceError::Format(error.to_string()))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let db = self.db.lock().expect("sqlite");
        let tx = db.unchecked_transaction().map_err(sqlite_error)?;
        if let Some(torn) = torn_to {
            tx.execute(
                "DELETE FROM events WHERE session_id = ?1 AND seq >= ?2",
                rusqlite::params![id, torn as i64],
            )
            .map_err(sqlite_error)?;
        }
        {
            let mut insert = tx
                .prepare("INSERT INTO events(session_id, seq, payload) VALUES (?1, ?2, ?3)")
                .map_err(sqlite_error)?;
            for (event, payload) in closers.iter().zip(closer_payloads.iter()) {
                insert
                    .execute(rusqlite::params![id, event.seq as i64, payload])
                    .map_err(sqlite_error)?;
            }
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(())
    }
}

/// Provide [`PersistenceRuntime`] over a SQLite file.
pub fn install(ctx: &Context, path: impl Into<PathBuf>) -> Result<Arc<PersistenceRuntime>> {
    install_with_options(
        ctx,
        path,
        dsh_session_persistence::DEFAULT_PREPARED_SESSION_CACHE_SIZE,
        dsh_session_persistence::DEFAULT_WRITE_BATCH_MAX_DELAY_MS,
    )
}

/// Provide [`PersistenceRuntime`] with explicit LRU and write-behind delay.
///
/// # Errors
/// Invalid cache/delay, open failure, or write-path registration.
pub fn install_with_options(
    ctx: &Context,
    path: impl Into<PathBuf>,
    prepared_session_cache_size: usize,
    write_batch_max_delay_ms: u64,
) -> Result<Arc<PersistenceRuntime>> {
    let backend = Arc::new(
        SqliteBackend::open(path)
            .map_err(|error| dsh_cordis::CordisError::plugin(error.to_string()))?,
    );
    let runtime = Arc::new(
        PersistenceRuntime::with_options(
            backend,
            prepared_session_cache_size,
            write_batch_max_delay_ms,
        )
        .map_err(|error| dsh_cordis::CordisError::plugin(error.to_string()))?,
    );
    ctx.provide(Arc::clone(&runtime))?;
    runtime.install_write_path(ctx)?;
    Ok(runtime)
}

fn load_stored_sqlite(
    backend: &SqliteBackend,
    id: &SessionId,
) -> std::result::Result<StoredSession, PersistenceError> {
    let key = id.as_str().to_string();
    let (header, rows) = {
        let db = backend.db.lock().expect("sqlite");
        let header: Option<String> = {
            let mut stmt = db
                .prepare("SELECT header FROM sessions WHERE id = ?1")
                .map_err(sqlite_error)?;
            stmt.query_row([&key], |row| row.get(0))
                .optional()
                .map_err(sqlite_error)?
        };
        let mut stmt = db
            .prepare("SELECT seq, payload FROM events WHERE session_id = ?1 ORDER BY seq")
            .map_err(sqlite_error)?;
        let mapped = stmt
            .query_map([&key], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        let rows = mapped
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        (header, rows)
    };
    let Some(header) = header else {
        return Err(PersistenceError::NotFound(id.as_str().to_string()));
    };
    let header: SessionHeader = serde_json::from_str(&header)
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
    if header.id.as_str() != id.as_str() {
        return Err(PersistenceError::Format(format!(
            "stored header id {} does not match requested session {}",
            header.id.as_str(),
            id.as_str()
        )));
    }
    let mut events = Vec::new();
    let mut torn_to = None;
    let mut expected = 0u64;
    for (seq, payload) in rows {
        let physical = seq as u64;
        if physical != expected {
            torn_to = Some(physical);
            break;
        }
        let value: Value = match serde_json::from_str(&payload) {
            Ok(value) => value,
            Err(_) => {
                torn_to = Some(physical);
                break;
            }
        };
        let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
        let ignorable = value
            .get("ignorable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if refuse_unknown(type_name, ignorable).is_err() {
            torn_to = Some(physical);
            break;
        }
        match session_event_from_value(value) {
            Ok(event) => events.push(event),
            Err(_) => {
                torn_to = Some(physical);
                break;
            }
        }
        expected += 1;
    }
    Ok(StoredSession {
        inspection: SessionInspection {
            meta: header,
            events,
        },
        torn_to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData, TurnEndReason};

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-sqlite-{name}-{nanos}.db"))
    }

    #[tokio::test]
    async fn round_trip_list_and_refuse_other_schema() {
        let path = tmp_path("round");
        let backend = SqliteBackend::open(&path).unwrap();
        let session = Session::new(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        backend.save(&session).await.unwrap();
        let loaded = backend.load(&session_id("s")).await.unwrap();
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(
            backend
                .list_ids()
                .await
                .unwrap()
                .iter()
                .map(|id| id.as_str().to_string())
                .collect::<Vec<_>>(),
            vec!["s".to_string()]
        );
        let headers = backend.list_headers().await.unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id.as_str(), "s");
        assert_eq!(headers[0].parent_session, None);

        let newer = tmp_path("newer");
        {
            let db = Connection::open(&newer).unwrap();
            db.pragma_update(
                None,
                "application_id",
                SESSION_PERSISTENCE_SQLITE_APPLICATION_ID,
            )
            .unwrap();
            db.pragma_update(None, "user_version", 99).unwrap();
        }
        let err = match SqliteBackend::open(&newer) {
            Ok(_) => panic!("newer schema must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(err, PersistenceError::Format(message) if message.contains("not supported"))
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&newer);
    }

    #[tokio::test]
    async fn load_missing_session_is_not_found() {
        let path = tmp_path("missing");
        let backend = SqliteBackend::open(&path).unwrap();
        let err = match backend.load(&session_id("nope")).await {
            Err(error) => error,
            Ok(_) => panic!("missing sqlite session must be NotFound"),
        };
        assert!(matches!(err, PersistenceError::NotFound(ref id) if id == "nope"));
        assert_eq!(err.to_string(), "session \"nope\" not found");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn crash_repair_closes_open_turn() {
        let path = tmp_path("repair");
        let backend = SqliteBackend::open(&path).unwrap();
        let session = Session::new(session_id("s"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend.save(&session).await.unwrap();
        let loaded = backend.load(&session_id("s")).await.unwrap();
        assert!(matches!(
            loaded.events().last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn inspect_repairs_an_open_turn_without_rewriting_rows() {
        let path = tmp_path("inspect");
        let backend = SqliteBackend::open(&path).unwrap();
        let session = Session::new(session_id("open"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend.save(&session).await.unwrap();
        let inspected = backend.inspect(&session_id("open")).await.unwrap();
        assert!(matches!(
            inspected.events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        let stored = {
            let db = backend.db.lock().expect("sqlite");
            let count: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                    ["open"],
                    |row| row.get(0),
                )
                .unwrap();
            count
        };
        assert_eq!(stored, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn snapshots_change_after_save_and_ignore_inspect_repair() {
        let path = tmp_path("snapshots");
        let backend = SqliteBackend::open(&path).unwrap();
        let id = session_id("snap");
        let session = Session::new(id.clone());
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend.save(&session).await.unwrap();
        let first = backend.read_stored_revision(&id).await.unwrap().unwrap();
        let listed = backend.list_snapshots().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].header.id.as_str(), "snap");
        assert_eq!(listed[0].revision, first);
        assert_eq!(backend.list_snapshots().await.unwrap()[0].revision, first);
        backend.inspect(&id).await.unwrap();
        assert_eq!(
            backend.read_stored_revision(&id).await.unwrap().as_ref(),
            Some(&first)
        );
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        backend.save(&session).await.unwrap();
        let changed = backend.read_stored_revision(&id).await.unwrap().unwrap();
        assert_ne!(changed, first);
        assert!(backend
            .read_stored_revision(&session_id("absent"))
            .await
            .unwrap()
            .is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn append_and_torn_load_commit_repair() {
        let path = tmp_path("append-torn");
        let backend = SqliteBackend::open(&path).unwrap();
        let header = SessionHeader::new(session_id("s"), None);
        let session = Session::with_header(header.clone());
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend
            .append_events(&header, &session.events(), false)
            .await
            .unwrap();
        {
            let db = backend.db.lock().expect("sqlite");
            db.execute(
                "INSERT INTO events(session_id, seq, payload) VALUES (?1, ?2, ?3)",
                rusqlite::params!["s", 1i64, "{not-json"],
            )
            .unwrap();
        }
        let stored = backend.load_stored(&header.id).await.unwrap().unwrap();
        assert_eq!(stored.inspection.events.len(), 1);
        assert_eq!(stored.torn_to, Some(1));
        let runtime = PersistenceRuntime::new(Arc::new(backend) as _);
        runtime.load(&header.id).await.unwrap();
        let reopened = SqliteBackend::open(&path).unwrap();
        let after = reopened.load_stored(&header.id).await.unwrap().unwrap();
        assert_eq!(after.inspection.events.len(), 2);
        assert!(after.torn_to.is_none());
        assert!(matches!(
            after.inspection.events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        let _ = std::fs::remove_file(&path);
    }
}
