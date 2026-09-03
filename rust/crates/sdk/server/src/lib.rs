//! JSON-RPC runtime server for out-of-process harness SDKs. Handles
//! `initialize`, `session/prompt`, and `shutdown`, and streams `session.event`
//! and `session.status` notifications for each driven turn.
//!
//! `session/prompt` enqueues via `followup` and returns `{ messageId }`
//! immediately on the live stdio writer. `session.event` / `session.status` /
//! `subagent.*` are forwarded from the Cordis bus while the agent runs.
//! Callers that do not attach a writer (HTTP `/rpc`, crate tests) still drive
//! the turn to quiescence inside the request and return the notification
//! batch: splice, `running`, turn events, `subagent.*`, then `idle`.

use dsh_agent::{Agent, AgentHandle, AgentRegistry};
use dsh_cordis::{Context, Service};
use dsh_llm::{LlmRuntime, UserMessage};
use dsh_sdk_protocol::{
    methods, InitializeParams, InitializeResult, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, JsonRpcStdout, ServerInfo, SessionPromptParams, SessionPromptResult,
    SERVER_NAME, SERVER_VERSION,
};
use dsh_session::{Session, SessionHeader, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One SDK-owned session: the live agent plus its registry disposer.
struct SessionEntry {
    agent: Arc<dyn Agent>,
    handle: Option<AgentHandle>,
    drive: Arc<tokio::sync::Mutex<()>>,
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
pub struct HarnessSdkJsonRpcServer {
    state: Mutex<State>,
    pending: Arc<Mutex<Vec<JsonRpcNotification>>>,
    outbound: Arc<Mutex<Option<JsonRpcStdout>>>,
    subscribed: AtomicBool,
    max_tokens_as_success: Arc<AtomicBool>,
}

impl Default for HarnessSdkJsonRpcServer {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            pending: Arc::new(Mutex::new(Vec::new())),
            outbound: Arc::new(Mutex::new(None)),
            subscribed: AtomicBool::new(false),
            max_tokens_as_success: Arc::new(AtomicBool::new(false)),
        }
    }
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
        let server = Arc::new(Self::new());
        server.subscribe(ctx);
        ctx.provide(server)
    }

    /// Write live notifications and the prompt receipt on this stdout.
    pub fn set_outbound(&self, stdout: JsonRpcStdout) {
        *self.outbound.lock().expect("sdk outbound") = Some(stdout);
    }

    /// Subscribe once for session, status, and subagent notifications.
    fn subscribe(&self, ctx: &Context) {
        if self.subscribed.swap(true, Ordering::SeqCst) {
            return;
        }
        let outbound = Arc::clone(&self.outbound);
        let _ = ctx.on("session/event", move |payload| {
            let Some(stdout) = outbound.lock().ok().and_then(|slot| slot.clone()) else {
                return;
            };
            let Some(session_id) = payload.get("sessionId").and_then(Value::as_str) else {
                return;
            };
            let Some(event) = payload.get("event") else {
                return;
            };
            let _ = stdout.write_json(&JsonRpcNotification::new(
                methods::SESSION_EVENT,
                Some(serde_json::json!({
                    "sessionId": session_id,
                    "event": event,
                })),
            ));
        });
        let outbound = Arc::clone(&self.outbound);
        let _ = ctx.on("agent/status", move |payload| {
            let Some(stdout) = outbound.lock().ok().and_then(|slot| slot.clone()) else {
                return;
            };
            let Some(status) = payload.get("status").and_then(Value::as_str) else {
                return;
            };
            if status != "running" && status != "idle" {
                return;
            }
            let Some(session_id) = payload
                .get("sessionId")
                .or_else(|| payload.get("agentId"))
                .and_then(Value::as_str)
            else {
                return;
            };
            let _ = stdout.write_json(&status_notification(session_id, status));
        });
        let pending = Arc::clone(&self.pending);
        let outbound = Arc::clone(&self.outbound);
        let _ = ctx.on("session/created", move |payload| {
            let Some(parent) = payload.get("parentSession").and_then(Value::as_str) else {
                return;
            };
            let Some(child) = payload.get("id").and_then(Value::as_str) else {
                return;
            };
            let notification = JsonRpcNotification::new(
                methods::SUBAGENT_STARTED,
                Some(serde_json::json!({
                    "parentSessionId": parent,
                    "childSessionId": child,
                })),
            );
            if let Some(stdout) = outbound.lock().ok().and_then(|slot| slot.clone()) {
                let _ = stdout.write_json(&notification);
                return;
            }
            pending.lock().expect("sdk pending").push(notification);
        });
        let pending = Arc::clone(&self.pending);
        let outbound = Arc::clone(&self.outbound);
        let max_tokens = Arc::clone(&self.max_tokens_as_success);
        let _ = ctx.on("subagent/end", move |payload| {
            if payload.get("local").and_then(Value::as_bool) != Some(true) {
                return;
            }
            let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
                return;
            };
            let Some(child) = payload.get("id").and_then(Value::as_str) else {
                return;
            };
            let Some(parent) = payload.get("parentSessionId").and_then(Value::as_str) else {
                return;
            };
            let reason = payload
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let mut body = serde_json::json!({
                "provider": provider,
                "agentId": child,
                "parentSessionId": parent,
                "childSessionId": child,
                "status": success_status(reason, max_tokens.load(Ordering::SeqCst)),
                "stopReason": reason,
            });
            if let Some(message) = payload.get("lastAssistantMessage") {
                body["lastAssistantMessage"] = message.clone();
            }
            let notification = JsonRpcNotification::new(methods::SUBAGENT_FINISHED, Some(body));
            if let Some(stdout) = outbound.lock().ok().and_then(|slot| slot.clone()) {
                let _ = stdout.write_json(&notification);
                return;
            }
            pending.lock().expect("sdk pending").push(notification);
        });
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
        self.subscribe(ctx);
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
        self.subscribe(ctx);
        let params: SessionPromptParams = match params
            .ok_or_else(|| "session/prompt requires params".to_string())
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        {
            Ok(params) => params,
            Err(error) => return (Vec::new(), JsonRpcResponse::error(id, -32603, error)),
        };
        let (agent, drive) = match self.get_or_create_session(ctx, &params.session_id) {
            Ok(pair) => pair,
            Err(error) => return (Vec::new(), JsonRpcResponse::error(id, -32603, error)),
        };
        // A registry-level reload disposes the loop's agents while this record
        // survives; a retained agent accepts followup() silently, so validate
        // the record against the live registry before delivery.
        let live_agent = ctx
            .service::<AgentRegistry>()
            .ok()
            .and_then(|agents| agents.get(agent.id()));
        if !live_agent.is_some_and(|live| Arc::ptr_eq(&live, &agent)) {
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
        agent.followup(message);
        {
            let _drive = drive.lock().await;
            if let Err(error) = agent.run().await {
                return (
                    Vec::new(),
                    JsonRpcResponse::error(id, -32603, error.to_string()),
                );
            }
            agent.when_idle().await;
            if let Err(error) = agent.run_maintenance().await {
                return (
                    Vec::new(),
                    JsonRpcResponse::error(id, -32603, error.to_string()),
                );
            }
        }
        if let Some(persistence) = ctx.get::<PersistenceRuntime>() {
            if let Err(error) = persistence.save(agent.session().as_ref()).await {
                return (
                    Vec::new(),
                    JsonRpcResponse::error(id, -32603, error.to_string()),
                );
            }
        }
        let mut notifications =
            turn_notifications(&params.session_id, agent.session().as_ref(), watermark);
        let extras: Vec<_> = self
            .pending
            .lock()
            .expect("sdk pending")
            .drain(..)
            .collect();
        if !extras.is_empty() {
            let idle = notifications.pop();
            notifications.extend(extras);
            if let Some(idle) = idle {
                notifications.push(idle);
            }
        }
        let receipt = SessionPromptResult { message_id };
        (
            notifications,
            JsonRpcResponse::result(id, serde_json::to_value(receipt).expect("prompt receipt")),
        )
    }

    async fn stdio_prompt(
        &self,
        ctx: &Context,
        request: JsonRpcRequest,
        stdout: &JsonRpcStdout,
    ) -> Result<(), String> {
        self.subscribe(ctx);
        let id = request.id.clone();
        let params: SessionPromptParams = match request
            .params
            .ok_or_else(|| "session/prompt requires params".to_string())
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        {
            Ok(params) => params,
            Err(error) => {
                stdout.write_json(&JsonRpcResponse::error(id, -32603, error))?;
                return Ok(());
            }
        };
        let (agent, drive) = match self.get_or_create_session(ctx, &params.session_id) {
            Ok(pair) => pair,
            Err(error) => {
                stdout.write_json(&JsonRpcResponse::error(id, -32603, error))?;
                return Ok(());
            }
        };
        let live_agent = ctx
            .service::<AgentRegistry>()
            .ok()
            .and_then(|agents| agents.get(agent.id()));
        if !live_agent.is_some_and(|live| Arc::ptr_eq(&live, &agent)) {
            stdout.write_json(&JsonRpcResponse::error(
                id,
                -32603,
                format!(
                    "session agent was disposed outside the server: {}",
                    params.session_id
                ),
            ))?;
            return Ok(());
        }
        let message = UserMessage::from_parts(params.content_blocks, dsh_llm::MessageSource::User);
        let receipt = SessionPromptResult {
            message_id: message.id.clone(),
        };
        agent.followup(message);
        stdout.write_json(&JsonRpcResponse::result(
            id,
            serde_json::to_value(receipt).expect("prompt receipt"),
        ))?;
        {
            let _drive = drive.lock().await;
            agent.run().await.map_err(|error| error.to_string())?;
            agent.when_idle().await;
            agent
                .run_maintenance()
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(persistence) = ctx.get::<PersistenceRuntime>() {
            persistence
                .save(agent.session().as_ref())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
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
    ) -> Result<(Arc<dyn Agent>, Arc<tokio::sync::Mutex<()>>), String> {
        let mut state = self.state.lock().expect("sdk state");
        if state.shutting_down {
            return Err("SDK server is shutting down".into());
        }
        if let Some(entry) = state.sessions.get(session_id) {
            return Ok((Arc::clone(&entry.agent), Arc::clone(&entry.drive)));
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
        let drive = Arc::new(tokio::sync::Mutex::new(()));
        state.sessions.insert(
            session_id.to_string(),
            SessionEntry {
                agent: Arc::clone(&agent),
                handle: Some(handle),
                drive: Arc::clone(&drive),
            },
        );
        Ok((agent, drive))
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

/// Deployment-specific status mapping for SDK turn and subagent outcomes.
fn success_status(reason: &str, max_tokens_as_success: bool) -> &'static str {
    if reason == "completed" {
        "ok"
    } else if reason == "max-tokens" && max_tokens_as_success {
        "ok"
    } else {
        "error"
    }
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

/// Serve newline-delimited JSON-RPC until EOF. `session/prompt` writes the
/// `{ messageId }` receipt as soon as `followup` succeeds, then streams
/// `session.event` / `session.status` / `subagent.*` while the agent runs.
/// `shutdown` waits for in-flight prompts before disposing server-owned agents.
pub async fn serve<R>(
    ctx: &Context,
    server: Arc<HarnessSdkJsonRpcServer>,
    reader: R,
    stdout: JsonRpcStdout,
) -> Result<(), String>
where
    R: BufRead + Send + 'static,
{
    server.set_outbound(stdout.clone());
    server.subscribe(ctx);
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
                        let Ok(request) = serde_json::from_str::<JsonRpcRequest>(&line) else {
                            continue;
                        };
                        if request.method == methods::SESSION_PROMPT {
                            let ctx = ctx.clone();
                            let server = Arc::clone(&server);
                            let stdout = stdout.clone();
                            prompts.spawn(async move {
                                server.stdio_prompt(&ctx, request, &stdout).await
                            });
                        } else if request.method == methods::SHUTDOWN {
                            while prompts.join_next().await.is_some() {}
                            let (notifications, response) =
                                server.handle_request(ctx, request).await;
                            for notification in notifications {
                                stdout.write_json(&notification)?;
                            }
                            stdout.write_json(&response)?;
                        } else {
                            let (notifications, response) =
                                server.handle_request(ctx, request).await;
                            for notification in notifications {
                                stdout.write_json(&notification)?;
                            }
                            stdout.write_json(&response)?;
                        }
                    }
                }
            }
            Some(joined) = prompts.join_next(), if !prompts.is_empty() => {
                joined.map_err(|error| error.to_string())??;
            }
        }
    }
    Ok(())
}

/// Serve the process stdio streams until stdin closes.
pub async fn serve_stdio(ctx: &Context) -> Result<(), String> {
    let server = ctx
        .service::<HarnessSdkJsonRpcServer>()
        .map_err(|error| error.to_string())?;
    serve(
        ctx,
        server,
        std::io::BufReader::new(std::io::stdin()),
        JsonRpcStdout::new(std::io::stdout()),
    )
    .await
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

    #[tokio::test]
    async fn prompt_emits_subagent_started_and_finished() {
        use dsh_agent_spine::{apply, apply_world};
        use dsh_llm_replay::{ReplayAdapter, ReplayToolCall, ReplayTurn};

        let ctx = Context::new();
        apply(
            &ctx,
            Arc::new(ReplayAdapter::new(vec![
                ReplayTurn {
                    text: String::new(),
                    tool: Some(ReplayToolCall {
                        id: "c1".into(),
                        name: "subagent".into(),
                        arguments:
                            r#"{"description":"child","prompt":"ping","run_in_background":false}"#
                                .into(),
                    }),
                    finish: None,
                    error: None,
                },
                ReplayTurn {
                    text: "child-done".into(),
                    tool: None,
                    finish: None,
                    error: None,
                },
                ReplayTurn {
                    text: "parent-done".into(),
                    tool: None,
                    finish: None,
                    error: None,
                },
            ])),
        )
        .unwrap();
        apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
        let server = HarnessSdkJsonRpcServer::new();
        let (notifications, response) = server
            .handle_request(
                &ctx,
                request(
                    1,
                    methods::SESSION_PROMPT,
                    serde_json::json!({
                        "sessionId": "44444444-4444-4444-4444-444444444444",
                        "contentBlocks": [{ "type": "text", "text": "delegate" }],
                    }),
                ),
            )
            .await;
        assert!(response.result.unwrap()["messageId"].is_string());
        let methods_seen: Vec<&str> = notifications
            .iter()
            .map(|notification| notification.method.as_str())
            .collect();
        assert!(methods_seen.contains(&methods::SUBAGENT_STARTED));
        assert!(methods_seen.contains(&methods::SUBAGENT_FINISHED));
        assert_eq!(*methods_seen.last().unwrap(), methods::SESSION_STATUS);
        let started = notifications
            .iter()
            .find(|notification| notification.method == methods::SUBAGENT_STARTED)
            .unwrap()
            .params
            .as_ref()
            .unwrap();
        assert_eq!(
            started["parentSessionId"],
            "44444444-4444-4444-4444-444444444444"
        );
        assert!(started["childSessionId"].as_str().is_some());
        let finished = notifications
            .iter()
            .find(|notification| notification.method == methods::SUBAGENT_FINISHED)
            .unwrap()
            .params
            .as_ref()
            .unwrap();
        assert_eq!(finished["provider"], "spawn");
        assert_eq!(finished["status"], "ok");
        assert_eq!(finished["stopReason"], "completed");
        assert_eq!(finished["parentSessionId"], started["parentSessionId"]);
        assert_eq!(finished["childSessionId"], started["childSessionId"]);
        assert_eq!(finished["lastAssistantMessage"][0]["text"], "child-done");
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

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
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

    #[tokio::test]
    async fn serve_returns_the_receipt_before_the_turn_runs() {
        use dsh_agent_spine::apply;

        let ctx = Context::new();
        apply(&ctx, Arc::new(SlowAdapter)).unwrap();
        let server = Arc::new(HarnessSdkJsonRpcServer::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let serve_task = {
            let ctx = ctx.clone();
            let server = Arc::clone(&server);
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                serve(
                    &ctx,
                    server,
                    std::io::BufReader::new(ChannelRead {
                        rx,
                        leftover: Vec::new(),
                    }),
                    JsonRpcStdout::new(Capture(captured)),
                )
                .await
            })
        };
        tx.send(Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "cwd": ".",
                    "provider": "deepseek-official",
                    "model": "deepseek-v4-flash",
                },
            })
            .to_string(),
        ))
        .unwrap();
        tx.send(Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": {
                    "sessionId": "11111111-1111-1111-1111-111111111111",
                    "contentBlocks": [{ "type": "text", "text": "go" }],
                },
            })
            .to_string(),
        ))
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let body = String::from_utf8(captured.lock().expect("capture").clone()).unwrap();
            let frames: Vec<Value> = body
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            let receipt = frames.iter().find(|frame| frame.get("id") == Some(&Value::from(2)));
            if let Some(receipt) = receipt {
                assert!(receipt["result"]["messageId"].is_string(), "{body}");
                let has_assistant = frames.iter().any(|frame| {
                    frame["method"] == methods::SESSION_EVENT
                        && frame["params"]["event"]["type"] == "assistant/message"
                });
                assert!(
                    !has_assistant,
                    "receipt must precede assistant/message: {body}"
                );
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for prompt receipt: {body}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tx.send(None).unwrap();
        serve_task.await.unwrap().unwrap();
    }
}
