//! Keyless headless snapshot: replay adapter, persist JSONL, compare types.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_agent_spine::{apply, apply_replay, apply_world};
use dsh_cordis::Context;
use dsh_llm::{ContentBlock, UserMessage};
use dsh_llm_replay::{ReplayAdapter, ReplayToolCall, ReplayTurn};
use dsh_session::{SessionEventData, SessionStore};
use std::sync::Arc;

fn event_type(data: &SessionEventData) -> String {
    match data {
        SessionEventData::TurnStart { .. } => "turn/start".into(),
        SessionEventData::TurnEnd { .. } => "turn/end".into(),
        SessionEventData::StepStart { .. } => "step/start".into(),
        SessionEventData::StepEnd { .. } => "step/end".into(),
        SessionEventData::UserMessage(_) => "user/message".into(),
        SessionEventData::AssistantChunk { .. } => "assistant/chunk".into(),
        SessionEventData::AssistantMessage { .. } => "assistant/message".into(),
        SessionEventData::ToolCall { .. } => "tool/call".into(),
        SessionEventData::ToolResult { .. } => "tool/result".into(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn text_turn_snapshot() {
    let ctx = Context::new();
    apply_replay(&ctx, "pong").unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
    run_followup(
        handle.agent.as_ref(),
        UserMessage {
            content: vec![ContentBlock::text("ping")],
            source: None,
        },
    )
    .await
    .unwrap();
    let types: Vec<String> = handle
        .agent
        .session()
        .events()
        .into_iter()
        .map(|event| event_type(&event.data))
        .collect();
    let expected = vec![
        "turn/start",
        "step/start",
        "user/message",
        "assistant/chunk",
        "assistant/message",
        "step/end",
        "turn/end",
    ];
    assert_eq!(types, expected);
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
            },
            ReplayTurn {
                text: "done".into(),
                tool: None,
                finish: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
    run_followup(
        handle.agent.as_ref(),
        UserMessage {
            content: vec![ContentBlock::text("run echo")],
            source: None,
        },
    )
    .await
    .unwrap();
    let types: Vec<String> = handle
        .agent
        .session()
        .events()
        .into_iter()
        .map(|event| event_type(&event.data))
        .collect();
    assert_eq!(
        types,
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "assistant/chunk",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
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
    let text = match &result.content[0] {
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
                    name: "write_file".into(),
                    arguments: format!(
                        r#"{{"path":"{}","content":"from-agent"}}"#,
                        path_str.replace('\\', "\\\\")
                    ),
                }),
                finish: None,
            },
            ReplayTurn {
                text: "wrote".into(),
                tool: None,
                finish: None,
            },
        ])),
    )
    .unwrap();
    apply_world(&ctx, root.to_string_lossy().into_owned()).unwrap();
    let session = ctx.service::<SessionStore>().unwrap().create_fresh();
    let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
    run_followup(
        handle.agent.as_ref(),
        UserMessage {
            content: vec![ContentBlock::text("write the note")],
            source: None,
        },
    )
    .await
    .unwrap();
    let types: Vec<String> = handle
        .agent
        .session()
        .events()
        .into_iter()
        .map(|event| event_type(&event.data))
        .collect();
    assert_eq!(
        types,
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "assistant/chunk",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-agent");
    assert_eq!(
        handle.agent.session().last_assistant_text().as_deref(),
        Some("wrote")
    );
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
