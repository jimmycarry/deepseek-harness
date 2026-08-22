//! File-backed session store using `{schema_version, events}`. Newer schemas are refused.

use async_trait::async_trait;
use dsh_atomic_write::{write_file_atomic, AtomicWriteError, WriteFileAtomicOptions};
use dsh_session::{
    refuse_unknown, session_event_from_value, Session, SessionEvent, SessionEventData, SessionId,
    TurnEndReason,
};
use dsh_session_persistence::{PersistenceError, SessionStoreBackend};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::fs;

/// Physical record version stamped on every file this build writes.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SqliteFile {
    schema_version: u32,
    events: Vec<Value>,
}

/// Directory-backed store. Each session is `{dir}/{id}.json`.
pub struct SqliteBackend {
    dir: PathBuf,
}

impl SqliteBackend {
    /// Persist under `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Artifact path for one session id.
    pub fn path_for(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{}.json", id.as_str()))
    }
}

#[async_trait]
impl SessionStoreBackend for SqliteBackend {
    async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
        let events = session
            .events()
            .into_iter()
            .map(|event| serde_json::to_value(event).map_err(|error| PersistenceError::Format(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let body = serde_json::to_string_pretty(&SqliteFile {
            schema_version: SCHEMA_VERSION,
            events,
        })
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
        write_file_atomic(
            self.path_for(session.id()),
            body,
            WriteFileAtomicOptions {
                mode: 0o600,
                dir_mode: Some(0o700),
            },
        )
        .await
        .map_err(atomic_error)
    }

    async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        let body = fs::read_to_string(self.path_for(id)).await?;
        let file: SqliteFile = serde_json::from_str(&body)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        if file.schema_version > SCHEMA_VERSION {
            return Err(PersistenceError::Format(format!(
                "schema version {} is newer than supported {SCHEMA_VERSION}",
                file.schema_version
            )));
        }
        let mut events: Vec<SessionEvent> = Vec::new();
        for value in file.events {
            let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
            let ignorable = value
                .get("ignorable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            refuse_unknown(type_name, ignorable)?;
            events.push(session_event_from_value(value)?);
        }
        repair_open_turn(&mut events);
        Ok(Session::replay(id.clone(), events)?)
    }
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
            data: SessionEventData::TurnEnd {
                turn,
                reason: TurnEndReason::Interrupted,
            },
            surface_op: None,
            ignorable: false,
        });
    }
}

fn atomic_error(error: AtomicWriteError) -> PersistenceError {
    match error {
        AtomicWriteError::Io(io) => PersistenceError::Io(io),
        AtomicWriteError::LockTimeout(path) => {
            PersistenceError::Format(format!("atomic-write lock timeout at {path}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData};

    fn tmp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-sqlite-{name}-{nanos}"))
    }

    #[tokio::test]
    async fn round_trip_and_refuse_newer_schema() {
        let dir = tmp_dir("round");
        let backend = SqliteBackend::new(&dir);
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

        let path = backend.path_for(&session_id("newer"));
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            &path,
            r#"{"schema_version":99,"events":[]}"#,
        )
        .await
        .unwrap();
        let err = match backend.load(&session_id("newer")).await {
            Ok(_) => panic!("newer schema must be refused"),
            Err(error) => error,
        };
        assert!(matches!(err, PersistenceError::Format(message) if message.contains("newer")));
        let _ = fs::remove_dir_all(&dir).await;
    }
}
