//! dsh-headless patch layer plus the one-shot startup and runner plugins.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_cordis_loader::{parse_patch_list, EntryPatch};
use dsh_llm::UserMessage;
use dsh_session::{append_session_knobs, Session, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use serde_json::Value;
use std::sync::Arc;

/// Shipped bundle identity.
pub fn name() -> &'static str {
    "dsh-bundle-headless"
}

/// Embedded `cordis.patch.yml` text.
pub fn patch_yaml() -> &'static str {
    include_str!("../cordis.patch.yml")
}

/// Patches from the shipped file: replace shared rows, then insert the runner.
pub fn patches() -> Vec<EntryPatch> {
    parse_patch_list(patch_yaml()).expect("shipped dsh-headless patch")
}

/// Task published by `@deepseek-ai/dsh-headless/startup` (`ctx.headlessStartup`).
pub struct HeadlessStartup {
    /// Positional task text.
    pub task: String,
}

impl Service for HeadlessStartup {
    const KEY: &'static str = "headlessStartup";
}

/// Provide `ctx.headlessStartup` from config.task or an already-mounted value.
pub fn apply_startup(ctx: &Context, config: Option<Value>) -> Result<()> {
    if ctx.has_service(HeadlessStartup::KEY) {
        return Ok(());
    }
    let task = config
        .as_ref()
        .and_then(|value| value.get("task"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            CordisError::plugin("headless-startup: a task is required, for example: dsh --profile headless \"run the tests\"")
        })?;
    if task.trim().is_empty() {
        return Err(CordisError::plugin(
            "headless-startup: a task is required, for example: dsh --profile headless \"run the tests\"",
        ));
    }
    ctx.provide(Arc::new(HeadlessStartup { task }))?;
    Ok(())
}

/// Record that the runner row mounted. The launcher drives the turn after the tree is up.
pub fn apply_runner(_ctx: &Context, _config: Option<Value>) -> Result<()> {
    Ok(())
}

/// Create an Agent, drive `task`, print the last assistant text.
pub async fn run(ctx: &Context) -> std::result::Result<(), String> {
    let session = run_session(ctx).await?;
    if let Some(text) = session.last_assistant_text() {
        println!("{text}");
    }
    Ok(())
}

/// Drive the positional task and return the live session after flush.
pub async fn run_session(ctx: &Context) -> std::result::Result<Arc<Session>, String> {
    let task = ctx
        .service::<HeadlessStartup>()
        .map_err(|error| error.to_string())?
        .task
        .clone();
    let session = ctx
        .service::<SessionStore>()
        .map_err(|error| error.to_string())?
        .create_fresh();
    let handle = ctx
        .service::<AgentRegistry>()
        .map_err(|error| error.to_string())?
        .create(session)
        .map_err(|error| error.to_string())?;
    let mode = std::env::var("DSH_PERMISSION_MODE").unwrap_or_else(|_| "workspace-write".into());
    let policy = if mode == "danger-full-access" {
        "never"
    } else {
        "ask"
    };
    append_session_knobs(handle.agent.session().as_ref(), &mode, &mode, policy)
        .map_err(|error| error.to_string())?;
    run_followup(handle.agent.as_ref(), UserMessage::text(task))
        .await
        .map_err(|error| error.to_string())?;
    // Continuable background children accepted work during the root turn.
    // Drive them to settlement — each settlement notice wakes the root — and
    // run every root turn those notices open, until the whole tree is quiet.
    if let Some(subagents) = ctx.get::<dsh_subagent::SubagentRuntime>() {
        loop {
            let ran = subagents.run_pending().await;
            if handle.agent.inbox().has_pending() {
                handle
                    .agent
                    .run()
                    .await
                    .map_err(|error| error.to_string())?;
                continue;
            }
            if !ran {
                break;
            }
        }
    }
    if let Some(persistence) = ctx.get::<PersistenceRuntime>() {
        persistence
            .save(handle.agent.session().as_ref())
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(handle.agent.session())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_role() {
        assert!(!name().is_empty());
        assert!(patch_yaml().contains("id: headless-runner"));
        let patches = patches();
        assert!(patches.iter().any(|patch| {
            patch.insert.as_ref().is_some_and(|rows| {
                rows.iter()
                    .any(|entry| entry.id.as_deref() == Some("headless-runner"))
            })
        }));
        assert!(patches.iter().any(|patch| {
            patch.id.as_deref() == Some("hmr")
                && patch.disabled.as_ref().and_then(|value| value.as_bool()) == Some(true)
        }));
    }

    #[test]
    fn startup_requires_a_task() {
        let ctx = Context::new();
        let err = apply_startup(&ctx, None).unwrap_err();
        assert!(err.to_string().contains("task is required"));
    }

    #[test]
    fn startup_provides_the_service() {
        let ctx = Context::new();
        apply_startup(&ctx, Some(serde_json::json!({"task": "ping"}))).unwrap();
        assert_eq!(ctx.service::<HeadlessStartup>().unwrap().task, "ping");
    }
}
