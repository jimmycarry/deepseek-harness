//! Profile-path snapshots: compose + mount + runner, then compare type sequence
//! and key payloads to the TypeScript headless-profile fixture vocabulary.

use dsh_agent::AgentRegistry;
use dsh_app_boot::{
    compose_profile, register_profile_plugins, shipped_bundles, PermissionPresetService,
};
use dsh_bundle_headless::HeadlessStartup;
use dsh_cordis::Context;
use dsh_cordis_loader::{Entry, EntryPatch, Loader};
use dsh_llm::{
    call_id, AssistantMessage, ContentBlock, FinishReason, StreamChunk, ToolResultMessage,
    UserMessage,
};
use dsh_session::{event_type_name, session_id, Session, SessionEventData, SessionStore, SurfaceOp};
use dsh_settings_file::SettingsRuntime;
use dsh_subagent::SubagentRuntime;
use dsh_tools::ToolRuntime;
use serde_json::Value;
use std::sync::Arc;

fn replay_text_overlay(text: &str) -> Vec<EntryPatch> {
    replay_overlay(serde_json::json!({ "text": text }))
}

fn replay_overlay(config: Value) -> Vec<EntryPatch> {
    let mut disable = EntryPatch::replace("llm-deepseek");
    disable.disabled = Some(Value::Bool(true));
    let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
    replay.config = Some(config);
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
    let (_ctx, session) = run_profile_host(task, overlay).await;
    events_of(&session)
}

/// Mount and drive the profile, keeping the host context alive so a test can
/// inspect other sessions in the catalog or call services after the run.
async fn run_profile_host(task: &str, overlay: Vec<EntryPatch>) -> (Context, Arc<Session>) {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-d-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    run_profile_host_in(&dir, task, overlay).await
}

fn mount_profile_in(dir: &std::path::Path, task: &str, overlay: Vec<EntryPatch>) -> Context {
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

async fn run_profile_host_in(
    dir: &std::path::Path,
    task: &str,
    overlay: Vec<EntryPatch>,
) -> (Context, Arc<Session>) {
    let ctx = mount_profile_in(dir, task, overlay);
    let session = dsh_bundle_headless::run_session(&ctx).await.unwrap();
    (ctx, session)
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
    "session/title-llm-request",
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
    "session/title-llm-request",
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
    assert_eq!(
        events[9]["data"]["messageSeqs"],
        serde_json::json!([events[7]["seq"]])
    );
    assert_eq!(events[10]["data"]["reason"], "initial");
    for event in &events {
        assert!(event["seq"].is_u64(), "envelope seq: {event}");
        assert!(event["time"].is_u64(), "envelope time: {event}");
    }
    let title_request = events
        .iter()
        .find(|event| event["type"] == "session/title-llm-request")
        .expect("title llm request");
    assert_eq!(
        title_request["data"]["titleProvider"],
        "session-title-first-prompt-llm"
    );
    assert_eq!(
        title_request["data"]["messageSeqs"],
        serde_json::json!([events[7]["seq"]])
    );
    assert_eq!(
        title_request["data"]["route"],
        serde_json::json!({
            "provider": events[10]["data"]["header"]["config"]["provider"],
            "model": events[10]["data"]["header"]["config"]["model"],
        })
    );
    let title_system = title_request["data"]["system"].as_str().unwrap_or("");
    assert!(
        title_system.starts_with(
            "Create a concise title for an AI coding-assistant session from the supplied human messages."
        ),
        "{title_system}"
    );
    assert!(
        title_system.ends_with("Aim for about 5 words in non-CJK languages or 10 CJK characters."),
        "{title_system}"
    );
    assert_eq!(title_request["data"]["maxTokens"], 64);
    let title_message = &title_request["data"]["messages"][0];
    assert_eq!(title_message["role"], "user");
    assert_eq!(title_message["source"]["kind"], "plugin");
    assert_eq!(title_message["source"]["plugin"], "dsh-session-title-llm");
    assert!(
        title_message["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .starts_with("Generate the session title from this JSON array of human messages:"),
        "{title_message}"
    );
    let chunk_seqs: Vec<Value> = events
        .iter()
        .filter(|event| event["type"] == "assistant/chunk")
        .map(|event| event["seq"].clone())
        .collect();
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant/message")
        .expect("assistant message");
    assert_eq!(message["sourceEventSeqs"], Value::Array(chunk_seqs));
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
async fn provider_rate_limit_retries_then_answers() {
    let events = run_profile(
        "ping the product path",
        replay_overlay(serde_json::json!({
            "turns": [
                {
                    "error": {
                        "message": "snapshot transient failure",
                        "code": "RATE_LIMIT",
                        "status": 429
                    }
                },
                { "text": "RETRY_OK" }
            ],
            "providers": [{
                "id": "deepseek-official",
                "retryPolicy": {
                    "mode": "normal",
                    "maxRetries": 1,
                    "retryableCodes": ["RATE_LIMIT"],
                    "backoff": {
                        "initialDelayMs": 1,
                        "maxDelayMs": 1,
                        "jitterRatio": 0
                    }
                }
            }]
        })),
    )
    .await;
    let types = types_of(&events);
    assert!(types.contains(&"llm/retry".to_string()), "{types:?}");
    assert!(
        types.contains(&"llm/retry-started".to_string()),
        "{types:?}"
    );
    let retry = events
        .iter()
        .find(|event| event["type"] == "llm/retry")
        .expect("llm/retry");
    assert_eq!(retry["data"]["provider"], "deepseek-official");
    assert_eq!(retry["data"]["mode"], "normal");
    assert_eq!(
        retry["data"]["policyKey"],
        "[\"normal\",1,[\"RATE_LIMIT\"],1,1,0]"
    );
    assert_eq!(retry["data"]["retry"], 1);
    assert_eq!(retry["data"]["maxRetries"], 1);
    assert_eq!(retry["data"]["delayMs"], 1);
    assert_eq!(retry["data"]["failure"]["code"], "RATE_LIMIT");
    assert_eq!(
        retry["data"]["failure"]["message"],
        "snapshot transient failure"
    );
    assert_eq!(retry["data"]["failure"]["status"], 429);
    let retry_index = types.iter().position(|name| name == "llm/retry").unwrap();
    let started_index = types
        .iter()
        .position(|name| name == "llm/retry-started")
        .unwrap();
    assert!(retry_index < started_index, "{types:?}");
    let message = events
        .iter()
        .rev()
        .find(|event| event["type"] == "assistant/message")
        .expect("assistant message");
    assert_eq!(
        message["data"]["message"]["content"][0]["text"]
            .as_str()
            .unwrap_or(""),
        "RETRY_OK"
    );
}

#[test]
fn replay_negative_max_retries_fails_at_mount() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-retry-neg-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("DSH_HOME", &dir);
    std::env::set_var("DSH_PERMISSION_MODE", "danger-full-access");
    let overlay = replay_overlay(serde_json::json!({
        "turns": [{ "text": "x" }],
        "providers": [{
            "id": "deepseek-official",
            "retryPolicy": { "mode": "normal", "maxRetries": -1 }
        }]
    }));
    let layers = shipped_bundles("headless").unwrap();
    let entries = compose_profile(&layers, &[], &[], &overlay).unwrap();
    let ctx = Context::new();
    ctx.provide(Arc::new(HeadlessStartup {
        task: "x".into(),
        cwd: Some(dir.to_string_lossy().into_owned()),
        resume_session_id: None,
    }))
    .unwrap();
    let loader = Loader::new();
    register_profile_plugins(&loader);
    let error = loader.mount(&ctx, &entries).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("maxRetries"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_instructions_baseline_is_model_visible() {
    let dir =
        std::env::temp_dir().join(format!("dsh-instr-{}-{}", std::process::id(), uuid_stamp()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("AGENTS.md"),
        "Prefer cargo test over ad-hoc scripts.",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let (_ctx, session) = run_profile_host_in(
        &dir,
        "reply with the word pong",
        replay_text_overlay("pong"),
    )
    .await;
    let events = events_of(&session);
    let instruction = events
        .iter()
        .find(|event| {
            event["type"] == "user/message"
                && event["data"]["source"]["kind"] == "agent-instructions"
        })
        .expect("agent-instructions baseline");
    assert_eq!(instruction["data"]["source"]["form"], "instructions");
    assert_eq!(instruction["data"]["source"]["baseline"], true);
    let text = instruction["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.starts_with("<system-reminder>\n"), "{text}");
    assert!(
        text.contains("The following workspace instructions may be relevant to your work."),
        "{text}"
    );
    assert!(text.contains("Instructions from: AGENTS.md"), "{text}");
    assert!(
        text.contains("Prefer cargo test over ad-hoc scripts."),
        "{text}"
    );
    assert!(text.ends_with("</system-reminder>"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(text.trim(), "hello");
    let call = events
        .iter()
        .find(|event| event["type"] == "tool/call")
        .expect("tool call");
    assert_eq!(result["sourceEventSeqs"], serde_json::json!([call["seq"]]));
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("found.txt"), "glob result: {text}");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn str_replace_editor_turn_profile_rereads_file() {
    // `ctx.fs` is confined to the process cwd (the sandbox workspace root).
    let workspace = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
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
    let text = create["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("\"activation\":\"armed\""), "{text}");
    let has_goal_round = events
        .iter()
        .any(|event| event["type"] == "user/message" && event["data"]["source"]["kind"] == "goal");
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
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
    assert!(
        types.contains(&"tool-workflow/run-start".into()),
        "{types:?}"
    );
    assert!(types.contains(&"tool-workflow/run-end".into()), "{types:?}");
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(text, "child-done");
}

#[tokio::test]
async fn ralph_turn_profile() {
    let complete = "{\"status\":\"complete\",\"summary\":\"The objective is complete.\",\"evidence\":[\"All required gates pass.\"],\"nextSteps\":[],\"blocker\":\"\"}";
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "ralph",
                "arguments": "{\"objective\":\"Finish the migration.\",\"maxRounds\":1}"
            }
        },
        { "text": complete },
        { "text": "parent-done" }
    ]);
    let events = run_profile("run ralph", replay_turns_overlay(turns)).await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("Ralph worker reported completion after 1 round."),
        "{text}"
    );
    assert!(text.contains("All required gates pass."), "{text}");
}

#[tokio::test]
async fn background_bash_job_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "bash",
                "arguments": "{\"command\":\"echo hello\",\"run_in_background\":true}"
            }
        },
        {
            "text": "",
            "tool": {
                "id": "c2",
                "name": "job_output",
                "arguments": "{\"job_id\":\"bash-1\",\"wait\":true}"
            }
        },
        { "text": "done" }
    ]);
    let events = run_profile("bg echo", replay_turns_overlay(turns)).await;
    let texts = tool_result_texts(&events);
    assert_eq!(texts[0].0, "started background job bash-1");
    assert!(!texts[0].1);
    assert!(texts[1].0.contains("hello"), "{}", texts[1].0);
    assert!(
        texts[1].0.contains("[status: completed, exit code: 0]"),
        "{}",
        texts[1].0
    );
}

/// Every tool/result in log order as (text, isError).
fn tool_result_texts(events: &[Value]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter(|event| event["type"] == "tool/result")
        .map(|event| {
            let block = &event["data"]["message"]["content"][0];
            (
                block["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                block["isError"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

/// The durable child id from the first `started subagent <id>` tool result.
fn started_child_id(events: &[Value]) -> String {
    let (text, is_error) = tool_result_texts(events)
        .into_iter()
        .next()
        .expect("delegation tool result");
    assert!(!is_error, "{text}");
    text.strip_prefix("started subagent ")
        .unwrap_or_else(|| panic!("unexpected delegation result: {text}"))
        .to_string()
}

fn turn_numbers(events: &[Value]) -> Vec<u64> {
    events
        .iter()
        .filter(|event| event["type"] == "turn/start")
        .filter_map(|event| event["data"]["turn"].as_u64())
        .collect()
}

#[tokio::test]
async fn continuable_settlement_turn_profile() {
    // The parent never polls: the runtime's settlement notice must carry the
    // child's closing message into the parent's second turn.
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "subagent",
                "arguments": "{\"description\":\"child task\",\"prompt\":\"Reply with exactly CHILD_RESULT\"}"
            }
        },
        { "text": "waiting" },
        { "text": "CHILD_RESULT" },
        { "text": "PARENT_RECEIVED_CHILD_RESULT" }
    ]);
    let (ctx, session) =
        run_profile_host("delegate in background", replay_turns_overlay(turns)).await;
    let events = events_of(&session);
    let child_id = started_child_id(&events);
    let notice = events
        .iter()
        .find(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "subagent-settled"
        })
        .expect("settlement notice");
    assert_eq!(notice["data"]["source"]["form"], "notice");
    assert_eq!(
        notice["data"]["source"]["senderSessionId"],
        Value::String(child_id.clone())
    );
    let summary = format!(
        "Background subagent {child_id} finished and will do no further work unless you send it more."
    );
    assert_eq!(
        notice["data"]["source"]["summary"],
        Value::String(summary.clone())
    );
    let content = notice["data"]["content"]
        .as_array()
        .expect("notice content");
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert_eq!(
        texts,
        [summary.as_str(), "Its closing message:", "CHILD_RESULT"]
    );
    assert_eq!(turn_numbers(&events), vec![1, 2]);
    assert_eq!(
        session.last_assistant_text().as_deref(),
        Some("PARENT_RECEIVED_CHILD_RESULT")
    );
    let store = ctx.get::<SessionStore>().expect("session store");
    let child = store.get(&session_id(&child_id)).expect("child session");
    let header = child.header().clone();
    assert_eq!(header.parent_session.as_ref(), Some(session.id()));
    assert_eq!(header.origin.as_deref(), Some("subagent"));
    assert_eq!(header.delegation_depth, 1);
    let child_events = events_of(&child);
    let descriptor = child_events
        .iter()
        .find(|event| event["type"] == "subagent/descriptor")
        .expect("child descriptor");
    assert_eq!(descriptor["data"]["version"], 2);
    assert_eq!(descriptor["data"]["mode"], "continuable");
    assert_eq!(descriptor["data"]["provider"], "spawn");
    assert_eq!(descriptor["data"]["label"], "child task");
    assert_eq!(turn_numbers(&child_events), vec![1]);
    assert_eq!(child.last_assistant_text().as_deref(), Some("CHILD_RESULT"));
    let sandbox: Vec<_> = child_events
        .iter()
        .filter(|event| event["type"] == "sandbox/mode")
        .collect();
    assert_eq!(sandbox.len(), 1, "{child_events:?}");
    assert_eq!(sandbox[0]["data"]["source"], "delegation");
    let approval: Vec<_> = child_events
        .iter()
        .filter(|event| event["type"] == "approval/policy")
        .collect();
    assert_eq!(approval.len(), 1);
    assert_eq!(
        approval[0]["data"],
        serde_json::json!({ "policy": "never", "source": "delegation" })
    );
    let context = child_events.iter().find(|event| {
        event["type"] == "user/message"
            && event["data"]["source"]["plugin"] == "@deepseek-ai/dsh-system-prompt"
    });
    let context_text = context
        .and_then(|event| event["data"]["content"][0]["text"].as_str())
        .unwrap_or("");
    assert!(
        context_text.contains("You are a delegated subagent"),
        "{context_text}"
    );
}

#[tokio::test]
async fn continuable_report_turn_profile() {
    // The child's scope-local report steers the parent; the unconditional
    // settlement notice follows, and the parent claims both in causal order.
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "subagent",
                "arguments": "{\"description\":\"reporting child\",\"prompt\":\"Report REPORT_PAYLOAD to your parent.\"}"
            }
        },
        { "text": "spawned" },
        {
            "text": "",
            "tool": {
                "id": "c2",
                "name": "report",
                "arguments": "{\"output\":\"REPORT_PAYLOAD\"}"
            }
        },
        { "text": "child closing" },
        { "text": "PARENT_GOT_REPORT" }
    ]);
    let (ctx, session) = run_profile_host("delegate and listen", replay_turns_overlay(turns)).await;
    let events = events_of(&session);
    let child_id = started_child_id(&events);
    let store = ctx.get::<SessionStore>().expect("session store");
    let child = store.get(&session_id(&child_id)).expect("child session");
    let child_events = events_of(&child);
    let (report_text, report_error) = tool_result_texts(&child_events)
        .into_iter()
        .next()
        .expect("child report result");
    assert!(!report_error, "{report_text}");
    assert!(
        report_text.starts_with("report accepted by the agent that started you as message "),
        "{report_text}"
    );
    let report = events
        .iter()
        .find(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "subagent-report"
        })
        .expect("parent-facing report");
    assert_eq!(report["data"]["source"]["form"], "relay");
    assert_eq!(
        report["data"]["source"]["senderSessionId"],
        Value::String(child_id.clone())
    );
    let report_texts: Vec<&str> = report["data"]["content"]
        .as_array()
        .expect("report content")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert_eq!(
        report_texts,
        [
            format!("Background subagent {child_id} reported:").as_str(),
            "REPORT_PAYLOAD"
        ]
    );
    let settled = events
        .iter()
        .find(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "subagent-settled"
        })
        .expect("settlement notice");
    assert!(
        report["seq"].as_u64() < settled["seq"].as_u64(),
        "report must precede the settlement notice"
    );
    assert_eq!(
        session.last_assistant_text().as_deref(),
        Some("PARENT_GOT_REPORT")
    );
    // The child-scoped report tool never reaches the root's schema set.
    let tools = ctx.get::<ToolRuntime>().expect("tool runtime");
    assert!(tools
        .schemas_for(Some(session.id().as_str()))
        .iter()
        .all(|schema| schema.name != "report"));
}

#[tokio::test]
async fn list_agents_and_unknown_send_message_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "subagent",
                "arguments": "{\"description\":\"child task\",\"prompt\":\"Reply with exactly CHILD_OK\"}"
            }
        },
        {
            "text": "",
            "tool": { "id": "c2", "name": "list_agents", "arguments": "{}" }
        },
        {
            "text": "",
            "tool": {
                "id": "c3",
                "name": "send_message",
                "arguments": "{\"subagent_id\":\"22222222-2222-4222-8222-222222222222\",\"message\":\"Please continue.\"}"
            }
        },
        { "text": "DONE" },
        { "text": "CHILD_OK" },
        { "text": "acknowledged" }
    ]);
    let events = run_profile("list then misaddress", replay_turns_overlay(turns)).await;
    let child_id = started_child_id(&events);
    let results = tool_result_texts(&events);
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(
        results[1],
        (format!("{child_id} [idle] — child task"), false)
    );
    assert_eq!(
        results[2],
        (
            "Error: subagent \"22222222-2222-4222-8222-222222222222\" is unavailable".to_string(),
            true
        )
    );
}

#[tokio::test]
async fn send_message_resumes_settled_child() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "subagent",
                "arguments": "{\"description\":\"child task\",\"prompt\":\"Reply with exactly CHILD_RESULT\"}"
            }
        },
        { "text": "waiting" },
        { "text": "CHILD_RESULT" },
        { "text": "SECOND_OK" }
    ]);
    let (ctx, session) =
        run_profile_host("delegate then continue", replay_turns_overlay(turns)).await;
    let child_id = started_child_id(&events_of(&session));
    let tools = ctx.get::<ToolRuntime>().expect("tool runtime");
    let delivered = tools
        .execute_for(
            &ctx,
            "send_message",
            serde_json::json!({
                "subagent_id": child_id,
                "message": "Now reply with exactly SECOND_OK."
            }),
            Some(session.id().as_str()),
        )
        .await
        .expect("send_message execution");
    assert!(!delivered.outcome.is_error);
    let ContentBlock::Text { text } = &delivered.outcome.content[0] else {
        panic!("send_message outcome must be text");
    };
    assert_eq!(
        text,
        &format!("message queued as the next turn for subagent {child_id}")
    );
    let subagents = ctx.get::<SubagentRuntime>().expect("subagent runtime");
    assert!(subagents.run_pending().await, "resumed child must run");
    let store = ctx.get::<SessionStore>().expect("session store");
    let child = store.get(&session_id(&child_id)).expect("child session");
    let child_events = events_of(&child);
    let followup = child_events
        .iter()
        .find(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "coordinator"
        })
        .expect("coordinator follow-up");
    assert_eq!(followup["data"]["source"]["form"], "relay");
    assert_eq!(
        followup["data"]["source"]["senderSessionId"],
        Value::String(session.id().as_str().to_string())
    );
    assert_eq!(
        followup["data"]["content"][0]["text"],
        "Now reply with exactly SECOND_OK."
    );
    // The resumed continuation extends the same log instead of restarting at 1.
    assert_eq!(turn_numbers(&child_events), vec![1, 2]);
    assert_eq!(child.last_assistant_text().as_deref(), Some("SECOND_OK"));
    let approval: Vec<_> = child_events
        .iter()
        .filter(|event| event["type"] == "approval/policy")
        .collect();
    assert_eq!(approval.len(), 1);
    assert_eq!(approval[0]["data"]["source"], "delegation");
    let sandbox: Vec<_> = child_events
        .iter()
        .filter(|event| event["type"] == "sandbox/mode")
        .collect();
    assert_eq!(sandbox.len(), 1);
    assert_eq!(sandbox[0]["data"]["source"], "delegation");
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
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.contains("Full formatted result stored at:"), "{text}");
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

#[tokio::test]
async fn todo_write_turn_profile() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "todo_write",
                "arguments": "{\"todos\":[{\"content\":\"ship\",\"status\":\"in_progress\"},{\"content\":\"test\",\"status\":\"pending\"}]}"
            }
        },
        { "text": "tracked" }
    ]);
    let events = run_profile("track the work", replay_turns_overlay(turns)).await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(
        text,
        "Updated todo list: 1 pending, 1 in progress, 0 completed."
    );
    let write = events
        .iter()
        .find(|event| event["type"] == "todo/write")
        .expect("todo/write event");
    assert!(
        write["ignorable"].is_null(),
        "todo/write is required-on-read"
    );
    assert_eq!(write["data"]["todos"][0]["content"], "ship");
    assert_eq!(write["data"]["todos"][0]["status"], "in_progress");
    assert_eq!(write["data"]["todos"][1]["status"], "pending");
}

#[tokio::test]
async fn skill_turn_profile_publishes_catalog_and_loads_skill() {
    let skills_dir = std::env::temp_dir().join(format!(
        "dsh-wave-h-skills-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    std::fs::create_dir_all(skills_dir.join("demo")).unwrap();
    std::fs::write(
        skills_dir.join("demo").join("SKILL.md"),
        "---\nname: demo\ndescription: demo skill for the snapshot\n---\nfollow the demo steps",
    )
    .unwrap();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "skill",
                "arguments": "{\"name\":\"demo\"}"
            }
        },
        { "text": "loaded" }
    ]);
    let mut overlay = replay_turns_overlay(turns);
    let mut skill_fs = EntryPatch::replace("skill-filesystem");
    skill_fs.config = Some(serde_json::json!({
        "includeDefaultRoots": false,
        "customSkillDirs": [skills_dir.to_string_lossy()]
    }));
    overlay.push(skill_fs);
    let events = run_profile("use the demo skill", overlay).await;
    let catalog = events
        .iter()
        .find(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "skill-catalog"
        })
        .expect("skill catalog publication");
    assert_eq!(catalog["data"]["source"]["form"], "catalog");
    assert_eq!(catalog["data"]["source"]["entries"][0]["name"], "demo");
    let body = catalog["data"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(body.starts_with("<system-reminder>"), "{body}");
    assert!(
        body.contains("- `demo`: demo skill for the snapshot"),
        "{body}"
    );
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(text.starts_with("<skill_content name=\"demo\">"), "{text}");
    assert!(
        text.contains("<skill_instructions>\nfollow the demo steps\n</skill_instructions>"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&skills_dir);
}

#[tokio::test]
async fn exit_plan_mode_outside_plan_mode_fails() {
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "exit_plan_mode",
                "arguments": "{\"plan\":\"# Plan\"}"
            }
        },
        { "text": "stayed" }
    ]);
    let events = run_profile("try to exit", replay_turns_overlay(turns)).await;
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    assert_eq!(result["data"]["message"]["content"][0]["isError"], true);
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert_eq!(text, "Error: exit_plan_mode is only available in plan mode");
}

#[tokio::test]
async fn feedback_command_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-m-feedback-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".anonymous-user-id"),
        "01234567-89ab-4cde-8f01-23456789abcd\n",
    )
    .unwrap();
    let ctx = mount_profile_in(&dir, "unused", vec![]);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let outcome = ctx
        .service::<dsh_commands::CommandRegistry>()
        .unwrap()
        .execute(session.as_ref(), "/feedback  the diff view is unreadable")
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.success);
    assert_eq!(
        outcome.text,
        format!(
            "Feedback recorded for session {}\nAnonymous user: 01234567-89ab-4cde-8f01-23456789abcd. Session sharing is disabled.",
            session.id()
        )
    );
    let events = events_of(&session);
    assert_eq!(
        types_of(&events),
        vec![
            "command/run".to_string(),
            "feedback/record".to_string(),
            "command/done".to_string()
        ]
    );
    assert!(events[0]["data"].get("args").is_none());
    assert_eq!(events[1]["data"]["text"], "the diff view is unreadable");
    assert!(session.derive_messages().is_empty());
}

#[tokio::test]
async fn write_requires_prior_observation_profile() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-wave-n-fs-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("note.txt");
    std::fs::write(&path, "old").unwrap();
    let ctx = mount_profile_in(&dir, "unused", vec![]);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let tools = ctx.service::<ToolRuntime>().unwrap();
    let path_str = path.to_string_lossy().into_owned();
    let denied = tools
        .execute_for(
            &ctx,
            "write",
            serde_json::json!({ "file_path": path_str, "content": "new" }),
            Some(session.id().as_str()),
        )
        .await
        .unwrap();
    assert!(denied.outcome.is_error);
    let denied_text = match &denied.outcome.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        _ => "",
    };
    assert!(
        denied_text.contains("without reading it first"),
        "{denied_text}"
    );
    assert!(
        denied_text.contains("read the file, then retry"),
        "{denied_text}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    let read = tools
        .execute_for(
            &ctx,
            "read",
            serde_json::json!({ "file_path": path_str }),
            Some(session.id().as_str()),
        )
        .await
        .unwrap();
    assert!(!read.outcome.is_error);
    let wrote = tools
        .execute_for(
            &ctx,
            "write",
            serde_json::json!({ "file_path": path_str, "content": "new" }),
            Some(session.id().as_str()),
        )
        .await
        .unwrap();
    assert!(!wrote.outcome.is_error, "{:?}", wrote.outcome.content);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    let edited = tools
        .execute_for(
            &ctx,
            "edit",
            serde_json::json!({
                "file_path": path_str,
                "old_string": "new",
                "new_string": "edited"
            }),
            Some(session.id().as_str()),
        )
        .await
        .unwrap();
    assert!(!edited.outcome.is_error, "{:?}", edited.outcome.content);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn permission_read_only_denies_write() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-perm-ro-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = mount_profile_in(&dir, "unused", vec![]);
    let session = ctx
        .service::<SessionStore>()
        .unwrap()
        .create_in(Some(dir.to_string_lossy().into_owned()));
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    ctx.service::<PermissionPresetService>()
        .unwrap()
        .pin_initial(handle.agent.session().as_ref())
        .unwrap();
    let commands = ctx.service::<dsh_commands::CommandRegistry>().unwrap();
    let current = commands
        .execute(handle.agent.session().as_ref(), "/permission")
        .await
        .unwrap()
        .unwrap();
    assert!(current.success, "{}", current.text);
    assert_eq!(
        current.text,
        "current preset danger-full-access (available: read-only, workspace-write, danger-full-access)"
    );
    let switched = commands
        .execute(handle.agent.session().as_ref(), "/permission read-only")
        .await
        .unwrap()
        .unwrap();
    assert!(switched.success, "{}", switched.text);
    assert_eq!(switched.text, "preset read-only");
    let unknown = commands
        .execute(handle.agent.session().as_ref(), "/permission nope")
        .await
        .unwrap()
        .unwrap();
    assert!(!unknown.success);
    assert_eq!(
        unknown.text,
        "unknown preset \"nope\" (available: read-only, workspace-write, danger-full-access)"
    );
    let path = dir.join("fresh.txt");
    let path_str = path.to_string_lossy().into_owned();
    let tools = ctx.service::<ToolRuntime>().unwrap();
    let denied = tools
        .execute_for(
            &ctx,
            "write",
            serde_json::json!({ "file_path": path_str, "content": "secret" }),
            Some(handle.agent.session().id().as_str()),
        )
        .await
        .unwrap();
    assert!(denied.outcome.is_error);
    let denied_text = match &denied.outcome.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        _ => "",
    };
    assert!(
        denied_text.contains("[sandbox: file access denied under read-only mode]"),
        "{denied_text}"
    );
    assert!(
        denied_text.contains("retry this exact operation once with sandbox_permissions"),
        "{denied_text}"
    );
    assert!(!path.exists(), "read-only write must not create the file");
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sandbox_escalation_grants_a_read_only_write() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-esc-write-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = mount_profile_in(&dir, "unused", vec![]);
    ctx.on_waterfall("approval/request", |_payload, _next| {
        serde_json::json!("allowed-once")
    })
    .unwrap();
    let session = ctx
        .service::<SessionStore>()
        .unwrap()
        .create_in(Some(dir.to_string_lossy().into_owned()));
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    ctx.service::<PermissionPresetService>()
        .unwrap()
        .pin_initial(handle.agent.session().as_ref())
        .unwrap();
    let commands = ctx.service::<dsh_commands::CommandRegistry>().unwrap();
    let switched = commands
        .execute(handle.agent.session().as_ref(), "/permission read-only")
        .await
        .unwrap()
        .unwrap();
    assert!(switched.success, "{}", switched.text);
    session
        .append(SessionEventData::TurnStart { turn: 1 }, None)
        .unwrap();
    let path = dir.join("granted.txt");
    let path_str = path.to_string_lossy().into_owned();
    let tools = ctx.service::<ToolRuntime>().unwrap();
    let missing = tools
        .execute_for(
            &ctx,
            "write",
            serde_json::json!({
                "file_path": path_str,
                "content": "secret",
                "sandbox_permissions": "workspace-write"
            }),
            Some(handle.agent.session().id().as_str()),
        )
        .await
        .unwrap();
    let missing_text = match &missing.outcome.content[0] {
        ContentBlock::Text { text } => text.as_str(),
        _ => "",
    };
    assert!(
        missing_text.contains("sandbox_permissions requires a justification"),
        "{missing_text}"
    );
    assert!(!path.exists());
    let granted = tools
        .execute_for(
            &ctx,
            "write",
            serde_json::json!({
                "file_path": path_str,
                "content": "secret",
                "sandbox_permissions": "workspace-write",
                "justification": "the snapshot needs a workspace write"
            }),
            Some(handle.agent.session().id().as_str()),
        )
        .await
        .unwrap();
    assert!(
        !granted.outcome.is_error,
        "{:?}",
        granted.outcome.content
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn headless_runner_executes_slash_feedback() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-wave-o-slash-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".anonymous-user-id"),
        "01234567-89ab-4cde-8f01-23456789abcd\n",
    )
    .unwrap();
    let (_ctx, session) = run_profile_host_in(
        &dir,
        "/feedback the runner intercepted this",
        vec![],
    )
    .await;
    let events = events_of(&session);
    let types = types_of(&events);
    assert!(
        types.iter().any(|name| name == "command/run"),
        "{types:?}"
    );
    assert!(
        types.iter().any(|name| name == "feedback/record"),
        "{types:?}"
    );
    assert!(
        types.iter().any(|name| name == "command/done"),
        "{types:?}"
    );
    assert!(!types.iter().any(|name| name == "user/message"));
    assert!(!types.iter().any(|name| name == "assistant/message"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn agent_instructions_update_after_write() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-instr-write-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("AGENTS.md"), "old workspace rule").unwrap();
    let path = dir.join("AGENTS.md");
    let path_str = path.to_string_lossy();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "read",
                "arguments": format!("{{\"file_path\":{}}}", serde_json::to_string(&*path_str).unwrap())
            }
        },
        {
            "text": "",
            "tool": {
                "id": "c2",
                "name": "write",
                "arguments": format!("{{\"file_path\":{},\"content\":\"new workspace rule\"}}", serde_json::to_string(&*path_str).unwrap())
            }
        },
        { "text": "done" }
    ]);
    let (_ctx, session) = run_profile_host_in(&dir, "update the instructions", replay_turns_overlay(turns)).await;
    let events = events_of(&session);
    let instructions: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "user/message"
                && event["data"]["source"]["kind"] == "agent-instructions"
        })
        .collect();
    assert!(
        instructions.len() >= 2,
        "expected baseline plus file-touch update, got {instructions:?}"
    );
    assert_eq!(instructions[0]["data"]["source"]["baseline"], true);
    let first = instructions[0]["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(first.contains("old workspace rule"), "{first}");
    let update = instructions
        .iter()
        .find(|event| event["data"]["source"].get("baseline").is_none())
        .expect("file-touch update");
    let text = update["data"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Updated instructions from: AGENTS.md"),
        "{text}"
    );
    assert!(text.contains("new workspace rule"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn skill_catalog_replaces_after_new_skill_file() {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "dsh-skill-update-{}-{}",
            std::process::id(),
            uuid_stamp()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    let skills_dir = dir.join("skills");
    std::fs::create_dir_all(skills_dir.join("demo")).unwrap();
    std::fs::write(
        skills_dir.join("demo").join("SKILL.md"),
        "---\nname: demo\ndescription: demo skill for the snapshot\n---\nfollow the demo steps",
    )
    .unwrap();
    std::fs::create_dir_all(skills_dir.join("extra")).unwrap();
    let extra = skills_dir.join("extra").join("SKILL.md");
    let extra_str = extra.to_string_lossy();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "write",
                "arguments": format!(
                    "{{\"file_path\":{},\"content\":\"---\\nname: extra\\ndescription: extra skill\\n---\\nextra body\"}}",
                    serde_json::to_string(&*extra_str).unwrap()
                )
            }
        },
        { "text": "done" }
    ]);
    let mut overlay = replay_turns_overlay(turns);
    let mut skill_fs = EntryPatch::replace("skill-filesystem");
    skill_fs.config = Some(serde_json::json!({
        "includeDefaultRoots": false,
        "customSkillDirs": [skills_dir.to_string_lossy()]
    }));
    overlay.push(skill_fs);
    let (_ctx, session) = run_profile_host_in(&dir, "add a skill", overlay).await;
    let events = events_of(&session);
    let catalogs: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "user/message" && event["data"]["source"]["kind"] == "skill-catalog"
        })
        .collect();
    assert!(
        catalogs.len() >= 2,
        "expected first catalog plus replacement, got {catalogs:?}"
    );
    assert!(catalogs[0]["data"]["source"].get("update").is_none());
    let update = catalogs
        .iter()
        .find(|event| event["data"]["source"]["update"] == true)
        .expect("replacement catalog");
    let body = update["data"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        body.contains("This complete catalog replaces every earlier available-skills list"),
        "{body}"
    );
    assert!(body.contains("- `extra`: extra skill"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_file_reloads_after_external_edit() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-p-settings-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.yaml"),
        "llm-deepseek:\n  baseURL: https://first.test\n",
    )
    .unwrap();
    let mut overlay = Vec::new();
    let mut settings_row = EntryPatch::replace("settings");
    settings_row.config = Some(serde_json::json!({ "debounceMs": 0 }));
    overlay.push(settings_row);
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let settings = ctx.service::<SettingsRuntime>().unwrap();
    assert_eq!(
        settings
            .section("llm-deepseek")
            .and_then(|value| value.get("baseURL").cloned()),
        Some(serde_json::json!("https://first.test"))
    );
    std::fs::write(
        dir.join("settings.yaml"),
        "llm-deepseek:\n  baseURL: https://second.test\n",
    )
    .unwrap();
    assert_eq!(
        settings
            .section("llm-deepseek")
            .and_then(|value| value.get("baseURL").cloned()),
        Some(serde_json::json!("https://second.test"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn bash_injects_dsh_env_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-r-env-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turns = serde_json::json!([
        {
            "text": "",
            "tool": {
                "id": "c1",
                "name": "bash",
                "arguments": "{\"command\":\"printf '%s\\n' DSH_HOME=$DSH_HOME DSH_SHELL=$DSH_SHELL DSH_SESSION_ID=$DSH_SESSION_ID\"}"
            }
        },
        { "text": "done" }
    ]);
    let (_ctx, session) = run_profile_host_in(&dir, "echo env", replay_turns_overlay(turns)).await;
    let events = events_of(&session);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool/result")
        .expect("tool result");
    let text = result["data"]["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains(&format!("DSH_HOME={}", dir.display())),
        "{text}"
    );
    assert!(text.contains("DSH_SHELL=1"), "{text}");
    assert!(
        text.lines().any(|line| {
            line.starts_with("DSH_SESSION_ID=") && line.len() > "DSH_SESSION_ID=".len()
        }),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_file_persists_comment_preserving_update() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-s-persist-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.yaml"),
        "# personal settings\nllm-deepseek:\n  baseURL: https://first.test  # lab gateway\n",
    )
    .unwrap();
    let mut overlay = Vec::new();
    let mut settings_row = EntryPatch::replace("settings");
    settings_row.config = Some(serde_json::json!({ "watch": false }));
    overlay.push(settings_row);
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let settings = ctx.service::<SettingsRuntime>().unwrap();
    settings
        .update(
            "llm-deepseek",
            &serde_json::json!({ "baseURL": "https://second.test" }),
        )
        .unwrap();
    let written = std::fs::read_to_string(dir.join("settings.yaml")).unwrap();
    assert!(written.contains("# personal settings"), "{written}");
    assert!(written.contains("# lab gateway"), "{written}");
    assert!(written.contains("https://second.test"), "{written}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_command_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-q-compact-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut overlay = replay_overlay(serde_json::json!({
        "text": "pong",
        "auxiliary": {
            "compaction": "## Primary Request and Intent\n- keep the four notes"
        }
    }));
    let mut compact = EntryPatch::replace("compaction-basic");
    compact.config = Some(serde_json::json!({
        "modelPolicies": [{
            "provider": "replay",
            "model": "script",
            "maxTokens": 222,
            "summarizationProvider": "replay",
            "summarizationModel": "script"
        }]
    }));
    overlay.push(compact);
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let _handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    session
        .append(
            SessionEventData::RequestContext {
                provider: "replay".into(),
                model: "script".into(),
                context_window: None,
            },
            None,
        )
        .unwrap();
    for label in ["alpha", "bravo", "charlie", "delta"] {
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text(format!(
                    "{label} {}",
                    "context ".repeat(40)
                ))),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }
    let outcome = ctx
        .service::<dsh_commands::CommandRegistry>()
        .unwrap()
        .execute(session.as_ref(), "/compact")
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.success, "{}", outcome.text);
    assert!(
        outcome.text.starts_with("Compacted ") && outcome.text.contains(" history items (~"),
        "{}",
        outcome.text
    );
    let events = events_of(&session);
    let types = types_of(&events);
    for name in [
        "command/run",
        "compaction/start",
        "compaction/summary",
        "compaction/end",
        "command/done",
    ] {
        assert!(types.iter().any(|found| found == name), "{types:?}");
    }
    let summary = events
        .iter()
        .find(|event| event["type"] == "compaction/summary")
        .expect("compaction/summary");
    assert_eq!(
        outcome.source_event_seq,
        summary["seq"].as_u64(),
        "{outcome:?} {summary}"
    );
    let done = events
        .iter()
        .find(|event| event["type"] == "command/done")
        .expect("command/done");
    assert_eq!(done["data"]["sourceEventSeq"], summary["seq"]);
    assert_eq!(summary["data"]["provider"], "replay");
    assert_eq!(summary["data"]["model"], "script");
    assert_eq!(summary["data"]["llmStreamCall"], true);
    assert_eq!(summary["data"]["maxTokens"], 222);
    assert_eq!(
        summary["data"]["summary"],
        serde_json::json!([{
            "type": "text",
            "text": "## Primary Request and Intent\n- keep the four notes"
        }])
    );
    assert_eq!(
        summary["data"]["rawOutput"],
        summary["data"]["summary"]
    );
    let checkpoint = session
        .derive_messages()
        .into_iter()
        .find_map(|message| match message {
            dsh_llm::Message::User(user)
                if user.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text } if text.contains("<compacted-summary>")
                )) =>
            {
                Some(
                    user.content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                )
            }
            _ => None,
        })
        .expect("checkpoint");
    assert!(
        checkpoint.contains("Continue the task directly from the messages that follow"),
        "{checkpoint}"
    );
    assert!(
        checkpoint.contains("keep the four notes"),
        "{checkpoint}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_preserves_tool_pairing_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-q-compact-pair-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let overlay = replay_overlay(serde_json::json!({
        "text": "pong",
        "auxiliary": {
            "compaction": "keep the closed tool step"
        }
    }));
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let _handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    session
        .append(
            SessionEventData::RequestContext {
                provider: "replay".into(),
                model: "script".into(),
                context_window: None,
            },
            None,
        )
        .unwrap();
    for label in ["alpha", "bravo"] {
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text(format!(
                    "{label} {}",
                    "context ".repeat(40)
                ))),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }
    session
        .append(
            SessionEventData::AssistantMessage {
                turn: 1,
                step: 1,
                message: AssistantMessage::model(
                    vec![ContentBlock::ToolCall {
                        id: call_id("c1"),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    "replay",
                    "script",
                ),
                usage: None,
            },
            Some(SurfaceOp::append()),
        )
        .unwrap();
    session
        .append(
            SessionEventData::ToolResult {
                turn: 1,
                step: 1,
                message: ToolResultMessage::new(
                    call_id("c1"),
                    vec![ContentBlock::text("done")],
                    false,
                ),
            },
            Some(SurfaceOp::append()),
        )
        .unwrap();
    let outcome = ctx
        .service::<dsh_commands::CommandRegistry>()
        .unwrap()
        .execute(session.as_ref(), "/compact")
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.success, "{}", outcome.text);
    let mut calls = std::collections::BTreeSet::new();
    for message in session.derive_messages() {
        match message {
            dsh_llm::Message::Assistant(assistant) => {
                for block in assistant.content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        calls.insert(id.as_str().to_string());
                    }
                }
            }
            dsh_llm::Message::Tool(tool) => {
                let id = tool.tool_call_id().expect("call id");
                assert!(calls.contains(id), "orphaned tool result {id}");
            }
            _ => {}
        }
    }
    assert!(calls.contains("c1"), "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn overflow_recovery_retries_after_compact_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-q-overflow-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let overlay = replay_overlay(serde_json::json!({
        "turns": [
            {
                "error": {
                    "message": "context overflow",
                    "code": "CONTEXT_WINDOW_EXCEEDED"
                }
            },
            { "text": "recovered after compact" }
        ],
        "auxiliary": {
            "compaction": "## Primary Request and Intent\n- keep going after overflow"
        }
    }));
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    for label in ["alpha", "bravo", "charlie", "delta"] {
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text(format!(
                    "{label} {}",
                    "context ".repeat(40)
                ))),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }
    dsh_agent_loop::run_followup(
        handle.agent.as_ref(),
        UserMessage::text("continue after overflow"),
    )
    .await
    .unwrap();
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("recovered after compact")
    );
    let events = events_of(&session);
    let types = types_of(&events);
    for name in ["compaction/start", "compaction/summary", "compaction/end"] {
        assert!(types.iter().any(|found| found == name), "{types:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pressure_uses_adapter_context_window_profile() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-wave-q-pressure-{}-{}",
        std::process::id(),
        uuid_stamp()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut overlay = replay_overlay(serde_json::json!({
        "text": "pong",
        "auxiliary": {
            "compaction": "## Primary Request and Intent\n- keep going under pressure"
        },
        "providers": [{
            "id": "replay",
            "models": [{
                "id": "script",
                "contextWindow": 400
            }]
        }]
    }));
    let mut compact = EntryPatch::replace("compaction-basic");
    compact.config = Some(serde_json::json!({
        "thresholdRatio": 0.5,
        "retainRatio": 0.1
    }));
    overlay.push(compact);
    let ctx = mount_profile_in(&dir, "unused", overlay);
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let _handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(Arc::clone(&session))
        .unwrap();
    session
        .append(
            SessionEventData::RequestHeader {
                header: serde_json::json!({
                    "config": { "provider": "replay", "model": "script" }
                }),
                reason: "initial".into(),
            },
            None,
        )
        .unwrap();
    for label in ["alpha", "bravo", "charlie", "delta"] {
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text(format!(
                    "{label} {}",
                    "context ".repeat(40)
                ))),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }
    let _ = ctx
        .waterfall(
            "agent/pre-step",
            serde_json::json!({ "sessionId": session.id().as_str() }),
            |payload| payload,
        )
        .unwrap();
    let events = events_of(&session);
    let types = types_of(&events);
    for name in ["compaction/start", "compaction/summary", "compaction/end"] {
        assert!(types.iter().any(|found| found == name), "{types:?}");
    }
    assert!(
        !events.iter().any(|event| {
            event["type"] == "request/context"
                && event["data"].get("contextWindow").is_some()
                && event["data"]["contextWindow"] != Value::Null
        }),
        "pressure must come from resolveModelInfo, not a logged contextWindow"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn typescript_headless_profile_types_are_known() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../examples/headless-agent/tests/snapshots/headless-profile/session.expected.jsonl"
    );
    let body = std::fs::read_to_string(path).expect("typescript fixture");
    let deferred = ["session"];
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
