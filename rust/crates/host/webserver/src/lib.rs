//! HTTP route server (`ctx.webServer`). Speaks the same JSON-RPC the TS client uses.

use dsh_cordis::{Context, Service};
use dsh_sdk_protocol::JsonRpcRequest;
use dsh_sdk_server::handle;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// `ctx.webServer`.
pub struct WebServer {
    ctx: Context,
}

impl WebServer {
    /// Bind to a live spine context.
    pub fn new(ctx: Context) -> Self {
        Self { ctx }
    }

    /// Handle one JSON-RPC request the TS client would POST to `/rpc`.
    pub async fn rpc(&self, request: JsonRpcRequest) -> Value {
        serde_json::to_value(handle(&self.ctx, request).await).unwrap_or(Value::Null)
    }

    /// Health payload the TS client can GET from `/health`.
    pub fn health(&self) -> Value {
        serde_json::json!({ "ok": true })
    }

    /// Serve `/health` and `/rpc` on `addr` until the task is cancelled.
    pub async fn serve(self: Arc<Self>, addr: &str) -> Result<(), String> {
        let listener = TcpListener::bind(addr).await.map_err(|error| error.to_string())?;
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = stream.read(&mut buf).await else { return };
                let request = String::from_utf8_lossy(&buf[..n]);
                let body = if request.starts_with("GET /health") {
                    server.health().to_string()
                } else if let Some(json) = request.split("\r\n\r\n").nth(1) {
                    if let Ok(rpc) = serde_json::from_str::<JsonRpcRequest>(json) {
                        server.rpc(rpc).await.to_string()
                    } else {
                        serde_json::json!({"error":"bad rpc"}).to_string()
                    }
                } else {
                    serde_json::json!({"error":"not found"}).to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }
}

impl Service for WebServer {
    const KEY: &'static str = "webServer";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent_spine::apply_replay;
    use dsh_sdk_protocol::{methods, JsonRpcRequest};

    #[tokio::test]
    async fn health_and_rpc_share_the_spine() {
        let ctx = Context::new();
        apply_replay(&ctx, "pong").unwrap();
        let server = WebServer::new(ctx);
        assert_eq!(server.health()["ok"], true);
        let response = server
            .rpc(JsonRpcRequest::new(
                1,
                methods::SESSION_PROMPT,
                Some(serde_json::json!({"text":"hi"})),
            ))
            .await;
        assert_eq!(response["result"]["text"], "pong");
    }
}
