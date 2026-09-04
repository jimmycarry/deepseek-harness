//! API proxy (`ctx.apiProxy`). Forwards JSON-RPC over HTTP POST.

use dsh_cordis::Service;
use dsh_sdk_protocol::{JsonRpcRequest, JsonRpcResponse};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `ctx.apiProxy`.
pub struct ApiProxy {
    target: String,
}

impl ApiProxy {
    /// Forward to `target` (`http://host:port/path`).
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }

    /// POST one JSON-RPC request and return the response `result`.
    pub async fn forward(&self, request: JsonRpcRequest) -> Result<Value, String> {
        let (host, port, path) = parse_http_target(&self.target)?;
        let body = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        let mut stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|error| error.to_string())?;
        let header = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        stream
            .write_all(body.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        stream.flush().await.map_err(|error| error.to_string())?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|error| error.to_string())?;
        let response = String::from_utf8_lossy(&buf);
        let json = response
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| "HTTP response missing body".to_string())?;
        let parsed: JsonRpcResponse =
            serde_json::from_str(json.trim()).map_err(|error| error.to_string())?;
        Ok(parsed.result.unwrap_or(Value::Null))
    }
}

impl Service for ApiProxy {
    const KEY: &'static str = "apiProxy";
}

fn parse_http_target(target: &str) -> Result<(String, u16, String), String> {
    let rest = target
        .strip_prefix("http://")
        .ok_or_else(|| "apiProxy target must start with http://".to_string())?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hostport, path)) => (hostport, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match hostport.split_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|error| format!("invalid port: {error}"))?,
        ),
        None => (hostport.to_string(), 80),
    };
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ApiProxy::new("http://127.0.0.1:9/rpc")))
            .unwrap();
        assert!(ctx.has_service("apiProxy"));
        ctx.dispose();
        assert!(!ctx.has_service("apiProxy"));
    }

    #[tokio::test]
    async fn forward_posts_json_rpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST"));
            let json = request.split("\r\n\r\n").nth(1).unwrap();
            let rpc: JsonRpcRequest = serde_json::from_str(json.trim_end_matches('\0')).unwrap();
            let body =
                serde_json::to_string(&JsonRpcResponse::result(rpc.id, serde_json::json!({"ok": true})))
                    .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let proxy = ApiProxy::new(format!("http://{addr}/rpc"));
        let result = proxy
            .forward(JsonRpcRequest::new(1, "ping", None))
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
    }
}
