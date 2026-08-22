//! HTTP route server (`ctx.webServer`). Speaks the same JSON-RPC the TS client uses.

use dsh_cordis::{Context, Service};
use dsh_sdk_protocol::JsonRpcRequest;
use dsh_sdk_server::HarnessSdkJsonRpcServer;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// `ctx.webServer`.
pub struct WebServer {
    ctx: Context,
    server: HarnessSdkJsonRpcServer,
}

impl WebServer {
    /// Bind to a live spine context.
    pub fn new(ctx: Context) -> Self {
        Self {
            ctx,
            server: HarnessSdkJsonRpcServer::new(),
        }
    }

    /// Handle one JSON-RPC request the TS client would POST to `/rpc`.
    /// HTTP is request/response only: the notifications a stdio transport
    /// would stream for the same request are dropped here.
    pub async fn rpc(&self, request: JsonRpcRequest) -> Value {
        let (_notifications, response) = self.server.handle_request(&self.ctx, request).await;
        serde_json::to_value(response).unwrap_or(Value::Null)
    }

    /// Health payload the TS client can GET from `/health`.
    pub fn health(&self) -> Value {
        serde_json::json!({ "ok": true })
    }

    /// Serve `/health` and `/rpc` on `addr` until the task is cancelled.
    pub async fn serve(self: Arc<Self>, addr: &str) -> Result<(), String> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|error| error.to_string())?;
        self.serve_listener(listener).await
    }

    async fn serve_listener(self: Arc<Self>, listener: TcpListener) -> Result<(), String> {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
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
                let _ = stream.shutdown().await;
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
                Some(serde_json::json!({
                    "sessionId": "44444444-4444-4444-4444-444444444444",
                    "contentBlocks": [{ "type": "text", "text": "hi" }],
                })),
            ))
            .await;
        assert!(response["result"]["messageId"].is_string());
    }

    #[tokio::test]
    async fn serve_health_over_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::new(WebServer::new(Context::new()));
        tokio::spawn(async move {
            let _ = server.serve_listener(listener).await;
        });
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf);
        let body = response.split("\r\n\r\n").nth(1).expect("health body");
        let json: Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(json["ok"], true);
    }
}
