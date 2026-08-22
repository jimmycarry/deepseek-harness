//! HTTP fetch provider.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_web::{WebError, WebFetcher, WebRuntime};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Host HTTP/HTTPS fetcher.
pub struct HttpFetcher;

#[async_trait]
impl WebFetcher for HttpFetcher {
    async fn fetch(&self, url: &str) -> Result<String, WebError> {
        if url.starts_with("https://") {
            curl_get(url).await.map_err(WebError::Fetch)
        } else if url.starts_with("http://") {
            tcp_get(url).await.map_err(WebError::Fetch)
        } else {
            Err(WebError::Fetch(format!("unsupported url: {url}")))
        }
    }
}

/// Provide [`WebRuntime`] backed by [`HttpFetcher`].
pub fn install(ctx: &Context) -> Result<Arc<WebRuntime>> {
    let runtime = Arc::new(WebRuntime::new(Arc::new(HttpFetcher)));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-web-fetch-http"
}

async fn tcp_get(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not an http url: {url}"))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hostport, path)) => (hostport, format!("/{path}")),
        None => (rest, "/".into()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().map_err(|error| error.to_string())?,
        ),
        None => (hostport.to_string(), 80),
    };
    let mut stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|error| error.to_string())?;
    let host_header = if port == 80 {
        host
    } else {
        format!("{host}:{port}")
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    text.split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map(|(_, body)| body.to_string())
        .ok_or_else(|| "missing HTTP body".into())
}

async fn curl_get(url: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("curl")
        .arg("-sS")
        .arg("--http1.1")
        .arg(url)
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn http_fetcher_reads_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body = "hello-web";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let body = HttpFetcher
            .fetch(&format!("http://{addr}/page"))
            .await
            .unwrap();
        assert_eq!(body, "hello-web");
    }

    #[test]
    fn install_provides_web() {
        let ctx = Context::new();
        install(&ctx).unwrap();
        assert!(ctx.has_service("web"));
    }
}
