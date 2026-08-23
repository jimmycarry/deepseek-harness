//! Durability checkpoints before each model request and top-level tool
//! dispatch (`session-checkpoint-policy`).
//!
//! Flush rejects fail closed: the adapter never runs, and a top-level tool
//! body never starts. A missing persistence backend is a successful no-op.

use dsh_agent::AgentRegistry;
use dsh_cordis::Context;
use dsh_session::{session_id, Session, SessionStore};
use dsh_session_persistence::PersistenceRuntime;
use serde_json::Value;
use std::sync::Arc;

/// Model-visible text when a tool abort wins during the checkpoint.
pub const ABORTED_BEFORE_DISPATCH: &str = "Error: tool call aborted before dispatch";

/// Stable tool error code matching TypeScript `TOOL_ABORTED_BEFORE_DISPATCH`.
pub const TOOL_ABORTED_BEFORE_DISPATCH: &str = "TOOL_ABORTED_BEFORE_DISPATCH";

/// Flush `session` through `ctx.sessionPersistence` when that service exists.
///
/// # Errors
/// Persistence save failure. Missing persistence is success.
pub async fn flush_session(ctx: &Context, session: &Session) -> Result<(), String> {
    let Some(persistence) = ctx.get::<PersistenceRuntime>() else {
        return Ok(());
    };
    persistence
        .save(session)
        .await
        .map_err(|error| error.to_string())
}

/// Flush the live session named by `session_id`.
///
/// # Errors
/// Unknown session, or persistence save failure.
pub async fn flush_session_id(ctx: &Context, id: &str) -> Result<(), String> {
    let Some(sessions) = ctx.get::<SessionStore>() else {
        return Ok(());
    };
    let session = sessions
        .get(&session_id(id))
        .ok_or_else(|| format!("session \"{id}\" is not live in this store"))?;
    flush_session(ctx, session.as_ref()).await
}

/// Install the three TypeScript checkpoint hooks.
///
/// `agent/pre-step` and `llm/stream` mark the payload so the async loop can
/// await [`flush_session`] at those barriers. `tools/execute` is invoked from
/// the tool runtime before a top-level body.
///
/// # Errors
/// Waterfall registration failure.
pub fn install(ctx: &Context) -> dsh_cordis::Result<()> {
    ctx.provide(Arc::new(CheckpointPolicy))?;
    ctx.on_waterfall("agent/pre-step", |mut payload, next| {
        payload
            .as_object_mut()
            .map(|object| object.insert("checkpoint".into(), Value::Bool(true)));
        next.call(payload)
    })?;
    ctx.on_waterfall("llm/stream", |mut payload, next| {
        payload
            .as_object_mut()
            .map(|object| object.insert("checkpoint".into(), Value::Bool(true)));
        next.call(payload)
    })?;
    Ok(())
}

/// Marker that the checkpoint policy is mounted.
pub struct CheckpointPolicy;

impl dsh_cordis::Service for CheckpointPolicy {
    const KEY: &'static str = "sessionCheckpointPolicy";
}

/// Resolve the calling agent's session and flush it. Used by the tool
/// runtime for a top-level (`parent` absent) execute.
///
/// # Errors
/// Persistence save failure.
pub async fn flush_agent_session(ctx: &Context, agent_id: &str) -> Result<(), String> {
    let Some(agents) = ctx.get::<AgentRegistry>() else {
        return Ok(());
    };
    let Some(agent) = agents.get(&session_id(agent_id)) else {
        return Ok(());
    };
    flush_session(ctx, agent.session().as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::session_id;

    #[tokio::test]
    async fn missing_persistence_is_success() {
        let ctx = Context::new();
        let session = Session::new(session_id("s"));
        flush_session(&ctx, &session).await.unwrap();
    }
}
