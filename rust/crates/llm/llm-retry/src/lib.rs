//! Provider-routed model-request retry on `agent/request-error`.
//!
//! This executor has no Config; each provider owns `retryPolicy`. Scheduled
//! retries are durable (`llm/retry`, then `llm/retry-started`) before the
//! cancellable wait. Nothing about a retry is model-visible.

use dsh_agent::AgentRegistry;
use dsh_cordis::Context;
use dsh_llm::RetryPolicy;
use dsh_session::{session_id, SessionEventData};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

/// Install the `agent/request-error` recovery listener.
///
/// # Errors
/// Unknown `retryPolicy` key on this executor, or waterfall registration.
pub fn install(ctx: &Context, config: Option<&Value>) -> dsh_cordis::Result<()> {
    validate_config(config)?;
    let agents = ctx.service::<AgentRegistry>()?;
    ctx.on_waterfall("agent/request-error", move |payload, next| {
        recover(&agents, payload, |payload| next.call(payload))
    })?;
    Ok(())
}

fn validate_config(config: Option<&Value>) -> dsh_cordis::Result<()> {
    let Some(object) = config.and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(key) = object.keys().next() else {
        return Ok(());
    };
    if key == "retryPolicy" {
        return Err(dsh_cordis::CordisError::Validation(
            "llm-retry: retryPolicy belongs under each provider configuration".into(),
        ));
    }
    Err(dsh_cordis::CordisError::Validation(format!(
        "llm-retry: unknown key \"{key}\""
    )))
}

fn recover(agents: &AgentRegistry, payload: Value, mut next: impl FnMut(Value) -> Value) -> Value {
    let policy = payload
        .get("retryPolicy")
        .and_then(|value| serde_json::from_value::<RetryPolicy>(value.clone()).ok())
        .unwrap_or_default();
    let code = payload
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if policy.mode != "always" && !policy.retryable_codes.iter().any(|item| item == &code) {
        return next(payload);
    }
    if policy.mode == "always" {
        let downstream = next(payload.clone());
        if downstream.get("kind").and_then(Value::as_str) == Some("retry") {
            return downstream;
        }
    }
    let Some(agent_id) = payload.get("agentId").and_then(Value::as_str) else {
        return next(payload);
    };
    let Some(agent) = agents.get(&session_id(agent_id)) else {
        return next(payload);
    };
    let turn = payload.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
    let step = payload.get("step").and_then(Value::as_u64).unwrap_or(0) as u32;
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let policy_key = policy.policy_key();
    let previous = agent
        .session()
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match &event.data {
            SessionEventData::Extension { type_name, data }
                if type_name == "llm/retry"
                    && data.get("turn").and_then(Value::as_u64) == Some(u64::from(turn))
                    && data.get("step").and_then(Value::as_u64) == Some(u64::from(step))
                    && data.get("provider").and_then(Value::as_str) == Some(provider.as_str())
                    && data.get("policyKey").and_then(Value::as_str)
                        == Some(policy_key.as_str()) =>
            {
                Some((
                    data.get("retry").and_then(Value::as_u64).unwrap_or(0),
                    data.get("retryId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ))
            }
            _ => None,
        });
    let previous_retry = previous.as_ref().map(|(retry, _)| *retry).unwrap_or(0);
    if policy.mode == "normal" && previous_retry >= u64::from(policy.max_retries) {
        return next(payload);
    }
    let retry = previous_retry + 1;
    let retry_id = previous
        .and_then(|(_, id)| (!id.is_empty()).then_some(id))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let delay_ms = policy.local_delay(retry as u32, 0.5);
    let failure = payload.get("failure").cloned().unwrap_or_else(|| {
        json!({
            "message": payload.get("message").and_then(Value::as_str).unwrap_or(""),
            "code": code,
        })
    });
    let mut event = json!({
        "retryId": retry_id,
        "turn": turn,
        "step": step,
        "provider": provider,
        "mode": policy.mode,
        "policyKey": policy_key,
        "retry": retry,
        "delayMs": delay_ms,
        "failure": failure,
    });
    if policy.mode == "normal" {
        event
            .as_object_mut()
            .map(|object| object.insert("maxRetries".into(), json!(policy.max_retries)));
    }
    let _ = agent.session().append(
        SessionEventData::Extension {
            type_name: "llm/retry".into(),
            data: event,
        },
        None,
    );
    if delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    let _ = agent.session().append(
        SessionEventData::Extension {
            type_name: "llm/retry-started".into(),
            data: json!({
                "retryId": retry_id,
                "turn": turn,
                "step": step,
                "retry": retry,
            }),
        },
        None,
    );
    json!({ "kind": "retry" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_retry_policy_on_executor() {
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(AgentRegistry::new()))
            .unwrap();
        let error = install(&ctx, Some(&json!({"retryPolicy": {"mode": "normal"}}))).unwrap_err();
        assert!(error.to_string().contains("belongs under each provider"));
    }
}
