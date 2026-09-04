//! Session telemetry backend. The shipped default is `DISABLED`: no records
//! leave the process, and `ctx.sessionTelemetry.sharing` discloses `disabled`
//! so `/feedback` can report the standing policy. `FULL` and `FEEDBACK_ONLY`
//! construct an OTLP/HTTP JSON log pipeline. Transient collector failures
//! (429/502/503/504 or a transport error) retry inside
//! `processor.exportTimeoutMillis`, with each HTTP attempt capped by
//! `exporter.timeoutMillis`. `exporter.keepAlive` defaults to true and
//! reuses one HTTP/1.1 socket per worker; HTTPS still uses one-shot `curl`
//! and honors `Retry-After` delay-seconds and HTTP-date from the dumped
//! response headers. A full queue
//! drops the newest record.
//! The seam `flush` hint stays unimplemented: forwarding it would be a
//! second flusher against the batch worker, and the TypeScript backend
//! refuses that path because concurrent `forceFlush` can drop tail records.
//! This crate emits no OTel SDK metrics.

mod export;

use crate::export::{OtlpPipeline, PipelineSpec};
use dsh_cordis::{Context, CordisError, Result};
use dsh_session::{event_type_name, session_id, SessionStore};
use dsh_session_telemetry::{
    disabled_feedback_warning, non_canonical_feedback_warning, SessionTelemetry,
    SessionTelemetryCapture, SessionTelemetryCoordinator, SessionTelemetryRecord,
    SessionTelemetrySink, SharingStatus,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "session-telemetry-otel"
}

/// Default outer allowance for the complete shutdown sequence.
pub const DEFAULT_SHUTDOWN_TIMEOUT_MILLIS: f64 = 3_000.0;

const MAX_TIMER_DELAY_MILLIS: f64 = 2_147_483_647.0;
const DEFAULT_EXPORTER_TIMEOUT_MILLIS: u64 = 10_000;
const DEFAULT_EXPORT_TIMEOUT_MILLIS: u64 = 30_000;
const DEFAULT_SCHEDULED_DELAY_MILLIS: u64 = 5_000;
const DEFAULT_MAX_EXPORT_BATCH_SIZE: usize = 512;
const DEFAULT_MAX_QUEUE_SIZE: usize = 2_048;

/// Sharing policy selected by plugin `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTelemetryMode {
    /// Live capture of every adopted session.
    Full,
    /// Canonical-log capture at each `feedback/record`.
    FeedbackOnly,
    /// Disclosure only; no exporter.
    Disabled,
}

/// Resolve the default and reject unknown runtime values before transport setup.
pub fn resolve_mode(config: Option<&Value>) -> Result<SessionTelemetryMode> {
    let raw = config
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("DISABLED");
    match raw {
        "FULL" => Ok(SessionTelemetryMode::Full),
        "FEEDBACK_ONLY" => Ok(SessionTelemetryMode::FeedbackOnly),
        "DISABLED" => Ok(SessionTelemetryMode::Disabled),
        other => Err(CordisError::Validation(format!(
            "session-telemetry-otel: unknown mode {other:?}"
        ))),
    }
}

/// Map a mode onto the seam's sharing vocabulary.
pub fn resolve_sharing(config: Option<&Value>) -> Result<SharingStatus> {
    Ok(sharing_status_for(resolve_mode(config)?))
}

fn sharing_status_for(mode: SessionTelemetryMode) -> SharingStatus {
    match mode {
        SessionTelemetryMode::Full => SharingStatus::Full,
        SessionTelemetryMode::FeedbackOnly => SharingStatus::FeedbackOnly,
        SessionTelemetryMode::Disabled => SharingStatus::Disabled,
    }
}

/// Provide `ctx.sessionTelemetry` and, outside `DISABLED`, the OTLP pipeline.
///
/// # Errors
/// Unknown mode, missing or illegal `exporter.url`, non-positive batch size
/// or `exportTimeoutMillis`, or an illegal `shutdownTimeoutMillis` in an
/// uploading mode.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let mode = resolve_mode(config)?;
    match mode {
        SessionTelemetryMode::Disabled => install_disabled(ctx),
        SessionTelemetryMode::Full | SessionTelemetryMode::FeedbackOnly => {
            install_uploading(ctx, mode, config.unwrap_or(&Value::Null))
        }
    }
}

fn install_disabled(ctx: &Context) -> Result<()> {
    SessionTelemetry::install(ctx, SharingStatus::Disabled, None)?;
    let lookup = ctx.clone();
    ctx.on("session/event", move |payload| {
        let is_feedback = payload
            .get("event")
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str)
            == Some("feedback/record");
        if is_feedback {
            if let Some(telemetry) = lookup.get::<SessionTelemetry>() {
                telemetry.warn(disabled_feedback_warning());
            }
        }
    })?;
    Ok(())
}

fn install_uploading(ctx: &Context, mode: SessionTelemetryMode, config: &Value) -> Result<()> {
    let spec = parse_pipeline_spec(config)?;
    let pipeline = OtlpPipeline::start(spec);
    let sharing = sharing_status_for(mode);
    let service_sink: Arc<dyn SessionTelemetrySink> = match mode {
        SessionTelemetryMode::Full => pipeline.clone() as Arc<dyn SessionTelemetrySink>,
        SessionTelemetryMode::FeedbackOnly => Arc::new(DirectDrop {
            pipeline: Arc::clone(&pipeline),
        }),
        SessionTelemetryMode::Disabled => {
            return Err(CordisError::plugin(
                "session-telemetry-otel: uploading installer called for DISABLED",
            ))
        }
    };
    SessionTelemetry::install(ctx, sharing, Some(service_sink))?;
    let capture = match mode {
        SessionTelemetryMode::Full => SessionTelemetryCapture::Live,
        SessionTelemetryMode::FeedbackOnly => SessionTelemetryCapture::OnDemand,
        SessionTelemetryMode::Disabled => {
            return Err(CordisError::plugin(
                "session-telemetry-otel: uploading installer called for DISABLED",
            ))
        }
    };
    let coordinator = SessionTelemetryCoordinator::install(ctx, pipeline, capture)?;
    if mode == SessionTelemetryMode::FeedbackOnly {
        let lookup = ctx.clone();
        ctx.on("session/event", move |payload| {
            on_feedback_only_event(&lookup, &coordinator, &payload);
        })?;
    }
    Ok(())
}

struct DirectDrop {
    pipeline: Arc<OtlpPipeline>,
}

impl SessionTelemetrySink for DirectDrop {
    fn emit(&self, _record: SessionTelemetryRecord) {}

    fn shutdown(&self) -> std::result::Result<(), String> {
        self.pipeline.shutdown()
    }
}

fn on_feedback_only_event(
    ctx: &Context,
    coordinator: &SessionTelemetryCoordinator,
    payload: &Value,
) {
    let Some(event) = payload.get("event") else {
        return;
    };
    if event.get("type").and_then(Value::as_str) != Some("feedback/record") {
        return;
    }
    let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
        return;
    };
    let Some(id) = payload
        .get("sessionId")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(session) = ctx
        .get::<SessionStore>()
        .and_then(|store| store.get(&session_id(id)))
    else {
        return;
    };
    let canonical = session
        .events()
        .get(seq as usize)
        .is_some_and(|row| event_type_name(&row.data) == "feedback/record");
    if !canonical {
        if let Some(telemetry) = ctx.get::<SessionTelemetry>() {
            telemetry.warn(non_canonical_feedback_warning());
        }
        return;
    }
    coordinator.capture_session(&session, Some(seq));
}

fn parse_pipeline_spec(config: &Value) -> Result<PipelineSpec> {
    let url = config
        .get("exporter")
        .and_then(|exporter| exporter.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if url.is_empty() {
        return Err(CordisError::Validation(
            "session-telemetry-otel: exporter.url is required (the full OTLP logs endpoint)".into(),
        ));
    }
    let Some(protocol) = url_protocol(url) else {
        return Err(CordisError::Validation(format!(
            "session-telemetry-otel: exporter.url is not a valid URL: {}",
            serde_json::to_string(url).unwrap_or_else(|_| url.into())
        )));
    };
    if protocol != "http:" && protocol != "https:" {
        return Err(CordisError::Validation(format!(
            "session-telemetry-otel: exporter.url must be http(s), got {protocol}"
        )));
    }
    let batch_size = config
        .get("processor")
        .and_then(|processor| processor.get("maxExportBatchSize"));
    if let Some(batch_size) = batch_size {
        if positive_usize(batch_size).is_none() {
            return Err(CordisError::Validation(format!(
                "session-telemetry-otel: processor.maxExportBatchSize must be a positive integer, got {batch_size}"
            )));
        }
    }
    let shutdown_ms = config
        .get("shutdownTimeoutMillis")
        .cloned()
        .unwrap_or(Value::from(DEFAULT_SHUTDOWN_TIMEOUT_MILLIS));
    let shutdown_ms = shutdown_ms.as_f64().unwrap_or(f64::NAN);
    if !shutdown_ms.is_finite() || shutdown_ms <= 0.0 || shutdown_ms > MAX_TIMER_DELAY_MILLIS {
        return Err(CordisError::Validation(format!(
            "session-telemetry-otel: shutdownTimeoutMillis must be a positive finite number no greater than {}, got {shutdown_ms}",
            MAX_TIMER_DELAY_MILLIS as u64
        )));
    }
    let exporter = config.get("exporter");
    let processor = config.get("processor");
    let export_timeout_ms = match processor.and_then(|value| value.get("exportTimeoutMillis")) {
        None => DEFAULT_EXPORT_TIMEOUT_MILLIS,
        Some(value) => {
            let Some(ms) = value.as_u64().filter(|ms| *ms >= 1) else {
                return Err(CordisError::Validation(format!(
                    "session-telemetry-otel: processor.exportTimeoutMillis must be a positive integer, got {value}"
                )));
            };
            ms
        }
    };
    let keep_alive = match exporter.and_then(|value| value.get("keepAlive")) {
        None => true,
        Some(Value::Bool(flag)) => *flag,
        Some(value) => {
            return Err(CordisError::Validation(format!(
                "session-telemetry-otel: exporter.keepAlive must be a boolean, got {value}"
            )));
        }
    };
    Ok(PipelineSpec {
        url: url.to_string(),
        headers: header_list(exporter),
        timeout: Duration::from_millis(
            exporter
                .and_then(|value| value.get("timeoutMillis"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_EXPORTER_TIMEOUT_MILLIS),
        ),
        export_timeout: Duration::from_millis(export_timeout_ms),
        gzip: exporter
            .and_then(|value| value.get("compression"))
            .and_then(Value::as_str)
            == Some("gzip"),
        scheduled_delay: Duration::from_millis(
            processor
                .and_then(|value| value.get("scheduledDelayMillis"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_SCHEDULED_DELAY_MILLIS),
        ),
        max_export_batch_size: processor
            .and_then(|value| value.get("maxExportBatchSize"))
            .and_then(positive_usize)
            .unwrap_or(DEFAULT_MAX_EXPORT_BATCH_SIZE),
        max_queue_size: processor
            .and_then(|value| value.get("maxQueueSize"))
            .and_then(positive_usize)
            .unwrap_or(DEFAULT_MAX_QUEUE_SIZE),
        shutdown_timeout: Duration::from_secs_f64(shutdown_ms / 1_000.0),
        keep_alive,
    })
}

fn header_list(exporter: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(map)) = exporter.and_then(|value| value.get("headers")) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect()
}

fn positive_usize(value: &Value) -> Option<usize> {
    let number = value.as_u64()?;
    if number >= 1 {
        usize::try_from(number).ok()
    } else {
        None
    }
}

fn url_protocol(raw: &str) -> Option<String> {
    let colon = raw.find(':')?;
    let scheme = &raw[..colon];
    if scheme.is_empty() {
        return None;
    }
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|character| {
        character.is_ascii_alphanumeric()
            || character == '+'
            || character == '-'
            || character == '.'
    }) {
        return None;
    }
    Some(format!("{scheme}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_command_feedback::record_feedback;
    use dsh_session::{session_id, SessionEventData, TurnEndReason};
    use dsh_session_telemetry::RECORD_WATERFALL;
    use serde_json::{json, Map};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct Capture {
        headers: HashMap<String, String>,
        body: Value,
        status: u16,
    }

    struct ReplyScript {
        fail_first: usize,
        fail_status: u16,
        retry_after: Option<&'static str>,
    }

    struct HoldGate {
        arrived: Mutex<bool>,
        arrived_cv: Condvar,
        release: Mutex<bool>,
        release_cv: Condvar,
    }

    impl HoldGate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                arrived: Mutex::new(false),
                arrived_cv: Condvar::new(),
                release: Mutex::new(false),
                release_cv: Condvar::new(),
            })
        }

        fn wait_arrived(&self) {
            let mut flag = self.arrived.lock().expect("arrived");
            while !*flag {
                flag = self.arrived_cv.wait(flag).expect("arrived");
            }
        }

        fn open(&self) {
            *self.release.lock().expect("release") = true;
            self.release_cv.notify_all();
        }

        fn hold_first(&self, index: usize) {
            if index != 0 {
                return;
            }
            *self.arrived.lock().expect("arrived") = true;
            self.arrived_cv.notify_all();
            let mut flag = self.release.lock().expect("release");
            while !*flag {
                flag = self.release_cv.wait(flag).expect("release");
            }
        }
    }

    struct MockCollector {
        url: String,
        captures: Arc<Mutex<Vec<Capture>>>,
        accepts: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        addr: std::net::SocketAddr,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl MockCollector {
        fn start(hold: Option<Arc<HoldGate>>) -> Self {
            Self::start_with(hold, None)
        }

        fn start_with(hold: Option<Arc<HoldGate>>, reply: Option<ReplyScript>) -> Self {
            Self::start_configured(hold, reply, false)
        }

        fn start_reusing(reply: Option<ReplyScript>) -> Self {
            Self::start_configured(None, reply, true)
        }

        fn start_configured(
            hold: Option<Arc<HoldGate>>,
            reply: Option<ReplyScript>,
            reuse: bool,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("collector bind");
            listener.set_nonblocking(false).expect("collector blocking");
            let addr = listener.local_addr().expect("collector addr");
            let url = format!("http://127.0.0.1:{}/v1/logs", addr.port());
            let captures = Arc::new(Mutex::new(Vec::new()));
            let accepts = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let thread_captures = Arc::clone(&captures);
            let thread_accepts = Arc::clone(&accepts);
            let thread_stop = Arc::clone(&stop);
            let index = Arc::new(AtomicUsize::new(0));
            let thread = thread::spawn(move || {
                listener
                    .set_nonblocking(true)
                    .expect("collector nonblocking");
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            thread_accepts.fetch_add(1, Ordering::SeqCst);
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                            loop {
                                let Ok((headers, raw)) = read_http_request(&mut stream) else {
                                    break;
                                };
                                let n = index.fetch_add(1, Ordering::SeqCst);
                                if let Some(gate) = &hold {
                                    gate.hold_first(n);
                                }
                                let (status, retry_after) = match &reply {
                                    Some(script) if n < script.fail_first => {
                                        (script.fail_status, script.retry_after)
                                    }
                                    _ => (200, None),
                                };
                                let body = decode_body(&headers, &raw);
                                thread_captures.lock().expect("captures").push(Capture {
                                    headers,
                                    body,
                                    status,
                                });
                                let _ =
                                    stream.write_all(&collector_reply(status, retry_after, reuse));
                                if !reuse {
                                    break;
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                captures,
                accepts,
                stop,
                addr,
                thread: Some(thread),
            }
        }
    }

    fn collector_reply(status: u16, retry_after: Option<&str>, keep_alive: bool) -> Vec<u8> {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Error",
        };
        let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
        if let Some(retry_after) = retry_after {
            response.push_str(&format!("Retry-After: {retry_after}\r\n"));
        }
        let connection = if keep_alive { "keep-alive" } else { "close" };
        response.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: 2\r\nConnection: {connection}\r\n\r\n{{}}"
        ));
        response.into_bytes()
    }

    impl Drop for MockCollector {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50));
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn read_http_request(
        stream: &mut TcpStream,
    ) -> std::io::Result<(HashMap<String, String>, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte)?;
            buf.push(byte[0]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if buf.len() > 64 * 1024 {
                return Err(std::io::Error::other("headers too large"));
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let mut headers = HashMap::new();
        for line in text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if length > 0 {
            stream.read_exact(&mut body)?;
        }
        Ok((headers, body))
    }

    fn decode_body(headers: &HashMap<String, String>, raw: &[u8]) -> Value {
        let bytes = if headers.get("content-encoding").map(String::as_str) == Some("gzip") {
            gunzip(raw).unwrap_or_else(|_| raw.to_vec())
        } else {
            raw.to_vec()
        };
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn gunzip(raw: &[u8]) -> std::result::Result<Vec<u8>, String> {
        let mut child = Command::new("gzip")
            .arg("-dc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
        {
            let mut stdin = child.stdin.take().ok_or("gzip stdin")?;
            stdin.write_all(raw).map_err(|error| error.to_string())?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(output.stdout)
    }

    fn pin_home() {
        static HOME: OnceLock<PathBuf> = OnceLock::new();
        let home = HOME.get_or_init(|| {
            let path = std::env::temp_dir().join(format!("dsh-otel-home-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&path);
            path
        });
        std::env::set_var("DSH_HOME", home);
    }

    fn boot(url: &str) -> Context {
        pin_home();
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": url, "headers": { "authorization": "Bearer test-token" } },
                "processor": { "scheduledDelayMillis": 20, "maxExportBatchSize": 8 },
            })),
        )
        .unwrap();
        ctx
    }

    fn all_records(captures: &[Capture]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for capture in captures {
            let Some(logs) = capture.body.get("resourceLogs").and_then(Value::as_array) else {
                continue;
            };
            for resource in logs {
                let Some(scopes) = resource.get("scopeLogs").and_then(Value::as_array) else {
                    continue;
                };
                for scope in scopes {
                    let name = scope
                        .get("scope")
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let Some(records) = scope.get("logRecords").and_then(Value::as_array) else {
                        continue;
                    };
                    for record in records {
                        out.push((name.clone(), record.clone()));
                    }
                }
            }
        }
        out
    }

    fn event_types(captures: &[Capture]) -> Vec<String> {
        all_records(captures)
            .into_iter()
            .flat_map(|(_, record)| {
                record
                    .get("attributes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|attribute| {
                        if attribute.get("key").and_then(Value::as_str) == Some("event.type") {
                            attribute
                                .pointer("/value/stringValue")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn snapshot(collector: &MockCollector) -> Vec<Capture> {
        collector
            .captures
            .lock()
            .expect("captures")
            .drain(..)
            .collect()
    }

    #[test]
    fn defaults_to_disabled() {
        assert_eq!(resolve_mode(None).unwrap(), SessionTelemetryMode::Disabled);
        assert_eq!(
            resolve_sharing(Some(&json!({ "mode": "DISABLED" }))).unwrap(),
            SharingStatus::Disabled
        );
    }

    #[test]
    fn accepts_upload_modes_and_rejects_unknown() {
        assert_eq!(
            resolve_sharing(Some(&json!({ "mode": "FULL" }))).unwrap(),
            SharingStatus::Full
        );
        assert_eq!(
            resolve_sharing(Some(&json!({ "mode": "FEEDBACK_ONLY" }))).unwrap(),
            SharingStatus::FeedbackOnly
        );
        let err = resolve_sharing(Some(&json!({ "mode": "maybe" }))).unwrap_err();
        assert!(err.to_string().contains("maybe"));
    }

    #[test]
    fn mounts_disabled_disclosure() {
        pin_home();
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(&ctx, Some(&json!({ "mode": "DISABLED" }))).unwrap();
        assert_eq!(
            ctx.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::Disabled
        );
    }

    #[test]
    fn ships_session_records_and_ops_shutdown_through_http() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = boot(&collector.url);
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create_in(Some("/tmp/w".into()));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
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
        ctx.service::<SessionTelemetry>()
            .unwrap()
            .emit(SessionTelemetryRecord {
                channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                time: 1,
                severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                attributes: {
                    let mut map = Map::new();
                    map.insert("session.id".into(), json!("wire"));
                    map.insert("event.type".into(), json!("manual"));
                    map.insert("event.seq".into(), json!(99));
                    map
                },
                body: json!({ "direct": true }),
            });
        ctx.dispose();
        let captures = snapshot(&collector);
        assert!(!captures.is_empty());
        assert_eq!(
            captures[0].headers.get("authorization").map(String::as_str),
            Some("Bearer test-token")
        );
        let resource = captures[0].body["resourceLogs"][0]["resource"]["attributes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(resource.iter().any(|attribute| {
            attribute.get("key").and_then(Value::as_str) == Some("service.name")
                && attribute
                    .pointer("/value/stringValue")
                    .and_then(Value::as_str)
                    == Some("deepseek-harness")
        }));
        let user = dsh_anonymous_user_id::get_or_create_anonymous_user_id();
        assert!(resource.iter().any(|attribute| {
            attribute.get("key").and_then(Value::as_str) == Some("user.id")
                && attribute
                    .pointer("/value/stringValue")
                    .and_then(Value::as_str)
                    == Some(user.as_str())
        }));
        let records = all_records(&captures);
        let ledger: Vec<_> = records
            .iter()
            .filter(|(scope, _)| scope == "@deepseek-ai/dsh-session-telemetry-otel")
            .collect();
        let ops: Vec<_> = records
            .iter()
            .filter(|(scope, _)| scope.ends_with("/ops"))
            .collect();
        let start = ledger.iter().find(|(_, record)| {
            record
                .get("attributes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|attribute| {
                    attribute.get("key").and_then(Value::as_str) == Some("event.type")
                        && attribute
                            .pointer("/value/stringValue")
                            .and_then(Value::as_str)
                            == Some("turn/start")
                })
        });
        assert!(start.is_some());
        assert_eq!(start.unwrap().1.get("severityNumber"), Some(&json!(9)));
        let expected_nanos = (session.events()[0].time as u128 * 1_000_000).to_string();
        assert_eq!(
            start.unwrap().1.get("timeUnixNano").and_then(Value::as_str),
            Some(expected_nanos.as_str())
        );
        assert!(start
            .unwrap()
            .1
            .get("attributes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|attribute| {
                attribute.get("key").and_then(Value::as_str) == Some("session.cwd")
                    && attribute
                        .pointer("/value/stringValue")
                        .and_then(Value::as_str)
                        == Some("/tmp/w")
            }));
        let end = ledger.iter().find(|(_, record)| {
            record
                .get("attributes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|attribute| {
                    attribute.get("key").and_then(Value::as_str) == Some("event.type")
                        && attribute
                            .pointer("/value/stringValue")
                            .and_then(Value::as_str)
                            == Some("turn/end")
                })
        });
        assert_eq!(end.unwrap().1.get("severityNumber"), Some(&json!(17)));
        assert_eq!(
            end.unwrap().1.get("severityText").and_then(Value::as_str),
            Some("ERROR")
        );
        assert!(event_types(&captures).iter().any(|name| name == "manual"));
        assert_eq!(ops.len(), 1);
        assert!(ops[0]
            .1
            .get("attributes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|attribute| {
                attribute.get("key").and_then(Value::as_str) == Some("telemetry.op")
                    && attribute
                        .pointer("/value/stringValue")
                        .and_then(Value::as_str)
                        == Some("shutdown")
            }));
    }

    #[test]
    fn drains_records_enqueued_after_a_timer_export_began() {
        pin_home();
        let gate = HoldGate::new();
        let collector = MockCollector::start(Some(Arc::clone(&gate)));
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url },
                "processor": { "scheduledDelayMillis": 10 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("drain"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        gate.wait_arrived();
        let release = Arc::clone(&gate);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            release.open();
        });
        ctx.dispose();
        let captures = snapshot(&collector);
        let ops: Vec<_> = all_records(&captures)
            .into_iter()
            .filter(|(scope, _)| scope.ends_with("/ops"))
            .collect();
        assert_eq!(ops.len(), 1);
        assert!(ops[0]
            .1
            .get("attributes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|attribute| {
                attribute.get("key").and_then(Value::as_str) == Some("telemetry.op")
                    && attribute
                        .pointer("/value/stringValue")
                        .and_then(Value::as_str)
                        == Some("shutdown")
            }));
    }

    #[test]
    fn bounds_shutdown_when_in_flight_transport_never_settles() {
        pin_home();
        let gate = HoldGate::new();
        let collector = MockCollector::start(Some(Arc::clone(&gate)));
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 60_000 },
                "processor": { "scheduledDelayMillis": 10 },
                "shutdownTimeoutMillis": 50,
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("bounded-shutdown"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        gate.wait_arrived();
        let started = Instant::now();
        ctx.dispose();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(collector.captures.lock().expect("captures").is_empty());
        gate.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if collector.captures.lock().expect("captures").len() >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(collector.captures.lock().expect("captures").len() >= 2);
    }

    #[test]
    fn passes_gzip_compression_through_to_the_exporter() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "compression": "gzip" },
                "processor": { "scheduledDelayMillis": 20, "maxExportBatchSize": 8 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("gzip"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.dispose();
        let captures = snapshot(&collector);
        assert!(!captures.is_empty());
        assert_eq!(
            captures[0]
                .headers
                .get("content-encoding")
                .map(String::as_str),
            Some("gzip")
        );
        assert!(event_types(&captures)
            .iter()
            .any(|name| name == "turn/start"));
    }

    #[test]
    fn maps_warn_severity_from_record_policy() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = boot(&collector.url);
        ctx.on_waterfall(RECORD_WATERFALL, |payload, next| {
            let mut record = next.call(payload);
            if let Some(object) = record.as_object_mut() {
                object.insert("severity".into(), json!("warn"));
            }
            record
        })
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("warn"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.dispose();
        let captures = snapshot(&collector);
        let start = all_records(&captures).into_iter().find(|(_, record)| {
            record
                .get("attributes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|attribute| {
                    attribute.get("key").and_then(Value::as_str) == Some("event.type")
                        && attribute
                            .pointer("/value/stringValue")
                            .and_then(Value::as_str)
                            == Some("turn/start")
                })
        });
        assert_eq!(start.unwrap().1.get("severityNumber"), Some(&json!(13)));
    }

    #[test]
    fn feedback_only_replays_each_suffix_at_the_next_feedback_event() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FEEDBACK_ONLY",
                "exporter": { "url": collector.url },
                "processor": { "scheduledDelayMillis": 20, "maxExportBatchSize": 16 },
            })),
        )
        .unwrap();
        let telemetry = ctx.clone();
        ctx.on_waterfall(RECORD_WATERFALL, move |payload, next| {
            telemetry
                .service::<SessionTelemetry>()
                .unwrap()
                .emit(SessionTelemetryRecord {
                    channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                    time: 1,
                    severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                    attributes: {
                        let mut map = Map::new();
                        map.insert("event.type".into(), json!("direct-bypass"));
                        map
                    },
                    body: json!({ "mustStayLocal": true }),
                });
            next.call(payload)
        })
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("feedback-only"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        record_feedback(&session, "first report").unwrap();
        session
            .append(
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
                None,
            )
            .unwrap();
        record_feedback(&session, "second report").unwrap();
        session
            .append(SessionEventData::TurnStart { turn: 2 }, None)
            .unwrap();
        ctx.dispose();
        let captures = snapshot(&collector);
        let types = event_types(&captures);
        assert_eq!(
            types,
            vec![
                "turn/start".to_string(),
                "feedback/record".to_string(),
                "turn/end".to_string(),
                "feedback/record".to_string(),
            ]
        );
        let wire = serde_json::to_string(
            &captures
                .iter()
                .map(|capture| &capture.body)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(wire.contains("first report"));
        assert!(wire.contains("second report"));
        assert!(!all_records(&captures)
            .iter()
            .any(|(scope, _)| scope.ends_with("/ops")));
        assert!(!wire.contains("direct-bypass"));
    }

    #[test]
    fn ignores_direct_emits_and_non_canonical_feedback_in_feedback_only() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FEEDBACK_ONLY",
                "exporter": { "url": collector.url },
                "processor": { "scheduledDelayMillis": 20 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("no-feedback"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.service::<SessionTelemetry>()
            .unwrap()
            .emit(SessionTelemetryRecord {
                channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                time: 1,
                severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                attributes: Map::new(),
                body: json!({ "mustStayLocal": true }),
            });
        ctx.emit(
            "session/event",
            json!({
                "sessionId": session.id().as_str(),
                "event": {
                    "type": "feedback/record",
                    "seq": session.events().len(),
                    "time": 1,
                    "data": { "text": "not committed" },
                }
            }),
        );
        let warnings = ctx.service::<SessionTelemetry>().unwrap().warnings();
        ctx.dispose();
        assert!(warnings
            .iter()
            .any(|warning| warning == non_canonical_feedback_warning()));
        assert!(snapshot(&collector).is_empty());
    }

    #[test]
    fn constructs_no_disabled_transport_even_when_exporter_options_are_present() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "DISABLED",
                "exporter": { "url": collector.url },
                "processor": { "maxExportBatchSize": 0 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("disabled"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        record_feedback(&session, "local report").unwrap();
        let warnings = ctx.service::<SessionTelemetry>().unwrap().warnings();
        assert!(warnings
            .iter()
            .any(|warning| warning == disabled_feedback_warning()));
        ctx.service::<SessionTelemetry>()
            .unwrap()
            .emit(SessionTelemetryRecord {
                channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                time: 0,
                severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                attributes: Map::new(),
                body: Value::Null,
            });
        ctx.service::<SessionTelemetry>()
            .unwrap()
            .shutdown()
            .unwrap();
        ctx.dispose();
        record_feedback(&session, "after disposal").unwrap();
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| *warning == disabled_feedback_warning())
                .count(),
            1
        );
        assert!(snapshot(&collector).is_empty());
    }

    #[test]
    fn discloses_the_sharing_policy_for_every_mode() {
        pin_home();
        let collector = MockCollector::start(None);
        let full = Context::new();
        SessionStore::install(&full).unwrap();
        install(
            &full,
            Some(&json!({ "mode": "FULL", "exporter": { "url": collector.url } })),
        )
        .unwrap();
        assert_eq!(
            full.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::Full
        );
        full.dispose();

        let gated = Context::new();
        SessionStore::install(&gated).unwrap();
        install(
            &gated,
            Some(&json!({ "mode": "FEEDBACK_ONLY", "exporter": { "url": collector.url } })),
        )
        .unwrap();
        assert_eq!(
            gated.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::FeedbackOnly
        );
        gated.dispose();

        let disabled = Context::new();
        SessionStore::install(&disabled).unwrap();
        install(&disabled, Some(&json!({ "mode": "DISABLED" }))).unwrap();
        assert_eq!(
            disabled.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::Disabled
        );
        disabled.dispose();

        let defaulted = Context::new();
        SessionStore::install(&defaulted).unwrap();
        install(&defaulted, Some(&json!({}))).unwrap();
        assert_eq!(
            defaulted.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::Disabled
        );
        defaulted.dispose();
        assert!(snapshot(&collector).is_empty());
    }

    #[test]
    fn retries_a_transient_collector_rejection_inside_timeout() {
        // Retry-After: 0 keeps the retry inside the crate-test budget; without
        // the header the exporter uses jittered exponential backoff.
        pin_home();
        let collector = MockCollector::start_with(
            None,
            Some(ReplyScript {
                fail_first: 1,
                fail_status: 503,
                retry_after: Some("0"),
            }),
        );
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000 },
                "processor": { "scheduledDelayMillis": 60_000, "maxExportBatchSize": 8 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("retry-503"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.dispose();
        let captures = snapshot(&collector);
        assert!(captures.iter().any(|capture| capture.status == 503));
        let accepted: Vec<Capture> = captures
            .into_iter()
            .filter(|capture| capture.status == 200)
            .collect();
        assert!(!accepted.is_empty());
        assert!(event_types(&accepted)
            .iter()
            .any(|name| name == "turn/start"));
    }

    #[test]
    fn does_not_retry_a_permanent_collector_rejection() {
        pin_home();
        let collector = MockCollector::start_with(
            None,
            Some(ReplyScript {
                fail_first: usize::MAX,
                fail_status: 400,
                retry_after: None,
            }),
        );
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000 },
                "processor": { "scheduledDelayMillis": 60_000, "maxExportBatchSize": 8 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("reject-400"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.dispose();
        let captures = snapshot(&collector);
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].status, 400);
    }

    #[test]
    fn abandons_retry_when_retry_after_exceeds_remaining_timeout() {
        // Retry-After is compared to the remaining batch deadline
        // (`processor.exportTimeoutMillis`), not the per-attempt HTTP budget.
        pin_home();
        let collector = MockCollector::start_with(
            None,
            Some(ReplyScript {
                fail_first: usize::MAX,
                fail_status: 503,
                retry_after: Some("5"),
            }),
        );
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 10_000 },
                "processor": {
                    "scheduledDelayMillis": 60_000,
                    "maxExportBatchSize": 8,
                    "exportTimeoutMillis": 80,
                },
                "shutdownTimeoutMillis": 2_000,
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("retry-budget"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        let started = Instant::now();
        ctx.dispose();
        assert!(started.elapsed() < Duration::from_secs(1));
        let captures = snapshot(&collector);
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].status, 503);
    }

    #[test]
    fn bounds_a_held_export_at_export_timeout_millis() {
        pin_home();
        let gate = HoldGate::new();
        let collector = MockCollector::start(Some(Arc::clone(&gate)));
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 60_000 },
                "processor": { "scheduledDelayMillis": 10, "exportTimeoutMillis": 80 },
                "shutdownTimeoutMillis": 4_000,
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("export-timeout"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        gate.wait_arrived();
        let started = Instant::now();
        ctx.dispose();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(collector.captures.lock().expect("captures").is_empty());
        gate.open();
    }

    #[test]
    fn session_flush_does_not_export_before_shutdown() {
        pin_home();
        let collector = MockCollector::start(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000 },
                "processor": { "scheduledDelayMillis": 60_000, "maxExportBatchSize": 8 },
            })),
        )
        .unwrap();
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id("no-flush"));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
        ctx.emit("session/flush", json!({ "id": session.id().as_str() }));
        thread::sleep(Duration::from_millis(40));
        assert!(collector.captures.lock().expect("captures").is_empty());
        ctx.dispose();
        assert!(event_types(&snapshot(&collector))
            .iter()
            .any(|name| name == "turn/start"));
    }

    #[test]
    fn drops_the_newest_record_when_the_queue_is_full() {
        pin_home();
        let gate = HoldGate::new();
        let collector = MockCollector::start(Some(Arc::clone(&gate)));
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000 },
                "processor": {
                    "scheduledDelayMillis": 10,
                    "maxExportBatchSize": 1,
                    "maxQueueSize": 1,
                },
            })),
        )
        .unwrap();
        let telemetry = ctx.service::<SessionTelemetry>().unwrap();
        let emit = |name: &str| {
            telemetry.emit(SessionTelemetryRecord {
                channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                time: 1,
                severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                attributes: {
                    let mut map = Map::new();
                    map.insert("event.type".into(), json!(name));
                    map
                },
                body: Value::Null,
            });
        };
        emit("first");
        gate.wait_arrived();
        emit("second");
        emit("dropped");
        gate.open();
        ctx.dispose();
        let types = event_types(&snapshot(&collector));
        assert!(types.iter().any(|name| name == "first"));
        assert!(types.iter().any(|name| name == "second"));
        assert!(!types.iter().any(|name| name == "dropped"));
    }

    fn emit_turn(ctx: &Context, id: &str) {
        let session = ctx
            .service::<SessionStore>()
            .unwrap()
            .create(session_id(id));
        session
            .append(SessionEventData::TurnStart { turn: 1 }, None)
            .unwrap();
    }

    fn emit_named(ctx: &Context, name: &str) {
        ctx.service::<SessionTelemetry>()
            .unwrap()
            .emit(SessionTelemetryRecord {
                channel: dsh_session_telemetry::SessionTelemetryChannel::Ledger,
                time: 1,
                severity: dsh_session_telemetry::SessionTelemetrySeverity::Info,
                attributes: {
                    let mut map = Map::new();
                    map.insert("event.type".into(), json!(name));
                    map
                },
                body: Value::Null,
            });
    }

    #[test]
    fn reuses_an_http_socket_when_keepalive_is_enabled() {
        pin_home();
        let collector = MockCollector::start_reusing(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000, "keepAlive": true },
                "processor": {
                    "scheduledDelayMillis": 60_000,
                    "maxExportBatchSize": 1,
                },
            })),
        )
        .unwrap();
        emit_named(&ctx, "ka-1");
        emit_named(&ctx, "ka-2");
        ctx.dispose();
        let captures = snapshot(&collector);
        assert_eq!(captures.len(), 2);
        assert_eq!(collector.accepts.load(Ordering::SeqCst), 1);
        assert!(captures.iter().all(|capture| capture.status == 200));
    }

    #[test]
    fn opens_a_new_http_socket_when_keepalive_is_disabled() {
        pin_home();
        let collector = MockCollector::start_reusing(None);
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 2_000, "keepAlive": false },
                "processor": {
                    "scheduledDelayMillis": 60_000,
                    "maxExportBatchSize": 1,
                },
            })),
        )
        .unwrap();
        emit_named(&ctx, "close-1");
        emit_named(&ctx, "close-2");
        ctx.dispose();
        let captures = snapshot(&collector);
        assert_eq!(captures.len(), 2);
        assert_eq!(collector.accepts.load(Ordering::SeqCst), 2);
    }

    struct HttpsRetryCollector {
        url: String,
        count_path: PathBuf,
        child: std::process::Child,
        previous_ca_bundle: Option<std::ffi::OsString>,
        previous_ssl_cert: Option<std::ffi::OsString>,
    }

    impl Drop for HttpsRetryCollector {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            match self.previous_ca_bundle.take() {
                Some(value) => std::env::set_var("CURL_CA_BUNDLE", value),
                None => std::env::remove_var("CURL_CA_BUNDLE"),
            }
            match self.previous_ssl_cert.take() {
                Some(value) => std::env::set_var("SSL_CERT_FILE", value),
                None => std::env::remove_var("SSL_CERT_FILE"),
            }
        }
    }

    impl HttpsRetryCollector {
        fn request_count(&self) -> usize {
            std::fs::read_to_string(&self.count_path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0)
        }
    }

    #[test]
    fn honors_https_retry_after_from_curl_response_headers() {
        pin_home();
        let Some(collector) = start_https_retry_after_collector() else {
            return;
        };
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        install(
            &ctx,
            Some(&json!({
                "mode": "FULL",
                "exporter": { "url": collector.url, "timeoutMillis": 4_000, "keepAlive": true },
                "processor": {
                    "scheduledDelayMillis": 60_000,
                    "maxExportBatchSize": 8,
                },
                "shutdownTimeoutMillis": 6_000,
            })),
        )
        .unwrap();
        emit_turn(&ctx, "https-retry");
        ctx.dispose();
        assert!(
            collector.request_count() >= 2,
            "HTTPS Retry-After: 0 should retry the 503"
        );
    }

    fn start_https_retry_after_collector() -> Option<HttpsRetryCollector> {
        let openssl = Command::new("openssl").arg("version").output().ok()?;
        if !openssl.status.success() {
            return None;
        }
        let node = Command::new("node").arg("-v").output().ok()?;
        if !node.status.success() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "dsh-otel-https-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let port_path = dir.join("port");
        let count_path = dir.join("count");
        let generated = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-keyout",
                key.to_str()?,
                "-out",
                cert.to_str()?,
                "-days",
                "1",
                "-nodes",
                "-subj",
                "/CN=127.0.0.1",
            ])
            .output()
            .ok()?;
        if !generated.status.success() {
            return None;
        }
        let script = dir.join("server.js");
        std::fs::write(
            &script,
            r#"
const https = require('https');
const fs = require('fs');
let n = 0;
const server = https.createServer({
  key: fs.readFileSync(process.argv[1]),
  cert: fs.readFileSync(process.argv[2]),
}, (req, res) => {
  const chunks = [];
  req.on('data', (chunk) => chunks.push(chunk));
  req.on('end', () => {
    n += 1;
    fs.writeFileSync(process.argv[3], String(n));
    if (n === 1) {
      res.writeHead(503, {
        'Retry-After': '0',
        'Content-Type': 'application/json',
        'Content-Length': 2,
      });
      res.end('{}');
      return;
    }
    res.writeHead(200, {
      'Content-Type': 'application/json',
      'Content-Length': 2,
    });
    res.end('{}');
  });
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync(process.argv[4], String(server.address().port));
});
"#,
        )
        .ok()?;
        let child = Command::new("node")
            .arg(&script)
            .arg(&key)
            .arg(&cert)
            .arg(&count_path)
            .arg(&port_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let started = Instant::now();
        let mut port = String::new();
        while started.elapsed() < Duration::from_secs(3) {
            if let Ok(text) = std::fs::read_to_string(&port_path) {
                if !text.trim().is_empty() {
                    port = text.trim().to_string();
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        if port.is_empty() {
            return None;
        }
        Some(HttpsRetryCollector {
            url: format!("https://127.0.0.1:{port}/v1/logs"),
            count_path,
            child,
            previous_ca_bundle: {
                let previous = std::env::var_os("CURL_CA_BUNDLE");
                std::env::set_var("CURL_CA_BUNDLE", &cert);
                previous
            },
            previous_ssl_cert: {
                let previous = std::env::var_os("SSL_CERT_FILE");
                std::env::set_var("SSL_CERT_FILE", &cert);
                previous
            },
        })
    }

    #[test]
    fn config_fails_loud() {
        pin_home();
        let cases: Vec<(Value, &str)> = vec![
            (json!({ "mode": "FULL" }), "exporter.url is required"),
            (
                json!({ "mode": "FULL", "exporter": { "url": "" } }),
                "exporter.url is required",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "not a url" } }),
                "not a valid URL",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "ftp://collector" } }),
                "must be http(s)",
            ),
            (
                json!({ "mode": "FEEDBACK_ONLY" }),
                "exporter.url is required",
            ),
            (json!({ "mode": "INVALID" }), "INVALID"),
            (
                json!({ "mode": "FULL", "exporter": { "url": "http://c/v1/logs" }, "processor": { "maxExportBatchSize": 0 } }),
                "maxExportBatchSize",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "http://c/v1/logs" }, "processor": { "maxExportBatchSize": 0.5 } }),
                "maxExportBatchSize",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "http://c/v1/logs" }, "shutdownTimeoutMillis": 0 }),
                "shutdownTimeoutMillis",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "http://c/v1/logs" }, "processor": { "exportTimeoutMillis": 0 } }),
                "exportTimeoutMillis",
            ),
            (
                json!({ "mode": "FULL", "exporter": { "url": "http://c/v1/logs", "keepAlive": "yes" } }),
                "keepAlive",
            ),
        ];
        for (config, message) in cases {
            let ctx = Context::new();
            SessionStore::install(&ctx).unwrap();
            let err = install(&ctx, Some(&config)).unwrap_err();
            assert!(
                err.to_string().contains(message),
                "expected {message:?} in {}",
                err
            );
        }
    }

    #[test]
    fn rejects_unknown_mode_before_reading_transport_config() {
        pin_home();
        let ctx = Context::new();
        SessionStore::install(&ctx).unwrap();
        let err = install(
            &ctx,
            Some(&json!({ "mode": "INVALID", "exporter": { "url": "http://c/v1/logs" } })),
        )
        .unwrap_err();
        assert!(err.to_string().contains("INVALID"));
    }
}
