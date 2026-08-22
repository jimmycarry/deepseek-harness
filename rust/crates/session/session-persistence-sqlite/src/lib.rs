//! SQLite session store. Newer `user_version` values are refused.
//!
//! Physical records are one JSON event row per seq. This crate's
//! `SCHEMA_VERSION` is monotonic and starts at `1`; it does not read the
//! TypeScript packed schema.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_session::{
    now_ms, refuse_unknown, session_event_from_value, session_id, Session, SessionEvent,
    SessionEventData, SessionHeader, SessionId, TurnEndReason,
};
use dsh_session_persistence::{PersistenceError, PersistenceRuntime, SessionStoreBackend};
use rusqlite::Connection;
use serde_json::Value;
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
    db.pragma_update(None, "application_id", SESSION_PERSISTENCE_SQLITE_APPLICATION_ID)
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

fn repair_open_turn(events: &mut Vec<SessionEvent>) {
    let mut open: Option<u32> = None;
    for event in events.iter() {
        match &event.data {
            SessionEventData::TurnStart { turn } => open = Some(*turn),
            SessionEventData::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    if let Some(turn) = open {
        let seq = events.len() as u64;
        events.push(SessionEvent {
            seq,
            time: now_ms(),
            data: SessionEventData::TurnEnd {
                turn,
                reason: TurnEndReason::Interrupted,
            },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        });
    }
}

#[async_trait]
impl SessionStoreBackend for SqliteBackend {
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
            return Err(PersistenceError::Format(format!(
                "session {} is not in the store",
                id.as_str()
            )));
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
            let value: Value =
                serde_json::from_str(&payload).map_err(|error| PersistenceError::Format(error.to_string()))?;
            let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
            let ignorable = value
                .get("ignorable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            refuse_unknown(type_name, ignorable)?;
            events.push(session_event_from_value(value)?);
        }
        repair_open_turn(&mut events);
        let session = Session::with_header(header);
        for event in events {
            session.append_logged(event)?;
        }
        Ok(session)
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
}

/// Provide [`PersistenceRuntime`] over a SQLite file.
pub fn install(ctx: &Context, path: impl Into<PathBuf>) -> Result<Arc<PersistenceRuntime>> {
    let backend = Arc::new(
        SqliteBackend::open(path).map_err(|error| dsh_cordis::CordisError::plugin(error.to_string()))?,
    );
    let runtime = Arc::new(PersistenceRuntime::new(backend));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::session_id;

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

        let newer = tmp_path("newer");
        {
            let db = Connection::open(&newer).unwrap();
            db.pragma_update(None, "application_id", SESSION_PERSISTENCE_SQLITE_APPLICATION_ID)
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
}
