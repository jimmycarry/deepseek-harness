//! JSON-RPC server plugin. Drives `ctx.agents` and streams `session/event`.

use dsh_agent::AgentRegistry;
use dsh_agent_loop::run_followup;
use dsh_cordis::Context;
use dsh_llm::UserMessage;
use dsh_sdk_protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use dsh_session::SessionStore;
use serde_json::Value;

/// Handle one request against a live spine.
pub async fn handle(ctx: &Context, request: JsonRpcRequest) -> JsonRpcResponse {
    match request.method.as_str() {
        methods::INITIALIZE => JsonRpcResponse::result(request.id, serde_json::json!({"ok": true})),
        methods::SESSION_PROMPT => {
            let text = request
                .params
                .as_ref()
                .and_then(|params| params.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let Ok(store) = ctx.service::<SessionStore>() else {
                return JsonRpcResponse::result(request.id, serde_json::json!({"error": "no sessions"}));
            };
            let session = store.create_fresh();
            let Ok(agents) = ctx.service::<AgentRegistry>() else {
                return JsonRpcResponse::result(request.id, serde_json::json!({"error": "no agents"}));
            };
            let handle = match agents.create(session) {
                Ok(handle) => handle,
                Err(error) => {
                    return JsonRpcResponse::result(
                        request.id,
                        serde_json::json!({"error": error.to_string()}),
                    )
                }
            };
            let _ = run_followup(
                handle.agent.as_ref(),
                UserMessage::text(text),
            )
            .await;
            JsonRpcResponse::result(
                request.id,
                serde_json::json!({
                    "text": handle.agent.session().last_assistant_text(),
                    "events": handle.agent.session().events().len(),
                }),
            )
        }
        methods::SHUTDOWN => JsonRpcResponse::result(request.id, serde_json::json!({"ok": true})),
        other => JsonRpcResponse::result(request.id, serde_json::json!({"error": format!("unknown {other}")})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_spine::apply_replay;
    use dsh_sdk_protocol::JsonRpcRequest;

    #[tokio::test]
    async fn prompt_projects_the_loop() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let response = handle(
            &ctx,
            JsonRpcRequest::new(1, methods::SESSION_PROMPT, Some(serde_json::json!({"text":"ping"}))),
        )
        .await;
        assert_eq!(response.result.unwrap()["text"], "pong");
    }
}
