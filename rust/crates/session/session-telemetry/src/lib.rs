//! Session telemetry Service Definition: capture coordinator, redact waterfall,
//! and the backend sink that a reporting provider implements.

use dsh_cordis::{Context, Result, Service};
use dsh_session::{
    event_type_name, session_id, Session, SessionEvent, SessionEventData, SessionStore,
    TurnEndReason,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "session-telemetry"
}

/// Redact waterfall dispatched between projection and backend `emit`.
pub const RECORD_WATERFALL: &str = "session-telemetry/record";

const DISABLED_FEEDBACK_WARNING: &str =
    "session telemetry is DISABLED; nothing will be shared and this feedback remains local";
const NON_CANONICAL_FEEDBACK_WARNING: &str =
    "session telemetry ignored a feedback event absent from the canonical session log";

/// Ledger versus operational channel; backends keep the two under separate scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionTelemetryChannel {
    /// Session-log mirror.
    Ledger,
    /// Operational signal with no log home.
    Ops,
}

/// Pre-mapped alerting severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionTelemetrySeverity {
    /// Default for captured events without an outcome flag.
    Info,
    /// Available to redact policies and backends.
    Warn,
    /// Outcome-flagged tool results, turn-end errors, and agent-error ops.
    Error,
}

/// Deployment-selected session-sharing policy disclosed to `/feedback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharingStatus {
    /// Full session sharing.
    Full,
    /// Sharing is gated on recorded feedback.
    FeedbackOnly,
    /// Sharing is off.
    Disabled,
}

impl SharingStatus {
    /// Seam vocabulary used on the backend `sharing` member.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::FeedbackOnly => "feedback-only",
            Self::Disabled => "disabled",
        }
    }
}

/// One logical record handed to a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTelemetryRecord {
    /// Ledger or ops channel.
    pub channel: SessionTelemetryChannel,
    /// Unix epoch milliseconds.
    pub time: u64,
    /// Pre-mapped alerting severity.
    pub severity: SessionTelemetrySeverity,
    /// Identity attributes; values are strings or numbers.
    pub attributes: Map<String, Value>,
    /// Deep copy of session event `data`, or the ops payload.
    pub body: Value,
}

/// Minimum backend contract the coordinator requires.
pub trait SessionTelemetrySink: Send + Sync {
    /// Non-blocking enqueue of one record.
    fn emit(&self, record: SessionTelemetryRecord);
    /// Optional turn-end hint. The OTel backend leaves this unimplemented.
    fn flush(&self) {}
    /// Drain queued records and reach quiescence.
    ///
    /// # Errors
    /// Backend pipeline shutdown failure or deadline.
    fn shutdown(&self) -> std::result::Result<(), String>;
}

struct DropSink;

impl SessionTelemetrySink for DropSink {
    fn emit(&self, _record: SessionTelemetryRecord) {}
    fn shutdown(&self) -> std::result::Result<(), String> {
        Ok(())
    }
}

/// Whether capture follows live events or reads the canonical log on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTelemetryCapture {
    /// Follow `session/event` plus lifecycle listeners.
    Live,
    /// Wait for explicit [`SessionTelemetryCoordinator::capture_session`].
    OnDemand,
}

/// `ctx.sessionTelemetry`: disclosure plus the backend sink.
pub struct SessionTelemetry {
    /// Disclosed sharing policy.
    pub sharing: SharingStatus,
    sink: Arc<dyn SessionTelemetrySink>,
    warnings: Arc<Mutex<Vec<String>>>,
}

impl SessionTelemetry {
    /// Build a backend that discloses `sharing` and forwards to `sink`.
    pub fn new(sharing: SharingStatus, sink: Arc<dyn SessionTelemetrySink>) -> Self {
        Self {
            sharing,
            sink,
            warnings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Disclosure-only backend: `emit` and `shutdown` are no-ops.
    pub fn sharing_only(sharing: SharingStatus) -> Self {
        Self::new(sharing, Arc::new(DropSink))
    }

    /// Provide `ctx.sessionTelemetry` with `sharing` and optional `sink`.
    ///
    /// # Errors
    /// Duplicate service registration.
    pub fn install(
        ctx: &Context,
        sharing: SharingStatus,
        sink: Option<Arc<dyn SessionTelemetrySink>>,
    ) -> Result<Arc<Self>> {
        let service = Arc::new(match sink {
            Some(sink) => Self::new(sharing, sink),
            None => Self::sharing_only(sharing),
        });
        ctx.provide(Arc::clone(&service))?;
        Ok(service)
    }

    /// Hand one record to the backend sink. No-op when the sink drops records.
    pub fn emit(&self, record: SessionTelemetryRecord) {
        self.sink.emit(record);
    }

    /// Drain the backend pipeline.
    ///
    /// # Errors
    /// Backend shutdown failure or deadline.
    pub fn shutdown(&self) -> std::result::Result<(), String> {
        self.sink.shutdown()
    }

    /// Record a capture-side warning. Also printed to stderr.
    pub fn warn(&self, message: &str) {
        eprintln!("{message}");
        self.warnings
            .lock()
            .expect("telemetry warnings")
            .push(message.to_string());
    }

    /// Warnings recorded since construction, oldest first.
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().expect("telemetry warnings").clone()
    }
}

impl SessionTelemetrySink for SessionTelemetry {
    fn emit(&self, record: SessionTelemetryRecord) {
        self.sink.emit(record);
    }

    fn shutdown(&self) -> std::result::Result<(), String> {
        self.sink.shutdown()
    }
}

impl Service for SessionTelemetry {
    const KEY: &'static str = "sessionTelemetry";
}

/// Warn through the mounted service when present, otherwise stderr only.
pub fn warn_capture(ctx: &Context, message: &str) {
    if let Some(telemetry) = ctx.get::<SessionTelemetry>() {
        telemetry.warn(message);
    } else {
        eprintln!("{message}");
    }
}

static HANDOFF: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn handoff() -> &'static Mutex<HashMap<String, u64>> {
    HANDOFF.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Capture coordinator composed by a backend.
pub struct SessionTelemetryCoordinator {
    ctx: Context,
    backend: Arc<dyn SessionTelemetrySink>,
    adopted: Mutex<HashMap<String, Arc<Session>>>,
    chunk_seen: Mutex<HashMap<String, HashSet<String>>>,
}

impl SessionTelemetryCoordinator {
    /// Install capture listeners on `ctx` for `backend`.
    ///
    /// # Errors
    /// Effect registration failure.
    pub fn install(
        ctx: &Context,
        backend: Arc<dyn SessionTelemetrySink>,
        capture: SessionTelemetryCapture,
    ) -> Result<Arc<Self>> {
        let coordinator = Arc::new(Self {
            ctx: ctx.clone(),
            backend,
            adopted: Mutex::new(HashMap::new()),
            chunk_seen: Mutex::new(HashMap::new()),
        });
        if capture == SessionTelemetryCapture::Live {
            let adopt = Arc::clone(&coordinator);
            ctx.on("session/created", move |payload| {
                adopt.contain(|| {
                    if let Some(session) = lookup_session(&adopt.ctx, &payload) {
                        adopt.adopt(&session);
                    }
                });
            })?;
            let disposed = Arc::clone(&coordinator);
            ctx.on("session/disposed", move |payload| {
                disposed.contain(|| {
                    let Some(id) = payload.get("id").and_then(Value::as_str) else {
                        return;
                    };
                    let Some(session) = disposed.adopted.lock().expect("adopted").remove(id) else {
                        return;
                    };
                    let record = disposed.redact(shutdown_record(&session));
                    disposed.deliver(&session, record, None);
                });
            })?;
            let events = Arc::clone(&coordinator);
            ctx.on("session/event", move |payload| {
                events.contain(|| {
                    events.on_session_event(&payload);
                });
            })?;
            let flush = Arc::clone(&coordinator);
            ctx.on("session/flush", move |payload| {
                flush.contain(|| {
                    if let Some(session) = lookup_session(&flush.ctx, &payload) {
                        if flush
                            .adopted
                            .lock()
                            .expect("adopted")
                            .contains_key(session.id().as_str())
                        {
                            flush.backend.flush();
                        }
                    }
                });
            })?;
            let errors = Arc::clone(&coordinator);
            ctx.on("agent/error", move |payload| {
                errors.contain(|| {
                    errors.relay_agent_error(&payload);
                });
            })?;
            if let Some(store) = ctx.get::<SessionStore>() {
                for session in store.live() {
                    coordinator.adopt(&session);
                }
            }
        }
        let teardown = Arc::clone(&coordinator);
        ctx.effect("telemetry capture", move || {
            move || {
                let remaining: Vec<Arc<Session>> = teardown
                    .adopted
                    .lock()
                    .expect("adopted")
                    .values()
                    .cloned()
                    .collect();
                for session in remaining {
                    teardown.contain(|| {
                        let record = teardown.redact(shutdown_record(&session));
                        teardown.deliver(&session, record, None);
                    });
                }
                if let Err(error) = teardown.backend.shutdown() {
                    warn_capture(
                        &teardown.ctx,
                        &format!("telemetry: backend shutdown failed: {error}"),
                    );
                }
            }
        })?;
        Ok(coordinator)
    }

    /// Project and hand over the canonical session-log suffix after the handoff
    /// cursor, optionally stopping at an inclusive sequence boundary.
    pub fn capture_session(&self, session: &Session, through_seq: Option<u64>) {
        let cursor = {
            let map = handoff().lock().expect("handoff");
            map.get(session.id().as_str())
                .copied()
                .map(|seq| seq as i64)
                .unwrap_or(session.first_live_seq() as i64 - 1)
        };
        for event in session.events() {
            if let Some(through) = through_seq {
                if event.seq > through {
                    break;
                }
            }
            self.contain(|| {
                if (event.seq as i64) <= cursor {
                    self.track(session, &event);
                } else {
                    self.capture_event(session, &event);
                }
            });
        }
    }

    fn adopt(&self, session: &Arc<Session>) {
        if self
            .adopted
            .lock()
            .expect("adopted")
            .contains_key(session.id().as_str())
        {
            return;
        }
        self.adopted
            .lock()
            .expect("adopted")
            .insert(session.id().as_str().to_string(), Arc::clone(session));
        self.capture_session(session, None);
    }

    fn on_session_event(&self, payload: &Value) {
        let Some(session) = lookup_session(&self.ctx, payload) else {
            return;
        };
        let Some(event) = payload.get("event").cloned() else {
            return;
        };
        let Ok(event) = serde_json::from_value::<SessionEvent>(event) else {
            return;
        };
        if !self
            .adopted
            .lock()
            .expect("adopted")
            .contains_key(session.id().as_str())
        {
            self.adopt(&session);
            return;
        }
        self.capture_event(&session, &event);
    }

    fn track(&self, session: &Session, event: &SessionEvent) {
        if let SessionEventData::AssistantChunk { turn, step, .. } = &event.data {
            let _ = self.chunk_already_seen(session.id().as_str(), &format!("{turn}:{step}"));
        }
    }

    fn capture_event(&self, session: &Session, event: &SessionEvent) {
        if let SessionEventData::AssistantChunk { turn, step, .. } = &event.data {
            if self.chunk_already_seen(session.id().as_str(), &format!("{turn}:{step}")) {
                return;
            }
        }
        let record = self.redact(SessionTelemetryRecord {
            channel: SessionTelemetryChannel::Ledger,
            time: event.time,
            severity: severity_of(&event.data),
            attributes: identity_of(session, event),
            body: event_body(&event.data),
        });
        self.deliver(session, record, Some(event.seq));
    }

    fn chunk_already_seen(&self, session_id: &str, key: &str) -> bool {
        let mut map = self.chunk_seen.lock().expect("chunk seen");
        let set = map.entry(session_id.to_string()).or_default();
        if set.contains(key) {
            true
        } else {
            set.insert(key.to_string());
            false
        }
    }

    fn redact(&self, record: SessionTelemetryRecord) -> SessionTelemetryRecord {
        let payload = serde_json::to_value(&record).unwrap_or(Value::Null);
        let transformed = self
            .ctx
            .waterfall(RECORD_WATERFALL, payload, |value| value)
            .unwrap_or(Value::Null);
        serde_json::from_value(transformed).unwrap_or(record)
    }

    fn deliver(&self, session: &Session, record: SessionTelemetryRecord, seq: Option<u64>) {
        self.backend.emit(record);
        if let Some(seq) = seq {
            handoff()
                .lock()
                .expect("handoff")
                .insert(session.id().as_str().to_string(), seq);
        }
    }

    fn relay_agent_error(&self, payload: &Value) {
        let session = lookup_session(&self.ctx, payload).or_else(|| {
            payload
                .get("agent")
                .and_then(|agent| agent.get("sessionId").or_else(|| agent.get("id")))
                .and_then(Value::as_str)
                .and_then(|id| {
                    self.ctx
                        .get::<SessionStore>()
                        .and_then(|store| store.get(&session_id(id)))
                })
        });
        let Some(session) = session else {
            return;
        };
        let agent_id = payload
            .get("agent")
            .and_then(|agent| agent.get("id"))
            .and_then(Value::as_str)
            .unwrap_or(session.id().as_str())
            .to_string();
        let turn = payload.get("turn").and_then(Value::as_u64).unwrap_or(0);
        let step = payload.get("step").and_then(Value::as_u64).unwrap_or(0);
        let (name, message) = error_detail(payload.get("error"));
        let mut attributes = Map::new();
        attributes.insert("telemetry.op".into(), json!("agent-error"));
        attributes.insert("session.id".into(), json!(session.id().as_str()));
        attributes.insert("agent.id".into(), json!(agent_id));
        attributes.insert("error.name".into(), json!(name.clone()));
        attributes.insert("turn".into(), json!(turn));
        attributes.insert("step".into(), json!(step));
        let record = self.redact(SessionTelemetryRecord {
            channel: SessionTelemetryChannel::Ops,
            time: dsh_session::now_ms(),
            severity: SessionTelemetrySeverity::Error,
            attributes,
            body: json!({ "name": name, "message": message }),
        });
        self.deliver(&session, record, None);
    }

    fn contain(&self, step: impl FnOnce()) {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(step)) {
            let message = panic_message(panic);
            warn_capture(
                &self.ctx,
                &format!("telemetry: capture step failed: {message}"),
            );
        }
    }
}

fn lookup_session(ctx: &Context, payload: &Value) -> Option<Arc<Session>> {
    let id = payload
        .get("sessionId")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)?;
    ctx.get::<SessionStore>()?.get(&session_id(id))
}

fn shutdown_record(session: &Session) -> SessionTelemetryRecord {
    let mut attributes = Map::new();
    attributes.insert("telemetry.op".into(), json!("shutdown"));
    attributes.insert("session.id".into(), json!(session.id().as_str()));
    SessionTelemetryRecord {
        channel: SessionTelemetryChannel::Ops,
        time: dsh_session::now_ms(),
        severity: SessionTelemetrySeverity::Info,
        attributes,
        body: json!({ "op": "shutdown" }),
    }
}

fn severity_of(data: &SessionEventData) -> SessionTelemetrySeverity {
    match data {
        SessionEventData::ToolResult { message, .. } if message.is_error() => {
            SessionTelemetrySeverity::Error
        }
        SessionEventData::TurnEnd {
            reason: TurnEndReason::Error { .. },
            ..
        } => SessionTelemetrySeverity::Error,
        _ => SessionTelemetrySeverity::Info,
    }
}

fn identity_of(session: &Session, event: &SessionEvent) -> Map<String, Value> {
    let mut attributes = Map::new();
    attributes.insert("session.id".into(), json!(session.id().as_str()));
    attributes.insert("event.type".into(), json!(event_type_name(&event.data)));
    attributes.insert("event.seq".into(), json!(event.seq));
    let header = session.header();
    if let Some(cwd) = &header.cwd {
        attributes.insert("session.cwd".into(), json!(cwd));
    }
    if let Some(parent) = &header.parent_session {
        attributes.insert("session.parent_id".into(), json!(parent.as_str()));
    }
    if let Some(seed_length) = header.seed_length {
        attributes.insert("session.seed_length".into(), json!(seed_length));
    }
    attributes
}

fn event_body(data: &SessionEventData) -> Value {
    let wrapped = serde_json::to_value(data).unwrap_or(Value::Null);
    match wrapped {
        Value::Object(mut map) => map.remove("data").unwrap_or(Value::Object(map)),
        other => other,
    }
}

fn error_detail(error: Option<&Value>) -> (String, String) {
    match error {
        Some(Value::Object(map)) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Error")
                .to_string();
            let message = map
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (name, message)
        }
        Some(Value::String(text)) => ("Error".into(), text.clone()),
        Some(other) => ("Error".into(), other.to_string()),
        None => ("Error".into(), String::new()),
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".into()
    }
}

/// Stable DISABLED-mode warning when recorded feedback stays local.
pub fn disabled_feedback_warning() -> &'static str {
    DISABLED_FEEDBACK_WARNING
}

/// Warning when a `feedback/record` bus value is absent from the canonical log.
pub fn non_canonical_feedback_warning() -> &'static str {
    NON_CANONICAL_FEEDBACK_WARNING
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::FnPlugin;
    use dsh_llm::{call_id, StreamChunk, ToolResultMessage, UserMessage};
    use dsh_session::{session_id, SurfaceOp};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn next_session_id() -> dsh_session::SessionId {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        session_id(format!(
            "cap-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct FakeBackend {
        records: Mutex<Vec<SessionTelemetryRecord>>,
        fail_seq: Mutex<Option<u64>>,
        shutdown_error: Mutex<Option<String>>,
        flush_count: Mutex<u32>,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                records: Mutex::new(Vec::new()),
                fail_seq: Mutex::new(None),
                shutdown_error: Mutex::new(None),
                flush_count: Mutex::new(0),
            })
        }

        fn ledger(&self) -> Vec<SessionTelemetryRecord> {
            self.records
                .lock()
                .expect("records")
                .iter()
                .filter(|record| record.channel == SessionTelemetryChannel::Ledger)
                .cloned()
                .collect()
        }
    }

    impl SessionTelemetrySink for FakeBackend {
        fn emit(&self, record: SessionTelemetryRecord) {
            let reject = self
                .fail_seq
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(seq) = reject {
                if record.attributes.get("event.seq").and_then(Value::as_u64) == Some(seq) {
                    panic!("backend rejected seq {seq}");
                }
            }
            self.records
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(record);
        }

        fn flush(&self) {
            *self.flush_count.lock().expect("flush") += 1;
        }

        fn shutdown(&self) -> std::result::Result<(), String> {
            if let Some(error) = self.shutdown_error.lock().expect("shutdown").clone() {
                return Err(error);
            }
            Ok(())
        }
    }

    fn setup(
        capture: SessionTelemetryCapture,
    ) -> (
        Context,
        Arc<FakeBackend>,
        Arc<Session>,
        Arc<SessionTelemetryCoordinator>,
    ) {
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let backend = FakeBackend::new();
        let coordinator = SessionTelemetryCoordinator::install(
            &ctx,
            backend.clone(),
            capture,
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(next_session_id());
        (ctx, backend, session, coordinator)
    }

    #[test]
    fn hands_appended_events_with_envelope_identity() {
        let (_ctx, backend, session, _) = setup(SessionTelemetryCapture::Live);
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("hello")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        let ledger = backend.ledger();
        assert_eq!(ledger[0].attributes["event.type"], json!("turn/start"));
        assert_eq!(
            ledger[0].attributes["session.id"],
            json!(session.id().as_str())
        );
        assert_eq!(ledger[0].attributes["event.seq"], json!(0));
        assert_eq!(ledger[0].severity, SessionTelemetrySeverity::Info);
        assert_eq!(ledger[1].attributes["event.seq"], json!(1));
        assert_eq!(ledger[0].time, session.events()[0].time);
    }

    #[test]
    fn stamps_header_cwd_on_records() {
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let backend = FakeBackend::new();
        SessionTelemetryCoordinator::install(
            &ctx,
            backend.clone(),
            SessionTelemetryCapture::Live,
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create_in(Some("/tmp/proj".into()));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        assert_eq!(
            backend.ledger()[0].attributes["session.cwd"],
            json!("/tmp/proj")
        );
    }

    #[test]
    fn maps_outcome_flags_to_severity() {
        let (_ctx, backend, session, _) = setup(SessionTelemetryCapture::Live);
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::ToolResult {
                    turn: 1,
                    step: 1,
                    message: ToolResultMessage::new(call_id("c1"), vec![], true),
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Error {
                        message: "boom".into(),
                        code: "UNKNOWN".into(),
                    },
                },
                None,
            )
            .unwrap();
        let kinds: Vec<_> = backend
            .ledger()
            .into_iter()
            .map(|record| {
                (
                    record.attributes["event.type"].as_str().unwrap().to_string(),
                    record.severity,
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("turn/start".into(), SessionTelemetrySeverity::Info),
                ("tool/result".into(), SessionTelemetrySeverity::Error),
                ("turn/end".into(), SessionTelemetrySeverity::Error),
            ]
        );
    }

    #[test]
    fn ships_only_the_first_chunk_of_each_turn_step() {
        let (_ctx, backend, session, _) = setup(SessionTelemetryCapture::Live);
        let chunk = |text: &str, step: u32| SessionEventData::AssistantChunk {
            turn: 1,
            step,
            chunk: StreamChunk::TextDelta {
                index: 0,
                text: text.into(),
            },
        };
        session.append(chunk("first", 1), None).unwrap();
        session.append(chunk("second", 1), None).unwrap();
        session.append(chunk("next-step", 2), None).unwrap();
        let texts: Vec<_> = backend
            .ledger()
            .into_iter()
            .map(|record| record.body["chunk"]["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(texts, vec!["first".to_string(), "next-step".to_string()]);
    }

    #[test]
    fn on_demand_captures_a_prefix_and_ignores_later_appends() {
        let (_ctx, backend, session, coordinator) = setup(SessionTelemetryCapture::OnDemand);
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
        assert!(backend.ledger().is_empty());
        coordinator.capture_session(&session, Some(0));
        assert_eq!(
            backend.ledger()[0].attributes["event.type"],
            json!("turn/start")
        );
        assert_eq!(backend.ledger().len(), 1);
        coordinator.capture_session(&session, None);
        assert_eq!(backend.ledger().len(), 2);
    }

    #[test]
    fn redact_waterfall_scrubs_body_and_leaves_the_log() {
        let (ctx, backend, session, _) = setup(SessionTelemetryCapture::Live);
        ctx.on_waterfall(RECORD_WATERFALL, |payload, next| {
            let mut record = next.call(payload);
            if let Some(object) = record.as_object_mut() {
                object.insert("body".into(), json!({ "scrubbed": true }));
            }
            record
        })
        .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("secret")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        assert_eq!(backend.ledger()[0].body, json!({ "scrubbed": true }));
        let wire = serde_json::to_value(&session.events()[0]).unwrap();
        assert_eq!(wire["data"]["content"][0]["text"], "secret");
    }

    #[test]
    fn contains_backend_failures_and_continues() {
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let backend = FakeBackend::new();
        *backend.fail_seq.lock().expect("fail") = Some(1);
        SessionTelemetryCoordinator::install(
            &ctx,
            backend.clone(),
            SessionTelemetryCapture::Live,
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(next_session_id());
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("hello")),
                Some(SurfaceOp::append()),
            )
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
        let seqs: Vec<_> = backend
            .ledger()
            .into_iter()
            .map(|record| record.attributes["event.seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![0, 2]);
    }

    #[test]
    fn dispose_emits_shutdown_ops_then_backend_shutdown() {
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let backend = FakeBackend::new();
        let handle = ctx
            .plugin(FnPlugin::new("fake-telemetry", {
                let sink: Arc<dyn SessionTelemetrySink> = backend.clone();
                move |child| {
                    SessionTelemetryCoordinator::install(
                        child,
                        Arc::clone(&sink),
                        SessionTelemetryCapture::Live,
                    )?;
                    Ok(())
                }
            }))
            .unwrap();
        let _session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(next_session_id());
        handle.dispose();
        let ops: Vec<_> = backend
            .records
            .lock()
            .expect("records")
            .iter()
            .filter(|record| record.channel == SessionTelemetryChannel::Ops)
            .cloned()
            .collect();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].attributes["telemetry.op"], json!("shutdown"));
        assert!(ops[0].attributes.get("event.seq").is_none());
    }

    #[test]
    fn relays_agent_error_as_ops() {
        let (ctx, backend, session, _) = setup(SessionTelemetryCapture::Live);
        ctx.emit(
            "agent/error",
            json!({
                "sessionId": session.id().as_str(),
                "agent": { "id": "agent-1" },
                "turn": 3,
                "step": 2,
                "error": { "name": "TypeError", "message": "adapter exploded" },
            }),
        );
        let ops: Vec<_> = backend
            .records
            .lock()
            .expect("records")
            .iter()
            .filter(|record| record.channel == SessionTelemetryChannel::Ops)
            .cloned()
            .collect();
        assert_eq!(ops[0].severity, SessionTelemetrySeverity::Error);
        assert_eq!(ops[0].attributes["telemetry.op"], json!("agent-error"));
        assert_eq!(ops[0].attributes["agent.id"], json!("agent-1"));
        assert_eq!(
            ops[0].body,
            json!({ "name": "TypeError", "message": "adapter exploded" })
        );
    }

    #[test]
    fn on_demand_does_not_follow_live_events_or_flush() {
        let (ctx, backend, session, _) = setup(SessionTelemetryCapture::OnDemand);
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.emit(
            "session/flush",
            json!({ "sessionId": session.id().as_str() }),
        );
        assert!(backend.records.lock().expect("records").is_empty());
        assert_eq!(*backend.flush_count.lock().expect("flush"), 0);
    }
}
