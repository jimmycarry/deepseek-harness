//! OTLP/HTTP JSON log pipeline: bounded queue, batch worker, retryable export, synchronous shutdown.

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
use std::time::{Duration, Instant, SystemTime};

const LEDGER_SCOPE: &str = "@deepseek-ai/dsh-session-telemetry-otel";
const OPS_SCOPE: &str = "@deepseek-ai/dsh-session-telemetry-otel/ops";
/// OTLP exporter defaults from the OpenTelemetry protocol exporter specification.
const EXPORT_MAX_ATTEMPTS: u32 = 5;
const EXPORT_INITIAL_BACKOFF: Duration = Duration::from_millis(1_000);
const EXPORT_MAX_BACKOFF: Duration = Duration::from_millis(5_000);
const EXPORT_BACKOFF_MULTIPLIER: f64 = 1.5;

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
    /// Per-attempt socket timeout (`exporter.timeoutMillis`).
    pub timeout: Duration,
    /// Per-batch export deadline, including retries (`processor.exportTimeoutMillis`).
    pub export_timeout: Duration,
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
    /// SDK `exporter.keepAlive` (default true). HTTP/1.1 reuses one socket
    /// per worker when the collector also keeps the connection. HTTPS still
    /// uses one-shot `curl`; `false` passes `--no-keepalive`.
    pub keep_alive: bool,
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
        let export_timeout = spec.export_timeout;
        let gzip = spec.gzip;
        let delay = spec.scheduled_delay;
        let max_batch = spec.max_export_batch_size;
        let keep_alive = spec.keep_alive;
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
                        export_timeout,
                        gzip,
                        delay,
                        max_batch,
                        keep_alive,
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
    export_timeout: Duration,
    gzip: bool,
    delay: Duration,
    max_batch: usize,
    keep_alive: bool,
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
    let mut transport = ExportTransport {
        keep_alive: opts.keep_alive,
        http: None,
    };
    loop {
        match rx.recv_timeout(opts.delay) {
            Ok(Msg::Record(record)) => {
                queued.fetch_sub(1, Ordering::SeqCst);
                batch.push(record);
                if batch.len() >= opts.max_batch {
                    export_batch(&opts, &mut batch, &last_error, &mut transport);
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
                    export_batch(&opts, &mut batch, &last_error, &mut transport);
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    export_batch(&opts, &mut batch, &last_error, &mut transport);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    export_batch(&opts, &mut batch, &last_error, &mut transport);
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
    transport: &mut ExportTransport,
) {
    if batch.is_empty() {
        return;
    }
    let take = opts.max_batch.min(batch.len());
    let records: Vec<SessionTelemetryRecord> = batch.drain(..take).collect();
    let body = encode_resource_logs(&opts.resource, &opts.scope_version, &records);
    let payload = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    if let Err(error) = post_otlp(
        &opts.url,
        &opts.headers,
        &payload,
        opts.timeout,
        opts.export_timeout,
        opts.gzip,
        transport,
    ) {
        eprintln!("session-telemetry-otel: export failed: {error}");
        *last_error.lock().expect("otel error") = Some(error);
    }
}

struct ExportAttemptError {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl ExportAttemptError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            retry_after: None,
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn parse_http_status_line(status_line: &str) -> Option<u16> {
    status_line.split_whitespace().nth(1)?.parse().ok()
}

fn parse_retry_after_header(response: &str) -> Option<Duration> {
    let header_block = response.split("\r\n\r\n").next().unwrap_or(response);
    for line in header_block.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("retry-after") {
            return parse_retry_after_value(value.trim());
        }
    }
    None
}

fn parse_retry_after_value(value: &str) -> Option<Duration> {
    value.parse::<u64>().ok().map(Duration::from_secs)
}

fn jitter_delay(backoff: Duration) -> Duration {
    let max_ms = backoff.as_millis();
    if max_ms == 0 {
        return Duration::ZERO;
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let pick = (now.as_nanos() % max_ms) as u64;
    Duration::from_millis(pick)
}

fn next_backoff(backoff: Duration) -> Duration {
    let grown = (backoff.as_secs_f64() * EXPORT_BACKOFF_MULTIPLIER) * 1_000.0;
    Duration::from_millis(grown.min(EXPORT_MAX_BACKOFF.as_millis() as f64) as u64)
}

fn resource_attributes() -> Vec<Value> {
    vec![
        kv_string("service.name", APP_IDENTITY.product),
        kv_string("service.version", APP_IDENTITY.version),
        kv_string(
            "user.id",
            &dsh_anonymous_user_id::get_or_create_anonymous_user_id(),
        ),
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
    attempt_timeout: Duration,
    export_timeout: Duration,
    gzip: bool,
    transport: &mut ExportTransport,
) -> std::result::Result<(), String> {
    let payload = if gzip {
        gzip_bytes(body)?
    } else {
        body.to_vec()
    };
    let mut request_headers = Vec::from([
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Content-Length".to_string(), payload.len().to_string()),
    ]);
    if gzip {
        request_headers.push(("Content-Encoding".to_string(), "gzip".to_string()));
    }
    request_headers.extend(headers.iter().cloned());
    post_otlp_with_retry(
        url,
        &request_headers,
        &payload,
        attempt_timeout,
        export_timeout,
        transport,
    )
}

fn post_otlp_with_retry(
    url: &str,
    headers: &[(String, String)],
    payload: &[u8],
    attempt_timeout: Duration,
    export_timeout: Duration,
    transport: &mut ExportTransport,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + export_timeout;
    let mut backoff = EXPORT_INITIAL_BACKOFF;
    let mut last_error = String::from("export timeout");
    for attempt in 0..EXPORT_MAX_ATTEMPTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_error);
        }
        let attempt_budget = remaining.min(attempt_timeout);
        match send_otlp(url, headers, payload, attempt_budget, transport) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = error.message;
                if !error.retryable || attempt + 1 == EXPORT_MAX_ATTEMPTS {
                    return Err(last_error);
                }
                let delay = error.retry_after.unwrap_or_else(|| jitter_delay(backoff));
                let remaining = deadline.saturating_duration_since(Instant::now());
                if delay >= remaining {
                    return Err(last_error);
                }
                thread::sleep(delay);
                backoff = next_backoff(backoff);
            }
        }
    }
    Err(last_error)
}

fn send_otlp(
    url: &str,
    headers: &[(String, String)],
    payload: &[u8],
    timeout: Duration,
    transport: &mut ExportTransport,
) -> std::result::Result<(), ExportAttemptError> {
    if url.starts_with("https://") {
        curl_post(url, headers, payload, timeout, transport.keep_alive)
    } else if url.starts_with("http://") {
        http_post(url, headers, payload, timeout, transport)
    } else {
        Err(ExportAttemptError::fatal(format!("unsupported url: {url}")))
    }
}

struct PooledHttp {
    key: String,
    stream: TcpStream,
}

struct ExportTransport {
    keep_alive: bool,
    http: Option<PooledHttp>,
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
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
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

fn connect_http(
    parsed: &ParsedHttpUrl,
    timeout: Duration,
) -> std::result::Result<TcpStream, ExportAttemptError> {
    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?
        .next()
        .ok_or_else(|| ExportAttemptError::fatal(format!("unresolvable host: {}", parsed.host)))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    Ok(stream)
}

fn write_http_request(
    stream: &mut TcpStream,
    parsed: &ParsedHttpUrl,
    headers: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) -> std::result::Result<(), ExportAttemptError> {
    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nConnection: {connection}\r\n",
        parsed.path, host_header
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    stream
        .write_all(body)
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))
}

fn read_http_response(
    stream: &mut TcpStream,
) -> std::result::Result<(u16, String, bool), ExportAttemptError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
        buf.push(byte[0]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(ExportAttemptError::retryable("HTTP headers too large"));
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("").to_string();
    if status_line.is_empty() {
        return Err(ExportAttemptError::retryable("empty HTTP response"));
    }
    let Some(status) = parse_http_status_line(&status_line) else {
        return Err(ExportAttemptError::retryable(format!(
            "OTLP collector rejected export: {status_line}"
        )));
    };
    let mut connection_close = false;
    let mut length = None;
    for line in text.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        }
        if name.eq_ignore_ascii_case("connection") && value.trim().eq_ignore_ascii_case("close") {
            connection_close = true;
        }
    }
    if let Some(length) = length {
        let mut body = vec![0u8; length];
        if length > 0 {
            stream
                .read_exact(&mut body)
                .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
        }
    } else {
        let mut rest = Vec::new();
        stream
            .read_to_end(&mut rest)
            .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
        connection_close = true;
    }
    Ok((status, text.into_owned(), connection_close))
}

fn http_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    transport: &mut ExportTransport,
) -> std::result::Result<(), ExportAttemptError> {
    let parsed = parse_http_url(url).map_err(ExportAttemptError::fatal)?;
    let key = format!("{}:{}", parsed.host, parsed.port);
    let mut reused = false;
    let mut stream = if transport.keep_alive {
        match transport.http.take() {
            Some(pooled) if pooled.key == key => {
                reused = true;
                pooled.stream
            }
            _ => connect_http(&parsed, timeout)?,
        }
    } else {
        connect_http(&parsed, timeout)?
    };
    match write_http_request(&mut stream, &parsed, headers, body, transport.keep_alive) {
        Ok(()) => {}
        Err(_) if reused => {
            stream = connect_http(&parsed, timeout)?;
            write_http_request(&mut stream, &parsed, headers, body, transport.keep_alive)?;
        }
        Err(error) => return Err(error),
    }
    let (status, header_text, server_close) = read_http_response(&mut stream)?;
    if transport.keep_alive && !server_close {
        transport.http = Some(PooledHttp { key, stream });
    }
    if (200..300).contains(&status) {
        return Ok(());
    }
    let status_line = header_text.lines().next().unwrap_or("");
    Err(ExportAttemptError {
        message: format!("OTLP collector rejected export: {status_line}"),
        retryable: is_retryable_status(status),
        retry_after: parse_retry_after_header(&header_text),
    })
}

fn curl_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    keep_alive: bool,
) -> std::result::Result<(), ExportAttemptError> {
    let mut command = Command::new("curl");
    command
        .arg("-sS")
        .arg("--http1.1")
        .arg("-X")
        .arg("POST")
        .arg("-m")
        .arg(format!("{:.3}", timeout.as_secs_f64()))
        .arg("-D")
        .arg("-")
        .arg("-o")
        .arg("/dev/null")
        .arg("-w")
        .arg("\n%{http_code}");
    if !keep_alive {
        command.arg("--no-keepalive");
    }
    for (name, value) in headers {
        command.arg("-H").arg(format!("{name}: {value}"));
    }
    command.arg("--data-binary").arg("@-").arg(url);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExportAttemptError::retryable("curl stdin"))?;
        stdin
            .write_all(body)
            .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| ExportAttemptError::retryable(error.to_string()))?;
    if !output.status.success() {
        return Err(ExportAttemptError::retryable(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (headers, code) = match stdout.rsplit_once('\n') {
        Some((headers, code)) => (headers, code.trim()),
        None => ("", stdout.trim()),
    };
    let status = code.parse::<u16>().ok();
    if status.is_some_and(|value| (200..300).contains(&value)) {
        return Ok(());
    }
    let retryable = status.is_some_and(is_retryable_status);
    let error = format!("OTLP collector rejected export: HTTP {code}");
    if retryable {
        Err(ExportAttemptError {
            message: error,
            retryable: true,
            retry_after: parse_retry_after_header(headers),
        })
    } else {
        Err(ExportAttemptError::fatal(error))
    }
}

#[cfg(test)]
mod retry_classification_tests {
    use super::*;

    #[test]
    fn classifies_otlp_retryable_http_statuses() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(500));
        assert!(!is_retryable_status(200));
    }

    #[test]
    fn honors_retry_after_delta_seconds_and_ignores_http_dates() {
        assert_eq!(
            parse_retry_after_header(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\n\r\n{}"
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after_header("HTTP/1.1 429 Too Many Requests\r\nretry-after: 2\r\n\r\n{}"),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            parse_retry_after_header(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: Wed, 21 Oct 2015 07:28:00 GMT\r\n\r\n{}"
            ),
            None
        );
    }

    #[test]
    fn grows_backoff_up_to_the_otlp_cap() {
        assert_eq!(
            next_backoff(Duration::from_millis(1_000)),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            next_backoff(Duration::from_millis(4_000)),
            Duration::from_millis(5_000)
        );
    }
}
