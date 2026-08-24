//! Automation-only Agent Client Protocol server over newline-delimited
//! JSON-RPC. Carries prompt text, committed assistant text, and cancellation;
//! presentation and human-interaction features stay with the harness's UI
//! modules.
//!
//! The TypeScript bridge streams `session/update` while the turn runs and
//! answers `session/prompt` at quiescence. This server also answers at
//! quiescence, but reads stdin on a dedicated thread so `session/cancel` can
//! reach an in-flight prompt; every `agent_message_chunk` update still
//! precedes that prompt's response.

use dsh_agent::{Agent, AgentCancelCause, AgentHandle, AgentRegistry};
use dsh_agent_loop::run_followup;
use dsh_cordis::{Context, Service};
use dsh_llm::{ContentBlock, UserMessage};
use dsh_sdk_protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use dsh_session::{Session, SessionEventData, SessionHeader, SessionStore, TurnEndReason};
use dsh_session_persistence::PersistenceRuntime;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The single ACP protocol version this server speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// Wire-stable agent identity returned by `initialize`.
pub const AGENT_NAME: &str = "deepseek-harness-acp";
/// Agent version returned by `initialize`.
pub const AGENT_VERSION: &str = "0.0.1";

/// Map a harness turn ending to ACP's terminal reason vocabulary.
///
/// `cancelled` is reserved for explicit client cancellation and disposal; a
/// turn aborted by a hook or another owner is ordinary quiescence and reports
/// `end_turn`. Token-limit endings are not prompt-level stop reasons either.
pub fn turn_end_to_stop_reason(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::Interrupted => "cancelled",
        TurnEndReason::Completed
        | TurnEndReason::MaxTokens
        | TurnEndReason::Blocked
        | TurnEndReason::Aborted { .. }
        | TurnEndReason::Error { .. } => "end_turn",
    }
}

/// One bridge-owned session: the live agent plus its registry disposer.
struct SessionRecord {
    agent: Arc<dyn Agent>,
    handle: Option<AgentHandle>,
    inflight: bool,
    cancel_requested: Arc<AtomicBool>,
}

struct InflightGuard<'a> {
    server: &'a AcpServer,
    session_id: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.server.sessions.lock() {
            if let Some(record) = sessions.get_mut(&self.session_id) {
                record.inflight = false;
            }
        }
    }
}

/// Automation-only ACP server state (`ctx.acp`).
#[derive(Default)]
pub struct AcpServer {
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl Service for AcpServer {
    const KEY: &'static str = "acp";
}

fn invalid_params(id: Value, detail: &str) -> JsonRpcResponse {
    JsonRpcResponse::error(id, -32602, format!("Invalid params: {detail}"))
}

fn internal_error(id: Value, detail: &str) -> JsonRpcResponse {
    JsonRpcResponse::error(id, -32603, format!("Internal error: {detail}"))
}

impl AcpServer {
    /// Create a server with no sessions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount the server as `ctx.acp`.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<()> {
        ctx.provide(Arc::new(Self::new()))
    }

    /// Dispatch one incoming request, returning the `session/update`
    /// notifications produced while handling it (wire order) and the response.
    pub async fn handle_request(
        &self,
        ctx: &Context,
        request: JsonRpcRequest,
    ) -> (Vec<JsonRpcNotification>, JsonRpcResponse) {
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => (Vec::new(), initialize_response(id)),
            "authenticate" => (
                Vec::new(),
                JsonRpcResponse::result(id, serde_json::json!({})),
            ),
            "session/new" => (Vec::new(), self.new_session(ctx, id, request.params)),
            "session/prompt" => self.prompt(ctx, id, request.params).await,
            other => (
                Vec::new(),
                JsonRpcResponse::error(id, -32601, format!("\"Method not found\": {other}")),
            ),
        }
    }

    /// Handle the `session/cancel` notification. Unknown sessions are no-ops.
    /// A known session is cancelled with `{ kind: "user" }` so an in-flight
    /// prompt can settle as `cancelled` once the current step returns.
    pub fn handle_cancel(&self, params: Option<Value>) {
        let Some(session_id) = params
            .as_ref()
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let sessions = self.sessions.lock().expect("acp sessions");
        let Some(record) = sessions.get(session_id) else {
            return;
        };
        record
            .cancel_requested
            .store(true, Ordering::SeqCst);
        record.agent.cancel(AgentCancelCause {
            kind: "user".into(),
        });
    }

    /// Dispose every bridge-owned agent. Runs when the connection closes;
    /// later requests against the drained map report unknown sessions.
    pub fn quiesce(&self) {
        let mut sessions = self.sessions.lock().expect("acp sessions");
        for (_, mut record) in sessions.drain() {
            if let Some(handle) = record.handle.take() {
                handle.dispose();
            }
        }
    }

    /// Create a fresh agent+session pair for `session/new`.
    fn new_session(&self, ctx: &Context, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let params = params.unwrap_or(Value::Null);
        let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
        if !Path::new(cwd).is_absolute() {
            return invalid_params(id, &format!("cwd must be an absolute path: {cwd}"));
        }
        if params
            .get("additionalDirectories")
            .and_then(Value::as_array)
            .is_some_and(|directories| !directories.is_empty())
        {
            return invalid_params(id, "additionalDirectories is not supported");
        }
        if params
            .get("mcpServers")
            .and_then(Value::as_array)
            .is_some_and(|servers| !servers.is_empty())
        {
            return invalid_params(id, "mcpServers is not supported");
        }
        let store = match ctx.service::<SessionStore>() {
            Ok(store) => store,
            Err(error) => return internal_error(id, &error.to_string()),
        };
        let session_id = Uuid::new_v4().to_string();
        let header = SessionHeader::new(
            dsh_session::session_id(session_id.clone()),
            Some(cwd.to_string()),
        );
        let session = store.publish(Session::with_header(header));
        let handle = match ctx
            .service::<AgentRegistry>()
            .map_err(|error| error.to_string())
            .and_then(|agents| agents.create(session).map_err(|error| error.to_string()))
        {
            Ok(handle) => handle,
            Err(error) => return internal_error(id, &error),
        };
        self.sessions.lock().expect("acp sessions").insert(
            session_id.clone(),
            SessionRecord {
                agent: Arc::clone(&handle.agent),
                handle: Some(handle),
                inflight: false,
                cancel_requested: Arc::new(AtomicBool::new(false)),
            },
        );
        JsonRpcResponse::result(id, serde_json::json!({ "sessionId": session_id }))
    }

    /// Admit one text prompt, drive the turn, and emit committed assistant
    /// text as `agent_message_chunk` updates before the stop-reason response.
    async fn prompt(
        &self,
        ctx: &Context,
        id: Value,
        params: Option<Value>,
    ) -> (Vec<JsonRpcNotification>, JsonRpcResponse) {
        let params = params.unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut content = Vec::new();
        for block in &blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => content.push(ContentBlock::text(
                    block.get("text").and_then(Value::as_str).unwrap_or(""),
                )),
                Some("image") => {
                    return (
                        Vec::new(),
                        invalid_params(
                            id,
                            "inline image prompts were not advertised by this connection",
                        ),
                    )
                }
                Some("audio") => {
                    return (
                        Vec::new(),
                        invalid_params(id, "audio prompt content is not supported"),
                    )
                }
                Some("resource") => {
                    return (
                        Vec::new(),
                        invalid_params(id, "embedded resource prompt content is not supported"),
                    )
                }
                _ => {
                    return (
                        Vec::new(),
                        invalid_params(id, "unsupported ACP prompt content"),
                    )
                }
            }
        }
        let (agent, cancel_requested) = {
            let mut sessions = self.sessions.lock().expect("acp sessions");
            match sessions.get_mut(&session_id) {
                None => {
                    return (
                        Vec::new(),
                        invalid_params(id, &format!("unknown session: {session_id}")),
                    )
                }
                Some(record) if record.inflight => {
                    return (
                        Vec::new(),
                        invalid_params(id, "a prompt is already in flight for this session"),
                    )
                }
                Some(record) => {
                    record.inflight = true;
                    record.cancel_requested.store(false, Ordering::SeqCst);
                    (
                        Arc::clone(&record.agent),
                        Arc::clone(&record.cancel_requested),
                    )
                }
            }
        };
        let _guard = InflightGuard {
            server: self,
            session_id: session_id.clone(),
        };
        let message = UserMessage::from_parts(content, dsh_llm::MessageSource::User);
        let watermark = agent.session().events().len();
        if let Err(error) = run_followup(agent.as_ref(), message).await {
            return (Vec::new(), internal_error(id, &error.to_string()));
        }
        if let Some(persistence) = ctx.get::<PersistenceRuntime>() {
            if let Err(error) = persistence.save(agent.session().as_ref()).await {
                return (Vec::new(), internal_error(id, &error.to_string()));
            }
        }
        let events = agent.session().events();
        let mut notifications = Vec::new();
        let mut end_reason: Option<TurnEndReason> = None;
        for event in events.iter().skip(watermark) {
            match &event.data {
                SessionEventData::AssistantMessage { message, .. } => {
                    for block in &message.content {
                        if let ContentBlock::Text { text } = block {
                            notifications.push(agent_message_chunk(&session_id, text));
                        }
                    }
                }
                SessionEventData::TurnEnd { reason, .. } => {
                    end_reason = Some(reason.clone());
                }
                _ => {}
            }
        }
        if cancel_requested.load(Ordering::SeqCst) {
            return (
                notifications,
                JsonRpcResponse::result(id, serde_json::json!({ "stopReason": "cancelled" })),
            );
        }
        let response = match end_reason {
            Some(TurnEndReason::Error { message, .. }) => {
                internal_error(id, &format!("turn failed: {message}"))
            }
            Some(reason) => JsonRpcResponse::result(
                id,
                serde_json::json!({ "stopReason": turn_end_to_stop_reason(&reason) }),
            ),
            None => JsonRpcResponse::result(id, serde_json::json!({ "stopReason": "cancelled" })),
        };
        (notifications, response)
    }
}

/// The single-version `initialize` result: this server's one protocol version
/// and its fixed automation capabilities (no image, audio, or embedded context).
fn initialize_response(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::result(
        id,
        serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentInfo": { "name": AGENT_NAME, "version": AGENT_VERSION },
            "agentCapabilities": {
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
            },
            "authMethods": [],
        }),
    )
}

fn agent_message_chunk(session_id: &str, text: &str) -> JsonRpcNotification {
    JsonRpcNotification::new(
        "session/update",
        Some(serde_json::json!({
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text },
            },
        })),
    )
}

/// Serve newline-delimited ACP JSON-RPC until EOF. `session/prompt` runs as a
/// background task so later frames — including `session/cancel` — can be read
/// while the turn is in flight. Notifications for a request are written
/// before its response; `session/cancel` frames (no id) produce no output.
pub async fn serve<R, W>(
    ctx: Context,
    server: Arc<AcpServer>,
    reader: R,
    mut writer: W,
) -> Result<(), String>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
    std::thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(Some(line)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None);
    });
    let mut prompts = tokio::task::JoinSet::new();
    let mut stdin_open = true;
    while stdin_open || !prompts.is_empty() {
        tokio::select! {
            biased;
            msg = rx.recv(), if stdin_open => {
                match msg {
                    None | Some(None) => stdin_open = false,
                    Some(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if frame.get("id").is_none() {
                            if frame.get("method").and_then(Value::as_str) == Some("session/cancel") {
                                server.handle_cancel(frame.get("params").cloned());
                            }
                            continue;
                        }
                        let Ok(request) = serde_json::from_value::<JsonRpcRequest>(frame) else {
                            continue;
                        };
                        if request.method == "session/prompt" {
                            let ctx = ctx.clone();
                            let server = Arc::clone(&server);
                            prompts.spawn(async move { server.handle_request(&ctx, request).await });
                        } else {
                            let (notifications, response) = server.handle_request(&ctx, request).await;
                            for notification in notifications {
                                write_frame(&mut writer, &notification)?;
                            }
                            write_frame(&mut writer, &response)?;
                        }
                    }
                }
            }
            Some(joined) = prompts.join_next(), if !prompts.is_empty() => {
                let (notifications, response) = joined.map_err(|error| error.to_string())?;
                for notification in notifications {
                    write_frame(&mut writer, &notification)?;
                }
                write_frame(&mut writer, &response)?;
            }
        }
    }
    server.quiesce();
    Ok(())
}

/// Serve the process stdio streams until stdin closes.
pub async fn serve_stdio(ctx: &Context) -> Result<(), String> {
    let server = ctx
        .service::<AcpServer>()
        .map_err(|error| error.to_string())?;
    serve(
        ctx.clone(),
        server,
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
    )
    .await
}

fn write_frame<W: Write>(writer: &mut W, frame: &impl serde::Serialize) -> Result<(), String> {
    let line = serde_json::to_string(frame).map_err(|error| error.to_string())?;
    writeln!(writer, "{line}").map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_spine::apply_replay;

    fn request(id: u64, method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest::new(id, method, Some(params))
    }

    #[tokio::test]
    async fn handshake_new_session_prompt_round_trip() {
        let ctx = Context::new();
        apply_replay(&ctx, "ONE").unwrap();
        let server = AcpServer::new();
        let (_, response) = server
            .handle_request(
                &ctx,
                request(1, "initialize", serde_json::json!({"protocolVersion": 1})),
            )
            .await;
        let result = response.result.unwrap();
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], AGENT_NAME);
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"],
            serde_json::json!({"image": false, "audio": false, "embeddedContext": false})
        );
        assert_eq!(result["authMethods"], serde_json::json!([]));

        let (_, response) = server
            .handle_request(
                &ctx,
                request(2, "authenticate", serde_json::json!({"methodId": "none"})),
            )
            .await;
        assert_eq!(response.result.unwrap(), serde_json::json!({}));

        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    3,
                    "session/new",
                    serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
                ),
            )
            .await;
        let session_id = response.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let (notifications, response) = server
            .handle_request(
                &ctx,
                request(
                    4,
                    "session/prompt",
                    serde_json::json!({
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": "Reply with exactly the word: ONE." }],
                    }),
                ),
            )
            .await;
        assert_eq!(response.result.unwrap()["stopReason"], "end_turn");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].method, "session/update");
        assert_eq!(
            notifications[0].params.as_ref().unwrap()["update"],
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "ONE" },
            })
        );
    }

    #[tokio::test]
    async fn rejects_invalid_session_params_and_unknown_targets() {
        let ctx = Context::new();
        apply_replay(&ctx, "ok").unwrap();
        let server = AcpServer::new();
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    1,
                    "session/new",
                    serde_json::json!({"cwd": "relative/path"}),
                ),
            )
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: cwd must be an absolute path: relative/path"
        );
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    2,
                    "session/new",
                    serde_json::json!({"cwd": "/tmp", "mcpServers": [{"name": "x"}]}),
                ),
            )
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: mcpServers is not supported"
        );
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    3,
                    "session/new",
                    serde_json::json!({"cwd": "/tmp", "additionalDirectories": ["/x"]}),
                ),
            )
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: additionalDirectories is not supported"
        );
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    4,
                    "session/prompt",
                    serde_json::json!({"sessionId": "nope", "prompt": []}),
                ),
            )
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: unknown session: nope"
        );
        let (_, response) = server
            .handle_request(&ctx, request(5, "session/load", serde_json::json!({})))
            .await;
        let error = response.error.unwrap();
        assert_eq!(error["code"], -32601);
        assert_eq!(error["message"], "\"Method not found\": session/load");
    }

    #[tokio::test]
    async fn rejects_non_text_prompt_content() {
        let ctx = Context::new();
        apply_replay(&ctx, "ok").unwrap();
        let server = AcpServer::new();
        let (_, response) = server
            .handle_request(
                &ctx,
                request(1, "session/new", serde_json::json!({"cwd": "/tmp"})),
            )
            .await;
        let session_id = response.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let prompt = |id: u64, block: Value| {
            request(
                id,
                "session/prompt",
                serde_json::json!({"sessionId": session_id, "prompt": [block]}),
            )
        };
        let (_, response) = server
            .handle_request(
                &ctx,
                prompt(
                    2,
                    serde_json::json!({"type": "image", "data": "", "mimeType": "image/png"}),
                ),
            )
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: inline image prompts were not advertised by this connection"
        );
        let (_, response) = server
            .handle_request(&ctx, prompt(3, serde_json::json!({"type": "audio"})))
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: audio prompt content is not supported"
        );
        let (_, response) = server
            .handle_request(&ctx, prompt(4, serde_json::json!({"type": "resource"})))
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: embedded resource prompt content is not supported"
        );
        let (_, response) = server
            .handle_request(&ctx, prompt(5, serde_json::json!({"type": "mystery"})))
            .await;
        assert_eq!(
            response.error.unwrap()["message"],
            "Invalid params: unsupported ACP prompt content"
        );
    }

    #[test]
    fn stop_reason_mapping_matches_the_typescript_settlement() {
        assert_eq!(
            turn_end_to_stop_reason(&TurnEndReason::Completed),
            "end_turn"
        );
        assert_eq!(
            turn_end_to_stop_reason(&TurnEndReason::MaxTokens),
            "end_turn"
        );
        assert_eq!(turn_end_to_stop_reason(&TurnEndReason::Blocked), "end_turn");
        assert_eq!(
            turn_end_to_stop_reason(&TurnEndReason::Aborted {
                reason: "user".into()
            }),
            "end_turn"
        );
        assert_eq!(
            turn_end_to_stop_reason(&TurnEndReason::Interrupted),
            "cancelled"
        );
    }

    #[tokio::test]
    async fn rejects_a_second_prompt_while_one_is_in_flight() {
        let ctx = Context::new();
        dsh_agent_spine::apply(&ctx, Arc::new(SlowAdapter)).unwrap();
        let server = Arc::new(AcpServer::new());
        let (_, response) = server
            .handle_request(
                &ctx,
                request(1, "session/new", serde_json::json!({"cwd": "/tmp"})),
            )
            .await;
        let session_id = response.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let first = {
            let server = Arc::clone(&server);
            let ctx = ctx.clone();
            let prompt = request(
                2,
                "session/prompt",
                serde_json::json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "go" }],
                }),
            );
            tokio::spawn(async move { server.handle_request(&ctx, prompt).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, second) = server
            .handle_request(
                &ctx,
                request(
                    3,
                    "session/prompt",
                    serde_json::json!({
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": "again" }],
                    }),
                ),
            )
            .await;
        assert_eq!(
            second.error.unwrap()["message"],
            "Invalid params: a prompt is already in flight for this session"
        );
        server.handle_cancel(Some(serde_json::json!({ "sessionId": session_id })));
        let (_, first) = first.await.unwrap();
        assert_eq!(first.result.unwrap()["stopReason"], "cancelled");
    }

    struct ChannelRead {
        rx: std::sync::mpsc::Receiver<Option<String>>,
        leftover: Vec<u8>,
    }

    impl std::io::Read for ChannelRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.leftover.is_empty() {
                match self.rx.recv() {
                    Ok(Some(line)) => {
                        self.leftover.extend_from_slice(line.as_bytes());
                        self.leftover.push(b'\n');
                    }
                    Ok(None) | Err(_) => return Ok(0),
                }
            }
            let n = self.leftover.len().min(buf.len());
            buf[..n].copy_from_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            Ok(n)
        }
    }

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn serve_cancels_an_in_flight_prompt_from_a_later_frame() {
        let ctx = Context::new();
        dsh_agent_spine::apply(&ctx, Arc::new(SlowAdapter)).unwrap();
        let server = Arc::new(AcpServer::new());
        let (_, response) = server
            .handle_request(
                &ctx,
                request(1, "session/new", serde_json::json!({"cwd": "/tmp"})),
            )
            .await;
        let session_id = response.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let serve_task = {
            let ctx = ctx.clone();
            let server = Arc::clone(&server);
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                serve(
                    ctx,
                    server,
                    std::io::BufReader::new(ChannelRead {
                        rx,
                        leftover: Vec::new(),
                    }),
                    Capture(captured),
                )
                .await
            })
        };
        tx.send(Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": "go" }],
                },
            })
            .to_string(),
        ))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session_id },
            })
            .to_string(),
        ))
        .unwrap();
        tx.send(None).unwrap();
        serve_task.await.unwrap().unwrap();
        let body = String::from_utf8(captured.lock().expect("capture").clone()).unwrap();
        let prompt = body
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame.get("id") == Some(&Value::from(2)))
            .expect("prompt response");
        assert_eq!(prompt["result"]["stopReason"], "cancelled", "{body}");
    }

    #[tokio::test]
    async fn cancel_of_an_unknown_session_is_a_noop() {
        let server = AcpServer::new();
        server.handle_cancel(Some(serde_json::json!({ "sessionId": "missing" })));
    }

    struct SlowAdapter;

    #[async_trait::async_trait]
    impl dsh_llm::LlmAdapter for SlowAdapter {
        async fn stream(
            &self,
            _: dsh_llm::LlmRequest,
        ) -> std::result::Result<
            futures::stream::BoxStream<'static, dsh_llm::StreamChunk>,
            dsh_llm::LlmError,
        > {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(Box::pin(futures::stream::iter(
                dsh_llm::StreamChunk::text_stream("late"),
            )))
        }
    }
}
