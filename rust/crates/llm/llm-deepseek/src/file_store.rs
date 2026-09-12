//! DeepSeek Files API upload reuse, invalidation, and quota recovery.

use crate::files_api::DeepSeekFilesClient;
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
        Err(error) => {
            if !matches_quota(&error) {
                return Err(error);
            }
            let deleted = store
                .reclaim_oldest_owned(connection, policy.quota_cleanup_batch)
                .await?;
            if deleted == 0 {
                return Err(error);
            }
            upload_record(client, version, connection, policy).await
        }
    }
}

fn matches_quota(error: &LlmError) -> bool {
    let LlmError::Failure(failure) = error;
    let hay = format!("{} {}", failure.code, failure.message).to_ascii_lowercase();
    hay.contains("quota")
        || hay.contains("storage")
        || hay.contains("stored files")
        || hay.contains("file count")
        || hay.contains("too many files")
}

async fn upload_record(
    client: &DeepSeekFilesClient,
    version: &PreparedRequestImage,
    connection: &DeepSeekFileConnection,
    policy: &DeepSeekFilePolicy,
) -> Result<DeepSeekUploadRecord, LlmError> {
    let remote = match client
        .upload(
            &version.data,
            &version.media_type,
            &owned_filename(version),
            policy.expires_after_seconds,
        )
        .await
    {
        Ok(remote) => remote,
        Err(error) => return Err(error),
    };
    if remote.bytes != version.data.len() as u64 {
        return Err(LlmError::Failure(LlmFailure::new(
            "DeepSeek Files API upload response does not match the submitted image.",
            "INVALID_RESPONSE",
        )));
    }
    let expires_at = remote.expires_at.ok_or_else(|| {
        LlmError::Failure(LlmFailure::new(
            "DeepSeek Files API returned an invalid upload response.",
            "INVALID_RESPONSE",
        ))
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
}
