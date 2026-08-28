//! OTLP/HTTP JSON log pipeline: bounded queue, batch worker, synchronous shutdown.

use dsh_llm::APP_IDENTITY;
use dsh_session_telemetry::{
    SessionTelemetryChannel, SessionTelemetryRecord, SessionTelemetrySeverity, SessionTelemetrySink,
};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LEDGER_SCOPE: &str = "@deepseek-ai/dsh-session-telemetry-otel";
const OPS_SCOPE: &str = "@deepseek-ai/dsh-session-telemetry-otel/ops";

enum Msg {
    Record(SessionTelemetryRecord),
    Shutdown,
}

/// Batching OTLP/HTTP JSON exporter. `emit` is a non-blocking enqueue.
pub struct OtlpPipeline {
    tx: mpsc::Sender<Msg>,
    queued: Arc<AtomicUsize>,
    max_queue: usize,
    shutdown_timeout: Duration,
    done: Arc<AtomicBool>,
    shutdown_started: AtomicBool,
}

/// Transport and batch knobs resolved at plugin load.
pub struct PipelineSpec {
    /// Full logs endpoint (`http:` or `https:`).
    pub url: String,
    /// Extra exporter headers, forwarded verbatim.
    pub headers: Vec<(String, String)>,
    /// Per-attempt socket timeout.
    pub timeout: Duration,
    /// Whether the body is `gzip` compressed.
    pub gzip: bool,
    /// Batch timer.
    pub scheduled_delay: Duration,
    /// Records per export POST.
    pub max_export_batch_size: usize,
    /// In-flight queue cap; additional `emit`s drop.
    pub max_queue_size: usize,
    /// Outer bound for [`OtlpPipeline::shutdown`].
    pub shutdown_timeout: Duration,
}

impl OtlpPipeline {
    /// Spawn the export worker.
    pub fn start(spec: PipelineSpec) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let last_error = Arc::new(Mutex::new(None));
        let resource = resource_attributes();
        let scope_version = env!("CARGO_PKG_VERSION").to_string();
        let url = spec.url;
        let headers = spec.headers;
        let timeout = spec.timeout;
        let gzip = spec.gzip;
        let delay = spec.scheduled_delay;
        let max_batch = spec.max_export_batch_size;
        let worker_done = Arc::clone(&done);
        let worker_error = Arc::clone(&last_error);
        let worker_queued = Arc::clone(&queued);
        thread::Builder::new()
            .name("dsh-otel-export".into())
            .spawn(move || {
                worker_loop(
                    rx,
                    WorkerOpts {
                        url,
                        headers,
                        timeout,
                        gzip,
                        delay,
                        max_batch,
                        resource,
                        scope_version,
                    },
                    worker_done,
                    worker_error,
                    worker_queued,
                );
            })
            .expect("otel export worker");
        Arc::new(Self {
            tx,
            queued,
            max_queue: spec.max_queue_size,
            shutdown_timeout: spec.shutdown_timeout,
            done,
            shutdown_started: AtomicBool::new(false),
        })
    }

    fn wait_done(&self, timeout: Duration) -> std::result::Result<(), String> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.done.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        if self.done.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(format!(
                "session-telemetry-otel: provider shutdown exceeded {}ms",
                timeout.as_millis()
            ))
        }
    }
}

impl SessionTelemetrySink for OtlpPipeline {
    fn emit(&self, record: SessionTelemetryRecord) {
        if self.done.load(Ordering::SeqCst) {
            return;
        }
        if self.queued.load(Ordering::SeqCst) >= self.max_queue {
            return;
        }
        self.queued.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(Msg::Record(record)).is_err() {
            self.queued.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn shutdown(&self) -> std::result::Result<(), String> {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return self.wait_done(self.shutdown_timeout);
        }
        let _ = self.tx.send(Msg::Shutdown);
        self.wait_done(self.shutdown_timeout)
    }
}

struct WorkerOpts {
    url: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    gzip: bool,
    delay: Duration,
    max_batch: usize,
    resource: Vec<Value>,
    scope_version: String,
}

fn worker_loop(
    rx: mpsc::Receiver<Msg>,
    opts: WorkerOpts,
    done: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    queued: Arc<AtomicUsize>,
) {
    let mut batch: Vec<SessionTelemetryRecord> = Vec::new();
    loop {
        match rx.recv_timeout(opts.delay) {
            Ok(Msg::Record(record)) => {
                queued.fetch_sub(1, Ordering::SeqCst);
                batch.push(record);
                if batch.len() >= opts.max_batch {
                    export_batch(&opts, &mut batch, &last_error);
                }
            }
            Ok(Msg::Shutdown) => {
                while let Ok(msg) = rx.try_recv() {
                    if let Msg::Record(record) = msg {
                        queued.fetch_sub(1, Ordering::SeqCst);
                        batch.push(record);
                    }
                }
                while !batch.is_empty() {
                    export_batch(&opts, &mut batch, &last_error);
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    export_batch(&opts, &mut batch, &last_error);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    export_batch(&opts, &mut batch, &last_error);
                }
                break;
            }
        }
    }
    done.store(true, Ordering::SeqCst);
}

fn export_batch(
    opts: &WorkerOpts,
    batch: &mut Vec<SessionTelemetryRecord>,
    last_error: &Mutex<Option<String>>,
) {
    if batch.is_empty() {
        return;
    }
    let take = opts.max_batch.min(batch.len());
    let records: Vec<SessionTelemetryRecord> = batch.drain(..take).collect();
    let body = encode_resource_logs(&opts.resource, &opts.scope_version, &records);
    let payload = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    if let Err(error) = post_otlp(&opts.url, &opts.headers, &payload, opts.timeout, opts.gzip) {
        eprintln!("session-telemetry-otel: export failed: {error}");
        *last_error.lock().expect("otel error") = Some(error);
    }
}

fn resource_attributes() -> Vec<Value> {
    vec![
        kv_string("service.name", APP_IDENTITY.product),
        kv_string("service.version", APP_IDENTITY.version),
        kv_string("user.id", &dsh_anonymous_user_id::get_or_create_anonymous_user_id()),
    ]
}

fn encode_resource_logs(
    resource: &[Value],
    scope_version: &str,
    records: &[SessionTelemetryRecord],
) -> Value {
    let mut ledger = Vec::new();
    let mut ops = Vec::new();
    for record in records {
        let encoded = encode_log_record(record);
        match record.channel {
            SessionTelemetryChannel::Ledger => ledger.push(encoded),
            SessionTelemetryChannel::Ops => ops.push(encoded),
        }
    }
    let mut scope_logs = Vec::new();
    if !ledger.is_empty() {
        scope_logs.push(json!({
            "scope": { "name": LEDGER_SCOPE, "version": scope_version },
            "logRecords": ledger,
        }));
    }
    if !ops.is_empty() {
        scope_logs.push(json!({
            "scope": { "name": OPS_SCOPE, "version": scope_version },
            "logRecords": ops,
        }));
    }
    json!({
        "resourceLogs": [{
            "resource": { "attributes": resource },
            "scopeLogs": scope_logs,
        }]
    })
}

fn encode_log_record(record: &SessionTelemetryRecord) -> Value {
    let (severity_number, severity_text) = match record.severity {
        SessionTelemetrySeverity::Info => (9, "INFO"),
        SessionTelemetrySeverity::Warn => (13, "WARN"),
        SessionTelemetrySeverity::Error => (17, "ERROR"),
    };
    let nanos = (record.time as u128).saturating_mul(1_000_000).to_string();
    let mut encoded = json!({
        "timeUnixNano": nanos,
        "observedTimeUnixNano": (record.time as u128).saturating_mul(1_000_000).to_string(),
        "severityNumber": severity_number,
        "severityText": severity_text,
        "attributes": encode_attributes(&record.attributes),
    });
    if !record.body.is_null() {
        encoded["body"] = any_value(&record.body);
    }
    encoded
}

fn encode_attributes(attributes: &Map<String, Value>) -> Vec<Value> {
    attributes
        .iter()
        .map(|(key, value)| json!({ "key": key, "value": attribute_value(value) }))
        .collect()
}

fn attribute_value(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({ "stringValue": text }),
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                json!({ "intValue": int.to_string() })
            } else if let Some(uint) = number.as_u64() {
                json!({ "intValue": uint.to_string() })
            } else if let Some(float) = number.as_f64() {
                json!({ "doubleValue": float })
            } else {
                json!({ "stringValue": number.to_string() })
            }
        }
        other => json!({ "stringValue": other.to_string() }),
    }
}

fn any_value(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "stringValue": "" }),
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                json!({ "intValue": int.to_string() })
            } else if let Some(uint) = number.as_u64() {
                json!({ "intValue": uint.to_string() })
            } else if let Some(float) = number.as_f64() {
                json!({ "doubleValue": float })
            } else {
                json!({ "stringValue": number.to_string() })
            }
        }
        Value::String(text) => json!({ "stringValue": text }),
        Value::Array(items) => json!({
            "arrayValue": {
                "values": items.iter().map(any_value).collect::<Vec<_>>()
            }
        }),
        Value::Object(map) => json!({
            "kvlistValue": {
                "values": map.iter().map(|(key, entry)| json!({
                    "key": key,
                    "value": any_value(entry),
                })).collect::<Vec<_>>()
            }
        }),
    }
}

fn kv_string(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn post_otlp(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    gzip: bool,
) -> std::result::Result<(), String> {
    let payload = if gzip { gzip_bytes(body)? } else { body.to_vec() };
    let mut request_headers = Vec::from([
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Content-Length".to_string(), payload.len().to_string()),
    ]);
    if gzip {
        request_headers.push(("Content-Encoding".to_string(), "gzip".to_string()));
    }
    request_headers.extend(headers.iter().cloned());
    if url.starts_with("https://") {
        curl_post(url, &request_headers, &payload, timeout)
    } else if url.starts_with("http://") {
        http_post(url, &request_headers, &payload, timeout)
    } else {
        Err(format!("unsupported url: {url}"))
    }
}

fn gzip_bytes(body: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut child = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    {
        let mut stdin = child.stdin.take().ok_or("gzip stdin")?;
        stdin.write_all(body).map_err(|error| error.to_string())?;
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(output.stdout)
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> std::result::Result<ParsedHttpUrl, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not an http url: {url}"))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hostport, path)) => (hostport, format!("/{path}")),
        None => (rest, "/".into()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().map_err(|error| error.to_string())?,
        ),
        None => (hostport.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("not an http url: {url}"));
    }
    Ok(ParsedHttpUrl { host, port, path })
}

fn http_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> std::result::Result<(), String> {
    let parsed = parse_http_url(url)?;
    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .next()
        .ok_or_else(|| format!("unresolvable host: {}", parsed.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        parsed.path, host_header
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    stream.write_all(body).map_err(|error| error.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("");
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'));
    if ok {
        Ok(())
    } else if status_line.is_empty() {
        Err("empty HTTP response".into())
    } else {
        Err(format!("OTLP collector rejected export: {status_line}"))
    }
}

fn curl_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> std::result::Result<(), String> {
    let mut command = Command::new("curl");
    command
        .arg("-sS")
        .arg("--http1.1")
        .arg("-X")
        .arg("POST")
        .arg("-m")
        .arg(format!("{:.3}", timeout.as_secs_f64()))
        .arg("-o")
        .arg("/dev/null")
        .arg("-w")
        .arg("%{http_code}");
    for (name, value) in headers {
        command.arg("-H").arg(format!("{name}: {value}"));
    }
    command.arg("--data-binary").arg("@-").arg(url);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    {
        let mut stdin = child.stdin.take().ok_or("curl stdin")?;
        stdin.write_all(body).map_err(|error| error.to_string())?;
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let code = String::from_utf8_lossy(&output.stdout);
    if code.starts_with('2') {
        Ok(())
    } else {
        Err(format!("OTLP collector rejected export: HTTP {code}"))
    }
}
