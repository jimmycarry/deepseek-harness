//! DeepSeek Files API upload reuse, invalidation, and quota recovery.

use crate::files_api::{is_files_quota_error, DeepSeekFilesClient, DeepSeekFilesError};
use crate::upload_index::{deep_seek_file_scope, DeepSeekUploadIndex, DeepSeekUploadRecord};
use crate::PreparedRequestImage;
use dsh_llm::{LlmError, LlmFailure};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// DeepSeek chat accepts at most 32 MiB per image even when it is referenced by file id.
pub const MAX_CHAT_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const OWNED_FILE_PREFIX: &str = "dsh-";

/// Resolved file-store policy from the plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekFilePolicy {
    /// Explicit lifetime assigned to each uploaded image.
    pub expires_after_seconds: u32,
    /// Remaining lifetime below which an indexed file is replaced.
    pub refresh_margin_seconds: u32,
    /// Oldest harness-owned files deleted before one quota-recovery upload retry.
    pub quota_cleanup_batch: u32,
}

impl Default for DeepSeekFilePolicy {
    fn default() -> Self {
        Self {
            expires_after_seconds: crate::DEFAULT_FILE_EXPIRY_SECONDS,
            refresh_margin_seconds: crate::DEFAULT_FILE_REFRESH_MARGIN_SECONDS,
            quota_cleanup_batch: crate::DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
        }
    }
}

/// Connection facts needed by file operations.
#[derive(Debug, Clone)]
pub struct DeepSeekFileConnection {
    /// Provider endpoint with trailing slashes stripped by the client.
    pub base_url: String,
    /// Resolved API key for this request.
    pub api_key: String,
}

/// Result of one file-id resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekFileReference {
    /// Winning local mapping.
    pub record: DeepSeekUploadRecord,
    /// Whether this call published a new upload.
    pub uploaded: bool,
}

/// User-scoped durable file-id reuse for the DeepSeek route.
#[derive(Clone)]
pub struct DeepSeekFileStore {
    index: DeepSeekUploadIndex,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    inflight: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl DeepSeekFileStore {
    /// Default DSH-home index and wall clock.
    #[must_use]
    pub fn default_store() -> Self {
        Self::new(DeepSeekUploadIndex::new(None), None)
    }

    /// Testable index and optional clock (Unix milliseconds).
    #[must_use]
    pub fn new(
        index: DeepSeekUploadIndex,
        now_ms: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    ) -> Self {
        Self {
            index,
            now_ms: now_ms.unwrap_or_else(|| Arc::new(system_now_ms)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn now(&self) -> u64 {
        (self.now_ms)()
    }

    fn client(connection: &DeepSeekFileConnection) -> DeepSeekFilesClient {
        DeepSeekFilesClient::new(&connection.base_url, &connection.api_key)
    }

    /// Resolve or upload one deterministic request image. Concurrent calls share one upload.
    ///
    /// # Errors
    /// Chat per-image cap, Files transport or validation failures, or index IO.
    pub async fn ensure_uploaded(
        &self,
        version: &PreparedRequestImage,
        connection: &DeepSeekFileConnection,
        policy: &DeepSeekFilePolicy,
    ) -> Result<DeepSeekFileReference, LlmError> {
        let scope = deep_seek_file_scope(&connection.base_url, &connection.api_key);
        let key = format!("{scope}\0{}", version.variant_id);
        let gate = {
            let mut map = self.inflight.lock().await;
            map.entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = gate.lock().await;
        let result = self.ensure_uploaded_once(version, connection, policy).await;
        self.inflight.lock().await.remove(&key);
        result
    }

    async fn ensure_uploaded_once(
        &self,
        version: &PreparedRequestImage,
        connection: &DeepSeekFileConnection,
        policy: &DeepSeekFilePolicy,
    ) -> Result<DeepSeekFileReference, LlmError> {
        if version.data.len() > MAX_CHAT_IMAGE_BYTES {
            return Err(LlmError::Failure(LlmFailure::new(
                "DeepSeek chat image exceeds the 32 MiB per-image limit.",
                "INVALID_REQUEST",
            )));
        }
        let scope = deep_seek_file_scope(&connection.base_url, &connection.api_key);
        let now = self.now();
        let margin_ms = u64::from(policy.refresh_margin_seconds) * 1_000;
        if let Some(cached) = self
            .index
            .get(&scope, &version.variant_id, now, margin_ms)
            .await?
        {
            return Ok(DeepSeekFileReference {
                record: cached,
                uploaded: false,
            });
        }

        let client = Self::client(connection);
        let candidate = upload_or_quota(&client, version, connection, policy, self).await?;
        let committed = self
            .index
            .commit(candidate.clone(), self.now(), margin_ms)
            .await?;
        if !committed.accepted {
            if let Err(_duplicate_cleanup) = client.delete(&candidate.file_id).await {
                // The winning mapping is durable. A failed duplicate cleanup
                // affects quota only and is retried by recovery.
            }
        }
        Ok(DeepSeekFileReference {
            record: committed.record,
            uploaded: committed.accepted,
        })
    }

    /// Invalidate one exact local mapping after the chat endpoint rejects its remote id.
    ///
    /// # Errors
    /// Index IO.
    pub async fn invalidate(
        &self,
        version: &PreparedRequestImage,
        file_id: &str,
        connection: &DeepSeekFileConnection,
    ) -> Result<(), LlmError> {
        self.index
            .remove(
                &deep_seek_file_scope(&connection.base_url, &connection.api_key),
                &version.variant_id,
                file_id,
            )
            .await
    }

    /// Delete the oldest provider files whose names identify harness ownership.
    ///
    /// # Errors
    /// Files list or delete failures.
    pub async fn reclaim_oldest_owned(
        &self,
        connection: &DeepSeekFileConnection,
        count: u32,
    ) -> Result<usize, LlmError> {
        let client = Self::client(connection);
        let mut after: Option<String> = None;
        let mut owned = Vec::new();
        while owned.len() < count as usize {
            let page = client
                .list(after.as_deref(), Some(1_000), Some("asc"))
                .await
                .map_err(LlmError::from)?;
            for file in &page.data {
                if !file.filename.starts_with(OWNED_FILE_PREFIX) {
                    continue;
                }
                owned.push(file.id.clone());
                if owned.len() == count as usize {
                    break;
                }
            }
            if !page.has_more
                || page.last_id.as_deref().is_none()
                || page.last_id.as_deref() == after.as_deref()
            {
                break;
            }
            after = page.last_id;
        }
        for file_id in &owned {
            client.delete(file_id).await.map_err(LlmError::from)?;
        }
        Ok(owned.len())
    }

    /// Delete every remote harness-owned file in the active API-key namespace and clear its index.
    ///
    /// # Errors
    /// Files or index failures.
    pub async fn release_all(
        &self,
        connection: &DeepSeekFileConnection,
    ) -> Result<usize, LlmError> {
        let mut total = 0;
        loop {
            let deleted = self.reclaim_oldest_owned(connection, 1_000).await?;
            total += deleted;
            if deleted < 1_000 {
                break;
            }
        }
        self.index
            .clear(&deep_seek_file_scope(
                &connection.base_url,
                &connection.api_key,
            ))
            .await?;
        Ok(total)
    }
}

async fn upload_or_quota(
    client: &DeepSeekFilesClient,
    version: &PreparedRequestImage,
    connection: &DeepSeekFileConnection,
    policy: &DeepSeekFilePolicy,
    store: &DeepSeekFileStore,
) -> Result<DeepSeekUploadRecord, LlmError> {
    match upload_record(client, version, connection, policy).await {
        Ok(record) => Ok(record),
        Err(error) if is_files_quota_error(&error) => {
            let deleted = store
                .reclaim_oldest_owned(connection, policy.quota_cleanup_batch)
                .await?;
            if deleted == 0 {
                return Err(error.into());
            }
            upload_record(client, version, connection, policy)
                .await
                .map_err(LlmError::from)
        }
        Err(error) => Err(error.into()),
    }
}

async fn upload_record(
    client: &DeepSeekFilesClient,
    version: &PreparedRequestImage,
    connection: &DeepSeekFileConnection,
    policy: &DeepSeekFilePolicy,
) -> Result<DeepSeekUploadRecord, DeepSeekFilesError> {
    let remote = client
        .upload(
            &version.data,
            &version.media_type,
            &owned_filename(version),
            policy.expires_after_seconds,
        )
        .await?;
    if remote.bytes != version.data.len() as u64 {
        return Err(response_files_error(
            "DeepSeek Files API upload response does not match the submitted image.",
        ));
    }
    let expires_at = remote.expires_at.ok_or_else(|| {
        response_files_error("DeepSeek Files API returned an invalid upload response.")
    })?;
    Ok(DeepSeekUploadRecord {
        scope: deep_seek_file_scope(&connection.base_url, &connection.api_key),
        attachment_id: version.attachment_id.clone(),
        variant_id: version.variant_id.clone(),
        file_id: remote.id,
        bytes: remote.bytes,
        created_at: remote.created_at.saturating_mul(1_000),
        expires_at: expires_at.saturating_mul(1_000),
    })
}

fn response_files_error(message: &str) -> DeepSeekFilesError {
    DeepSeekFilesError {
        error: LlmError::Failure(LlmFailure::new(message, "INVALID_RESPONSE")),
        detail: String::new(),
    }
}

fn owned_filename(version: &PreparedRequestImage) -> String {
    let attachment = strip_sha_prefix(&version.attachment_id);
    let variant = strip_sha_prefix(&version.variant_id);
    let attachment: String = attachment.chars().take(16).collect();
    let variant: String = variant.chars().take(8).collect();
    format!(
        "{OWNED_FILE_PREFIX}{attachment}-{variant}.{}",
        extension(&version.media_type)
    )
}

fn strip_sha_prefix(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_uses_owned_prefix_and_media_extension() {
        let image = PreparedRequestImage::from_bytes(
            format!("sha256:{}", "ab".repeat(32)),
            "image/jpeg",
            vec![1, 2, 3],
            1,
            1,
        );
        let name = owned_filename(&image);
        assert!(name.starts_with("dsh-"), "{name}");
        assert!(name.ends_with(".jpeg"), "{name}");
        assert_eq!(name.matches('-').count(), 2);
    }

    #[test]
    fn chat_image_cap_message() {
        assert_eq!(MAX_CHAT_IMAGE_BYTES, 32 * 1024 * 1024);
    }

    #[tokio::test]
    async fn reclaims_owned_file_after_quota_and_retries_upload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uploads = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = uploads.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if n == 1 {
                        write_http(
                            &mut socket,
                            &http_json(
                                400,
                                r#"{"error":{"message":"stored file quota exceeded","code":"file_quota"}}"#,
                            ),
                        )
                        .await;
                    } else {
                        write_http(&mut socket, &upload_ok("file-api-recovered", 3)).await;
                    }
                } else if request.starts_with("DELETE /files/") {
                    write_http(
                        &mut socket,
                        &http_json(
                            200,
                            r#"{"id":"file-api-old","object":"file","deleted":true}"#,
                        ),
                    )
                    .await;
                } else {
                    write_http(&mut socket, &list_ok("file-api-old", "dsh-old.png")).await;
                }
            }
        });
        let store = test_store();
        let image = sample_image(3);
        let resolved = store
            .ensure_uploaded(
                &image,
                &DeepSeekFileConnection {
                    base_url: format!("http://{addr}"),
                    api_key: "key".into(),
                },
                &DeepSeekFilePolicy {
                    quota_cleanup_batch: 1,
                    ..DeepSeekFilePolicy::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(resolved.record.file_id, "file-api-recovered");
        assert!(resolved.uploaded);
        assert_eq!(uploads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn preserves_quota_error_when_no_owned_file() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    write_http(
                        &mut socket,
                        &http_json(
                            400,
                            r#"{"error":{"message":"file count quota exceeded","code":"file_quota"}}"#,
                        ),
                    )
                    .await;
                } else {
                    write_http(&mut socket, &list_ok("file-api-foreign", "foreign.png")).await;
                }
            }
        });
        let store = test_store();
        let image = sample_image(3);
        let error = store
            .ensure_uploaded(
                &image,
                &DeepSeekFileConnection {
                    base_url: format!("http://{addr}"),
                    api_key: "key".into(),
                },
                &DeepSeekFilePolicy::default(),
            )
            .await
            .unwrap_err();
        let LlmError::Failure(failure) = error;
        assert_eq!(failure.code, "FILES_API");
    }

    fn test_store() -> DeepSeekFileStore {
        let path = std::env::temp_dir()
            .join(format!(
                "dsh-files-quota-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time")
                    .as_nanos()
            ))
            .join("files-v3.json");
        DeepSeekFileStore::new(DeepSeekUploadIndex::new(Some(path)), None)
    }

    fn sample_image(bytes: usize) -> PreparedRequestImage {
        PreparedRequestImage::from_bytes(
            format!("sha256:{}", "ab".repeat(32)),
            "image/png",
            vec![1u8; bytes],
            1,
            1,
        )
    }

    fn http_json(status: u16, body: &str) -> String {
        let reason = if (200..300).contains(&status) {
            "OK"
        } else {
            "ERR"
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn upload_ok(id: &str, bytes: usize) -> String {
        let body = serde_json::json!({
            "id": id,
            "object": "file",
            "bytes": bytes,
            "created_at": 1_700_000_000,
            "filename": "dsh-recovered.png",
            "purpose": "user_data",
            "expires_at": 1_700_604_800
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn list_ok(id: &str, filename: &str) -> String {
        let body = serde_json::json!({
            "object": "list",
            "data": [{
                "id": id,
                "object": "file",
                "bytes": 3,
                "created_at": 1_700_000_000,
                "filename": filename,
                "purpose": "user_data"
            }],
            "first_id": id,
            "last_id": id,
            "has_more": false
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&buf[..header_end]);
                let content_length = header.lines().find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                });
                if let Some(length) = content_length {
                    let start = header_end + 4;
                    if buf.len() >= start + length {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn write_http(socket: &mut tokio::net::TcpStream, response: &str) {
        use tokio::io::AsyncWriteExt;
        socket.write_all(response.as_bytes()).await.unwrap();
    }
}
