//! OpenAI-compatible DeepSeek Files API transport.

use crate::http::{http_exchange, HttpBody, MultipartForm};
use crate::user_agent;
use dsh_llm::{LlmError, LlmFailure};
use serde_json::Value;

/// Minimum provider-supported file lifetime.
pub const MIN_FILE_EXPIRY_SECONDS: u32 = 3_600;
/// Maximum provider-supported file lifetime.
pub const MAX_FILE_EXPIRY_SECONDS: u32 = 2_592_000;
/// Maximum Files API upload size.
pub const MAX_FILE_UPLOAD_BYTES: usize = 128 * 1024 * 1024;
/// Current per-key file-count quota.
pub const MAX_STORED_FILE_COUNT: u32 = 10_000;
/// Current per-key storage quota.
pub const MAX_STORED_FILE_BYTES: u64 = 25 * 1024 * 1024 * 1024;

/// Validated file object returned by the OpenAI-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekFileObject {
    /// Provider file identifier.
    pub id: String,
    /// Declared byte length.
    pub bytes: u64,
    /// Provider `created_at` in Unix seconds.
    pub created_at: u64,
    /// Upload filename.
    pub filename: String,
    /// Always `user_data` after validation.
    pub purpose: String,
    /// Provider `expires_at` in Unix seconds, when present.
    pub expires_at: Option<u64>,
}

/// One page returned by `GET /files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekFilePage {
    /// Files in this page.
    pub data: Vec<DeepSeekFileObject>,
    /// First id on the page, when present.
    pub first_id: Option<String>,
    /// Last id on the page, when present.
    pub last_id: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
}

/// Files API operation failure with HTTP status and classification detail.
#[derive(Debug)]
pub struct DeepSeekFilesError {
    /// User-readable failure mapped onto [`LlmError`].
    pub error: LlmError,
    /// Provider `code` / `type` / `message` joined for quota classification.
    pub detail: String,
}

impl From<DeepSeekFilesError> for LlmError {
    fn from(error: DeepSeekFilesError) -> Self {
        error.error
    }
}

/// Whether an upload failure reports a provider storage or file-count quota.
#[must_use]
pub fn is_files_quota_error(error: &DeepSeekFilesError) -> bool {
    let detail = error.detail.to_ascii_lowercase();
    detail.contains("quota")
        || detail.contains("storage")
        || detail.contains("stored files")
        || detail.contains("file count")
        || detail.contains("too many files")
}

/// Direct client for the OpenAI-compatible `/files` endpoints.
#[derive(Debug, Clone)]
pub struct DeepSeekFilesClient {
    base_url: String,
    api_key: String,
}

impl DeepSeekFilesClient {
    /// Endpoint and API-key snapshot. Trailing slashes are stripped.
    #[must_use]
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: trim_base_url(&base_url.into()),
            api_key: api_key.into(),
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: HttpBody<'_>,
    ) -> Result<Value, DeepSeekFilesError> {
        let url = format!("{}{path}", self.base_url);
        let headers = [
            ("Authorization", format!("Bearer {}", self.api_key)),
            ("User-Agent", user_agent()),
        ];
        let response = match http_exchange(method, &url, &headers, body).await {
            Ok(response) => response,
            Err(message) => {
                return Err(DeepSeekFilesError {
                    error: LlmError::Failure(LlmFailure::new(
                        format!("DeepSeek Files API request to {} failed", self.base_url),
                        "TRANSPORT",
                    )),
                    detail: message,
                });
            }
        };
        if (200..300).contains(&response.status) {
            let value = if response.body.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&response.body).unwrap_or(Value::Null)
            };
            return Ok(value);
        }
        let (message, detail) = provider_error_detail(&response.body);
        Err(DeepSeekFilesError {
            error: LlmError::Failure(LlmFailure {
                message: message.unwrap_or_else(|| {
                    format!("DeepSeek Files API error (HTTP {})", response.status)
                }),
                code: files_http_error_code(response.status),
                status: Some(response.status),
                provider_retry_after_ms: None,
                request_id: response.request_id,
            }),
            detail,
        })
    }

    /// Upload one image with an explicit expiry. The response must include `expires_at`.
    ///
    /// # Errors
    /// Oversize or illegal expiry (`INVALID_REQUEST`), transport or provider
    /// failure (quota detail stays on [`DeepSeekFilesError`]), or an incomplete
    /// upload object (`INVALID_RESPONSE`).
    pub async fn upload(
        &self,
        data: &[u8],
        media_type: &str,
        filename: &str,
        expires_after_seconds: u32,
    ) -> Result<DeepSeekFileObject, DeepSeekFilesError> {
        if data.len() > MAX_FILE_UPLOAD_BYTES {
            return Err(local_files_error(
                "DeepSeek Files API upload exceeds 128 MiB.",
                "INVALID_REQUEST",
            ));
        }
        if !(MIN_FILE_EXPIRY_SECONDS..=MAX_FILE_EXPIRY_SECONDS).contains(&expires_after_seconds) {
            return Err(local_files_error(
                "DeepSeek file expiry must be between 3600 and 2592000 seconds.",
                "INVALID_REQUEST",
            ));
        }
        let value = self
            .request(
                "POST",
                "/files",
                HttpBody::Multipart(MultipartForm {
                    expires_after_seconds,
                    filename,
                    media_type,
                    file: data,
                }),
            )
            .await?;
        let file = parse_file_object(&value, "upload").map_err(files_error_from_llm)?;
        if file.expires_at.is_none() {
            return Err(files_error_from_llm(invalid_response("upload")));
        }
        Ok(file)
    }

    /// List one page of `user_data` files.
    ///
    /// # Errors
    /// Transport failure or an invalid list object.
    pub async fn list(
        &self,
        after: Option<&str>,
        limit: Option<u32>,
        order: Option<&str>,
    ) -> Result<DeepSeekFilePage, DeepSeekFilesError> {
        let mut query = vec!["purpose=user_data".to_string()];
        if let Some(after) = after {
            query.push(format!("after={}", encode_component(after)));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        if let Some(order) = order {
            query.push(format!("order={order}"));
        }
        let path = format!("/files?{}", query.join("&"));
        let value = self.request("GET", &path, HttpBody::Empty).await?;
        parse_file_page(&value).map_err(|error| DeepSeekFilesError {
            error,
            detail: String::new(),
        })
    }

    /// Retrieve one file object.
    ///
    /// # Errors
    /// Transport failure or an invalid file object.
    pub async fn retrieve(&self, file_id: &str) -> Result<DeepSeekFileObject, DeepSeekFilesError> {
        let path = format!("/files/{}", encode_component(file_id));
        let value = self.request("GET", &path, HttpBody::Empty).await?;
        parse_file_object(&value, "retrieve").map_err(|error| DeepSeekFilesError {
            error,
            detail: String::new(),
        })
    }

    /// Delete one provider file. The deleted id must match the request.
    ///
    /// # Errors
    /// Transport failure or an invalid delete object.
    pub async fn delete(&self, file_id: &str) -> Result<(), DeepSeekFilesError> {
        let path = format!("/files/{}", encode_component(file_id));
        let value = self.request("DELETE", &path, HttpBody::Empty).await?;
        parse_delete(&value, file_id).map_err(|error| DeepSeekFilesError {
            error,
            detail: String::new(),
        })
    }
}

fn files_http_error_code(status: u16) -> String {
    if status == 401 || status == 403 {
        "AUTH".into()
    } else if status == 429 {
        "RATE_LIMIT".into()
    } else if status >= 500 {
        "SERVER".into()
    } else {
        "FILES_API".into()
    }
}

fn local_files_error(message: &str, code: &str) -> DeepSeekFilesError {
    DeepSeekFilesError {
        error: LlmError::Failure(LlmFailure::new(message, code)),
        detail: String::new(),
    }
}

fn files_error_from_llm(error: LlmError) -> DeepSeekFilesError {
    DeepSeekFilesError {
        error,
        detail: String::new(),
    }
}

fn invalid_response(operation: &str) -> LlmError {
    LlmError::Failure(LlmFailure::new(
        format!("DeepSeek Files API returned an invalid {operation} response."),
        "INVALID_RESPONSE",
    ))
}

fn parse_file_object(value: &Value, operation: &str) -> Result<DeepSeekFileObject, LlmError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response(operation))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_response(operation))?;
    if object.get("object").and_then(Value::as_str) != Some("file") {
        return Err(invalid_response(operation));
    }
    let bytes = object
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_response(operation))?;
    let created_at = object
        .get("created_at")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_response(operation))?;
    let filename = object
        .get("filename")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_response(operation))?;
    if object.get("purpose").and_then(Value::as_str) != Some("user_data") {
        return Err(invalid_response(operation));
    }
    let expires_at = match object.get("expires_at") {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| invalid_response(operation))?),
    };
    Ok(DeepSeekFileObject {
        id: id.to_string(),
        bytes,
        created_at,
        filename: filename.to_string(),
        purpose: "user_data".into(),
        expires_at,
    })
}

fn parse_file_page(value: &Value) -> Result<DeepSeekFilePage, LlmError> {
    let object = value.as_object().ok_or_else(|| invalid_response("list"))?;
    if object.get("object").and_then(Value::as_str) != Some("list") {
        return Err(invalid_response("list"));
    }
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("list"))?;
    let has_more = object
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_response("list"))?;
    if object
        .get("first_id")
        .is_some_and(|value| !value.is_string())
        || object
            .get("last_id")
            .is_some_and(|value| !value.is_string())
    {
        return Err(invalid_response("list"));
    }
    Ok(DeepSeekFilePage {
        data: data
            .iter()
            .map(|item| parse_file_object(item, "list"))
            .collect::<Result<Vec<_>, _>>()?,
        first_id: object
            .get("first_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        last_id: object
            .get("last_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        has_more,
    })
}

fn parse_delete(value: &Value, file_id: &str) -> Result<(), LlmError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("delete"))?;
    if object.get("id").and_then(Value::as_str) != Some(file_id)
        || object.get("object").and_then(Value::as_str) != Some("file")
        || object.get("deleted").and_then(Value::as_bool) != Some(true)
    {
        return Err(invalid_response("delete"));
    }
    Ok(())
}

fn provider_error_detail(body: &str) -> (Option<String>, String) {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return (None, String::new());
    };
    let Some(error) = value.get("error") else {
        return (None, String::new());
    };
    if !error.is_object() {
        return (None, String::new());
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    let detail = [
        error.get("code").and_then(Value::as_str),
        error.get("type").and_then(Value::as_str),
        error.get("message").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    (message, detail)
}

pub(crate) fn trim_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_quota_from_joined_detail() {
        let error = DeepSeekFilesError {
            error: LlmError::Failure(LlmFailure::new("user storage quota exceeded", "FILES_API")),
            detail: "file_quota user storage quota exceeded".into(),
        };
        assert!(is_files_quota_error(&error));
        let error = DeepSeekFilesError {
            error: LlmError::Failure(LlmFailure::new("no", "FILES_API")),
            detail: String::new(),
        };
        assert!(!is_files_quota_error(&error));
    }

    #[test]
    fn maps_files_http_status() {
        assert_eq!(files_http_error_code(401), "AUTH");
        assert_eq!(files_http_error_code(403), "AUTH");
        assert_eq!(files_http_error_code(429), "RATE_LIMIT");
        assert_eq!(files_http_error_code(500), "SERVER");
        assert_eq!(files_http_error_code(400), "FILES_API");
    }

    #[test]
    fn rejects_incomplete_upload_object() {
        let value = serde_json::json!({
            "id": "file-api-1",
            "object": "file",
            "bytes": 3,
            "created_at": 1,
            "filename": "dsh-a.png",
            "purpose": "user_data"
        });
        let file = parse_file_object(&value, "upload").unwrap();
        assert!(file.expires_at.is_none());
        let err = invalid_response("upload");
        let LlmError::Failure(failure) = err;
        assert_eq!(failure.code, "INVALID_RESPONSE");
        assert_eq!(
            failure.message,
            "DeepSeek Files API returned an invalid upload response."
        );
    }

    #[test]
    fn rejects_non_integer_expiry_on_wire() {
        let value = serde_json::json!({
            "id": "file-api-1",
            "object": "file",
            "bytes": 3,
            "created_at": 1,
            "filename": "dsh-a.png",
            "purpose": "user_data",
            "expires_at": 1.5
        });
        let LlmError::Failure(failure) = parse_file_object(&value, "upload").unwrap_err();
        assert_eq!(failure.code, "INVALID_RESPONSE");
    }
}
