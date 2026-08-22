//! JSON-RPC runtime server for out-of-process harness SDKs. Handles
//! `initialize`, `session/prompt`, and `shutdown`, and streams `session.event`
//! and `session.status` notifications for each driven turn.
//!
//! The TypeScript server enqueues a prompt and lets the agent run in the
//! background, so its receipt response precedes most event notifications. This
//! server drives the turn to quiescence inside the request, so every
//! notification for the turn precedes the response; the notification order
//! itself (inbox splice, `running`, turn events, `idle`) matches TypeScript.

use dsh_agent::{Agent, AgentHandle, AgentRegistry};
use dsh_agent_loop::run_followup;
use dsh_cordis::{Context, Service};
use dsh_llm::{LlmRuntime, UserMessage};
use dsh_sdk_protocol::{
    methods, InitializeParams, InitializeResult, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, ServerInfo, SessionPromptParams, SessionPromptResult, SERVER_NAME,
    SERVER_VERSION,
};
use dsh_session::{Session, SessionHeader, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

/// One SDK-owned session: the live agent plus its registry disposer.
struct SessionEntry {
    agent: Arc<dyn Agent>,
    handle: Option<AgentHandle>,
}

/// Route and lifecycle state configured by `initialize`.
struct State {
    cwd: String,
    provider: String,
    model: String,
    shutting_down: bool,
    sessions: HashMap<String, SessionEntry>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default(),
            provider: "deepseek-official".into(),
            model: "deepseek-official".into(),
            shutting_down: false,
            sessions: HashMap::new(),
        }
    }
}

/// SDK server over one booted harness context. Construction is stateless;
/// `initialize` configures the route and `shutdown` disposes server-owned agents.
#[derive(Default)]
pub struct HarnessSdkJsonRpcServer {
    state: Mutex<State>,
}

impl Service for HarnessSdkJsonRpcServer {
    const KEY: &'static str = "sdkJsonRpcServer";
}

impl HarnessSdkJsonRpcServer {
    /// Create an unconfigured server.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount the server as `ctx.sdkJsonRpcServer`.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<()> {
        ctx.provide(Arc::new(Self::new()))
    }

    /// Dispatch one incoming request to its typed handler, returning the
    /// notifications produced while handling it (wire order) and the response.
    pub async fn handle_request(
        &self,
        ctx: &Context,
        request: JsonRpcRequest,
    ) -> (Vec<JsonRpcNotification>, JsonRpcResponse) {
        let id = request.id.clone();
        match request.method.as_str() {
            methods::INITIALIZE => (Vec::new(), self.initialize(ctx, id, request.params)),
            methods::SESSION_PROMPT => self.prompt(ctx, id, request.params).await,
            methods::SHUTDOWN => (Vec::new(), self.shutdown(id)),
            other => (
                Vec::new(),
                JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("unknown DeepSeek Harness SDK runtime method: {other}"),
                ),
            ),
        }
    }

    /// Configure the SDK route, mounting the DeepSeek fallback only when unowned.
    fn initialize(&self, ctx: &Context, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let params: InitializeParams = match params
            .ok_or_else(|| "initialize requires params".to_string())
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        {
            Ok(params) => params,
            Err(error) => return JsonRpcResponse::error(id, -32603, error),
        };
        if let Some(max_tokens) = params.max_tokens {
            if max_tokens <= 0 {
                return JsonRpcResponse::error(
                    id,
                    -32603,
                    "initialize maxTokens must be a positive safe integer",
                );
            }
        }
        if !ctx.has_service(LlmRuntime::KEY) {
            return JsonRpcResponse::error(
                id,
                -32603,
                format!("no adapter registered for provider \"{}\"", params.provider),
            );
        }
        let mut state = self.state.lock().expect("sdk state");
        state.cwd = std::path::absolute(&params.cwd)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or(params.cwd);
        state.provider = params.provider;
        state.model = params.model;
        let result = InitializeResult {
            server_info: ServerInfo {
                name: SERVER_NAME.into(),
                version: SERVER_VERSION.into(),
            },
        };
        JsonRpcResponse::result(id, serde_json::to_value(result).expect("initialize result"))
    }

    /// Queue one identified prompt, drive the turn to quiescence, and stream
    /// the turn's session events around the status transitions.
    async fn prompt(
        &self,
        ctx: &Context,
        id: Value,
        params: Option<Value>,
    ) -> (Vec<JsonRpcNotification>, JsonRpcResponse) {
        let params: SessionPromptParams = match params
            .ok_or_else(|| "session/prompt requires params".to_string())
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        {
            Ok(params) => params,
            Err(error) => return (Vec::new(), JsonRpcResponse::error(id, -32603, error)),
        };
        let agent = match self.get_or_create_session(ctx, &params.session_id) {
            Ok(agent) => agent,
            Err(error) => return (Vec::new(), JsonRpcResponse::error(id, -32603, error)),
        };
        // A registry-level reload disposes the loop's agents while this record
        // survives; a retained agent accepts followup() silently, so validate
        // the record against the live registry before delivery.
        let live = ctx
            .service::<AgentRegistry>()
            .ok()
            .and_then(|agents| agents.get(agent.id()));
        if !live.is_some_and(|live| Arc::ptr_eq(&live, &agent)) {
            return (
                Vec::new(),
                JsonRpcResponse::error(
                    id,
                    -32603,
                    format!(
                        "session agent was disposed outside the server: {}",
                        params.session_id
                    ),
                ),
            );
        }
        let message = UserMessage::from_parts(params.content_blocks, dsh_llm::MessageSource::User);
        let message_id = message.id.clone();
        let watermark = agent.session().events().len();
        if let Err(error) = run_followup(agent.as_ref(), message).await {
            return (
                Vec::new(),
                JsonRpcResponse::error(id, -32603, error.to_string()),
            );
        }
        if let Some(persistence) = ctx.get::<PersistenceRuntime>() {
            if let Err(error) = persistence.save(agent.session().as_ref()).await {
                return (
                    Vec::new(),
                    JsonRpcResponse::error(id, -32603, error.to_string()),
                );
            }
        }
        let notifications =
            turn_notifications(&params.session_id, agent.session().as_ref(), watermark);
        let receipt = SessionPromptResult { message_id };
        (
            notifications,
            JsonRpcResponse::result(id, serde_json::to_value(receipt).expect("prompt receipt")),
        )
    }

    /// Dispose server-owned agents. The surrounding context remains running.
    fn shutdown(&self, id: Value) -> JsonRpcResponse {
        let mut state = self.state.lock().expect("sdk state");
        state.shutting_down = true;
        for (_, mut entry) in state.sessions.drain() {
            if let Some(handle) = entry.handle.take() {
                handle.dispose();
            }
        }
        JsonRpcResponse::result(id, serde_json::json!({}))
    }

    fn get_or_create_session(
        &self,
        ctx: &Context,
        session_id: &str,
    ) -> Result<Arc<dyn Agent>, String> {
        let mut state = self.state.lock().expect("sdk state");
        if state.shutting_down {
            return Err("SDK server is shutting down".into());
        }
        if let Some(entry) = state.sessions.get(session_id) {
            return Ok(Arc::clone(&entry.agent));
        }
        let store = ctx
            .service::<SessionStore>()
            .map_err(|error| error.to_string())?;
        let header =
            SessionHeader::new(dsh_session::session_id(session_id), Some(state.cwd.clone()));
        let session = store.publish(Session::with_header(header));
        let handle = ctx
            .service::<AgentRegistry>()
            .map_err(|error| error.to_string())?
            .create(session)
            .map_err(|error| error.to_string())?;
        let agent = Arc::clone(&handle.agent);
        state.sessions.insert(
            session_id.to_string(),
            SessionEntry {
                agent: Arc::clone(&agent),
                handle: Some(handle),
            },
        );
        Ok(agent)
    }
}

/// Notifications for one driven turn: the inbox splice recorded by the
/// enqueue, `session.status running`, the remaining turn events in log order,
/// then `session.status idle`.
fn turn_notifications(
    session_id: &str,
    session: &Session,
    watermark: usize,
) -> Vec<JsonRpcNotification> {
    let events = session.events();
    let mut notifications = Vec::new();
    for (offset, event) in events.iter().enumerate().skip(watermark) {
        if offset == watermark + 1 {
            notifications.push(status_notification(session_id, "running"));
        }
        notifications.push(JsonRpcNotification::new(
            methods::SESSION_EVENT,
            Some(serde_json::json!({
                "sessionId": session_id,
                "event": event,
            })),
        ));
    }
    notifications.push(status_notification(session_id, "idle"));
    notifications
}

fn status_notification(session_id: &str, status: &str) -> JsonRpcNotification {
    JsonRpcNotification::new(
        methods::SESSION_STATUS,
        Some(serde_json::json!({
            "sessionId": session_id,
            "status": status,
        })),
    )
}

/// Serve newline-delimited JSON-RPC over the given reader/writer until EOF.
/// Notifications for a request are written before its response.
pub async fn serve<R: BufRead, W: Write>(
    ctx: &Context,
    server: &HarnessSdkJsonRpcServer,
    reader: R,
    writer: &mut W,
) -> Result<(), String> {
    for line in reader.lines() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<JsonRpcRequest>(&line) else {
            // Frames without an id (notifications) and malformed lines are
            // dropped; the SDK protocol defines no client-to-server notifications.
            continue;
        };
        let (notifications, response) = server.handle_request(ctx, request).await;
        for notification in notifications {
            write_frame(writer, &notification)?;
        }
        write_frame(writer, &response)?;
    }
    Ok(())
}

/// Serve the process stdio streams until stdin closes.
pub async fn serve_stdio(ctx: &Context) -> Result<(), String> {
    let server = ctx
        .service::<HarnessSdkJsonRpcServer>()
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
    async fn initialize_prompt_shutdown_projects_the_loop() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let server = HarnessSdkJsonRpcServer::new();
        let (notifications, response) = server
            .handle_request(
                &ctx,
                request(
                    1,
                    methods::INITIALIZE,
                    serde_json::json!({
                        "cwd": ".",
                        "provider": "deepseek-official",
                        "model": "deepseek-v4-flash",
                    }),
                ),
            )
            .await;
        assert!(notifications.is_empty());
        assert_eq!(response.result.unwrap()["serverInfo"]["name"], SERVER_NAME);

        let (notifications, response) = server
            .handle_request(
                &ctx,
                request(
                    2,
                    methods::SESSION_PROMPT,
                    serde_json::json!({
                        "sessionId": "11111111-1111-1111-1111-111111111111",
                        "contentBlocks": [{ "type": "text", "text": "ping" }],
                    }),
                ),
            )
            .await;
        assert!(response.result.unwrap()["messageId"].is_string());
        let methods_seen: Vec<&str> = notifications
            .iter()
            .map(|notification| notification.method.as_str())
            .collect();
        assert_eq!(methods_seen[0], methods::SESSION_EVENT);
        assert_eq!(methods_seen[1], methods::SESSION_STATUS);
        assert_eq!(*methods_seen.last().unwrap(), methods::SESSION_STATUS);
        let first = notifications[0].params.as_ref().unwrap();
        assert_eq!(first["event"]["type"], "agent/inbox/spliced");
        assert_eq!(
            notifications[1].params.as_ref().unwrap()["status"],
            "running"
        );
        assert_eq!(
            notifications.last().unwrap().params.as_ref().unwrap()["status"],
            "idle"
        );
        let event_types: Vec<String> = notifications
            .iter()
            .filter(|notification| notification.method == methods::SESSION_EVENT)
            .map(|notification| {
                notification.params.as_ref().unwrap()["event"]["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(event_types.contains(&"turn/start".to_string()));
        assert!(event_types.contains(&"assistant/message".to_string()));
        assert_eq!(event_types.last().unwrap(), "turn/end");

        let (_, response) = server
            .handle_request(&ctx, request(3, methods::SHUTDOWN, serde_json::json!({})))
            .await;
        assert_eq!(response.result.unwrap(), serde_json::json!({}));
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    4,
                    methods::SESSION_PROMPT,
                    serde_json::json!({
                        "sessionId": "22222222-2222-2222-2222-222222222222",
                        "contentBlocks": [{ "type": "text", "text": "ping" }],
                    }),
                ),
            )
            .await;
        assert!(response.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("shutting down"));
    }

    #[tokio::test]
    async fn prompt_reuses_one_session_per_id() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let server = HarnessSdkJsonRpcServer::new();
        let prompt = |id: u64| {
            request(
                id,
                methods::SESSION_PROMPT,
                serde_json::json!({
                    "sessionId": "33333333-3333-3333-3333-333333333333",
                    "contentBlocks": [{ "type": "text", "text": "ping" }],
                }),
            )
        };
        let (first_notifications, _) = server.handle_request(&ctx, prompt(1)).await;
        let (second_notifications, _) = server.handle_request(&ctx, prompt(2)).await;
        let first_seq = first_notifications[0].params.as_ref().unwrap()["event"]["seq"]
            .as_u64()
            .unwrap();
        let second_seq = second_notifications[0].params.as_ref().unwrap()["event"]["seq"]
            .as_u64()
            .unwrap();
        assert_eq!(first_seq, 0);
        assert!(second_seq > first_seq, "same session log continues");
    }

    #[tokio::test]
    async fn unknown_method_and_missing_adapter_fail_loud() {
        let ctx = Context::new();
        let server = HarnessSdkJsonRpcServer::new();
        let (_, response) = server
            .handle_request(&ctx, request(1, "nope", serde_json::json!({})))
            .await;
        assert!(response.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("unknown DeepSeek Harness SDK runtime method: nope"));
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    2,
                    methods::INITIALIZE,
                    serde_json::json!({
                        "cwd": ".",
                        "provider": "other",
                        "model": "m",
                    }),
                ),
            )
            .await;
        assert!(response.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("no adapter registered for provider \"other\""));
        let (_, response) = server
            .handle_request(
                &ctx,
                request(
                    3,
                    methods::INITIALIZE,
                    serde_json::json!({
                        "cwd": ".",
                        "provider": "deepseek-official",
                        "model": "m",
                        "maxTokens": 0,
                    }),
                ),
            )
            .await;
        assert!(response.error.unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("positive safe integer"));
    }
}
