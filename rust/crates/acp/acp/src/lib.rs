//! Automation-only Agent Client Protocol server over newline-delimited
//! JSON-RPC. Carries prompt text, committed assistant text, and cancellation;
//! presentation and human-interaction features stay with the harness's UI
//! modules.
//!
//! The TypeScript bridge streams `session/update` while the turn runs and
//! answers `session/prompt` at quiescence. This server drives the turn inside
//! the request, so the observable stdio order is identical: every
//! `agent_message_chunk` update precedes the prompt response.

use dsh_agent::{Agent, AgentHandle, AgentRegistry};
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

    /// Handle the `session/cancel` notification. This synchronous server has
    /// no prompt in flight between frames, so cancellation is a no-op for a
    /// known session and silently ignores an unknown one.
    pub fn handle_cancel(&self, _params: Option<Value>) {}

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
        let agent = {
            let sessions = self.sessions.lock().expect("acp sessions");
            match sessions.get(&session_id) {
                Some(record) => Arc::clone(&record.agent),
                None => {
                    return (
                        Vec::new(),
                        invalid_params(id, &format!("unknown session: {session_id}")),
                    )
                }
            }
        };
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
        let response = match end_reason {
            Some(TurnEndReason::Error { message, .. }) => {
                internal_error(id, &format!("turn failed: {message}"))
            }
            Some(reason) => JsonRpcResponse::result(
                id,
                serde_json::json!({ "stopReason": turn_end_to_stop_reason(&reason) }),
            ),
            // A turn that produced no ending is out-of-band cancellation.
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

/// Serve newline-delimited ACP JSON-RPC over the given reader/writer until
/// EOF. Notifications for a request are written before its response;
/// `session/cancel` frames (no id) produce no output.
pub async fn serve<R: BufRead, W: Write>(
    ctx: &Context,
    server: &AcpServer,
    reader: R,
    writer: &mut W,
) -> Result<(), String> {
    for line in reader.lines() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            // Malformed peer lines are ignored, matching the TS transport.
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
        let (notifications, response) = server.handle_request(ctx, request).await;
        for notification in notifications {
            write_frame(writer, &notification)?;
        }
        write_frame(writer, &response)?;
    }
    server.quiesce();
    Ok(())
}

/// Serve the process stdio streams until stdin closes.
pub async fn serve_stdio(ctx: &Context) -> Result<(), String> {
    let server = ctx
        .service::<AcpServer>()
        .map_err(|error| error.to_string())?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    serve(ctx, server.as_ref(), stdin.lock(), &mut stdout).await
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
}
