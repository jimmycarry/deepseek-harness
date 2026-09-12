//! DeepSeek LLM adapter. Chat uses SSE (`stream: true`); a missing `[DONE]` is
//! `STREAM_CLOSED`. Vision models upload request images through the Files API
//! and send `{type:"file",file_id}`; a failed or timed-out resolution rebuilds
//! the same request as `image_url` data-URLs. Self-skips with-key tests when
//! `DEEPSEEK_API_KEY` is unset.

mod file_store;
mod files_api;
mod http;
mod sse;
mod translate;
mod upload_index;

pub use file_store::{
    DeepSeekFileConnection, DeepSeekFilePolicy, DeepSeekFileReference, DeepSeekFileStore,
    MAX_CHAT_IMAGE_BYTES,
};
pub use files_api::{
    is_files_quota_error, DeepSeekFileObject, DeepSeekFilePage, DeepSeekFilesClient,
    DeepSeekFilesError, MAX_FILE_EXPIRY_SECONDS, MAX_FILE_UPLOAD_BYTES, MAX_STORED_FILE_BYTES,
    MAX_STORED_FILE_COUNT, MIN_FILE_EXPIRY_SECONDS,
};
pub use sse::{parse_sse, DONE};
pub use translate::{map_finish_reason, map_usage, translate};
pub use upload_index::{deep_seek_file_scope, DeepSeekUploadIndex, DeepSeekUploadRecord};

use async_trait::async_trait;
use dsh_credentials::{Credential, CredentialsRuntime};
use dsh_llm::{
    content_has_image, is_context_window_exceeded_error, is_quota_exceeded_error,
    provider_retry_after_ms, ContentBlock, LlmAdapter, LlmError, LlmFailure, LlmModelContext,
    LlmRequest, LlmResolvedModelInfo, Message, StreamChunk, APP_IDENTITY,
    CONTEXT_WINDOW_EXCEEDED_CODE, QUOTA_EXCEEDED_CODE,
};
use dsh_timeout::MAX_TIMER_DELAY_MS;
use futures::stream::{self, BoxStream};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;

/// The single provider route this plugin owns.
pub const PROVIDER: &str = "deepseek-official";

/// Default environment variable that holds the API key.
pub const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Positive context capacity used when a catalog entry has none (TypeScript default).
pub const DEFAULT_CONTEXT_WINDOW: u32 = 1_000_000;
/// Default explicit lifetime for uploaded images (seven days).
pub const DEFAULT_FILE_EXPIRY_SECONDS: u32 = 7 * 24 * 60 * 60;
/// Default proactive refresh window for indexed file ids (one hour).
pub const DEFAULT_FILE_REFRESH_MARGIN_SECONDS: u32 = 60 * 60;
/// Default number of oldest harness-owned files removed on quota recovery.
pub const DEFAULT_FILE_QUOTA_CLEANUP_BATCH: u32 = 100;
/// Default deadline for resolving one request image through the Files API.
pub const DEFAULT_FILES_API_TIMEOUT_MS: u64 = 60_000;

/// Deterministic request-image bytes uploaded or inlined for one attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequestImage {
    /// Durable attachment id (`sha256:` + hex).
    pub attachment_id: String,
    /// Request-version identity (`sha256:` + hex).
    pub variant_id: String,
    /// Media type of `data`.
    pub media_type: String,
    /// Exact request bytes.
    pub data: Vec<u8>,
    /// Request-image width used in the model-visible handle.
    pub width: u32,
    /// Request-image height used in the model-visible handle.
    pub height: u32,
}

impl PreparedRequestImage {
    /// Build a request image and derive `variant_id` from its bytes.
    #[must_use]
    pub fn from_bytes(
        attachment_id: impl Into<String>,
        media_type: impl Into<String>,
        data: Vec<u8>,
        width: u32,
        height: u32,
    ) -> Self {
        let attachment_id = attachment_id.into();
        let media_type = media_type.into();
        let variant_id = request_variant_id(&attachment_id, &media_type, &data);
        Self {
            attachment_id,
            variant_id,
            media_type,
            data,
            width,
            height,
        }
    }
}

/// SHA-256 identity of one prepared request image.
#[must_use]
pub fn request_variant_id(attachment_id: &str, media_type: &str, data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(attachment_id.as_bytes());
    hasher.update([0]);
    hasher.update(media_type.as_bytes());
    hasher.update([0]);
    hasher.update(data);
    format!("sha256:{:x}", hasher.finalize())
}

/// Stable model-facing handle for one exact request image.
#[must_use]
pub fn request_image_handle_text(attachment_id: &str, width: u32, height: u32) -> String {
    format!("Image {attachment_id}; request image {width}x{height}px.")
}

/// Harness `User-Agent` sent on every chat and Files request.
#[must_use]
pub fn user_agent() -> String {
    format!(
        "{}/{} (+{})",
        APP_IDENTITY.product, APP_IDENTITY.version, APP_IDENTITY.url
    )
}

/// Validated Files timeout and upload policy from plugin or settings config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRuntimeConfig {
    /// Per-image Files resolution deadline.
    pub files_api_timeout_ms: u64,
    /// Upload expiry, refresh, and quota-recovery policy.
    pub policy: DeepSeekFilePolicy,
}

impl Default for FileRuntimeConfig {
    fn default() -> Self {
        Self {
            files_api_timeout_ms: DEFAULT_FILES_API_TIMEOUT_MS,
            policy: DeepSeekFilePolicy::default(),
        }
    }
}

/// Validate Files API timeout and lifetime fields. Omitted keys use TypeScript defaults.
///
/// # Errors
/// A non-positive or over-cap `filesApiTimeoutMs`, an expiry outside 3600–2592000,
/// a refresh margin that is negative or not below expiry, or a quota batch
/// outside 1–1000.
pub fn resolve_file_runtime(config: Option<&Value>) -> Result<FileRuntimeConfig, String> {
    let files_api_timeout_ms = match config.and_then(|value| value.get("filesApiTimeoutMs")) {
        None => DEFAULT_FILES_API_TIMEOUT_MS,
        Some(value) => {
            let number = value.as_f64().ok_or_else(|| {
                format!(
                    "llm-deepseek: filesApiTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
                )
            })?;
            if !number.is_finite() || number <= 0.0 || number > MAX_TIMER_DELAY_MS as f64 {
                return Err(format!(
                    "llm-deepseek: filesApiTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
                ));
            }
            number as u64
        }
    };
    let expires_after_seconds = match config.and_then(|value| value.get("fileExpiresAfterSeconds"))
    {
        None => DEFAULT_FILE_EXPIRY_SECONDS,
        Some(value) => {
            let number = value.as_u64().ok_or_else(|| {
                "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000"
                    .to_string()
            })?;
            if !(3_600..=2_592_000).contains(&number) {
                return Err(
                    "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000"
                        .into(),
                );
            }
            u32::try_from(number).map_err(|_| {
                "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000"
                    .to_string()
            })?
        }
    };
    let refresh_margin_seconds = match config
        .and_then(|value| value.get("fileRefreshMarginSeconds"))
    {
        None => DEFAULT_FILE_REFRESH_MARGIN_SECONDS,
        Some(value) => {
            let number = value.as_u64().ok_or_else(|| {
                    "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds"
                        .to_string()
                })?;
            if number >= u64::from(expires_after_seconds) {
                return Err(
                        "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds"
                            .into(),
                    );
            }
            u32::try_from(number).map_err(|_| {
                    "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds"
                        .to_string()
                })?
        }
    };
    let quota_cleanup_batch = match config.and_then(|value| value.get("fileQuotaCleanupBatch")) {
        None => DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
        Some(value) => {
            let number = value.as_u64().ok_or_else(|| {
                "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000"
                    .to_string()
            })?;
            if !(1..=1_000).contains(&number) {
                return Err(
                    "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000"
                        .into(),
                );
            }
            u32::try_from(number).map_err(|_| {
                "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000"
                    .to_string()
            })?
        }
    };
    Ok(FileRuntimeConfig {
        files_api_timeout_ms,
        policy: DeepSeekFilePolicy {
            expires_after_seconds,
            refresh_margin_seconds,
            quota_cleanup_batch,
        },
    })
}

/// One advisory catalog entry used by `resolve_model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// Wire model id.
    pub id: String,
    /// Combined request/response context when configured.
    pub context_window: Option<u32>,
}

/// Default V4 Flash / Pro / Flash-Vision catalog.
pub fn default_models() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            id: "deepseek-v4-flash".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
        CatalogModel {
            id: "deepseek-v4-pro".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
        CatalogModel {
            id: "deepseek-v4-flash-vision-exp".into(),
            context_window: Some(DEFAULT_CONTEXT_WINDOW),
        },
    ]
}

/// Validate `defaultContextWindow` and `models` from plugin config or a settings section.
///
/// # Errors
/// A non-positive `defaultContextWindow`, a non-array `models` value, an empty or
/// duplicate catalog id, or a non-positive per-model `contextWindow`.
pub fn resolve_catalog(config: Option<&Value>) -> Result<(u32, Vec<CatalogModel>), String> {
    let default_context_window = match config.and_then(|value| value.get("defaultContextWindow")) {
        None => DEFAULT_CONTEXT_WINDOW,
        Some(value) => positive_u32(value).ok_or_else(|| {
            "llm-deepseek: defaultContextWindow must be a positive integer".to_string()
        })?,
    };
    let models = match config.and_then(|value| value.get("models")) {
        None => default_models(),
        Some(value) => resolve_models(value)?,
    };
    Ok((default_context_window, models))
}

/// Context capacity for `model`: exact catalog value, else `default_context_window`.
pub fn context_window_for(
    model: &str,
    default_context_window: u32,
    models: &[CatalogModel],
) -> u32 {
    models
        .iter()
        .find(|entry| entry.id == model)
        .and_then(|entry| entry.context_window)
        .unwrap_or(default_context_window)
}

fn resolve_models(value: &Value) -> Result<Vec<CatalogModel>, String> {
    let Some(items) = value.as_array() else {
        return Err("llm-deepseek: models must be an array".into());
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut models = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            return Err("llm-deepseek: catalog model ids must be non-empty".into());
        }
        if !seen.insert(id.clone()) {
            return Err(format!("llm-deepseek: duplicate catalog model \"{id}\""));
        }
        let context_window = match item.get("contextWindow") {
            None => None,
            Some(value) => Some(positive_u32(value).ok_or_else(|| {
                format!(
                    "llm-deepseek: catalog model \"{id}\" contextWindow must be a positive integer"
                )
            })?),
        };
        models.push(CatalogModel { id, context_window });
    }
    Ok(models)
}

fn positive_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number > 0)
}

fn overlay_section(plugin: Option<&Value>, settings: Option<&Value>) -> Value {
    let mut map = match plugin {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    if let Some(Value::Object(section)) = settings {
        for (key, value) in section {
            map.insert(key.clone(), value.clone());
        }
    }
    Value::Object(map)
}

/// Layer a live `llm-deepseek` settings section over plugin config.
pub fn merge_connection_config(plugin: Option<&Value>, settings: Option<&Value>) -> Value {
    overlay_section(plugin, settings)
}

/// Exact TypeScript `MISSING_CREDENTIAL` failure for a missing API key.
pub fn missing_api_key(api_key_env: &str) -> LlmError {
    LlmError::Failure(LlmFailure::new(
        format!(
            "llm-deepseek: no API key for provider route \"{PROVIDER}\"; store {api_key_env} through the credentials service (the web Models page writes it), or export {api_key_env} in the launching environment"
        ),
        "MISSING_CREDENTIAL",
    ))
}

/// Resolve the API key through `ctx.credentials` when mounted, else the launch env.
///
/// # Errors
/// [`MISSING_CREDENTIAL`](missing_api_key) when no usable key exists.
pub fn resolve_api_key(
    credentials: Option<&CredentialsRuntime>,
    api_key_env: &str,
) -> std::result::Result<String, LlmError> {
    if let Some(credentials) = credentials {
        match credentials.resolve(api_key_env) {
            Credential::Set(value) => Ok(value),
            Credential::Unset => Err(missing_api_key(api_key_env)),
        }
    } else {
        std::env::var(api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| missing_api_key(api_key_env))
    }
}

fn catalog_error(message: String) -> LlmError {
    LlmError::Failure(LlmFailure::new(message, "CONFIG"))
}

/// Map an HTTP status to a stable `LlmError` code.
#[must_use]
pub fn http_error_code(status: u16, error: Option<&WireErrorDetail>) -> String {
    if status == 401 || status == 403 {
        return "AUTH".into();
    }
    if status == 413 {
        return "INVALID_REQUEST".into();
    }
    let detail = [
        error.and_then(|item| item.code.as_deref()),
        error.and_then(|item| item.r#type.as_deref()),
        error.and_then(|item| item.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    if is_quota_exceeded_error(&detail) {
        return QUOTA_EXCEEDED_CODE.into();
    }
    if status == 429 {
        return "RATE_LIMIT".into();
    }
    if status == 400 {
        if is_context_window_exceeded_error(&detail) {
            return CONTEXT_WINDOW_EXCEEDED_CODE.into();
        }
        return "INVALID_REQUEST".into();
    }
    if status >= 500 {
        return "SERVER".into();
    }
    format!("HTTP_{status}")
}

/// Provider `error` object fields used for classification.
#[derive(Debug, Clone, Default)]
pub struct WireErrorDetail {
    /// Provider machine code.
    pub code: Option<String>,
    /// Provider error type.
    pub r#type: Option<String>,
    /// Provider human-readable message.
    pub message: Option<String>,
}

/// DeepSeek chat adapter.
pub struct DeepSeekAdapter {
    /// API key resolved at construction.
    pub api_key: String,
    /// Optional base URL override.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Prepared request images keyed by attachment id.
    pub images: HashMap<String, PreparedRequestImage>,
    /// Upload reuse store. `None` sends inline data-URLs without calling Files.
    pub files: Option<DeepSeekFileStore>,
    /// Upload expiry, refresh, and quota-recovery policy.
    pub file_policy: DeepSeekFilePolicy,
    /// Per-image Files resolution deadline.
    pub files_api_timeout_ms: u64,
}

/// Whether this catalog model accepts image input.
pub fn model_accepts_image(model: &str) -> bool {
    model.contains("vision")
}

impl DeepSeekAdapter {
    /// Chat-only adapter with no Files store (inline images if any).
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            images: HashMap::new(),
            files: None,
            file_policy: DeepSeekFilePolicy::default(),
            files_api_timeout_ms: DEFAULT_FILES_API_TIMEOUT_MS,
        }
    }

    /// Build from the process environment. Missing key fails loud.
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = resolve_api_key(None, DEFAULT_API_KEY_ENV)?;
        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".into());
        Ok(Self {
            files: Some(DeepSeekFileStore::default_store()),
            ..Self::new(api_key, base_url, "deepseek-chat")
        })
    }

    async fn complete_chat(&self, url: &str, request: &LlmRequest) -> Result<String, LlmError> {
        let has_images = request_has_user_image(&request.messages);
        let mut representation = if has_images && self.files.is_some() {
            ImageWire::File
        } else {
            ImageWire::Base64
        };
        let mut file_attempt = 0u8;
        loop {
            let (body, used) = match representation {
                ImageWire::File => match self.resolve_file_ids().await {
                    Ok(used) => {
                        let ids = used
                            .iter()
                            .map(|item| (item.attachment_id.clone(), item.file_id.clone()))
                            .collect();
                        (
                            request_body(
                                &self.model,
                                request,
                                &self.images,
                                ImageWireKind::File(ids),
                            )?,
                            used,
                        )
                    }
                    Err(error) if is_file_resolution_failure(&error) => {
                        representation = ImageWire::Base64;
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                ImageWire::Base64 => (
                    request_body(&self.model, request, &self.images, ImageWireKind::Base64)?,
                    Vec::new(),
                ),
            };
            match post_json(url, &self.api_key, &body).await {
                Ok(raw) => return Ok(raw),
                Err(failure) => {
                    let stale = !used.is_empty() && provider_rejected_file_id(&failure.detail);
                    if stale {
                        if let Some(store) = &self.files {
                            let connection = DeepSeekFileConnection {
                                base_url: self.base_url.clone(),
                                api_key: self.api_key.clone(),
                            };
                            for item in stale_mappings(&used, &failure.detail) {
                                if let Some(image) = self.images.get(&item.attachment_id) {
                                    store.invalidate(image, &item.file_id, &connection).await?;
                                }
                            }
                        }
                        if file_attempt == 0 {
                            file_attempt = 1;
                            continue;
                        }
                    }
                    return Err(failure.error);
                }
            }
        }
    }

    async fn resolve_file_ids(&self) -> Result<Vec<UsedRequestFile>, LlmError> {
        let store = self
            .files
            .as_ref()
            .expect("file representation requires a file store");
        let connection = DeepSeekFileConnection {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
        };
        let mut used = Vec::new();
        for image in self.images.values() {
            let resolved = match tokio::time::timeout(
                Duration::from_millis(self.files_api_timeout_ms),
                store.ensure_uploaded(image, &connection, &self.file_policy),
            )
            .await
            {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(error)) => {
                    return Err(file_resolution_failure(error));
                }
                Err(_deadline) => {
                    return Err(file_resolution_failure(LlmError::Failure(LlmFailure::new(
                        "DeepSeek Files API could not resolve a request image.",
                        "DEEPSEEK_FILES_API_TIMEOUT",
                    ))));
                }
            };
            used.push(UsedRequestFile {
                attachment_id: image.attachment_id.clone(),
                file_id: resolved.record.file_id,
            });
        }
        Ok(used)
    }
}

#[async_trait]
impl LlmAdapter for DeepSeekAdapter {
    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<BoxStream<'static, StreamChunk>, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Failure(LlmFailure::new(
                "empty key",
                "MISSING_CREDENTIAL",
            )));
        }
        let url = join_url(&self.base_url, "/chat/completions");
        let raw = self.complete_chat(&url, &request).await?;
        let payloads = parse_sse(&raw)?;
        let chunks = translate(&payloads)?;
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let (default_window, models) = resolve_catalog(None).map_err(catalog_error)?;
        Ok(LlmResolvedModelInfo {
            context: Some(LlmModelContext {
                context_window: context_window_for(model, default_window, &models),
            }),
            ..LlmResolvedModelInfo::identity(provider, model)
        })
    }
}

enum ImageWire {
    File,
    Base64,
}

enum ImageWireKind {
    File(HashMap<String, String>),
    Base64,
}

#[derive(Clone)]
struct UsedRequestFile {
    attachment_id: String,
    file_id: String,
}

struct ChatFailure {
    error: LlmError,
    detail: String,
}

const FILE_RESOLUTION_CODE: &str = "FILE_RESOLUTION_FAILURE";

fn file_resolution_failure(cause: LlmError) -> LlmError {
    let LlmError::Failure(failure) = cause;
    LlmError::Failure(LlmFailure {
        message: "DeepSeek Files API could not resolve a request image.".into(),
        code: FILE_RESOLUTION_CODE.into(),
        status: failure.status,
        provider_retry_after_ms: failure.provider_retry_after_ms,
        request_id: failure.request_id,
    })
}

fn is_file_resolution_failure(error: &LlmError) -> bool {
    let LlmError::Failure(failure) = error;
    failure.code == FILE_RESOLUTION_CODE
}

fn request_has_user_image(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => content_has_image(&user.content),
        _ => false,
    })
}

fn provider_rejected_file_id(detail: &str) -> bool {
    let hay = detail.to_ascii_lowercase();
    let file = hay.contains("file");
    let missing = hay.contains("expired")
        || hay.contains("not found")
        || hay.contains("not_found")
        || hay.contains("not-found")
        || hay.contains("deleted")
        || hay.contains("does not exist")
        || hay.contains("do not exist")
        || hay.contains("not created under this account")
        || hay.contains("not created under your account");
    let invalid_id = invalid_near_file_id(&hay);
    file && (missing || invalid_id)
}

fn invalid_near_file_id(detail: &str) -> bool {
    let bytes = detail.as_bytes();
    let mut start = 0;
    while let Some(rel) = detail[start..].find("invalid") {
        let index = start + rel;
        let window = &detail[index.saturating_sub(20)..(index + 27).min(detail.len())];
        if window.contains("file") {
            return true;
        }
        start = index + 7;
        if start >= bytes.len() {
            break;
        }
    }
    false
}

fn detail_names_file_id(detail: &str, file_id: &str) -> bool {
    let mut start = 0;
    while let Some(rel) = detail[start..].find(file_id) {
        let index = start + rel;
        let before = index
            .checked_sub(1)
            .and_then(|pos| detail[pos..].chars().next());
        let after = detail[index + file_id.len()..].chars().next();
        let before_ok = before.is_none_or(|ch| !is_id_char(ch));
        let after_ok = after.is_none_or(|ch| !is_id_char(ch));
        if before_ok && after_ok {
            return true;
        }
        start = index + file_id.len();
        if start >= detail.len() {
            break;
        }
    }
    false
}

fn is_id_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

fn stale_mappings<'a>(files: &'a [UsedRequestFile], detail: &str) -> Vec<&'a UsedRequestFile> {
    let exact: Vec<&UsedRequestFile> = files
        .iter()
        .filter(|file| detail_names_file_id(detail, &file.file_id))
        .collect();
    if exact.is_empty() {
        files.iter().collect()
    } else {
        exact
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn request_body(
    model: &str,
    request: &LlmRequest,
    images: &HashMap<String, PreparedRequestImage>,
    wire: ImageWireKind,
) -> Result<String, LlmError> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        match message {
            Message::User(user) => {
                messages.push(json!({
                    "role": "user",
                    "content": user_content(&user.content, model, images, &wire)?,
                }));
            }
            Message::Assistant(assistant) => {
                if content_has_image(&assistant.content) {
                    return Err(unsupported_image_role("assistant"));
                }
                messages.push(json!({ "role": "assistant", "content": assistant.text() }));
            }
            Message::Tool(tool) => {
                let blocks = tool.result_blocks();
                if content_has_image(blocks) {
                    return Err(unsupported_image_role("tool"));
                }
                messages.push(json!({
                    "role": "tool",
                    "content": blocks_text(blocks),
                    "tool_call_id": tool.tool_call_id().unwrap_or(""),
                }));
            }
        }
    }
    Ok(json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    })
    .to_string())
}

fn unsupported_image_role(role: &str) -> LlmError {
    LlmError::Failure(LlmFailure::new(
        format!(
            "The DeepSeek chat-completions adapter cannot represent image content in a {role} message."
        ),
        "UNSUPPORTED_CONTENT",
    ))
}

fn user_content(
    blocks: &[ContentBlock],
    model: &str,
    images: &HashMap<String, PreparedRequestImage>,
    wire: &ImageWireKind,
) -> Result<Value, LlmError> {
    if !content_has_image(blocks) {
        return Ok(Value::String(blocks_text(blocks)));
    }
    if !model_accepts_image(model) {
        return Err(LlmError::Failure(LlmFailure::new(
            format!("DeepSeek model \"{model}\" does not accept image input."),
            "UNSUPPORTED_CONTENT",
        )));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    parts.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentBlock::Image { attachment } => {
                let Some(image) = images.get(&attachment.attachment_id) else {
                    return Err(LlmError::Failure(LlmFailure::new(
                        format!(
                            "DeepSeek request image {} was not prepared.",
                            attachment.attachment_id
                        ),
                        "INVALID_REQUEST",
                    )));
                };
                let handle =
                    request_image_handle_text(&attachment.attachment_id, image.width, image.height);
                let handle = if parts.is_empty() {
                    handle
                } else {
                    format!("\n{handle}")
                };
                parts.push(json!({ "type": "text", "text": handle }));
                match wire {
                    ImageWireKind::File(ids) => {
                        let Some(file_id) = ids.get(&attachment.attachment_id) else {
                            return Err(LlmError::Failure(LlmFailure::new(
                                format!(
                                    "DeepSeek request image {} was not prepared.",
                                    attachment.attachment_id
                                ),
                                "INVALID_REQUEST",
                            )));
                        };
                        parts.push(json!({ "type": "file", "file_id": file_id }));
                    }
                    ImageWireKind::Base64 => {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!(
                                    "data:{};base64,{}",
                                    image.media_type,
                                    encode_base64(&image.data)
                                )
                            }
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Value::Array(parts))
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if b1.is_none() {
            out.push('=');
            out.push('=');
        } else {
            out.push(
                TABLE[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            );
            if b2.is_none() {
                out.push('=');
            } else {
                out.push(TABLE[(b2.unwrap_or(0) & 0x3f) as usize] as char);
            }
        }
        i += 3;
    }
    out
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn post_json(url: &str, api_key: &str, body: &str) -> Result<String, ChatFailure> {
    let headers = [
        ("Authorization", format!("Bearer {api_key}")),
        ("Accept", "text/event-stream".into()),
        ("User-Agent", user_agent()),
    ];
    let response = crate::http::http_exchange(
        "POST",
        url,
        &headers,
        crate::http::HttpBody::Bytes {
            content_type: "application/json",
            data: body.as_bytes(),
        },
    )
    .await
    .map_err(|message| ChatFailure {
        error: LlmError::Failure(LlmFailure::new(message, "TRANSPORT")),
        detail: String::new(),
    })?;
    classify_or_body(response)
}

fn classify_or_body(response: crate::http::HttpResponse) -> Result<String, ChatFailure> {
    if (200..300).contains(&response.status) {
        return Ok(response.body);
    }
    Err(http_failure(response))
}

fn http_failure(response: crate::http::HttpResponse) -> ChatFailure {
    let (provider_message, detail) = parse_wire_error(&response.body);
    let joined = [
        detail.as_ref().and_then(|item| item.code.as_deref()),
        detail.as_ref().and_then(|item| item.r#type.as_deref()),
        detail.as_ref().and_then(|item| item.message.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let message = provider_message
        .unwrap_or_else(|| format!("DeepSeek API error (HTTP {})", response.status));
    let delay = response
        .retry_after
        .as_deref()
        .and_then(|value| provider_retry_after_ms(value, std::time::SystemTime::now()));
    ChatFailure {
        error: LlmError::Failure(LlmFailure {
            message,
            code: http_error_code(response.status, detail.as_ref()),
            status: Some(response.status),
            provider_retry_after_ms: delay,
            request_id: response.request_id,
        }),
        detail: joined,
    }
}

fn parse_wire_error(body: &str) -> (Option<String>, Option<WireErrorDetail>) {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return (None, None);
    };
    let Some(error) = value.get("error") else {
        return (None, None);
    };
    let detail = WireErrorDetail {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string),
        r#type: error
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    (detail.message.clone(), Some(detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{LlmCallConfig, MessageSource, UserMessage};
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn from_env_fails_loud_without_key() {
        let previous = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("DEEPSEEK_API_KEY");
        let Err(error) = DeepSeekAdapter::from_env() else {
            panic!("from_env must fail when DEEPSEEK_API_KEY is unset");
        };
        let LlmError::Failure(failure) = error;
        assert_eq!(failure.code, "MISSING_CREDENTIAL");
        assert!(
            failure
                .message
                .contains("no API key for provider route \"deepseek-official\""),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("DEEPSEEK_API_KEY"),
            "{}",
            failure.message
        );
        if let Some(previous) = previous {
            std::env::set_var("DEEPSEEK_API_KEY", previous);
        }
    }

    #[tokio::test]
    async fn stream_posts_http_and_parses_content() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16_384];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let adapter = DeepSeekAdapter::new("test-key", format!("http://{addr}"), "deepseek-chat");
        let stream = adapter
            .stream(LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::text("ping"))],
                tools: vec![],
                purpose: None,
            })
            .await
            .unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TextDelta { text, .. } if text == "pong"
        )));
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(request.contains("Authorization: Bearer test-key"));
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("text/event-stream"));
    }

    #[test]
    fn default_catalog_windows_and_settings_overrides() {
        let (default_window, models) = resolve_catalog(None).unwrap();
        assert_eq!(default_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(
            context_window_for("deepseek-v4-flash", default_window, &models),
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(
            context_window_for("unlisted-pass-through", default_window, &models),
            DEFAULT_CONTEXT_WINDOW
        );
        let (default_window, models) = resolve_catalog(Some(&json!({
            "defaultContextWindow": 256_000,
            "models": [
                { "id": "private-fast", "contextWindow": 32_000 },
                { "id": "inherits-default" }
            ]
        })))
        .unwrap();
        assert_eq!(default_window, 256_000);
        assert_eq!(
            context_window_for("private-fast", default_window, &models),
            32_000
        );
        assert_eq!(
            context_window_for("inherits-default", default_window, &models),
            256_000
        );
        assert_eq!(
            context_window_for("unlisted-pass-through", default_window, &models),
            256_000
        );
        let err = resolve_catalog(Some(&json!({ "defaultContextWindow": 0 }))).unwrap_err();
        assert!(
            err.contains("defaultContextWindow must be a positive integer"),
            "{err}"
        );
        let err = resolve_catalog(Some(&json!({
            "models": [{ "id": "m", "contextWindow": 0 }]
        })))
        .unwrap_err();
        assert!(
            err.contains("contextWindow must be a positive integer"),
            "{err}"
        );
        let merged = merge_connection_config(
            Some(&json!({ "defaultContextWindow": 1000, "baseURL": "https://plugin.test" })),
            Some(&json!({ "defaultContextWindow": 2000 })),
        );
        assert_eq!(merged["defaultContextWindow"], 2000);
        assert_eq!(merged["baseURL"], "https://plugin.test");
    }

    fn sample_image() -> (ContentBlock, HashMap<String, PreparedRequestImage>) {
        let id = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let block = ContentBlock::Image {
            attachment: dsh_llm::ImageContentRef {
                attachment_id: id.into(),
                media_type: "image/png".into(),
                bytes: 4,
                width: 1,
                height: 1,
                name: None,
            },
        };
        let mut images = HashMap::new();
        images.insert(
            id.into(),
            PreparedRequestImage::from_bytes(id, "image/png", vec![1, 2, 3, 4], 1, 1),
        );
        (block, images)
    }

    fn sample_request(block: ContentBlock) -> LlmRequest {
        LlmRequest {
            config: LlmCallConfig::default(),
            adapter_defaults: None,
            system: None,
            messages: vec![Message::User(UserMessage::from_parts(
                vec![ContentBlock::text("see"), block],
                MessageSource::User,
            ))],
            tools: vec![],
            purpose: None,
        }
    }

    #[test]
    fn vision_user_image_becomes_data_url() {
        let (block, images) = sample_image();
        let body = request_body(
            "deepseek-v4-flash-vision-exp",
            &sample_request(block),
            &images,
            ImageWireKind::Base64,
        )
        .unwrap();
        assert!(body.contains("\"stream\":true"));
        assert!(body.contains("image_url"));
        assert!(body.contains("data:image/png;base64,"));
        assert!(body.contains(
            "Image sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; request image 1x1px."
        ));
    }

    #[test]
    fn vision_user_image_uses_file_id_when_resolved() {
        let (block, images) = sample_image();
        let mut ids = HashMap::new();
        ids.insert(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "file-api-1".into(),
        );
        let body = request_body(
            "deepseek-v4-flash-vision-exp",
            &sample_request(block),
            &images,
            ImageWireKind::File(ids),
        )
        .unwrap();
        assert!(body.contains("\"type\":\"file\""));
        assert!(body.contains("\"file_id\":\"file-api-1\""));
        assert!(!body.contains("image_url"));
    }

    #[test]
    fn non_vision_model_refuses_image_input() {
        let (block, images) = sample_image();
        let err = request_body(
            "deepseek-chat",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::from_parts(
                    vec![block],
                    MessageSource::User,
                ))],
                tools: vec![],
                purpose: None,
            },
            &images,
            ImageWireKind::Base64,
        )
        .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "UNSUPPORTED_CONTENT");
        assert_eq!(
            failure.message,
            "DeepSeek model \"deepseek-chat\" does not accept image input."
        );
    }

    #[test]
    fn missing_prepared_image_fails_loud() {
        let (block, _) = sample_image();
        let err = request_body(
            "deepseek-v4-flash-vision-exp",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::from_parts(
                    vec![block],
                    MessageSource::User,
                ))],
                tools: vec![],
                purpose: None,
            },
            &HashMap::new(),
            ImageWireKind::Base64,
        )
        .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "INVALID_REQUEST");
        assert!(
            failure.message.contains("was not prepared"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn assistant_and_tool_images_are_unsupported() {
        let (block, images) = sample_image();
        let assistant = request_body(
            "deepseek-v4-flash-vision-exp",
            &LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::Assistant(dsh_llm::AssistantMessage::model(
                    vec![block.clone()],
                    "deepseek-official",
                    "deepseek-v4-flash-vision-exp",
                ))],
                tools: vec![],
                purpose: None,
            },
            &images,
            ImageWireKind::Base64,
        )
        .unwrap_err();
        let LlmError::Failure(failure) = assistant;
        assert_eq!(failure.code, "UNSUPPORTED_CONTENT");
        assert!(
            failure
                .message
                .contains("cannot represent image content in a assistant message."),
            "{}",
            failure.message
        );
    }

    #[test]
    fn classifies_http_status_and_provider_detail() {
        assert_eq!(http_error_code(401, None), "AUTH");
        assert_eq!(http_error_code(403, None), "AUTH");
        assert_eq!(
            http_error_code(
                400,
                Some(&WireErrorDetail {
                    message: Some("request too large for model context".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            CONTEXT_WINDOW_EXCEEDED_CODE
        );
        assert_eq!(
            http_error_code(
                400,
                Some(&WireErrorDetail {
                    message: Some(
                        "invalid input: temperature exceeds maximum allowed value".into()
                    ),
                    ..WireErrorDetail::default()
                }),
            ),
            "INVALID_REQUEST"
        );
        assert_eq!(
            http_error_code(
                413,
                Some(&WireErrorDetail {
                    code: Some("context_length_exceeded".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            "INVALID_REQUEST"
        );
        assert_eq!(
            http_error_code(
                429,
                Some(&WireErrorDetail {
                    code: Some("insufficient_quota".into()),
                    message: Some("account credits exhausted".into()),
                    ..WireErrorDetail::default()
                }),
            ),
            QUOTA_EXCEEDED_CODE
        );
        assert_eq!(http_error_code(429, None), "RATE_LIMIT");
        assert_eq!(http_error_code(503, None), "SERVER");
        assert_eq!(http_error_code(418, None), "HTTP_418");
    }

    #[tokio::test]
    async fn retains_status_retry_after_seconds_and_request_id() {
        let failure = stream_http_error(
            429,
            r#"{"error":{"message":"slow down"}}"#,
            &[("Retry-After", "2"), ("x-request-id", "req-429")],
        )
        .await;
        assert_eq!(failure.code, "RATE_LIMIT");
        assert_eq!(failure.message, "slow down");
        assert_eq!(failure.status, Some(429));
        assert_eq!(failure.provider_retry_after_ms, Some(2_000));
        assert_eq!(failure.request_id.as_deref(), Some("req-429"));
    }

    #[tokio::test]
    async fn parses_future_retry_after_http_date_and_deepseek_request_id() {
        let when = std::time::SystemTime::now() + std::time::Duration::from_secs(3);
        let header = retry_after_imf(when);
        let failure = stream_http_error(
            503,
            r#"{"error":{"message":"come back later"}}"#,
            &[
                ("Retry-After", header.as_str()),
                ("x-deepseek-request-id", "deepseek-503"),
            ],
        )
        .await;
        assert_eq!(failure.code, "SERVER");
        assert_eq!(failure.message, "come back later");
        assert_eq!(failure.status, Some(503));
        let delay = failure.provider_retry_after_ms.expect("HTTP-date delay");
        assert!(
            (1_000..=5_000).contains(&delay),
            "expected ~3000ms, got {delay}"
        );
        assert_eq!(failure.request_id.as_deref(), Some("deepseek-503"));
    }

    #[tokio::test]
    async fn omits_zero_invalid_and_past_retry_after() {
        for value in [
            "0",
            &"9".repeat(400),
            "not-a-date",
            "Thu, 01 Jan 1970 00:00:00 GMT",
        ] {
            let failure = stream_http_error(
                429,
                r#"{"error":{"message":"retry later"}}"#,
                &[("Retry-After", value)],
            )
            .await;
            assert_eq!(failure.code, "RATE_LIMIT");
            assert_eq!(failure.status, Some(429));
            assert_eq!(failure.provider_retry_after_ms, None, "{value}");
        }
    }

    async fn stream_http_error(status: u16, body: &str, headers: &[(&str, &str)]) -> LlmFailure {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let extra = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16_384];
            let _ = socket.read(&mut buf).await.unwrap();
            let response = format!(
                "HTTP/1.1 {status} ERR\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = DeepSeekAdapter::new("test-key", format!("http://{addr}"), "deepseek-chat");
        let error = adapter
            .stream(LlmRequest {
                config: LlmCallConfig::default(),
                adapter_defaults: None,
                system: None,
                messages: vec![Message::User(UserMessage::text("ping"))],
                tools: vec![],
                purpose: None,
            })
            .await
            .err()
            .expect("non-2xx must fail");
        let LlmError::Failure(failure) = error;
        failure
    }

    fn retry_after_imf(when: std::time::SystemTime) -> String {
        const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = (secs / 86_400) as i64;
        let tod = secs % 86_400;
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u32;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = y + i64::from(m <= 2);
        format!(
            "{}, {d:02} {} {year} {:02}:{:02}:{:02} GMT",
            WEEKDAYS[days.rem_euclid(7) as usize],
            MONTHS[(m - 1) as usize],
            tod / 3_600,
            (tod % 3_600) / 60,
            tod % 60
        )
    }

    #[test]
    fn file_runtime_defaults_and_rejects_illegal_bounds() {
        let resolved = resolve_file_runtime(None).unwrap();
        assert_eq!(resolved.files_api_timeout_ms, DEFAULT_FILES_API_TIMEOUT_MS);
        assert_eq!(
            resolved.policy.expires_after_seconds,
            DEFAULT_FILE_EXPIRY_SECONDS
        );
        let err = resolve_file_runtime(Some(&json!({ "filesApiTimeoutMs": 0 }))).unwrap_err();
        assert!(err.contains("filesApiTimeoutMs must be a positive finite"));
        let err =
            resolve_file_runtime(Some(&json!({ "fileExpiresAfterSeconds": 3599 }))).unwrap_err();
        assert!(err.contains("fileExpiresAfterSeconds must be an integer from 3600"));
        let err = resolve_file_runtime(Some(&json!({
            "fileExpiresAfterSeconds": 3600,
            "fileRefreshMarginSeconds": 3600
        })))
        .unwrap_err();
        assert!(err.contains("fileRefreshMarginSeconds must be a non-negative integer below"));
        let err = resolve_file_runtime(Some(&json!({ "fileQuotaCleanupBatch": 0 }))).unwrap_err();
        assert!(err.contains("fileQuotaCleanupBatch must be an integer from 1 through 1000"));
    }

    #[test]
    fn classifies_stale_file_provider_messages() {
        for detail in [
            "file-api-1 expired",
            "file_id file-api-10 invalid; file_id file-api-1 expired",
            "file_not_found",
            "file_id file-api-1 deleted",
            "invalid file_id file-api-1",
            "file reference expired",
        ] {
            assert!(provider_rejected_file_id(detail), "{detail}");
        }
        assert!(!provider_rejected_file_id("invalid temperature"));
        assert!(detail_names_file_id(
            "file_id file-api-1 expired",
            "file-api-1"
        ));
        assert!(!detail_names_file_id(
            "file_id file-api-10 expired",
            "file-api-1"
        ));
    }

    fn files_store() -> DeepSeekFileStore {
        let path = std::env::temp_dir()
            .join(format!(
                "dsh-files-store-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time")
                    .as_nanos()
            ))
            .join("files-v3.json");
        DeepSeekFileStore::new(DeepSeekUploadIndex::new(Some(path)), None)
    }

    fn sse_ok() -> String {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn http_json(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} ERR\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    fn upload_ok(id: &str, bytes: usize) -> String {
        let body = serde_json::json!({
            "id": id,
            "object": "file",
            "bytes": bytes,
            "created_at": 1_700_000_000,
            "filename": "dsh-aaaaaaaaaaaaaaaa-bbbbbbbb.png",
            "purpose": "user_data",
            "expires_at": 1_700_604_800
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn uploads_once_and_sends_file_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let seen = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                seen.lock().await.push(request.clone());
                if request.starts_with("POST /files") {
                    write_http(&mut socket, &upload_ok("file-api-1", 4)).await;
                } else {
                    write_http(&mut socket, &sse_ok()).await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let stream = adapter.stream(sample_request(block)).await.unwrap();
        let _chunks: Vec<_> = stream.collect().await;
        let requests = captured.lock().await.clone();
        assert!(
            requests.iter().any(|item| item.starts_with("POST /files")),
            "{requests:?}"
        );
        let chat = requests
            .iter()
            .find(|item| item.contains("POST /chat/completions"))
            .expect("chat");
        assert!(chat.contains("\"file_id\":\"file-api-1\""));
        assert!(!chat.contains("image_url"));
        assert!(chat.contains("User-Agent:"));
        assert!(requests.iter().any(|item| {
            item.starts_with("POST /files")
                && item.contains("purpose")
                && item.contains("user_data")
        }));
    }

    #[tokio::test]
    async fn files_failure_falls_back_to_all_base64() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let seen = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                seen.lock().await.push(request.clone());
                if request.starts_with("POST /files") {
                    write_http(
                        &mut socket,
                        &http_json(503, r#"{"error":{"message":"files down"}}"#),
                    )
                    .await;
                } else {
                    write_http(&mut socket, &sse_ok()).await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let stream = adapter.stream(sample_request(block)).await.unwrap();
        let _: Vec<_> = stream.collect().await;
        let requests = captured.lock().await.clone();
        let chat = requests
            .iter()
            .find(|item| item.contains("POST /chat/completions"))
            .expect("chat");
        assert!(chat.contains("image_url"));
        assert!(!chat.contains("file_id"));
    }

    #[tokio::test]
    async fn files_deadline_falls_back_without_aborting_chat() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    write_http(&mut socket, &upload_ok("file-api-late", 4)).await;
                } else {
                    write_http(&mut socket, &sse_ok()).await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            files_api_timeout_ms: 50,
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let stream = adapter.stream(sample_request(block)).await.unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::TextDelta { text, .. } if text == "pong"
        )));
    }

    #[tokio::test]
    async fn generic_chat_error_does_not_switch_to_base64() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chats = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = chats.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    write_http(&mut socket, &upload_ok("file-api-1", 4)).await;
                } else {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    write_http(
                        &mut socket,
                        &http_json(503, r#"{"error":{"message":"come back later"}}"#),
                    )
                    .await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let error = adapter.stream(sample_request(block)).await.err().unwrap();
        let LlmError::Failure(failure) = error;
        assert_eq!(failure.code, "SERVER");
        assert_eq!(chats.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_file_id_reuploads_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chats = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let seen = chats.clone();
        let uploads = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let upload_count = uploads.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    let n = upload_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    write_http(&mut socket, &upload_ok(&format!("file-api-{n}"), 4)).await;
                } else if request.starts_with("POST /chat") {
                    seen.lock().await.push(request);
                    let attempt = seen.lock().await.len();
                    if attempt == 1 {
                        write_http(
                            &mut socket,
                            &http_json(
                                400,
                                r#"{"error":{"message":"file_id file-api-1 expired"}}"#,
                            ),
                        )
                        .await;
                    } else {
                        write_http(&mut socket, &sse_ok()).await;
                    }
                } else {
                    write_http(&mut socket, &http_json(404, "{}")).await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let stream = adapter.stream(sample_request(block)).await.unwrap();
        let _: Vec<_> = stream.collect().await;
        let chats = chats.lock().await;
        assert_eq!(chats.len(), 2);
        assert!(chats[0].contains("file-api-1"));
        assert!(chats[1].contains("file-api-2"));
        assert_eq!(uploads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn second_stale_rejection_is_returned() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                if request.starts_with("POST /files") {
                    write_http(&mut socket, &upload_ok("file-api-1", 4)).await;
                } else {
                    write_http(
                        &mut socket,
                        &http_json(400, r#"{"error":{"message":"file_id file-api-1 expired"}}"#),
                    )
                    .await;
                }
            }
        });
        let (block, images) = sample_image();
        let adapter = DeepSeekAdapter {
            images,
            files: Some(files_store()),
            ..DeepSeekAdapter::new(
                "test-key",
                format!("http://{addr}"),
                "deepseek-v4-flash-vision-exp",
            )
        };
        let error = adapter.stream(sample_request(block)).await.err().unwrap();
        let LlmError::Failure(failure) = error;
        assert_eq!(failure.code, "INVALID_REQUEST");
        assert!(failure.message.contains("expired"));
    }

    #[tokio::test]
    async fn upload_rejects_oversize_and_illegal_expiry_without_io() {
        let client = DeepSeekFilesClient::new("http://127.0.0.1:1", "key");
        let oversized = vec![0u8; MAX_FILE_UPLOAD_BYTES + 1];
        let err = client
            .upload(
                &oversized,
                "image/png",
                "dsh-a.png",
                DEFAULT_FILE_EXPIRY_SECONDS,
            )
            .await
            .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "INVALID_REQUEST");
        assert_eq!(
            failure.message,
            "DeepSeek Files API upload exceeds 128 MiB."
        );
        let err = client
            .upload(&[1, 2, 3], "image/png", "dsh-a.png", 3_599)
            .await
            .unwrap_err();
        let LlmError::Failure(failure) = err;
        assert_eq!(
            failure.message,
            "DeepSeek file expiry must be between 3600 and 2592000 seconds."
        );
    }
}
