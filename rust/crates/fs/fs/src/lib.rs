//! Filesystem seam (`ctx.fs`).

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use dsh_sandbox::SandboxMode;
use serde_json::{json, Value};
use thiserror::Error;

/// `fs/write-intent` waterfall name.
pub const FS_WRITE_INTENT: &str = "fs/write-intent";
/// `fs/edit-intent` waterfall name.
pub const FS_EDIT_INTENT: &str = "fs/edit-intent";
/// `fs/observed` emit name.
pub const FS_OBSERVED: &str = "fs/observed";

/// Filesystem failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FsError {
    /// Host I/O failure.
    #[error("{0}")]
    Io(String),
    /// Sandbox policy denied this path.
    #[error("denied: {0}")]
    Denied(String),
    /// Typed seam failure carrying a stable code.
    #[error("{message}")]
    Coded {
        /// Machine-routable code (`FS_NOT_OBSERVED`, …).
        code: FsErrorCode,
        /// Human-facing condition (remedies are appended at the tool boundary).
        message: String,
    },
}

/// Stable codes matching TypeScript `FsErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorCode {
    /// Target is absent.
    NotFound,
    /// Mutation requires a prior observation this session does not have.
    NotObserved,
    /// Observed version no longer matches the file.
    StaleVersion,
    /// Target exists but is not a regular file.
    NotRegularFile,
    /// The file-effect fence refused this mutation.
    SandboxDenied,
}

impl FsErrorCode {
    /// Wire code string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "FS_NOT_FOUND",
            Self::NotObserved => "FS_NOT_OBSERVED",
            Self::StaleVersion => "FS_STALE_VERSION",
            Self::NotRegularFile => "FS_NOT_REGULAR_FILE",
            Self::SandboxDenied => "FS_SANDBOX_DENIED",
        }
    }
}

impl FsError {
    /// `FS_NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::Coded {
            code: FsErrorCode::NotFound,
            message: message.into(),
        }
    }

    /// `FS_NOT_OBSERVED`.
    pub fn not_observed(message: impl Into<String>) -> Self {
        Self::Coded {
            code: FsErrorCode::NotObserved,
            message: message.into(),
        }
    }

    /// `FS_STALE_VERSION`.
    pub fn stale(message: impl Into<String>) -> Self {
        Self::Coded {
            code: FsErrorCode::StaleVersion,
            message: message.into(),
        }
    }

    /// `FS_NOT_REGULAR_FILE`.
    pub fn not_regular(message: impl Into<String>) -> Self {
        Self::Coded {
            code: FsErrorCode::NotRegularFile,
            message: message.into(),
        }
    }

    /// `FS_SANDBOX_DENIED`.
    pub fn sandbox_denied(message: impl Into<String>) -> Self {
        Self::Coded {
            code: FsErrorCode::SandboxDenied,
            message: message.into(),
        }
    }

    /// Wire code, when this is a typed seam failure.
    pub fn code(&self) -> Option<FsErrorCode> {
        match self {
            Self::Coded { code, .. } => Some(*code),
            Self::Io(_) | Self::Denied(_) => None,
        }
    }

    /// Append the model-facing recovery sentence for guarded-mutation failures.
    pub fn remediate(self) -> Self {
        match self {
            Self::Coded {
                code: FsErrorCode::NotObserved,
                message,
            } if !message.contains(" — ") => Self::Coded {
                code: FsErrorCode::NotObserved,
                message: format!("{message} — read the file, then retry"),
            },
            Self::Coded {
                code: FsErrorCode::StaleVersion,
                message,
            } if !message.contains(" — ") => Self::Coded {
                code: FsErrorCode::StaleVersion,
                message: format!("{message} — re-read the file, then retry"),
            },
            other => other,
        }
    }
}

/// File or directory kind returned by [`FsProvider::stat`] and [`FsProvider::list_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Neither a regular file nor a directory (symlink to a special, socket, …).
    Other,
}

/// Stat result for an existing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInfo {
    /// File, directory, or other.
    pub kind: FsKind,
}

/// One directory entry from [`FsProvider::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Basename, not a path.
    pub name: String,
    /// Entry kind.
    pub kind: FsKind,
}

/// Resolved backend identity. `target_key` is opaque; `display_path` is model-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsTarget {
    /// Opaque key used for stale guards (local backend: realpath-like string).
    pub target_key: String,
    /// Path rendered to the model and UI.
    pub display_path: String,
}

impl FsTarget {
    /// Build a target from a backend key and display path.
    pub fn new(target_key: impl Into<String>, display_path: impl Into<String>) -> Self {
        Self {
            target_key: target_key.into(),
            display_path: display_path.into(),
        }
    }

    /// JSON object for `fs/*` events.
    pub fn to_value(&self) -> Value {
        json!({
            "targetKey": self.target_key,
            "displayPath": self.display_path,
        })
    }

    /// Parse a target from an event payload field.
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            target_key: value.get("targetKey")?.as_str()?.to_string(),
            display_path: value.get("displayPath")?.as_str()?.to_string(),
        })
    }
}

/// Authoritative presence or absence observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsObservation {
    /// File exists at `version`.
    Present {
        /// Opaque freshness token.
        version: String,
    },
    /// Confirmed absence.
    Absent,
}

impl FsObservation {
    /// JSON object for `fs/observed`.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Present { version } => json!({ "kind": "present", "version": version }),
            Self::Absent => json!({ "kind": "absent" }),
        }
    }

    /// Parse an observation from an event payload field.
    pub fn from_value(value: &Value) -> Option<Self> {
        match value.get("kind")?.as_str()? {
            "present" => Some(Self::Present {
                version: value.get("version")?.as_str()?.to_string(),
            }),
            "absent" => Some(Self::Absent),
            _ => None,
        }
    }
}

/// Guarded write intent. Omission of the whole intent means unconditional write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsWriteIntent {
    /// Create only when the target is still absent.
    CreateIfAbsent,
    /// Replace only when the current version matches.
    ReplaceIfVersion {
        /// Observed version to compare.
        version: String,
    },
}

impl FsWriteIntent {
    /// JSON object returned by `fs/write-intent`.
    pub fn to_value(&self) -> Value {
        match self {
            Self::CreateIfAbsent => json!({ "kind": "createIfAbsent" }),
            Self::ReplaceIfVersion { version } => {
                json!({ "kind": "replaceIfVersion", "version": version })
            }
        }
    }

    /// Parse a write-intent decision. `null` / missing kind means no policy.
    pub fn from_value(value: &Value) -> Option<Self> {
        match value.get("kind")?.as_str()? {
            "createIfAbsent" => Some(Self::CreateIfAbsent),
            "replaceIfVersion" => Some(Self::ReplaceIfVersion {
                version: value.get("version")?.as_str()?.to_string(),
            }),
            _ => None,
        }
    }
}

/// Outcome of a full-file write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWriteOutcome {
    /// `create` or `update`.
    pub operation: &'static str,
    /// Version after the write.
    pub version: String,
}

/// Opaque tool-execution actor used to derive the observed-state owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsObservationActor {
    /// Session id of the calling agent, when the loop supplied one.
    pub session_id: Option<String>,
}

impl FsObservationActor {
    /// Actor for a tool call's `agent_id` (the session id).
    pub fn from_agent_id(agent_id: Option<&str>) -> Self {
        Self {
            session_id: agent_id.filter(|id| !id.is_empty()).map(str::to_string),
        }
    }

    /// JSON `actor` object. Missing session yields `{}` so the policy sees no owner.
    pub fn to_value(&self) -> Value {
        match &self.session_id {
            Some(id) => json!({ "agent": { "session": { "id": id } } }),
            None => json!({}),
        }
    }

    /// Parse an actor; only `agent.session.id` is the owner key.
    pub fn from_value(value: Option<&Value>) -> Self {
        let session_id = value
            .and_then(|value| value.pointer("/agent/session/id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        Self { session_id }
    }

    /// Observed-state owner, when one can be derived.
    pub fn owner(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Build the shared `fs/*` event payload.
pub fn fs_event_payload(
    target: &FsTarget,
    actor: &FsObservationActor,
    observation: Option<&FsObservation>,
) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert("target".into(), target.to_value());
    payload.insert("actor".into(), actor.to_value());
    if let Some(observation) = observation {
        payload.insert("observation".into(), observation.to_value());
    }
    Value::Object(payload)
}

/// Parse a waterfall/emit error object `{ error: { code, message } }`.
pub fn error_from_event(value: &Value) -> Option<FsError> {
    let error = value.get("error")?;
    let code = error.get("code")?.as_str()?;
    let message = error.get("message")?.as_str()?.to_string();
    let code = match code {
        "FS_NOT_FOUND" => FsErrorCode::NotFound,
        "FS_NOT_OBSERVED" => FsErrorCode::NotObserved,
        "FS_STALE_VERSION" => FsErrorCode::StaleVersion,
        "FS_NOT_REGULAR_FILE" => FsErrorCode::NotRegularFile,
        "FS_SANDBOX_DENIED" => FsErrorCode::SandboxDenied,
        _ => return None,
    };
    Some(FsError::Coded { code, message })
}

/// Encode a typed failure for a waterfall return.
pub fn error_to_event(error: &FsError) -> Value {
    match error {
        FsError::Coded { code, message } => json!({
            "error": { "code": code.as_str(), "message": message }
        }),
        FsError::Io(message) | FsError::Denied(message) => json!({
            "error": { "code": "FS_IO_ERROR", "message": message }
        }),
    }
}

/// Per-call file-effect policy forwarded from `ctx.sandboxPolicy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWritePolicy {
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    pub mode: String,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
}

/// Provider interface.
#[async_trait]
pub trait FsProvider: Send + Sync {
    /// Read a UTF-8 file.
    async fn read_text(&self, path: &str) -> Result<String, FsError>;
    /// Write a UTF-8 file, creating parents. Unconditional create-or-overwrite.
    async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError>;
    /// Whether `path` exists. Denied paths are [`FsError::Denied`], not `false`.
    async fn exists(&self, path: &str) -> Result<bool, FsError>;
    /// Stat `path`. `Ok(None)` means the path is absent; denied paths are [`FsError::Denied`].
    async fn stat(&self, path: &str) -> Result<Option<FsInfo>, FsError>;
    /// List immediate children of a directory.
    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError>;
    /// Resolve a model-supplied path to a stable target.
    async fn resolve(&self, path: &str) -> Result<FsTarget, FsError>;
    /// Current freshness token, or `None` when the target is absent.
    async fn version_of(&self, target: &FsTarget) -> Result<Option<String>, FsError>;
    /// Atomic write honoring an optional intent.
    async fn write_intended(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
    ) -> Result<FsWriteOutcome, FsError>;

    /// Atomic write under an explicit per-call file-effect policy.
    ///
    /// The default ignores `policy` and delegates to [`Self::write_intended`].
    async fn write_intended_with_policy(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
        policy: Option<&FsWritePolicy>,
    ) -> Result<FsWriteOutcome, FsError> {
        let _ = policy;
        self.write_intended(target, content, intent).await
    }
}

/// `ctx.fs`.
pub struct FsRuntime {
    backend: std::sync::Arc<dyn FsProvider>,
    sandbox_mode: Option<SandboxMode>,
}

impl FsRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn FsProvider>) -> Self {
        Self {
            backend,
            sandbox_mode: None,
        }
    }

    /// Record the backend's standing sandbox mode (the capability fact).
    pub fn with_sandbox_mode(mut self, mode: SandboxMode) -> Self {
        self.sandbox_mode = Some(mode);
        self
    }

    /// Standing sandbox mode when the backend confines.
    pub fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.sandbox_mode
    }

    /// `ctx.fs` as of this call, or `fallback` when the service is absent.
    ///
    /// TypeScript tools read `ctx.fs` during execute. Headless dump order
    /// mounts `fs-sandbox` after `tool-fs`, so execute must look up the live
    /// service to apply the sandbox wrapper.
    pub fn from_context(ctx: &Context, fallback: &std::sync::Arc<Self>) -> std::sync::Arc<Self> {
        ctx.get::<Self>()
            .unwrap_or_else(|| std::sync::Arc::clone(fallback))
    }

    /// Read text.
    pub async fn read_text(&self, path: &str) -> Result<String, FsError> {
        self.backend.read_text(path).await
    }

    /// Write text unconditionally.
    pub async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.backend.write_text(path, content).await
    }

    /// Whether `path` exists.
    pub async fn exists(&self, path: &str) -> Result<bool, FsError> {
        self.backend.exists(path).await
    }

    /// Stat `path`.
    pub async fn stat(&self, path: &str) -> Result<Option<FsInfo>, FsError> {
        self.backend.stat(path).await
    }

    /// List immediate children of a directory.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        self.backend.list_dir(path).await
    }

    /// Resolve a path.
    pub async fn resolve(&self, path: &str) -> Result<FsTarget, FsError> {
        self.backend.resolve(path).await
    }

    /// Current version, or `None` when absent.
    pub async fn version_of(&self, target: &FsTarget) -> Result<Option<String>, FsError> {
        self.backend.version_of(target).await
    }

    /// Guarded write.
    pub async fn write_intended(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
    ) -> Result<FsWriteOutcome, FsError> {
        self.backend.write_intended(target, content, intent).await
    }

    /// Guarded write under an explicit per-call file-effect policy.
    pub async fn write_intended_with_policy(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
        policy: Option<&FsWritePolicy>,
    ) -> Result<FsWriteOutcome, FsError> {
        self.backend
            .write_intended_with_policy(target, content, intent, policy)
            .await
    }
}

impl Service for FsRuntime {
    const KEY: &'static str = "fs";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remediates_guarded_codes_once() {
        let error = FsError::not_observed(r#"cannot overwrite existing "/x" without reading it first"#);
        let remediated = error.clone().remediate();
        assert!(remediated.to_string().contains(" — read the file, then retry"));
        assert_eq!(remediated.clone().remediate().to_string(), remediated.to_string());
        let stale = FsError::stale(r#"cannot write "/x": file changed since it was read"#).remediate();
        assert!(stale.to_string().contains(" — re-read the file, then retry"));
    }

    #[test]
    fn actor_owner_is_session_id() {
        assert_eq!(
            FsObservationActor::from_agent_id(Some("sess-1")).owner(),
            Some("sess-1")
        );
        assert_eq!(FsObservationActor::from_agent_id(None).owner(), None);
        assert_eq!(
            FsObservationActor::from_value(Some(&json!({ "agent": {} }))).owner(),
            None
        );
    }
}
