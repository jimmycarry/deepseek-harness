//! Durable DeepSeek attachment-to-file-id index at `$DSH_HOME/llm-deepseek/files-v3.json`.

use dsh_atomic_write::{with_file_lock, write_file_atomic, WriteFileAtomicOptions};
use dsh_home_paths::resolve_dsh_home;
use dsh_llm::{LlmError, LlmFailure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// On-disk index format version. Other versions are treated as empty.
pub const UPLOAD_INDEX_FORMAT_VERSION: u32 = 3;

/// One durable remote upload mapping. Unix times are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSeekUploadRecord {
    /// SHA-256 hex of normalized base URL + NUL + API key.
    pub scope: String,
    /// Normalized attachment id (`sha256:` + hex).
    pub attachment_id: String,
    /// Request-version identity (`sha256:` + hex).
    pub variant_id: String,
    /// Provider file identifier.
    pub file_id: String,
    /// Uploaded byte length.
    pub bytes: u64,
    /// Provider `created_at` converted to milliseconds.
    pub created_at: u64,
    /// Provider `expires_at` converted to milliseconds.
    pub expires_at: u64,
}

/// Candidate commit outcome when another process already published a reusable upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIndexCommit {
    /// Winning record.
    pub record: DeepSeekUploadRecord,
    /// Whether the candidate entered the index.
    pub accepted: bool,
}

/// Derive a non-secret namespace digest without persisting the API key.
#[must_use]
pub fn deep_seek_file_scope(base_url: &str, api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(trim_base(base_url).as_bytes());
    hasher.update([0]);
    hasher.update(api_key.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Atomic local index shared by every DeepSeek session in this DSH home.
#[derive(Debug, Clone)]
pub struct DeepSeekUploadIndex {
    /// Absolute owner-private JSON index path.
    pub path: PathBuf,
}

impl DeepSeekUploadIndex {
    /// Default `$DSH_HOME/llm-deepseek/files-v3.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        resolve_dsh_home(None)
            .join("llm-deepseek")
            .join("files-v3.json")
    }

    /// Explicit test path, or the default DSH-home index when omitted.
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: path.unwrap_or_else(Self::default_path),
        }
    }

    async fn load(&self) -> Result<Vec<DeepSeekUploadRecord>, LlmError> {
        match tokio::fs::read_to_string(&self.path).await {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(index_io(error)),
            Ok(text) => match parse_index(&text) {
                Ok(records) => Ok(records),
                Err(_) => Ok(Vec::new()),
            },
        }
    }

    async fn save(&self, records: &[DeepSeekUploadRecord]) -> Result<(), LlmError> {
        let document = serde_json::json!({
            "formatVersion": UPLOAD_INDEX_FORMAT_VERSION,
            "records": records,
        });
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&document).expect("upload index json")
        );
        write_file_atomic(
            &self.path,
            body,
            WriteFileAtomicOptions {
                mode: 0o600,
                dir_mode: None,
            },
        )
        .await
        .map_err(|error| index_io(error))
    }

    /// Read one reusable mapping.
    ///
    /// # Errors
    /// Filesystem failures other than a missing or corrupt index.
    pub async fn get(
        &self,
        scope: &str,
        variant_id: &str,
        now_ms: u64,
        refresh_margin_ms: u64,
    ) -> Result<Option<DeepSeekUploadRecord>, LlmError> {
        let records = self.load().await?;
        Ok(records.into_iter().find(|record| {
            record.scope == scope
                && record.variant_id == variant_id
                && reusable(record, now_ms, refresh_margin_ms)
        }))
    }

    /// Publish a completed upload unless another process already published a reusable mapping.
    ///
    /// # Errors
    /// Filesystem failures while locking or writing the index.
    pub async fn commit(
        &self,
        candidate: DeepSeekUploadRecord,
        now_ms: u64,
        refresh_margin_ms: u64,
    ) -> Result<UploadIndexCommit, LlmError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(index_io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let path = self.path.clone();
        let locked_path = path.clone();
        let index = self.clone();
        with_file_lock(&path, None, move || {
            let index = index;
            let candidate = candidate;
            let locked_path = locked_path;
            async move {
                match index
                    .commit_locked(candidate, now_ms, refresh_margin_ms)
                    .await
                {
                    Ok(commit) => Ok(commit),
                    Err(error) => Err(lock_error(&locked_path, error)),
                }
            }
        })
        .await
        .map_err(|error| index_io(error))
    }

    async fn commit_locked(
        &self,
        candidate: DeepSeekUploadRecord,
        now_ms: u64,
        refresh_margin_ms: u64,
    ) -> Result<UploadIndexCommit, LlmError> {
        let records = self.load().await?;
        if let Some(existing) = records.iter().find(|record| {
            record.scope == candidate.scope
                && record.variant_id == candidate.variant_id
                && reusable(record, now_ms, refresh_margin_ms)
        }) {
            return Ok(UploadIndexCommit {
                record: existing.clone(),
                accepted: false,
            });
        }
        let mut records: Vec<DeepSeekUploadRecord> = records
            .into_iter()
            .filter(|record| {
                reusable(record, now_ms, refresh_margin_ms)
                    && !(record.scope == candidate.scope
                        && record.variant_id == candidate.variant_id)
            })
            .collect();
        records.push(candidate.clone());
        self.save(&records).await?;
        Ok(UploadIndexCommit {
            record: candidate,
            accepted: true,
        })
    }

    /// Remove one exact mapping without deleting a concurrently installed successor.
    ///
    /// # Errors
    /// Filesystem failures while locking or writing the index.
    pub async fn remove(
        &self,
        scope: &str,
        variant_id: &str,
        file_id: &str,
    ) -> Result<(), LlmError> {
        let path = self.path.clone();
        let locked_path = path.clone();
        let index = self.clone();
        let scope = scope.to_string();
        let variant_id = variant_id.to_string();
        let file_id = file_id.to_string();
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(index_io)?;
        }
        with_file_lock(&path, None, move || {
            let index = index;
            let locked_path = locked_path;
            async move {
                match index.remove_locked(&scope, &variant_id, &file_id).await {
                    Ok(()) => Ok(()),
                    Err(error) => Err(lock_error(&locked_path, error)),
                }
            }
        })
        .await
        .map_err(|error| index_io(error))
    }

    async fn remove_locked(
        &self,
        scope: &str,
        variant_id: &str,
        file_id: &str,
    ) -> Result<(), LlmError> {
        let records = self.load().await?;
        let next: Vec<DeepSeekUploadRecord> = records
            .iter()
            .filter(|record| {
                !(record.scope == scope
                    && record.variant_id == variant_id
                    && record.file_id == file_id)
            })
            .cloned()
            .collect();
        if next.len() != records.len() {
            self.save(&next).await?;
        }
        Ok(())
    }

    /// Remove every local mapping for one remote namespace.
    ///
    /// # Errors
    /// Filesystem failures while locking or writing the index.
    pub async fn clear(&self, scope: &str) -> Result<(), LlmError> {
        let path = self.path.clone();
        let locked_path = path.clone();
        let index = self.clone();
        let scope = scope.to_string();
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(index_io)?;
        }
        with_file_lock(&path, None, move || {
            let index = index;
            let locked_path = locked_path;
            async move {
                match index.clear_locked(&scope).await {
                    Ok(()) => Ok(()),
                    Err(error) => Err(lock_error(&locked_path, error)),
                }
            }
        })
        .await
        .map_err(|error| index_io(error))
    }

    async fn clear_locked(&self, scope: &str) -> Result<(), LlmError> {
        let records = self.load().await?;
        let next: Vec<DeepSeekUploadRecord> = records
            .iter()
            .filter(|record| record.scope != scope)
            .cloned()
            .collect();
        if next.len() != records.len() {
            self.save(&next).await?;
        }
        Ok(())
    }
}

fn reusable(record: &DeepSeekUploadRecord, now_ms: u64, refresh_margin_ms: u64) -> bool {
    record.expires_at.saturating_sub(now_ms) > refresh_margin_ms
}

fn parse_index(text: &str) -> Result<Vec<DeepSeekUploadRecord>, ()> {
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    if object.get("formatVersion").and_then(Value::as_u64)
        != Some(u64::from(UPLOAD_INDEX_FORMAT_VERSION))
    {
        return Err(());
    }
    let records = object.get("records").and_then(Value::as_array).ok_or(())?;
    let mut parsed = Vec::with_capacity(records.len());
    let mut keys = HashSet::new();
    for record in records {
        let record = parse_record(record).ok_or(())?;
        let key = format!("{}\0{}", record.scope, record.variant_id);
        if !keys.insert(key) {
            return Err(());
        }
        parsed.push(record);
    }
    Ok(parsed)
}

fn parse_record(value: &Value) -> Option<DeepSeekUploadRecord> {
    let object = value.as_object()?;
    let scope = object.get("scope")?.as_str()?;
    if !is_hex64(scope) {
        return None;
    }
    let attachment_id = object.get("attachmentId")?.as_str()?;
    if !is_sha256_id(attachment_id) {
        return None;
    }
    let variant_id = object.get("variantId")?.as_str()?;
    if !is_sha256_id(variant_id) {
        return None;
    }
    let file_id = object.get("fileId")?.as_str()?;
    if file_id.is_empty() {
        return None;
    }
    let bytes = object.get("bytes")?.as_u64()?;
    let created_at = object.get("createdAt")?.as_u64()?;
    let expires_at = object.get("expiresAt")?.as_u64()?;
    Some(DeepSeekUploadRecord {
        scope: scope.to_string(),
        attachment_id: attachment_id.to_string(),
        variant_id: variant_id.to_string(),
        file_id: file_id.to_string(),
        bytes,
        created_at,
        expires_at,
    })
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_sha256_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_hex64)
}

fn trim_base(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

fn index_io(error: impl std::fmt::Display) -> LlmError {
    LlmError::Failure(LlmFailure::new(error.to_string(), "TRANSPORT"))
}

fn lock_error(path: &Path, error: LlmError) -> dsh_atomic_write::AtomicWriteError {
    dsh_atomic_write::AtomicWriteError::Io(std::io::Error::other(format!(
        "{}: {error:?}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_stable_and_strips_trailing_slash() {
        let left = deep_seek_file_scope("https://api.deepseek.com/", "key");
        let right = deep_seek_file_scope("https://api.deepseek.com", "key");
        assert_eq!(left, right);
        assert_eq!(left.len(), 64);
        assert_ne!(
            deep_seek_file_scope("https://api.deepseek.com", "key"),
            deep_seek_file_scope("https://api.deepseek.com", "other")
        );
    }

    #[test]
    fn corrupt_index_text_is_empty() {
        assert!(parse_index("{").is_err());
        assert!(parse_index(r#"{"formatVersion":2,"records":[]}"#).is_err());
        assert!(parse_index(r#"{"formatVersion":3,"records":[{}]}"#).is_err());
    }

    #[tokio::test]
    async fn commit_get_remove_and_corrupt_load() {
        let path = std::env::temp_dir()
            .join(format!(
                "dsh-files-index-{}-{}",
                std::process::id(),
                UuidLike::now()
            ))
            .join("files-v3.json");
        let _ = tokio::fs::remove_file(&path).await;
        let index = DeepSeekUploadIndex::new(Some(path.clone()));
        let scope = deep_seek_file_scope("https://api.deepseek.com", "key");
        let record = DeepSeekUploadRecord {
            scope: scope.clone(),
            attachment_id: format!("sha256:{}", "a".repeat(64)),
            variant_id: format!("sha256:{}", "b".repeat(64)),
            file_id: "file-api-1".into(),
            bytes: 4,
            created_at: 1_000,
            expires_at: 10_000_000,
        };
        let committed = index.commit(record.clone(), 0, 3_600_000).await.unwrap();
        assert!(committed.accepted);
        let hit = index
            .get(&scope, &record.variant_id, 0, 3_600_000)
            .await
            .unwrap();
        assert_eq!(
            hit.as_ref().map(|item| item.file_id.as_str()),
            Some("file-api-1")
        );
        let near_expiry = index
            .get(&scope, &record.variant_id, 9_000_000, 3_600_000)
            .await
            .unwrap();
        assert!(near_expiry.is_none());
        index
            .remove(&scope, &record.variant_id, "file-api-1")
            .await
            .unwrap();
        assert!(index
            .get(&scope, &record.variant_id, 0, 3_600_000)
            .await
            .unwrap()
            .is_none());
        tokio::fs::write(&path, "{not-json").await.unwrap();
        assert!(index
            .get(&scope, &record.variant_id, 0, 3_600_000)
            .await
            .unwrap()
            .is_none());
        let _ = tokio::fs::remove_file(&path).await;
    }

    struct UuidLike;
    impl UuidLike {
        fn now() -> u128 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        }
    }
}
