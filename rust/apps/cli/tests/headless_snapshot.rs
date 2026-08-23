//! Keyless headless snapshot: replay adapter, persist JSONL, compare types.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_agent_spine::{apply, apply_replay, apply_world};
use dsh_cordis::Context;
use dsh_llm::{ContentBlock, UserMessage};
use dsh_llm_replay::{ReplayAdapter, ReplayToolCall, ReplayTurn};
use dsh_session::{event_type_name, SessionEventData, SessionStore};
use std::sync::Arc;

fn event_types(session: &dsh_session::Session) -> Vec<String> {
    session
        .events()
        .into_iter()
        .map(|event| event_type_name(&event.data).to_string())
        .collect()
}

const TEXT_TURN_TYPES: &[&str] = &[
    "agent/inbox/spliced",
    "turn/start",
    "agent/inbox/spliced",
    "step/start",
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

const TOOL_TURN_TYPES: &[&str] = &[
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
async fn text_turn_snapshot() {
    let ctx = Context::new();
    apply_replay(&ctx, "pong").unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("ping"))
        .await
        .unwrap();
    assert_eq!(
        event_types(handle.agent.session().as_ref()),
        TEXT_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("pong")
    );
}

#[tokio::test]
async fn bash_turn_snapshot() {
    let root = std::env::temp_dir().join(format!("dsh-bash-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo hello"}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "done".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("run echo"))
        .await
        .unwrap();
    assert_eq!(
        event_types(handle.agent.session().as_ref()),
        TOOL_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("done")
    );
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert_eq!(text.trim(), "hello");
}

#[tokio::test]
async fn fs_edit_turn_snapshot() {
    let root = std::env::temp_dir().join(format!("dsh-fs-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("note.txt");
    let path_str = path.to_string_lossy();
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "write".into(),
                    arguments: format!(
                        r#"{{"file_path":"{}","content":"from-agent"}}"#,
                        path_str.replace('\\', "\\\\")
                    ),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "wrote".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("write the note"))
        .await
        .unwrap();
    assert_eq!(
        event_types(handle.agent.session().as_ref()),
        TOOL_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-agent");
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("wrote")
    );
}

#[tokio::test]
async fn glob_turn_snapshot() {
    let root = std::env::temp_dir().join(format!("dsh-glob-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("found.txt"), "x").unwrap();
    let path = root.to_string_lossy().replace('\\', "\\\\");
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "glob".into(),
                    arguments: format!(r#"{{"pattern":"*.txt","path":"{path}"}}"#),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "listed".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("find txt"))
        .await
        .unwrap();
    assert_eq!(
        event_types(handle.agent.session().as_ref()),
        TOOL_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("expected text"),
    };
    assert!(text.contains("found.txt"), "glob result: {text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn str_replace_editor_turn_snapshot() {
    let root = std::env::temp_dir().join(format!("dsh-editor-snap-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("note.txt");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "str_replace_editor".into(),
                    arguments: format!(
                        r#"{{"command":"create","path":"{path_str}","file_text":"from-editor"}}"#
                    ),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "created".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("create the note"))
        .await
        .unwrap();
    assert_eq!(
        event_types(handle.agent.session().as_ref()),
        TOOL_TURN_TYPES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-editor");
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("created")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn goal_turn_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "create_goal".into(),
                    arguments: r#"{"objective":"ship the rust port","max_goal_rounds":2}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c2".into(),
                    name: "get_goal".into(),
                    arguments: "{}".into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "created".into(),
                tool: None,
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "continuing".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("track this"))
        .await
        .unwrap();
    let types = event_types(handle.agent.session().as_ref());
    assert!(types.contains(&"goal/change".into()), "{types:?}");
    let results: Vec<_> = handle
        .agent
        .session()
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    let create = match &results[0].result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert!(create.contains("\"activation\":\"armed\""), "{create}");
    assert!(create.contains("ship the rust port"), "{create}");
    let has_goal_round = handle.agent.session().events().iter().any(|event| {
        matches!(
            &event.data,
            SessionEventData::UserMessage(message)
                if matches!(message.source, dsh_llm::MessageSource::Goal { .. })
        )
    });
    assert!(has_goal_round, "expected admitted goal_round");
}

#[tokio::test]
async fn web_search_turn_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "web_search".into(),
                    arguments: r#"{"queries":["rust"]}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "cited".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("search"))
        .await
        .unwrap();
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert!(text.contains("[Example](https://example.test)"), "{text}");
    assert!(text.contains("Cite the relevant URLs"), "{text}");
}

#[tokio::test]
async fn workflow_turn_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "workflow".into(),
                    arguments: r#"{"script":"return {\"ok\":true}","meta":{"name":"snapshot-flow","description":"test"}}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "done".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("run workflow"))
        .await
        .unwrap();
    let types = event_types(handle.agent.session().as_ref());
    assert!(
        types.contains(&"tool-workflow/run-start".into()),
        "{types:?}"
    );
    assert!(types.contains(&"tool-workflow/run-end".into()), "{types:?}");
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert!(
        text.contains("workflow \"snapshot-flow\" completed (0 agents)."),
        "{text}"
    );
}

#[tokio::test]
async fn subagent_turn_snapshot() {
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
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("delegate"))
        .await
        .unwrap();
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert_eq!(text, "child-done");
    let store = ctx.service::<SessionStore>().unwrap();
    let child_has_descriptor = store.live().iter().any(|session| {
        session
            .events()
            .iter()
            .any(|event| dsh_session::event_type_name(&event.data) == "subagent/descriptor")
    });
    assert!(child_has_descriptor, "child session missing descriptor");
    let child = store
        .live()
        .into_iter()
        .find(|session| session.header().parent_session.is_some())
        .expect("child session");
    assert_eq!(
        child.header().parent_session.as_ref(),
        Some(handle.agent.id())
    );
    assert_eq!(child.header().origin.as_deref(), Some("subagent"));
}

#[tokio::test]
async fn ralph_turn_snapshot() {
    let complete = r#"{"status":"complete","summary":"The objective is complete.","evidence":["All required gates pass."],"nextSteps":[],"blocker":""}"#;
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "ralph".into(),
                    arguments: r#"{"objective":"Finish the migration.","maxRounds":1}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: complete.into(),
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
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("run ralph"))
        .await
        .unwrap();
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert!(
        text.contains("Ralph worker reported completion after 1 round."),
        "{text}"
    );
    assert!(text.contains("All required gates pass."), "{text}");
    assert!(!result.is_error());
}

#[tokio::test]
async fn background_bash_job_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo hello","run_in_background":true}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c2".into(),
                    name: "job_output".into(),
                    arguments: r#"{"job_id":"bash-1","wait":true}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "done".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("bg echo"))
        .await
        .unwrap();
    let texts: Vec<String> = handle
        .agent
        .session()
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => match &message.result_blocks()[0] {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(texts[0], "started background job bash-1");
    assert!(texts[1].contains("hello"), "{}", texts[1]);
    assert!(
        texts[1].contains("[status: completed, exit code: 0]"),
        "{}",
        texts[1]
    );
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn repeat_tool_reminder_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo hi"}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c2".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo hi"}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c3".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"echo hi"}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "stopped".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("loop"))
        .await
        .unwrap();
    let notice = handle
        .agent
        .session()
        .events()
        .iter()
        .find_map(|event| match &event.data {
            SessionEventData::UserMessage(message) => match &message.source {
                dsh_llm::MessageSource::Plugin {
                    plugin,
                    form,
                    summary,
                    ..
                } if plugin == "repeat-tool-reminder" => {
                    Some((form.clone(), summary.clone(), message.clone()))
                }
                _ => None,
            },
            _ => None,
        });
    let (form, summary, message) = notice.expect("repeat-tool-reminder notice");
    assert_eq!(form.as_deref(), Some("notice"));
    assert_eq!(summary.as_deref(), Some("bash × 3"));
    let text = match &message.content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
    assert_eq!(text, dsh_repeat_tool_reminder::GENTLE_REMINDER);
}

#[tokio::test]
async fn spill_policy_turn_snapshot() {
    let ctx = Context::new();
    apply(
        &ctx,
        Arc::new(ReplayAdapter::new(vec![
            ReplayTurn {
                text: String::new(),
                tool: Some(ReplayToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"yes x | head -c 60000"}"#.into(),
                }),
                finish: None,
                error: None,
            },
            ReplayTurn {
                text: "spilled".into(),
                tool: None,
                finish: None,
                error: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, std::env::temp_dir().display().to_string()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .unwrap()
        .create(session)
        .unwrap();
    run_followup(handle.agent.as_ref(), UserMessage::text("huge"))
        .await
        .unwrap();
    let result = handle
        .agent
        .session()
        .events()
        .into_iter()
        .find_map(|event| match event.data {
            SessionEventData::ToolResult { message, .. } => Some(message),
            _ => None,
        })
        .expect("tool result");
    let text = match &result.result_blocks()[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("text"),
    };
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
async fn with_key_e2e_self_skips_without_secret() {
    if std::env::var("DEEPSEEK_API_KEY").is_err() {
        return;
    }
    let ctx = Context::new();
    apply_replay(&ctx, "pong").unwrap();
    assert!(ctx.has_service("llm"));
}
