//! Real headless composition: session-telemetry-otel through app-boot, a
//! mocked-model bash turn, and a mock OTLP/HTTP collector.

use dsh_agent::AgentRegistry;
use dsh_app_boot::{compose_profile, register_profile_plugins, shipped_bundles};
use dsh_bundle_headless::HeadlessStartup;
use dsh_command_feedback::record_feedback;
use dsh_cordis::Context;
use dsh_cordis_loader::{Entry, EntryPatch, Loader};
use dsh_llm::UserMessage;
use dsh_session_telemetry::{disabled_feedback_warning, SessionTelemetry, RECORD_WATERFALL};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const FIXTURE_SECRET: &str = "sk-e2efixture1234567890";
const FIXTURE_PLACEHOLDER: &str = "[E2E-REDACTED]";
const TASK: &str = "prove telemetry with key sk-e2efixture1234567890";

struct Capture {
    body: Value,
}

struct MockCollector {
    url: String,
    captures: Arc<Mutex<Vec<Capture>>>,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockCollector {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("collector bind");
        let addr = listener.local_addr().expect("collector addr");
        let url = format!("http://127.0.0.1:{}/v1/logs", addr.port());
        let captures = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_captures = Arc::clone(&captures);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking");
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        if let Ok((_, raw)) = read_http_request(&mut stream) {
                            let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                            thread_captures
                                .lock()
                                .expect("captures")
                                .push(Capture { body });
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                            );
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
            stop,
            addr,
            thread: Some(thread),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.captures
            .lock()
            .expect("captures")
            .iter()
            .map(|capture| capture.body.clone())
            .collect()
    }
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

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<(HashMap<String, String>, Vec<u8>)> {
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

fn replay_turns_overlay(turns: Value) -> Vec<EntryPatch> {
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(json!({ "turns": turns }));
    vec![disable, EntryPatch::insert_row(replay)]
}

fn bash_turns() -> Value {
    json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "bash",
                "arguments": "{\"command\":\"echo hello\",\"description\":\"Print hello to stdout\"}"
            }
        },
        { "text": "done" }
    ])
}

fn telemetry_overlay(mode: &str, url: &str) -> Vec<EntryPatch> {
    let mut patches = replay_turns_overlay(bash_turns());
    let mut telemetry = EntryPatch::replace("session-telemetry-otel");
    telemetry.config = Some(json!({
        "mode": mode,
        "exporter": { "url": url },
        "processor": { "scheduledDelayMillis": 20, "maxExportBatchSize": 64 },
        "shutdownTimeoutMillis": 3000
    }));
    patches.push(telemetry);
    patches
}

fn mount_profile_in(dir: &Path, task: &str, overlay: Vec<EntryPatch>) -> Context {
    std::env::set_var("DSH_HOME", dir);
    std::env::set_var("DSH_PERMISSION_MODE", "danger-full-access");
    let layers = shipped_bundles("headless").unwrap();
    let entries = compose_profile(&layers, &[], &[], &overlay).unwrap();
    let ctx = Context::new();
    ctx.provide(Arc::new(HeadlessStartup {
        task: task.into(),
        cwd: Some(dir.to_string_lossy().into_owned()),
        resume_session_id: None,
    }))
    .unwrap();
    let loader = Loader::new();
    register_profile_plugins(&loader);
    loader.mount(&ctx, &entries).unwrap();
    ctx
}

fn mount_redact(ctx: &Context) {
    ctx.on_waterfall(RECORD_WATERFALL, |payload, next| {
        let mut record = next.call(payload);
        if let Some(body) = record.get("body").cloned() {
            if let Some(object) = record.as_object_mut() {
                object.insert("body".into(), scrub_json(body));
            }
        }
        record
    })
    .unwrap();
}

fn scrub_json(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(FIXTURE_SECRET, FIXTURE_PLACEHOLDER)),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, entry)| (key, scrub_json(entry)))
                .collect(),
        ),
        other => other,
    }
}

fn temp_home(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dsh-otel-e2e-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn jsonl_content(home: &Path) -> String {
    let mut files = Vec::new();
    walk_jsonl(&home.join("sessions"), &mut files);
    assert_eq!(files.len(), 1, "{files:?}");
    std::fs::read_to_string(&files[0]).unwrap()
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn event_types(bodies: &[Value]) -> Vec<String> {
    let mut types = Vec::new();
    for body in bodies {
        let Some(logs) = body.get("resourceLogs").and_then(Value::as_array) else {
            continue;
        };
        for resource in logs {
            let Some(scopes) = resource.get("scopeLogs").and_then(Value::as_array) else {
                continue;
            };
            for scope in scopes {
                let Some(records) = scope.get("logRecords").and_then(Value::as_array) else {
                    continue;
                };
                for record in records {
                    if let Some(attributes) = record.get("attributes").and_then(Value::as_array) {
                        for attribute in attributes {
                            if attribute.get("key").and_then(Value::as_str) == Some("event.type") {
                                if let Some(name) = attribute
                                    .pointer("/value/stringValue")
                                    .and_then(Value::as_str)
                                {
                                    types.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    types
}

fn has_ops(bodies: &[Value]) -> bool {
    bodies.iter().any(|body| {
        body.get("resourceLogs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|resource| {
                resource
                    .get("scopeLogs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|scope| {
                        scope
                            .pointer("/scope/name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| name.ends_with("/ops"))
                    })
            })
    })
}

#[tokio::test]
async fn full_mode_exports_redacted_ledger_and_keeps_the_canonical_secret() {
    let home = temp_home("full");
    let collector = MockCollector::start();
    let ctx = mount_profile_in(&home, TASK, telemetry_overlay("FULL", &collector.url));
    mount_redact(&ctx);
    let _session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    ctx.dispose();
    let bodies = collector.bodies();
    assert!(!bodies.is_empty());
    let types = event_types(&bodies);
    for expected in [
        "turn/start",
        "user/message",
        "tool/call",
        "tool/result",
        "assistant/message",
        "turn/end",
    ] {
        assert!(types.iter().any(|found| found == expected), "{expected} in {types:?}");
    }
    assert!(has_ops(&bodies));
    let wire = serde_json::to_string(&bodies).unwrap();
    assert!(!wire.contains(FIXTURE_SECRET));
    assert!(wire.contains(FIXTURE_PLACEHOLDER));
    assert!(wire.contains("prove telemetry with key"));
    let log = jsonl_content(&home);
    assert!(log.contains(FIXTURE_SECRET));
    assert!(!log.contains(FIXTURE_PLACEHOLDER));
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn feedback_only_exports_the_prefix_ending_in_feedback() {
    let home = temp_home("feedback");
    let collector = MockCollector::start();
    let ctx = mount_profile_in(
        &home,
        TASK,
        telemetry_overlay("FEEDBACK_ONLY", &collector.url),
    );
    mount_redact(&ctx);
    let session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    record_feedback(session.as_ref(), "fixture feedback").unwrap();
    let agent = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .get(session.id())
        .expect("live root agent");
    dsh_agent_loop::run_followup(
        agent.as_ref(),
        UserMessage::text("post-feedback private suffix"),
    )
    .await
    .unwrap();
    ctx.dispose();
    let bodies = collector.bodies();
    let wire = serde_json::to_string(&bodies).unwrap();
    assert!(event_types(&bodies).iter().any(|name| name == "feedback/record"));
    assert!(wire.contains("fixture feedback"));
    assert!(wire.contains("prove telemetry with key"));
    assert!(!wire.contains("post-feedback private suffix"));
    let log = jsonl_content(&home);
    assert!(log.contains("post-feedback private suffix"));
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn disabled_mode_keeps_feedback_local_and_records_the_stable_warning() {
    let home = temp_home("disabled");
    let collector = MockCollector::start();
    let ctx = mount_profile_in(&home, TASK, telemetry_overlay("DISABLED", &collector.url));
    mount_redact(&ctx);
    let session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    record_feedback(session.as_ref(), "fixture feedback").unwrap();
    let warnings = ctx.service::<SessionTelemetry>().unwrap().warnings();
    ctx.dispose();
    assert!(collector.bodies().is_empty());
    let log = jsonl_content(&home);
    assert!(log.contains("fixture feedback"));
    assert!(warnings.iter().any(|warning| warning == disabled_feedback_warning()));
    let _ = std::fs::remove_dir_all(&home);
}
