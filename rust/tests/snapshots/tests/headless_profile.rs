//! Profile-path snapshots: compose + mount + runner, then compare type sequence
//! and key payloads to the TypeScript headless-profile fixture vocabulary.

use dsh_app_boot::{compose_profile, register_profile_plugins, shipped_bundles};
use dsh_bundle_headless::HeadlessStartup;
use dsh_cordis::Context;
use dsh_cordis_loader::{Entry, EntryPatch, Loader};
use dsh_llm::{FinishReason, StreamChunk};
use dsh_session::{event_type_name, Session, SessionEventData};
use serde_json::Value;
use std::sync::Arc;

fn replay_text_overlay(text: &str) -> Vec<EntryPatch> {
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(serde_json::json!({ "text": text }));
    vec![disable, EntryPatch::insert_row(replay)]
}

fn replay_turns_overlay(turns: Value) -> Vec<EntryPatch> {
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(serde_json::json!({ "turns": turns }));
    vec![disable, EntryPatch::insert_row(replay)]
}

async fn run_profile(task: &str, overlay: Vec<EntryPatch>) -> Vec<Value> {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-d-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("DSH_HOME", &dir);
    std::env::set_var("DSH_PERMISSION_MODE", "danger-full-access");
    let layers = shipped_bundles("headless").unwrap();
    let entries = compose_profile(&layers, &[], &[], &overlay).unwrap();
    let ctx = Context::new();
    ctx.provide(Arc::new(HeadlessStartup { task: task.into() }))
        .unwrap();
    let loader = Loader::new();
    register_profile_plugins(&loader);
    loader.mount(&ctx, &entries).unwrap();
    let session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    events_of(&session)
}

fn uuid_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn events_of(session: &Session) -> Vec<Value> {
    session
        .events()
        .into_iter()
        .map(|event| serde_json::to_value(event).expect("session event"))
        .collect()
}

fn types_of(events: &[Value]) -> Vec<String> {
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

const TEXT_TURN_TYPES: &[&str] = &[
    "permission/preset",
    "sandbox/mode",
    "approval/policy",
    "agent/inbox/spliced",
    "turn/start",
    "agent/inbox/spliced",
    "step/start",
    "user/message",
    "user/message",
    "session/title",
    "request/header",
    "request/context",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/message",
    "step/end",
    "turn/end",
];

const BASH_TURN_TYPES: &[&str] = &[
    "permission/preset",
    "sandbox/mode",
    "approval/policy",
    "agent/inbox/spliced",
    "turn/start",
    "agent/inbox/spliced",
    "step/start",
    "user/message",
    "user/message",
    "session/title",
    "request/header",
    "request/context",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/message",
    "tool/call",
    "tool/result",
    "step/end",
    "step/start",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/chunk",
    "assistant/message",
    "step/end",
    "turn/end",
];

#[tokio::test]
async fn text_turn_profile_types_and_payloads() {
    let events = run_profile("ping the product path", replay_text_overlay("pong")).await;
    assert_eq!(
        types_of(&events),
        TEXT_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(events[0]["data"]["preset"], "danger-full-access");
    assert_eq!(events[1]["data"]["mode"], "danger-full-access");
    assert_eq!(events[2]["data"]["policy"], "never");
    assert_eq!(events[7]["data"]["source"]["kind"], "user");
    assert_eq!(events[8]["data"]["source"]["kind"], "plugin");
    assert_eq!(
        events[8]["data"]["source"]["plugin"],
        "@deepseek-ai/dsh-system-prompt"
    );
    assert_eq!(events[9]["data"]["source"]["kind"], "fallback");
    assert_eq!(events[9]["data"]["title"], "ping the product path");
    assert_eq!(events[10]["data"]["reason"], "initial");
    let chunks: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "assistant/chunk")
        .filter_map(|event| event["data"]["chunk"]["type"].as_str())
        .collect();
    assert_eq!(chunks, ["block-start", "text-delta", "block-end", "finish"]);
    assert_eq!(
        events
            .iter()
            .find(|event| event["type"] == "assistant/chunk"
                && event["data"]["chunk"]["type"] == "finish")
            .and_then(|event| event["data"]["chunk"]["reason"]["kind"].as_str()),
        Some("stop")
    );
}

#[tokio::test]
async fn bash_turn_profile_types_and_payloads() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "bash",
                "arguments": "{\"command\":\"echo hello\"}"
            }
        },
        { "text": "done" }
    ]);
    let events = run_profile("run echo", replay_turns_overlay(turns)).await;
    assert_eq!(
        types_of(&events),
        BASH_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    let finish = events
        .iter()
        .find(|event| {
            event["type"] == "assistant/chunk" && event["data"]["chunk"]["type"] == "finish"
        })
        .expect("first finish");
    let reason: FinishReason =
        serde_json::from_value(finish["data"]["chunk"]["reason"].clone()).unwrap();
    assert_eq!(reason, FinishReason::ToolCalls);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(text.trim(), "hello");
}

#[tokio::test]
async fn glob_turn_profile_rereads_workspace() {
    let workspace = std::env::temp_dir().join(format!(
        "dsh-wave-e-glob-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("found.txt"), "x").unwrap();
    let path = workspace.to_string_lossy();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "glob",
                "arguments": format!("{{\"pattern\":\"*.txt\",\"path\":\"{path}\"}}")
            }
        },
        { "text": "listed" }
    ]);
    let events = run_profile("find txt", replay_turns_overlay(turns)).await;
    assert_eq!(
        types_of(&events),
        BASH_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("found.txt"), "glob result: {text}");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn str_replace_editor_turn_profile_rereads_file() {
    // `ctx.fs` is confined to the process cwd (the sandbox workspace root).
    let workspace = std::env::current_dir().unwrap().join("target").join(format!(
        "dsh-wave-e-editor-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let path = workspace.join("note.txt");
    let path_str = path.to_string_lossy();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "str_replace_editor",
                "arguments": format!("{{\"command\":\"create\",\"path\":\"{path_str}\",\"file_text\":\"from-editor\"}}")
            }
        },
        { "text": "created" }
    ]);
    let events = run_profile("create the note", replay_turns_overlay(turns)).await;
    assert_eq!(
        types_of(&events),
        BASH_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("New file created successfully"),
        "str_replace_editor result: {text}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-editor");
    let _ = std::fs::remove_dir_all(&workspace);
}

fn replay_turns_and_search(turns: Value, replay: Value) -> Vec<EntryPatch> {
    let mut patches = replay_turns_overlay(turns);
    let mut search = EntryPatch::replace("web-search-deepseek");
    search.config = Some(serde_json::json!({
        "apiKeyEnv": "DEEPSEEK_API_KEY",
        "replay": replay
    }));
    patches.push(search);
    patches
}

#[tokio::test]
async fn goal_turn_profile_writes_goal_change() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "create_goal",
                "arguments": "{\"objective\":\"ship the rust port\",\"max_goal_rounds\":2}"
            }
        },
        {
            "text": "",
            "tool": {
                "id": "c2",
                "name": "get_goal",
                "arguments": "{}"
            }
        },
        { "text": "created" },
        { "text": "continuing" }
    ]);
    let events = run_profile("track this", replay_turns_overlay(turns)).await;
    let types = types_of(&events);
    assert!(types.contains(&"goal/change".into()), "{types:?}");
    let create = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = create["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("\"activation\":\"armed\""), "{text}");
    let has_goal_round = events.iter().any(|event| {
        event["type"] == "user/message" && event["data"]["source"]["kind"] == "goal"
    });
    assert!(has_goal_round, "expected goal_round user/message");
}

#[tokio::test]
async fn web_search_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "web_search",
                "arguments": "{\"queries\":[\"rust\"]}"
            }
        },
        { "text": "cited" }
    ]);
    let events = run_profile(
        "search",
        replay_turns_and_search(
            turns,
            serde_json::json!({
                "content": "fixture answer",
                "sources": [{
                    "url": "https://example.test",
                    "title": "Example",
                    "snippet": "hello"
                }],
                "truncated": false
            }),
        ),
    )
    .await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("[Example](https://example.test)"), "{text}");
    assert!(text.contains("Cite the relevant URLs"), "{text}");
}

#[tokio::test]
async fn workflow_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "workflow",
                "arguments": "{\"script\":\"return {\\\"ok\\\":true}\",\"meta\":{\"name\":\"snapshot-flow\",\"description\":\"test\"}}"
            }
        },
        { "text": "done" }
    ]);
    let events = run_profile("run workflow", replay_turns_overlay(turns)).await;
    let types = types_of(&events);
    assert!(types.contains(&"tool-workflow/run-start".into()), "{types:?}");
    assert!(types.contains(&"tool-workflow/run-end".into()), "{types:?}");
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("workflow \"snapshot-flow\" completed (0 agents)."),
        "{text}"
    );
}

#[tokio::test]
async fn subagent_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "subagent",
                "arguments": "{\"description\":\"child\",\"prompt\":\"ping\",\"run_in_background\":false}"
            }
        },
        { "text": "child-done" },
        { "text": "parent-done" }
    ]);
    let events = run_profile("delegate", replay_turns_overlay(turns)).await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(text, "child-done");
}

#[tokio::test]
async fn spill_policy_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "bash",
                "arguments": "{\"command\":\"yes x | head -c 60000\"}"
            }
        },
        { "text": "spilled" }
    ]);
    let events = run_profile("huge", replay_turns_overlay(turns)).await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("Full formatted result stored at:"),
        "{text}"
    );
    assert!(text.contains("Omitted"), "{text}");
    assert!(text.len() <= 50_000, "{}", text.len());
    let locator = text
        .split("stored at: ")
        .nth(1)
        .and_then(|rest| rest.split(". Use read").next())
        .expect("locator");
    let spilled = std::fs::read_to_string(locator.trim()).expect("host reread");
    assert!(spilled.len() >= 50_000, "{}", spilled.len());
}

#[tokio::test]
async fn repeat_tool_reminder_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": { "id": "c1", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" }
        },
        {
            "text": "",
            "tool": { "id": "c2", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" }
        },
        {
            "text": "",
            "tool": { "id": "c3", "name": "bash", "arguments": "{\"command\":\"echo hi\"}" }
        },
        { "text": "stopped" }
    ]);
    let events = run_profile("loop", replay_turns_overlay(turns)).await;
    let notice = events.iter().find(|event| {
        event["type"] == "user/message"
            && event["data"]["source"]["plugin"] == "repeat-tool-reminder"
    });
    let notice = notice.expect("repeat-tool-reminder notice");
    assert_eq!(notice["data"]["source"]["form"], "notice");
    assert_eq!(notice["data"]["source"]["summary"], "bash × 3");
}

#[test]
fn typescript_headless_profile_types_are_known() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../examples/headless-agent/tests/snapshots/headless-profile/session.expected.jsonl"
    );
    let body = std::fs::read_to_string(path).expect("typescript fixture");
    let deferred = ["session", "session/title-llm-request"];
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).unwrap();
        let type_name = value.get("type").and_then(Value::as_str).unwrap_or("");
        if deferred.contains(&type_name) {
            continue;
        }
        assert!(
            dsh_session::is_known_session_event_type(type_name),
            "TypeScript fixture type `{type_name}` is missing from KNOWN_SESSION_EVENT_TYPES"
        );
        let _ = event_type_name(&SessionEventData::TurnStart { turn: 1 });
    }
    let _ = StreamChunk::text_stream("x");
}
