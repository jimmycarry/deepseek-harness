//! `tools/execute` deadline enforcer.

use dsh_cordis::{Context, Result};
use serde_json::{json, Value};

/// Stamp `timeoutMs` onto every `tools/pre-execute` payload.
pub fn install(ctx: &Context, timeout_ms: u64) -> Result<()> {
    ctx.on_waterfall("tools/pre-execute", move |mut payload, next| {
        if let Value::Object(map) = &mut payload {
            map.insert("timeoutMs".into(), json!(timeout_ms));
        }
        next.call(payload)
    })
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-timeout-policy"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_tools::{ScriptTool, ToolRuntime};
    use std::sync::Arc;

    #[tokio::test]
    async fn stamps_timeout_ms_on_pre_execute() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("echo", "echo", |_| {
            dsh_tools::ToolOutcome::text("ok")
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        install(&ctx, 1_500).unwrap();
        let stamped = ctx
            .waterfall(
                "tools/pre-execute",
                json!({ "name": "echo", "args": {} }),
                |payload| payload,
            )
            .unwrap();
        assert_eq!(stamped.get("timeoutMs").and_then(Value::as_u64), Some(1_500));
        tools.execute(&ctx, "echo", json!({})).await.unwrap();
    }
}
