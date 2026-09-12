//! Shared HTTP/1.1 exchange for chat and the Files API. HTTPS uses `curl`;
//! `http://` uses a handwritten TCP client.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

/// Parsed provider HTTP response after the status line and hop-by-hop headers.
pub(crate) struct HttpResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub request_id: Option<String>,
    pub body: String,
}

/// Request body variants the transport can send.
pub(crate) enum HttpBody<'a> {
    /// No entity body (GET / DELETE).
    Empty,
    /// One `Content-Type` plus raw bytes (JSON chat).
    Bytes {
        content_type: &'a str,
        data: &'a [u8],
    },
    /// OpenAI-compatible Files multipart upload.
    Multipart(MultipartForm<'a>),
}

/// Fields for `POST /files`.
pub(crate) struct MultipartForm<'a> {
    pub expires_after_seconds: u32,
    pub filename: &'a str,
    pub media_type: &'a str,
    pub file: &'a [u8],
}

/// Exchange one HTTP/1.1 request and parse the response status and body.
pub(crate) async fn http_exchange(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: HttpBody<'_>,
) -> Result<HttpResponse, String> {
    let raw = if url.starts_with("https://") {
        curl_exchange(method, url, headers, body).await?
    } else if url.starts_with("http://") {
        tcp_exchange(method, url, headers, body).await?
    } else {
        return Err(format!("unsupported url: {url}"));
    };
    parse_http_response(&raw)
}

async fn curl_exchange(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: HttpBody<'_>,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new("curl");
    command
        .arg("-sS")
        .arg("--http1.1")
        .arg("-i")
        .arg("-X")
        .arg(method);
    for (name, value) in headers {
        command.arg("-H").arg(format!("{name}: {value}"));
    }
    let temp = match body {
        HttpBody::Empty => None,
        HttpBody::Bytes { content_type, data } => {
            command
                .arg("-H")
                .arg(format!("Content-Type: {content_type}"));
            command
                .arg("--data-binary")
                .arg(String::from_utf8_lossy(data).into_owned());
            None
        }
        HttpBody::Multipart(form) => {
            let path = std::env::temp_dir().join(format!("dsh-files-{}.bin", Uuid::new_v4()));
            tokio::fs::write(&path, form.file)
                .await
                .map_err(|error| error.to_string())?;
            command.arg("-F").arg("purpose=user_data");
            command.arg("-F").arg("expires_after[anchor]=created_at");
            command.arg("-F").arg(format!(
                "expires_after[seconds]={}",
                form.expires_after_seconds
            ));
            command.arg("-F").arg(format!(
                "file={};filename={};type={}",
                curl_file_ref(&path),
                form.filename,
                form.media_type
            ));
            Some(path)
        }
    };
    command.arg(url);
    let output = command.output().await.map_err(|error| error.to_string());
    if let Some(path) = temp {
        let _ = tokio::fs::remove_file(path).await;
    }
    let output = output?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn curl_file_ref(path: &std::path::Path) -> String {
    format!("@{}", path.display())
}

async fn tcp_exchange(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: HttpBody<'_>,
) -> Result<String, String> {
    let parsed = parse_http_url(url)?;
    let (content_type, payload) = match body {
        HttpBody::Empty => (None, Vec::new()),
        HttpBody::Bytes { content_type, data } => (Some(content_type.to_string()), data.to_vec()),
        HttpBody::Multipart(form) => {
            let (content_type, payload) = encode_multipart(&form);
            (Some(content_type), payload)
        }
    };
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .map_err(|error| error.to_string())?;
    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut request = format!(
        "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nContent-Length: {}\r\n",
        parsed.path,
        payload.len()
    );
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    let mut wire = request.into_bytes();
    wire.extend_from_slice(&payload);
    stream
        .write_all(&wire)
        .await
        .map_err(|error| error.to_string())?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn encode_multipart(form: &MultipartForm<'_>) -> (String, Vec<u8>) {
    let boundary = format!("----dsh-files-{}", Uuid::new_v4().simple());
    let mut body = Vec::new();
    append_text_part(&mut body, &boundary, "purpose", "user_data");
    append_text_part(&mut body, &boundary, "expires_after[anchor]", "created_at");
    append_text_part(
        &mut body,
        &boundary,
        "expires_after[seconds]",
        &form.expires_after_seconds.to_string(),
    );
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n",
            form.filename
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", form.media_type).as_bytes());
    body.extend_from_slice(form.file);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn append_text_part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<HttpUrl, String> {
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
    Ok(HttpUrl { host, port, path })
}

fn parse_http_status_line(status_line: &str) -> Option<u16> {
    status_line.split_whitespace().nth(1)?.parse().ok()
}

pub(crate) fn parse_http_response(raw: &str) -> Result<HttpResponse, String> {
    let (header, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .ok_or_else(|| "missing HTTP body".to_string())?;
    let mut lines = header.lines();
    let status_line = lines.next().unwrap_or("");
    let status = parse_http_status_line(status_line)
        .ok_or_else(|| format!("missing HTTP status: {status_line}"))?;
    let mut retry_after = None;
    let mut request_id = None;
    let mut deepseek_request_id = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("retry-after") {
            retry_after = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-request-id") {
            request_id = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-deepseek-request-id") {
            deepseek_request_id = Some(value.to_string());
        }
    }
    Ok(HttpResponse {
        status,
        retry_after,
        request_id: request_id.or(deepseek_request_id),
        body: body.to_string(),
    })
}
