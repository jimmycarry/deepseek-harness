//! Three-end fixture: the same replayed task driven through the headless,
//! ACP, and JSON-RPC shipped profiles must produce one session-event stream
//! and one final assistant text, and the two automation wires must match the
//! TypeScript fixture vocabulary (`session/update` frames, `session.event` /
//! `session.status` notifications).

use dsh_acp::AcpServer;
use dsh_app_boot::{compose_profile, register_profile_plugins, shipped_bundles};
use dsh_bundle_headless::HeadlessStartup;
use dsh_cordis::Context;
use dsh_cordis_loader::{Entry, EntryPatch, Loader};
use dsh_sdk_protocol::JsonRpcRequest;
use dsh_sdk_server::HarnessSdkJsonRpcServer;
use dsh_session::{session_id, SessionStore};
use serde_json::Value;
use std::sync::Arc;

const TASK: &str = "Reply with exactly: SDK snapshot OK";
const REPLY: &str = "SDK snapshot OK";
const SDK_SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";

fn replay_overlay(text: &str) -> Vec<EntryPatch> {
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(serde_json::json!({ "text": text }));
    vec![disable, EntryPatch::insert_row(replay)]
}

/// Mount one shipped profile with the replay overlay under a fresh DSH_HOME.
fn mount_profile(profile: &str, provide_task: bool) -> Context {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-f-{profile}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("DSH_HOME", &dir);
    let layers = shipped_bundles(profile).unwrap();
    let entries = compose_profile(&layers, &[], &[], &replay_overlay(REPLY)).unwrap();
    let ctx = Context::new();
    if provide_task {
        ctx.provide(Arc::new(HeadlessStartup { task: TASK.into() }))
            .unwrap();
    }
    let loader = Loader::new();
    register_profile_plugins(&loader);
    loader.mount(&ctx, &entries).unwrap();
    ctx
}

fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn session_events(ctx: &Context, id: &str) -> Vec<Value> {
    ctx.service::<SessionStore>()
        .unwrap()
        .get(&session_id(id))
        .expect("live session")
        .events()
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

fn last_assistant_text(ctx: &Context, id: &str) -> String {
    ctx.service::<SessionStore>()
        .unwrap()
        .get(&session_id(id))
        .expect("live session")
        .last_assistant_text()
        .expect("assistant text")
}

async fn run_headless_end() -> (Vec<String>, String) {
    let ctx = mount_profile("headless", true);
    let session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    let events: Vec<Value> = session
        .events()
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let types = event_types(&events);
    // The headless runner writes the three permission knob events before the
    // turn; the automation servers do not select a preset, so the shared
    // stream starts after them.
    assert_eq!(
        &types[..3],
        ["permission/preset", "sandbox/mode", "approval/policy"]
    );
    (
        types[3..].to_vec(),
        session.last_assistant_text().expect("assistant text"),
    )
}

async fn run_acp_end() -> (Vec<String>, String, Vec<String>) {
    let ctx = mount_profile("acp", false);
    let server = ctx.service::<AcpServer>().unwrap();
    let mut wire = Vec::new();
    let mut push = |notifications: Vec<dsh_sdk_protocol::JsonRpcNotification>,
                    response: dsh_sdk_protocol::JsonRpcResponse| {
        for notification in notifications {
            wire.push(serde_json::to_string(&notification).unwrap());
        }
        let line = serde_json::to_string(&response).unwrap();
        wire.push(line.clone());
        line
    };
    let (notifications, response) = server
        .handle_request(
            &ctx,
            JsonRpcRequest::new(
                1,
                "initialize",
                Some(serde_json::json!({ "protocolVersion": 1 })),
            ),
        )
        .await;
    push(notifications, response);
    let (notifications, response) = server
        .handle_request(
            &ctx,
            JsonRpcRequest::new(
                2,
                "session/new",
                Some(serde_json::json!({ "cwd": "/tmp", "mcpServers": [] })),
            ),
        )
        .await;
    let new_line = push(notifications, response);
    let new_frame: Value = serde_json::from_str(&new_line).unwrap();
    let acp_session = new_frame["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let (notifications, response) = server
        .handle_request(
            &ctx,
            JsonRpcRequest::new(
                3,
                "session/prompt",
                Some(serde_json::json!({
                    "sessionId": acp_session,
                    "prompt": [{ "type": "text", "text": TASK }],
                })),
            ),
        )
        .await;
    push(notifications, response);
    let types = event_types(&session_events(&ctx, &acp_session));
    let text = last_assistant_text(&ctx, &acp_session);
    let normalized = wire
        .into_iter()
        .map(|line| line.replace(&acp_session, "{{sessionId}}"))
        .collect();
    (types, text, normalized)
}

async fn run_jsonrpc_end() -> (Vec<String>, String, Vec<String>, Vec<Value>) {
    let ctx = mount_profile("jsonrpc", false);
    let server = ctx.service::<HarnessSdkJsonRpcServer>().unwrap();
    let (notifications, response) = server
        .handle_request(
            &ctx,
            JsonRpcRequest::new(
                1,
                "initialize",
                Some(serde_json::json!({
                    "cwd": "/tmp",
                    "provider": "deepseek-official",
                    "model": "deepseek-v4-flash",
                })),
            ),
        )
        .await;
    assert!(notifications.is_empty());
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"0.0.1"}}}"#
    );
    let (notifications, response) = server
        .handle_request(
            &ctx,
            JsonRpcRequest::new(
                2,
                "session/prompt",
                Some(serde_json::json!({
                    "sessionId": SDK_SESSION_ID,
                    "contentBlocks": [{ "type": "text", "text": TASK }],
                })),
            ),
        )
        .await;
    assert!(response.result.unwrap()["messageId"].is_string());
    let methods_seen: Vec<String> = notifications
        .iter()
        .map(|notification| notification.method.clone())
        .collect();
    let payloads: Vec<Value> = notifications
        .iter()
        .map(|notification| notification.params.clone().unwrap())
        .collect();
    let (_, response) = server
        .handle_request(&ctx, JsonRpcRequest::new(3, "shutdown", None))
        .await;
    assert_eq!(response.result.unwrap(), serde_json::json!({}));
    let types = event_types(&session_events(&ctx, SDK_SESSION_ID));
    let text = last_assistant_text(&ctx, SDK_SESSION_ID);
    (types, text, methods_seen, payloads)
}

#[tokio::test]
async fn three_ends_project_one_replayed_turn() {
    let (headless_types, headless_text) = run_headless_end().await;
    let (acp_types, acp_text, acp_wire) = run_acp_end().await;
    let (sdk_types, sdk_text, sdk_methods, sdk_payloads) = run_jsonrpc_end().await;

    // One turn, three surfaces: identical event stream and final text.
    assert_eq!(headless_types, acp_types);
    assert_eq!(headless_types, sdk_types);
    assert_eq!(headless_text, REPLY);
    assert_eq!(acp_text, REPLY);
    assert_eq!(sdk_text, REPLY);

    // The ACP wire matches the TypeScript fixture vocabulary byte for byte
    // after the session id is normalized.
    assert_eq!(
        acp_wire[0],
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"deepseek-harness-acp","version":"0.0.1"},"agentCapabilities":{"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false}},"authMethods":[]}}"#
    );
    assert_eq!(
        acp_wire[1],
        r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"{{sessionId}}"}}"#
    );
    assert_eq!(
        acp_wire[2],
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{{{{sessionId}}}}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{REPLY}"}}}}}}}}"#
        )
    );
    assert_eq!(
        acp_wire[3],
        r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#
    );

    // The SDK notification stream brackets the turn events with status
    // transitions: the enqueue splice, `running`, the turn, then `idle`.
    assert_eq!(sdk_methods[0], "session.event");
    assert_eq!(sdk_methods[1], "session.status");
    assert_eq!(*sdk_methods.last().unwrap(), "session.status");
    assert_eq!(sdk_payloads[0]["event"]["type"], "agent/inbox/spliced");
    assert_eq!(sdk_payloads[0]["sessionId"], SDK_SESSION_ID);
    assert_eq!(sdk_payloads[1]["status"], "running");
    assert_eq!(sdk_payloads.last().unwrap()["status"], "idle");
    let sdk_event_types: Vec<String> = sdk_methods
        .iter()
        .zip(&sdk_payloads)
        .filter(|(method, _)| *method == "session.event")
        .map(|(_, payload)| payload["event"]["type"].as_str().unwrap().to_string())
        .collect();
    // Every logged event of the turn is streamed, in log order.
    assert_eq!(sdk_event_types, sdk_types);
}
