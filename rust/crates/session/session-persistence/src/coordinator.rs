//! Live write path and public coordinator methods.
//!
//! Ports the create / append / prepare / readFrom / flush / session-event
//! orchestration from `packages/session/session-persistence/src/coordinator.ts`.

use std::sync::Arc;

use dsh_cordis::Context;
use dsh_session::{
    session_event_from_value, session_id, Session, SessionEvent, SessionHeader, SessionId,
    SessionStore,
};
use serde_json::Value;
use tokio::sync::watch;

use crate::write_behind::{ReportBackgroundFailureFn, SessionWriteBehind, WriteBatchFn};
use crate::{PersistenceError, PersistenceRuntime, SessionInspection, SessionPersistState};

pub(crate) struct LiveSession {
    init: watch::Receiver<Option<Result<(), PersistenceError>>>,
    pub(crate) writes: Arc<SessionWriteBehind>,
}

impl LiveSession {
    async fn wait_init(&self) -> Result<(), PersistenceError> {
        let mut rx = self.init.clone();
        loop {
            if let Some(result) = rx.borrow().clone() {
                return result;
            }
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
    }
}

impl PersistenceRuntime {
    fn upgrade(&self) -> Option<Arc<Self>> {
        self.self_weak
            .lock()
            .expect("persistence self")
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    pub(crate) async fn serialize<T, F, Fut>(
        &self,
        id: &SessionId,
        work: F,
    ) -> Result<T, PersistenceError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, PersistenceError>>,
    {
        let lock = {
            let mut chains = self.chains.lock().expect("persistence chains");
            chains
                .entry(id.as_str().to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        work().await
    }

    /// Mount `session/created` / `session/event` / `session/flush` / `session/disposed`.
    ///
    /// # Errors
    /// Listener registration failure.
    pub fn install_write_path(self: &Arc<Self>, ctx: &Context) -> dsh_cordis::Result<()> {
        *self.self_weak.lock().expect("persistence self") = Some(Arc::downgrade(self));
        *self.ctx.lock().expect("persistence ctx") = Some(ctx.clone());

        let created = Arc::clone(self);
        ctx.on("session/created", move |payload| {
            let created = Arc::clone(&created);
            tokio::spawn(async move {
                if let Err(error) = created.on_session_created_payload(&payload).await {
                    tracing::warn!(
                        "{}: session create path failed: {error}",
                        created.backend.name()
                    );
                }
            });
        })?;

        let events = Arc::clone(self);
        ctx.on("session/event", move |payload| {
            events.on_session_event_payload(&payload);
        })?;

        let flush = Arc::clone(self);
        ctx.on("session/flush", move |payload| {
            let flush = Arc::clone(&flush);
            tokio::spawn(async move {
                if let Err(error) = flush.on_session_flush_payload(&payload).await {
                    tracing::warn!("{}: session flush failed: {error}", flush.backend.name());
                }
            });
        })?;

        let disposed = Arc::clone(self);
        ctx.on("session/disposed", move |payload| {
            let disposed = Arc::clone(&disposed);
            tokio::spawn(async move {
                if let Err(error) = disposed.on_session_disposed_payload(&payload).await {
                    tracing::warn!(
                        "{}: session \"{}\" retirement failed: {error}",
                        disposed.backend.name(),
                        payload
                            .get("id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("unknown")
                    );
                }
            });
        })?;

        Ok(())
    }

    fn lookup_session(&self, payload: &Value) -> Option<Arc<Session>> {
        let id = payload
            .get("id")
            .or_else(|| payload.get("sessionId"))
            .and_then(|value| value.as_str())?;
        let ctx = self.ctx.lock().expect("persistence ctx").clone()?;
        ctx.get::<SessionStore>()
            .and_then(|store| store.get(&session_id(id)))
    }

    fn is_live(&self, id: &SessionId) -> bool {
        let Some(ctx) = self.ctx.lock().expect("persistence ctx").clone() else {
            return false;
        };
        ctx.get::<SessionStore>()
            .and_then(|store| store.get(id))
            .is_some()
    }

    async fn on_session_created_payload(&self, payload: &Value) -> Result<(), PersistenceError> {
        let Some(session) = self.lookup_session(payload) else {
            return Ok(());
        };
        if let Some(this) = self.upgrade() {
            this.init_for(&session).wait_init().await
        } else {
            Ok(())
        }
    }

    fn on_session_event_payload(&self, payload: &Value) {
        let Some(session) = self.lookup_session(payload) else {
            return;
        };
        let Some(event_value) = payload.get("event").cloned() else {
            return;
        };
        let Ok(event) = session_event_from_value(event_value) else {
            return;
        };
        let Some(this) = self.upgrade() else {
            return;
        };
        this.init_for(&session).writes.enqueue(event);
    }

    async fn on_session_flush_payload(&self, payload: &Value) -> Result<(), PersistenceError> {
        let Some(session) = self.lookup_session(payload) else {
            return Ok(());
        };
        self.flush(session.as_ref()).await
    }

    async fn on_session_disposed_payload(&self, payload: &Value) -> Result<(), PersistenceError> {
        let Some(session) = self.lookup_session(payload) else {
            let Some(id) = payload.get("id").and_then(|value| value.as_str()) else {
                return Ok(());
            };
            self.live.lock().expect("live").remove(id);
            self.states.lock().expect("states").remove(id);
            return Ok(());
        };
        self.flush(session.as_ref()).await?;
        self.live
            .lock()
            .expect("live")
            .remove(session.id().as_str());
        self.states
            .lock()
            .expect("states")
            .remove(session.id().as_str());
        Ok(())
    }

    fn init_for(self: &Arc<Self>, session: &Session) -> Arc<LiveSession> {
        {
            let live = self.live.lock().expect("live");
            if let Some(existing) = live.get(session.id().as_str()) {
                return Arc::clone(existing);
            }
        }
        let (init_tx, init_rx) = watch::channel(None);
        let writes = self.create_write_behind(session);
        let created = Arc::new(LiveSession {
            init: init_rx,
            writes,
        });
        self.live
            .lock()
            .expect("live")
            .insert(session.id().as_str().to_string(), Arc::clone(&created));
        let this = Arc::clone(self);
        let id = session.id().clone();
        let header = session.header().clone();
        let seed = session.events();
        tokio::spawn(async move {
            let result = this
                .serialize(&id, || this.on_created(&id, header, seed))
                .await;
            let _ = init_tx.send(Some(result));
        });
        created
    }

    fn create_write_behind(self: &Arc<Self>, session: &Session) -> Arc<SessionWriteBehind> {
        let this = Arc::clone(self);
        let id = session.id().clone();
        let write: WriteBatchFn = Arc::new(move |batch: Vec<SessionEvent>| {
            let this = Arc::clone(&this);
            let id = id.clone();
            Box::pin(async move {
                this.serialize(&id, || this.append_live_batch(&id, batch))
                    .await
            })
        });
        let backend_name = self.backend.name().to_string();
        let session_id_text = session.id().as_str().to_string();
        let report: ReportBackgroundFailureFn = Arc::new(move |error: PersistenceError| {
            tracing::warn!(
                "{backend_name}: background write for session \"{session_id_text}\" failed (buffered events retained): {error}"
            );
        });
        SessionWriteBehind::new(self.write_batch_max_delay_ms, write, report)
    }

    async fn on_created(
        &self,
        id: &SessionId,
        header: SessionHeader,
        seed: Vec<SessionEvent>,
    ) -> Result<(), PersistenceError> {
        if self
            .states
            .lock()
            .expect("states")
            .contains_key(id.as_str())
        {
            let cursor = self
                .states
                .lock()
                .expect("states")
                .get(id.as_str())
                .map(|state| state.cursor)
                .unwrap_or(0);
            if seed.len() as u64 > cursor {
                self.append_core(id, &seed[cursor as usize..]).await?;
            }
            return Ok(());
        }
        match self.backend.load_stored(id).await? {
            Some(stored) => {
                if stored.torn_to.is_some() {
                    self.backend
                        .commit_repair(&stored.inspection.meta, stored.torn_to, &[])
                        .await?;
                }
                let cursor = stored.inspection.events.len() as u64;
                self.states.lock().expect("states").insert(
                    id.as_str().to_string(),
                    SessionPersistState {
                        meta: stored.inspection.meta,
                        cursor,
                        materialized: true,
                    },
                );
                if seed.len() as u64 > cursor {
                    self.append_core(id, &seed[cursor as usize..]).await?;
                }
            }
            None => {
                self.create_core(header).await?;
                if !seed.is_empty() {
                    self.append_core(id, &seed).await?;
                }
            }
        }
        Ok(())
    }

    async fn append_live_batch(
        &self,
        id: &SessionId,
        batch: Vec<SessionEvent>,
    ) -> Result<(), PersistenceError> {
        let cursor = self
            .states
            .lock()
            .expect("states")
            .get(id.as_str())
            .map(|state| state.cursor)
            .unwrap_or(0);
        let fresh: Vec<SessionEvent> = batch
            .into_iter()
            .filter(|event| event.seq >= cursor)
            .collect();
        self.append_core(id, &fresh).await
    }

    async fn create_core(&self, meta: SessionHeader) -> Result<(), PersistenceError> {
        if self
            .states
            .lock()
            .expect("states")
            .contains_key(meta.id.as_str())
            || self.preparations.has(&meta.id)
        {
            return Err(PersistenceError::Format(format!(
                "session \"{}\" already exists in this backend",
                meta.id.as_str()
            )));
        }
        if self.backend.load_stored(&meta.id).await?.is_some() {
            return Err(PersistenceError::Format(format!(
                "session \"{}\" already has a persisted log on disk; load/resume it instead of creating",
                meta.id.as_str()
            )));
        }
        self.states.lock().expect("states").insert(
            meta.id.as_str().to_string(),
            SessionPersistState {
                meta,
                cursor: 0,
                materialized: false,
            },
        );
        Ok(())
    }

    async fn append_core(
        &self,
        id: &SessionId,
        events: &[SessionEvent],
    ) -> Result<(), PersistenceError> {
        if events.is_empty() {
            return Ok(());
        }
        self.preparations.assert_writable(id)?;
        if !self
            .states
            .lock()
            .expect("states")
            .contains_key(id.as_str())
        {
            self.adopt(id).await?;
        }
        let (meta, cursor, materialized) = {
            let states = self.states.lock().expect("states");
            let state = states
                .get(id.as_str())
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))?;
            (state.meta.clone(), state.cursor, state.materialized)
        };
        for (index, event) in events.iter().enumerate() {
            let expected = cursor + index as u64;
            if event.seq != expected {
                return Err(PersistenceError::Format(format!(
                    "append seq mismatch for \"{}\": expected {expected} at index {index}, got {}",
                    id.as_str(),
                    event.seq
                )));
            }
        }
        self.backend
            .append_events(&meta, events, materialized)
            .await?;
        if let Some(state) = self.states.lock().expect("states").get_mut(id.as_str()) {
            state.materialized = true;
            state.cursor += events.len() as u64;
        }
        self.preparations.invalidate(id);
        Ok(())
    }

    async fn adopt(&self, id: &SessionId) -> Result<(), PersistenceError> {
        let Some(stored) = self.backend.load_stored(id).await? else {
            return Err(PersistenceError::NotFound(id.as_str().to_string()));
        };
        self.states.lock().expect("states").insert(
            id.as_str().to_string(),
            SessionPersistState {
                meta: stored.inspection.meta,
                cursor: stored.inspection.events.len() as u64,
                materialized: true,
            },
        );
        Ok(())
    }

    /// Register detached session metadata for lazy creation on the first append.
    ///
    /// # Errors
    /// Duplicate tracked or persisted ids.
    pub async fn create(&self, meta: SessionHeader) -> Result<(), PersistenceError> {
        self.serialize(&meta.id.clone(), || self.create_core(meta))
            .await
    }

    /// Durably persist a contiguous event batch.
    ///
    /// # Errors
    /// Seq mismatch, reserved preparation, or backend write failure.
    pub async fn append(
        &self,
        id: &SessionId,
        events: &[SessionEvent],
    ) -> Result<(), PersistenceError> {
        self.serialize(id, || self.append_core(id, events)).await
    }

    /// Prepare an unpublished Session. Refuses while the id is live.
    ///
    /// # Errors
    /// The session is live, missing, or failed validation.
    pub async fn prepare(&self, id: &SessionId) -> Result<Session, PersistenceError> {
        if self.is_live(id) {
            return Err(PersistenceError::Format(format!(
                "cannot prepare session \"{}\" while it is live",
                id.as_str()
            )));
        }
        let inspection = self.inspect(id).await?;
        self.preparations.hold(id);
        inspection.into_session()
    }

    /// Read stored events from `from_seq` onward without applying closers.
    ///
    /// # Errors
    /// Missing session or backend read failure.
    pub async fn read_from(
        &self,
        id: &SessionId,
        from_seq: u64,
    ) -> Result<SessionInspection, PersistenceError> {
        let stored = self.backend.load_stored_from(id, from_seq).await?;
        Ok(stored.inspection)
    }

    /// Drain write-behind for a live session, or rewrite the snapshot when
    /// the write path is not mounted.
    ///
    /// # Errors
    /// Backend write failure.
    pub async fn flush(&self, session: &Session) -> Result<(), PersistenceError> {
        if let Some(this) = self.upgrade() {
            let live = this.init_for(session);
            live.wait_init().await?;
            return live.writes.flush().await;
        }
        self.save(session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionStoreBackend, StoredSession};
    use async_trait::async_trait;
    use dsh_session::{session_id, SessionEventData, TurnEndReason};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemoryBackend {
        store: Mutex<HashMap<String, StoredSession>>,
        appends: Mutex<Vec<usize>>,
    }

    impl MemoryBackend {
        fn new() -> Self {
            Self {
                store: Mutex::new(HashMap::new()),
                appends: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionStoreBackend for MemoryBackend {
        fn name(&self) -> &str {
            "memory"
        }

        async fn save(&self, session: &Session) -> Result<(), PersistenceError> {
            self.store.lock().expect("mem").insert(
                session.id().as_str().to_string(),
                StoredSession {
                    inspection: SessionInspection {
                        meta: session.header().clone(),
                        events: session.events(),
                    },
                    torn_to: None,
                },
            );
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> Result<Session, PersistenceError> {
            self.inspect(id).await?.into_session()
        }

        async fn inspect(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
            self.store
                .lock()
                .expect("mem")
                .get(id.as_str())
                .map(|stored| stored.inspection.clone())
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))
        }

        async fn list_ids(&self) -> Result<Vec<SessionId>, PersistenceError> {
            Ok(self
                .store
                .lock()
                .expect("mem")
                .keys()
                .map(session_id)
                .collect())
        }

        async fn load_stored(
            &self,
            id: &SessionId,
        ) -> Result<Option<StoredSession>, PersistenceError> {
            Ok(self.store.lock().expect("mem").get(id.as_str()).cloned())
        }

        async fn append_events(
            &self,
            header: &SessionHeader,
            events: &[SessionEvent],
            materialized: bool,
        ) -> Result<(), PersistenceError> {
            let mut store = self.store.lock().expect("mem");
            let entry = store
                .entry(header.id.as_str().to_string())
                .or_insert_with(|| StoredSession {
                    inspection: SessionInspection {
                        meta: header.clone(),
                        events: Vec::new(),
                    },
                    torn_to: None,
                });
            if !materialized {
                entry.inspection.meta = header.clone();
                entry.inspection.events.clear();
            }
            entry.inspection.events.extend(events.iter().cloned());
            self.appends.lock().expect("appends").push(events.len());
            Ok(())
        }

        async fn commit_repair(
            &self,
            header: &SessionHeader,
            torn_to: Option<u64>,
            closers: &[SessionEvent],
        ) -> Result<(), PersistenceError> {
            let mut store = self.store.lock().expect("mem");
            let Some(entry) = store.get_mut(header.id.as_str()) else {
                return Err(PersistenceError::NotFound(header.id.as_str().to_string()));
            };
            if let Some(torn) = torn_to {
                entry.inspection.events.retain(|event| event.seq < torn);
            }
            entry.inspection.events.extend(closers.iter().cloned());
            entry.torn_to = None;
            Ok(())
        }
    }

    fn turn_start(seq: u64) -> SessionEvent {
        SessionEvent {
            seq,
            time: seq,
            data: SessionEventData::TurnStart { turn: 1 },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        }
    }

    #[tokio::test]
    async fn create_append_read_from_and_seq_mismatch() {
        let runtime = PersistenceRuntime::new(Arc::new(MemoryBackend::new()));
        let header = SessionHeader::new(session_id("s"), None);
        runtime.create(header.clone()).await.unwrap();
        runtime.append(&header.id, &[turn_start(0)]).await.unwrap();
        let suffix = runtime.read_from(&header.id, 0).await.unwrap();
        assert_eq!(suffix.events.len(), 1);
        let empty = runtime.read_from(&header.id, 1).await.unwrap();
        assert!(empty.events.is_empty());
        let err = runtime
            .append(&header.id, &[turn_start(0)])
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("append seq mismatch for \"s\": expected 1 at index 0, got 0"));
        let duplicate = runtime.create(header.clone()).await.unwrap_err();
        assert!(duplicate
            .to_string()
            .contains("already exists in this backend"));
        let other = PersistenceRuntime::new(runtime.backend.clone());
        let persisted = other.create(header).await.unwrap_err();
        assert!(persisted
            .to_string()
            .contains("already has a persisted log on disk"));
    }

    #[tokio::test]
    async fn load_commits_interrupted_closer() {
        let backend = Arc::new(MemoryBackend::new());
        let header = SessionHeader::new(session_id("open"), None);
        backend
            .append_events(&header, &[turn_start(0)], false)
            .await
            .unwrap();
        let runtime = PersistenceRuntime::new(Arc::clone(&backend) as Arc<dyn SessionStoreBackend>);
        let inspected = runtime.inspect(&header.id).await.unwrap();
        assert!(matches!(
            inspected.events.last().unwrap().data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        assert_eq!(
            backend
                .load_stored(&header.id)
                .await
                .unwrap()
                .unwrap()
                .inspection
                .events
                .len(),
            1
        );
        runtime.load(&header.id).await.unwrap();
        assert_eq!(
            backend
                .load_stored(&header.id)
                .await
                .unwrap()
                .unwrap()
                .inspection
                .events
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn prepare_refuses_a_live_session() {
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let runtime = Arc::new(PersistenceRuntime::new(Arc::new(MemoryBackend::new())));
        ctx.provide(Arc::clone(&runtime)).unwrap();
        runtime.install_write_path(&ctx).unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("live"));
        let err = match runtime.prepare(session.id()).await {
            Err(error) => error,
            Ok(_) => panic!("prepare must refuse a live session"),
        };
        assert_eq!(
            err.to_string(),
            "cannot prepare session \"live\" while it is live"
        );
        ctx.dispose();
    }

    #[tokio::test]
    async fn reserved_prepare_blocks_append() {
        let backend = Arc::new(MemoryBackend::new());
        let header = SessionHeader::new(session_id("held"), None);
        backend
            .append_events(&header, &[turn_start(0)], false)
            .await
            .unwrap();
        let runtime = PersistenceRuntime::new(Arc::clone(&backend) as Arc<dyn SessionStoreBackend>);
        runtime.prepare(&header.id).await.unwrap();
        let err = runtime
            .append(
                &header.id,
                &[SessionEvent {
                    seq: 1,
                    time: 1,
                    data: SessionEventData::TurnEnd {
                        turn: 1,
                        reason: TurnEndReason::Completed,
                    },
                    source_event_seqs: None,
                    surface_op: None,
                    ignorable: false,
                }],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(
            "cannot append session \"held\" while its persisted preparation is reserved"
        ));
    }
}
