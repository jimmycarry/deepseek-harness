//! JSON-RPC MCP transports: stdio Content-Length and Streamable HTTP via curl.

use crate::Config;
use dsh_subprocess::scrubbed_parent_env;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

const PROTOCOL_VERSION: &str = "2025-03-26";

/// One connected MCP generation.
pub struct McpSession {
    transport: Mutex<Transport>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    list_changed: Arc<Notify>,
}

enum Transport {
    Stdio {
        child: Child,
        stdin: ChildStdin,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        session_id: Option<String>,
    },
}

impl McpSession {
    /// Connect, initialize, and notify `initialized`.
    pub async fn connect(config: &Config) -> Result<Arc<Self>, String> {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let list_changed = Arc::new(Notify::new());
        let transport = match &config.transport {
            crate::TransportKind::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut child_env = scrubbed_parent_env();
                child_env.extend(env.clone());
                let mut cmd = Command::new(command);
                cmd.args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .env_clear();
                for (key, value) in &child_env {
                    cmd.env(key, value);
                }
                if !cwd.is_empty() {
                    cmd.current_dir(cwd);
                }
                let mut child = cmd
                    .spawn()
                    .map_err(|error| format!("failed to spawn MCP server: {error}"))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| "MCP server stdin is unavailable".to_string())?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| "MCP server stdout is unavailable".to_string())?;
                let session = Arc::new(Self {
                    transport: Mutex::new(Transport::Stdio { child, stdin }),
                    next_id: AtomicU64::new(1),
                    pending: Arc::clone(&pending),
                    list_changed: Arc::clone(&list_changed),
                });
                std::thread::spawn({
                    let pending = Arc::clone(&pending);
                    let list_changed = Arc::clone(&list_changed);
                    move || read_stdio(stdout, pending, list_changed)
                });
                session.initialize().await?;
                return Ok(session);
            }
            crate::TransportKind::StreamableHttp { url, headers } => Transport::Http {
                url: url.clone(),
                headers: headers.clone().into_iter().collect(),
                session_id: None,
            },
        };
        let session = Arc::new(Self {
            transport: Mutex::new(transport),
            next_id: AtomicU64::new(1),
            pending,
            list_changed,
        });
        session.initialize().await?;
        Ok(session)
    }

    /// Wait until the server announces a tool-list change.
    pub fn list_changed(&self) -> Arc<Notify> {
        Arc::clone(&self.list_changed)
    }

    async fn initialize(self: &Arc<Self>) -> Result<(), String> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "dsh-mcp-client", "version": "0.0.1" },
                }),
                None,
            )
            .await?;
        if result.get("error").is_some() {
            return Err(format!("MCP initialize failed: {result}"));
        }
        self.notify("notifications/initialized", json!({})).await
    }

    /// Paginated `tools/list`.
    pub async fn list_tools(&self, cursor: Option<String>) -> Result<Value, String> {
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let response = self.request("tools/list", params, None).await?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("tools/list missing result: {response}"))
    }

    /// `tools/call` with the raw MCP name.
    pub async fn call_tool(
        &self,
        raw_name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let response = self
            .request(
                "tools/call",
                json!({ "name": raw_name, "arguments": arguments }),
                Some(timeout),
            )
            .await?;
        if let Some(error) = response.get("error") {
            return Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(&error.to_string())
                .to_string());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("tools/call missing result: {response}"))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write(&message)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending").insert(id, tx);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write(&message) {
            self.pending.lock().expect("pending").remove(&id);
            return Err(error);
        }
        if matches!(
            *self.transport.lock().expect("transport"),
            Transport::Http { .. }
        ) {
            // HTTP writes are request/response; the write path already stored the body.
            if let Some(value) = self.take_http_response(id) {
                return Ok(value);
            }
        }
        let recv = async { rx.await.map_err(|_| "MCP request cancelled".to_string()) };
        match timeout {
            Some(limit) => tokio::time::timeout(limit, recv)
                .await
                .map_err(|_| format!("MCP request timed out after {}ms", limit.as_millis()))?,
            None => recv.await,
        }
    }

    fn take_http_response(&self, id: u64) -> Option<Value> {
        // HTTP is handled inside write(); leftover pending entries stay empty.
        let _ = id;
        None
    }

    fn write(&self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        let mut transport = self.transport.lock().expect("transport");
        match &mut *transport {
            Transport::Stdio { stdin, .. } => {
                write!(stdin, "Content-Length: {}\r\n\r\n", body.len())
                    .and_then(|_| stdin.write_all(&body))
                    .and_then(|_| stdin.flush())
                    .map_err(|error| error.to_string())
            }
            Transport::Http {
                url,
                headers,
                session_id,
            } => {
                let response = http_post(url, headers, session_id.as_deref(), &body)?;
                if let Some(next) = response.session_id {
                    *session_id = Some(next);
                }
                dispatch_message(&response.body, &self.pending, &self.list_changed);
                Ok(())
            }
        }
    }

    /// Close the generation.
    pub async fn close(&self) -> Result<(), String> {
        let mut transport = self.transport.lock().expect("transport");
        match &mut *transport {
            Transport::Stdio { child, .. } => {
                let _ = child.kill();
                let _ = child.wait();
                Ok(())
            }
            Transport::Http { .. } => Ok(()),
        }
    }
}

struct HttpResponse {
    body: Value,
    session_id: Option<String>,
}

fn http_post(
    url: &str,
    headers: &[(String, String)],
    session_id: Option<&str>,
    body: &[u8],
) -> Result<HttpResponse, String> {
    let mut cmd = Command::new("curl");
    cmd.arg("-sS")
        .arg("-D")
        .arg("-")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-H")
        .arg("Accept: application/json, text/event-stream")
        .arg("-H")
        .arg(format!("MCP-Protocol-Version: {PROTOCOL_VERSION}"))
        .arg("--data-binary")
        .arg("@-")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in headers {
        cmd.arg("-H").arg(format!("{key}: {value}"));
    }
    if let Some(session) = session_id {
        cmd.arg("-H").arg(format!("Mcp-Session-Id: {session}"));
    }
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("curl spawn failed: {error}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "curl stdin unavailable".to_string())?;
        stdin
            .write_all(body)
            .map_err(|error| format!("curl write failed: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("curl failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (header_text, body_text) = split_http(&text);
    let session = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("mcp-session-id") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let parsed = parse_http_body(body_text)?;
    Ok(HttpResponse {
        body: parsed,
        session_id: session,
    })
}

fn split_http(text: &str) -> (&str, &str) {
    if let Some(index) = text.find("\r\n\r\n") {
        (&text[..index], &text[index + 4..])
    } else if let Some(index) = text.find("\n\n") {
        (&text[..index], &text[index + 2..])
    } else {
        ("", text)
    }
}

fn parse_http_body(body: &str) -> Result<Value, String> {
    let trimmed = body.trim();
    if trimmed.starts_with("data:") || trimmed.contains("\ndata:") {
        let data: String = trimmed
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .filter(|line| !line.is_empty() && *line != "[DONE]")
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&data).map_err(|error| error.to_string())
    } else {
        serde_json::from_str(trimmed).map_err(|error| error.to_string())
    }
}

fn read_stdio(
    mut stdout: impl Read,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    list_changed: Arc<Notify>,
) {
    loop {
        match read_framed_message(&mut stdout) {
            Ok(Some(value)) => dispatch_message(&value, &pending, &list_changed),
            Ok(None) => return,
            Err(_) => return,
        }
    }
}

fn read_framed_message(stdout: &mut impl Read) -> Result<Option<Value>, String> {
    let mut headers = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        if stdout.read(&mut buf).map_err(|error| error.to_string())? == 0 {
            return Ok(None);
        }
        headers.push(buf[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 8_192 {
            return Err("MCP stdio headers exceeded 8KiB".into());
        }
    }
    let header_text = String::from_utf8_lossy(&headers);
    let length = header_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| "MCP stdio message missing Content-Length".to_string())?;
    let mut body = vec![0u8; length];
    stdout
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn dispatch_message(
    value: &Value,
    pending: &Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    list_changed: &Notify,
) {
    if value.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed") {
        list_changed.notify_waiters();
        return;
    }
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        if let Some(sender) = pending.lock().expect("pending").remove(&id) {
            let _ = sender.send(value.clone());
        }
    }
}
