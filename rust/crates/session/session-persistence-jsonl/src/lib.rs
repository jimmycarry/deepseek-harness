//! JSONL persistence provider. `append_events` writes new rows; torn last-line
//! `commit_repair` truncates to the last complete newline. Crash repair closes
//! an open turn with `interrupted`.

use async_trait::async_trait;
use dsh_atomic_write::{write_file_atomic, AtomicWriteError, WriteFileAtomicOptions};
use dsh_cordis::Context;
use dsh_session::{
    interrupted_turn_closers, now_ms, refuse_unknown, session_event_from_value, Session,
    SessionEvent, SessionEventData, SessionHeader, SessionId, SessionStore, TurnEndReason,
    SESSION_FORMAT_VERSION,
};
use dsh_session_persistence::{
    session_persistence_revision, PersistenceError, PersistenceRuntime, SessionInspection,
    SessionLocation, SessionPersistenceRevision, SessionPersistenceSnapshot, SessionStoreBackend,
    StoredSession,
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
    fn name(&self) -> &str {
        "jsonl"
    }

    async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
        write_jsonl(self.path_for(session.id()), session).await
    }

    async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        inspect_jsonl(self.path_for(id), id).await?.into_session()
    }

    async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
        inspect_jsonl(self.path_for(id), id).await
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

    async fn read_stored_revision(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionPersistenceRevision>, PersistenceError> {
        revision_for_path(&self.path_for(id)).await
    }

    async fn list_snapshots(&self) -> Result<Vec<SessionPersistenceSnapshot>, PersistenceError> {
        let mut snapshots = Vec::new();
        for (id, path) in self.list_jsonl_paths().await? {
            let header = match read_jsonl_header(&path, &id).await {
                Ok(header) => header,
                Err(PersistenceError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            let Some(revision) = revision_for_path(&path).await? else {
                continue;
            };
            snapshots.push(SessionPersistenceSnapshot { header, revision });
        }
        Ok(snapshots)
    }

    async fn load_stored(&self, id: &SessionId) -> Result<Option<StoredSession>, PersistenceError> {
        match load_stored_jsonl(self.path_for(id), id).await {
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
    ) -> Result<(), PersistenceError> {
        let path = self.path_for(&header.id);
        if materialized {
            append_jsonl_events(&path, events).await
        } else {
            materialize_jsonl(&path, header, events).await
        }
    }

    async fn commit_repair(
        &self,
        header: &SessionHeader,
        torn_to: Option<u64>,
        closers: &[SessionEvent],
    ) -> Result<(), PersistenceError> {
        let path = self.path_for(&header.id);
        if let Some(offset) = torn_to {
            truncate_jsonl(&path, offset).await?;
        }
        if !closers.is_empty() {
            append_jsonl_events(&path, closers).await?;
        }
        Ok(())
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
    install_with_options(
        ctx,
        dir,
        dsh_session_persistence::DEFAULT_PREPARED_SESSION_CACHE_SIZE,
        dsh_session_persistence::DEFAULT_WRITE_BATCH_MAX_DELAY_MS,
    )
}

/// Provide [`PersistenceRuntime`] with explicit LRU and write-behind delay.
///
/// # Errors
/// Invalid cache/delay, service provide, or write-path registration.
pub fn install_with_options(
    ctx: &Context,
    dir: impl AsRef<Path>,
    prepared_session_cache_size: usize,
    write_batch_max_delay_ms: u64,
) -> dsh_cordis::Result<Arc<PersistenceRuntime>> {
    let backend = Arc::new(JsonlBackend::new(dir.as_ref()));
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
    inspect_jsonl(path, id).await?.into_session()
}

/// Committed prefix plus an optional byte-offset torn tail. No closers.
pub async fn load_stored_jsonl(
    path: impl AsRef<Path>,
    id: &SessionId,
) -> Result<StoredSession, PersistenceError> {
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PersistenceError::NotFound(id.as_str().to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    parse_stored_jsonl(&bytes, id)
}

fn parse_stored_jsonl(bytes: &[u8], id: &SessionId) -> Result<StoredSession, PersistenceError> {
    let last_nl = bytes.iter().rposition(|byte| *byte == b'\n');
    let (complete, remainder, remainder_offset) = match last_nl {
        None => (&[][..], bytes, 0u64),
        Some(index) => (&bytes[..index], &bytes[index + 1..], (index + 1) as u64),
    };
    let mut torn_to = None;
    let mut lines: Vec<&[u8]> = complete.split(|byte| *byte == b'\n').collect();
    if !remainder.is_empty() {
        match std::str::from_utf8(remainder)
            .ok()
            .filter(|text| !text.trim().is_empty())
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            Some(_) => lines.push(remainder),
            None => torn_to = Some(remainder_offset),
        }
    }
    let mut records = lines
        .into_iter()
        .filter(|line| !line.is_empty() && !line.iter().all(|byte| byte.is_ascii_whitespace()));
    let header_text = records
        .next()
        .ok_or_else(|| PersistenceError::Format("missing session header line".into()))?;
    let header_str = std::str::from_utf8(header_text)
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
    let header = parse_header_line(header_str, id)?;
    let mut events: Vec<SessionEvent> = Vec::new();
    for line in records {
        let text = std::str::from_utf8(line)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        let value: Value = serde_json::from_str(text)
            .map_err(|error| PersistenceError::Format(error.to_string()))?;
        let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
        let ignorable = value
            .get("ignorable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        refuse_unknown(type_name, ignorable)?;
        events.push(session_event_from_value(value)?);
    }
    Ok(StoredSession {
        inspection: SessionInspection {
            meta: header,
            events,
        },
        torn_to,
    })
}

async fn materialize_jsonl(
    path: &Path,
    header: &SessionHeader,
    events: &[SessionEvent],
) -> Result<(), PersistenceError> {
    let mut body = serde_json::to_string(&header_line(header))
        .map_err(|error| PersistenceError::Format(error.to_string()))?;
    body.push('\n');
    for event in events {
        body.push_str(
            &serde_json::to_string(event)
                .map_err(|error| PersistenceError::Format(error.to_string()))?,
        );
        body.push('\n');
    }
    write_file_atomic(
        path,
        body,
        WriteFileAtomicOptions {
            mode: 0o600,
            dir_mode: Some(0o700),
        },
    )
    .await
    .map_err(atomic_error)
}

async fn append_jsonl_events(path: &Path, events: &[SessionEvent]) -> Result<(), PersistenceError> {
    if events.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut body = String::new();
    for event in events {
        body.push_str(
            &serde_json::to_string(event)
                .map_err(|error| PersistenceError::Format(error.to_string()))?,
        );
        body.push('\n');
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(body.as_bytes()).await?;
    file.sync_all().await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

async fn truncate_jsonl(path: &Path, offset: u64) -> Result<(), PersistenceError> {
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(offset).await?;
    file.sync_all().await?;
    Ok(())
}

/// Read-only logical view. An interrupted trailing turn receives an
/// in-memory closer; the file is not rewritten.
pub async fn inspect_jsonl(
    path: impl AsRef<Path>,
    id: &SessionId,
) -> Result<SessionInspection, PersistenceError> {
    let stored = load_stored_jsonl(path, id).await?;
    let mut events = stored.inspection.events;
    events.extend(interrupted_turn_closers(&events));
    Ok(SessionInspection {
        meta: stored.inspection.meta,
        events,
    })
}

async fn revision_for_path(
    path: &Path,
) -> Result<Option<SessionPersistenceRevision>, PersistenceError> {
    match fs::metadata(path).await {
        Ok(meta) => Ok(Some(file_revision(&meta))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// TypeScript JSONL `fileRevision`: `dev:ino:size:mtimeNs:ctimeNs`.
fn file_revision(meta: &std::fs::Metadata) -> SessionPersistenceRevision {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mtime_ns = (meta.mtime() as u128)
            .saturating_mul(1_000_000_000)
            .saturating_add(u128::from(meta.mtime_nsec() as u32));
        let ctime_ns = (meta.ctime() as u128)
            .saturating_mul(1_000_000_000)
            .saturating_add(u128::from(meta.ctime_nsec() as u32));
        session_persistence_revision(format!(
            "{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.size(),
            mtime_ns,
            ctime_ns
        ))
    }
    #[cfg(not(unix))]
    {
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        session_persistence_revision(format!("{}:{}", meta.len(), modified))
    }
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
    async fn inspect_repairs_an_open_turn_without_rewriting_the_file() {
        let dir = tmp_dir("inspect");
        fs::create_dir_all(&dir).await.unwrap();
        let backend = JsonlBackend::new(&dir);
        let session = Session::new(session_id("open"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend.save(&session).await.unwrap();
        let path = backend.path_for(&session_id("open"));
        let before = fs::read_to_string(&path).await.unwrap();
        let inspected = backend.inspect(&session_id("open")).await.unwrap();
        assert!(matches!(
            inspected.events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        let after = fs::read_to_string(&path).await.unwrap();
        assert_eq!(before, after);
        assert!(!after.contains("interrupted"));
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn snapshots_use_stat_identity_and_ignore_inspect_repair() {
        let dir = tmp_dir("snapshots");
        fs::create_dir_all(&dir).await.unwrap();
        let backend = JsonlBackend::new(&dir);
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
        let repeated = backend.list_snapshots().await.unwrap();
        assert_eq!(repeated[0].revision, first);
        let reopened = JsonlBackend::new(&dir);
        assert_eq!(
            reopened.read_stored_revision(&id).await.unwrap().as_ref(),
            Some(&first)
        );
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
        let other = tmp_dir("snapshots-other");
        fs::create_dir_all(&other).await.unwrap();
        let other_backend = JsonlBackend::new(&other);
        other_backend.save(&session).await.unwrap();
        assert_ne!(
            other_backend.read_stored_revision(&id).await.unwrap(),
            Some(changed)
        );
        let _ = fs::remove_dir_all(&dir).await;
        let _ = fs::remove_dir_all(&other).await;
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

    #[tokio::test]
    async fn append_and_torn_load_commit_repair() {
        let dir = tmp_dir("append-torn");
        fs::create_dir_all(&dir).await.unwrap();
        let backend = JsonlBackend::new(&dir);
        let header = SessionHeader::new(session_id("s"), None);
        let session = Session::with_header(header.clone());
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        backend
            .append_events(&header, &session.events(), false)
            .await
            .unwrap();
        let path = backend.path_for(&header.id);
        let mut body = fs::read_to_string(&path).await.unwrap();
        body.push_str("{\"seq\":1,\"type\":\"turn/end\"");
        fs::write(&path, body).await.unwrap();
        let stored = backend.load_stored(&header.id).await.unwrap().unwrap();
        assert_eq!(stored.inspection.events.len(), 1);
        assert!(stored.torn_to.is_some());
        let runtime = PersistenceRuntime::new(Arc::new(backend) as _);
        runtime.load(&header.id).await.unwrap();
        let repaired = fs::read_to_string(&path).await.unwrap();
        assert!(repaired.contains("interrupted"));
        assert!(!repaired.contains("{\"seq\":1,\"type\":\"turn/end\""));
        let _ = fs::remove_dir_all(&dir).await;
    }
}
