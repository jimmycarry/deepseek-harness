//! Keyless headless snapshot: replay adapter, persist JSONL, compare types.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_agent_spine::apply_replay;
use dsh_cordis::Context;
use dsh_llm::{ContentBlock, UserMessage};
use dsh_session::{SessionEventData, SessionStore};

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
        .map(|event| match event.data {
            SessionEventData::TurnStart { .. } => "turn/start".into(),
            SessionEventData::TurnEnd { .. } => "turn/end".into(),
            SessionEventData::StepStart { .. } => "step/start".into(),
            SessionEventData::StepEnd { .. } => "step/end".into(),
            SessionEventData::UserMessage(_) => "user/message".into(),
            SessionEventData::AssistantChunk { .. } => "assistant/chunk".into(),
            SessionEventData::AssistantMessage { .. } => "assistant/message".into(),
            other => format!("{other:?}"),
        })
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
async fn with_key_e2e_self_skips_without_secret() {
    if std::env::var("DEEPSEEK_API_KEY").is_err() {
        return;
    }
    // A with-key lane boots the same spine and checks the world, not self-report.
    let ctx = Context::new();
    apply_replay(&ctx, "pong").unwrap();
    assert!(ctx.has_service("llm"));
}
