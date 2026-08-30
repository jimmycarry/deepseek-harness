//! JSONL persistence provider. Crash repair closes an open turn with `interrupted`.

use async_trait::async_trait;
use dsh_atomic_write::{write_file_atomic, AtomicWriteError, WriteFileAtomicOptions};
use dsh_cordis::Context;
use dsh_session::{
    now_ms, refuse_unknown, session_event_from_value, Session, SessionEvent, SessionEventData,
    SessionHeader, SessionId, SessionStore, TurnEndReason, SESSION_FORMAT_VERSION,
};
use dsh_session_persistence::{
    PersistenceError, PersistenceRuntime, SessionLocation, SessionStoreBackend,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

/// Directory-backed JSONL backend. Each session is `{dir}/{id}.jsonl`.
pub struct JsonlBackend {
    dir: PathBuf,
}

impl JsonlBackend {
    /// Persist under `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Artifact path for one session id.
    pub fn path_for(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", id.as_str()))
    }

    async fn list_jsonl_paths(&self) -> Result<Vec<(SessionId, PathBuf)>, PersistenceError> {
        let mut paths = Vec::new();
        let mut entries = match fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                paths.push((dsh_session::session_id(stem), path));
            }
        }
        paths.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        Ok(paths)
    }
}

#[async_trait]
impl SessionStoreBackend for JsonlBackend {
    async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
        write_jsonl(self.path_for(session.id()), session).await
    }

    async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        read_jsonl(self.path_for(id), id).await
    }

    async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
        Ok(self
            .list_jsonl_paths()
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    async fn list_headers(&self) -> Result<Vec<SessionHeader>, PersistenceError> {
        let mut headers = Vec::new();
        for (id, path) in self.list_jsonl_paths().await? {
            headers.push(read_jsonl_header(&path, &id).await?);
        }
        Ok(headers)
    }

    fn locate(&self, id: &SessionId) -> Option<SessionLocation> {
        Some(SessionLocation::Jsonl {
            path: self.path_for(id),
        })
    }
}

/// Provide [`PersistenceRuntime`] over a JSONL directory.
///
/// Dispose drains every live session with a synchronous rewrite so a
/// post-checkpoint append (including `/feedback`) reaches disk. The
/// disposer cannot await, so this path uses `std::fs` rather than Tokio.
pub fn install(
    ctx: &Context,
    dir: impl AsRef<Path>,
) -> dsh_cordis::Result<Arc<PersistenceRuntime>> {
    let backend = Arc::new(JsonlBackend::new(dir.as_ref()));
    let runtime = Arc::new(PersistenceRuntime::new(backend));
    ctx.provide(Arc::clone(&runtime))?;
    let drain_dir = dir.as_ref().to_path_buf();
    let lookup = ctx.clone();
    ctx.effect("jsonl persistence drain", move || {
        move || {
            let Some(store) = lookup.get::<SessionStore>() else {
                return;
            };
            let backend = JsonlBackend::new(&drain_dir);
            for session in store.live() {
                if let Err(error) =
                    write_jsonl_sync(&backend.path_for(session.id()), session.as_ref())
                {
                    eprintln!("session-persistence-jsonl: dispose drain failed: {error}");
                }
            }
        }
    })?;
    Ok(runtime)
}

fn encode_jsonl(session: &Session) -> Result<String, PersistenceError> {
    let mut body = serde_json::to_string(&header_line(session.header()))
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
    body.push('\n');
    for event in session.events() {
        body.push_str(
            &serde_json::to_string(&event)
                .map_err(|error| PersistenceError::Format(error.to_string()))?,
        );
        body.push('\n');
    }
    Ok(body)
}

/// Write the session header line plus one JSON object per event line.
pub async fn write_jsonl(
    path: impl AsRef<Path>,
    session: &Session,
) -> Result<(), PersistenceError> {
    write_file_atomic(
        path,
        encode_jsonl(session)?,
        WriteFileAtomicOptions {
            mode: 0o600,
            dir_mode: Some(0o700),
        },
    )
    .await
    .map_err(atomic_error)
}

fn write_jsonl_sync(path: &Path, session: &Session) -> Result<(), PersistenceError> {
    let body = encode_jsonl(session)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700)?;
    }
    let temp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("jsonl")
    ));
    let written = (|| {
        std::fs::write(&temp, body.as_bytes())?;
        set_mode(&temp, 0o600)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Project one session header as its persisted first line (`type` first).
fn header_line(header: &SessionHeader) -> Value {
    let mut line = serde_json::Map::new();
    line.insert("type".into(), Value::String("session".into()));
    if let Ok(Value::Object(fields)) = serde_json::to_value(header) {
        line.extend(fields);
    }
    Value::Object(line)
}

/// Parse and validate one persisted header line against the requested id.
fn parse_header_line(line: &str, id: &SessionId) -> Result<SessionHeader, PersistenceError> {
    let mut value: Value =
        serde_json::from_str(line).map_err(|error| PersistenceError::Format(error.to_string()))?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Err(PersistenceError::Format(
            "header line is not a session record".into(),
        ));
    }
    let version = value.get("version").and_then(Value::as_u64);
    if version != Some(u64::from(SESSION_FORMAT_VERSION)) {
        return Err(PersistenceError::Format(format!(
            "session format version {version:?} is not {SESSION_FORMAT_VERSION}"
        )));
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    let header: SessionHeader = serde_json::from_value(value)
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
    if header.id.as_str() != id.as_str() {
        return Err(PersistenceError::Format(format!(
            "header id {} does not match requested session {}",
            header.id.as_str(),
            id.as_str()
        )));
    }
    Ok(header)
}

/// Read only the persisted header line.
pub async fn read_jsonl_header(
    path: impl AsRef<Path>,
    id: &SessionId,
) -> Result<SessionHeader, PersistenceError> {
    let body = match fs::read_to_string(&path).await {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PersistenceError::NotFound(id.as_str().to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    let header_text = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| PersistenceError::Format("missing session header line".into()))?;
    parse_header_line(header_text, id)
}

/// Load a log, refusing unknown required-on-read types and repairing a trailing open turn.
pub async fn read_jsonl(
    path: impl AsRef<Path>,
    id: &SessionId,
) -> Result<Session, PersistenceError> {
    let body = match fs::read_to_string(&path).await {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PersistenceError::NotFound(id.as_str().to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    let header_text = lines
        .next()
        .ok_or_else(|| PersistenceError::Format("missing session header line".into()))?;
    let header = parse_header_line(header_text, id)?;
    let session = Session::with_header(header);
    let mut events: Vec<SessionEvent> = Vec::new();
    for line in lines {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
        let ignorable = value
            .get("ignorable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        refuse_unknown(type_name, ignorable)?;
        events.push(session_event_from_value(value)?);
    }
    repair_open_turn(&mut events);
    for event in events {
        session.append_logged(event)?;
    }
    Ok(session)
}

/// Close a dangling `turn/start` with `interrupted`.
pub fn repair_open_turn(events: &mut Vec<SessionEvent>) {
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
        std::env::temp_dir().join(format!("dsh-jsonl-{name}-{nanos}"))
    }

    #[test]
    fn crash_repair_closes_open_turn() {
        let mut events = vec![SessionEvent {
            seq: 0,
            time: 0,
            data: SessionEventData::TurnStart { turn: 1 },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        }];
        repair_open_turn(&mut events);
        assert!(matches!(
            events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn seam_key_is_stable() {
        assert_eq!(
            <PersistenceRuntime as dsh_cordis::Service>::KEY,
            "sessionPersistence"
        );
    }

    #[tokio::test]
    async fn write_header_and_refuse_unknown_required() {
        let dir = tmp_dir("roundtrip");
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("s.jsonl");
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
        write_jsonl(&path, &session).await.unwrap();
        let body = fs::read_to_string(&path).await.unwrap();
        let first_line = body.lines().next().unwrap();
        assert!(first_line.starts_with("{\"type\":\"session\",\"version\":0,\"id\":\"s\","));
        assert!(first_line.contains("\"createdAt\":"));
        assert!(first_line.ends_with("\"delegationDepth\":0}"));
        let loaded = read_jsonl(&path, &session_id("s")).await.unwrap();
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(loaded.header().version, SESSION_FORMAT_VERSION);
        let header_only = read_jsonl_header(&path, &session_id("s")).await.unwrap();
        assert_eq!(header_only.id.as_str(), "s");
        assert_eq!(header_only.parent_session, None);

        let header = first_line.to_string();
        fs::write(
            &path,
            format!("{header}\n{{\"seq\":0,\"type\":\"future/event\"}}\n"),
        )
        .await
        .unwrap();
        let err = match read_jsonl(&path, &session_id("s")).await {
            Ok(_) => panic!("unknown required event must be refused"),
            Err(error) => error,
        };
        assert!(matches!(err, PersistenceError::Session(_)));

        fs::write(&path, "{\"sessionFormatVersion\":0}\n")
            .await
            .unwrap();
        let err = match read_jsonl(&path, &session_id("s")).await {
            Ok(_) => panic!("legacy header must be refused"),
            Err(error) => error,
        };
        assert!(matches!(err, PersistenceError::Format(_)));
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn list_headers_reads_parent_session_without_events() {
        let dir = tmp_dir("headers");
        fs::create_dir_all(&dir).await.unwrap();
        let backend = JsonlBackend::new(&dir);
        let header = SessionHeader::for_subagent_child(None, session_id("parent"));
        let child_id = header.id.clone();
        let session = Session::with_header(header);
        backend.save(&session).await.unwrap();
        let headers = backend.list_headers().await.unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id, child_id);
        assert_eq!(
            headers[0].parent_session.as_ref().map(|id| id.as_str()),
            Some("parent")
        );
        assert_eq!(headers[0].origin.as_deref(), Some("subagent"));
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn install_provides_runtime() {
        let dir = tmp_dir("install");
        let ctx = Context::new();
        install(&ctx, &dir).unwrap();
        assert!(ctx.has_service("sessionPersistence"));
        ctx.dispose();
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn dispose_drains_unflushed_events() {
        let dir = tmp_dir("drain");
        fs::create_dir_all(&dir).await.unwrap();
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(&ctx, &dir).unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("drain"));
        session
            .append(
                SessionEventData::Extension {
                    type_name: "feedback/record".into(),
                    data: serde_json::json!({ "text": "fixture feedback" }),
                },
                None,
            )
            .unwrap();
        ctx.dispose();
        let body = std::fs::read_to_string(dir.join("drain.jsonl")).unwrap();
        assert!(body.contains("fixture feedback"));
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn load_missing_file_is_not_found() {
        let dir = tmp_dir("missing");
        fs::create_dir_all(&dir).await.unwrap();
        let err = match read_jsonl(dir.join("nope.jsonl"), &session_id("nope")).await {
            Err(error) => error,
            Ok(_) => panic!("missing jsonl must be NotFound"),
        };
        assert!(matches!(err, PersistenceError::NotFound(id) if id == "nope"));
        let _ = fs::remove_dir_all(&dir).await;
    }
}
