//! High-level run API over [`JsonRpcClient`]: [`DeepSeekHarness`] owns one
//! runtime subprocess across many sessions; [`HarnessSession::run`] sends a
//! prompt and settles when the root session next becomes idle.

use crate::error::{SdkError, SdkProtocolError};
use crate::notification::{HarnessNotification, SessionParents};
use crate::run::{normalize_input, RunCollector, RunInput, RunResult};
use crate::JsonRpcClient;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

/// Default provider route for SDK-created agents.
pub const DEFAULT_PROVIDER: &str = "deepseek-official";
/// Default model for SDK-created agents.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// Launch spec for the runtime subprocess.
#[derive(Debug, Clone)]
pub struct HarnessClientOptions {
    /// Runtime executable.
    pub command: String,
    /// Arguments passed to [`Self::command`].
    pub args: Vec<String>,
    /// Working directory for the runtime process itself.
    pub cwd: Option<PathBuf>,
    /// Complete child environment. `None` inherits the parent; `Some` replaces it.
    pub env: Option<HashMap<String, String>>,
}

/// Options for the high-level [`DeepSeekHarness`] wrapper.
#[derive(Debug, Clone)]
pub struct DeepSeekHarnessOptions {
    /// Launch spec for the runtime subprocess.
    pub launch: HarnessClientOptions,
    /// Workspace cwd recorded on every SDK-created session.
    pub cwd: Option<PathBuf>,
    /// Provider route (default [`DEFAULT_PROVIDER`]).
    pub provider: Option<String>,
    /// Model (default [`DEFAULT_MODEL`]).
    pub model: Option<String>,
    /// Optional positive output-token cap.
    pub max_tokens: Option<i64>,
}

/// Per-run options: target session and streaming observer.
pub struct RunOptions<'a> {
    /// Session id to run on; omitted mints a fresh session per call.
    pub session_id: Option<String>,
    /// Observer invoked with every collected notification, in wire order.
    pub on_notification: Option<&'a mut dyn FnMut(&HarnessNotification)>,
}

impl Default for RunOptions<'_> {
    fn default() -> Self {
        Self {
            session_id: None,
            on_notification: None,
        }
    }
}

struct Inner {
    client: Option<JsonRpcClient>,
    initialized: bool,
    parents: SessionParents,
}

/// Reusable SDK for running DeepSeek Harness agent turns in a runtime subprocess.
///
/// The subprocess starts lazily on first use and stays owned until
/// [`DeepSeekHarness::close`]. A failed handshake reaps the runtime and
/// swaps in a fresh client so a later call retries, unless `close` already
/// ended this harness.
pub struct DeepSeekHarness {
    launch: HarnessClientOptions,
    cwd: PathBuf,
    provider: String,
    model: String,
    max_tokens: Option<i64>,
    closed: AtomicBool,
    inner: Mutex<Inner>,
}

impl DeepSeekHarness {
    /// Build a harness. Workspace cwd is resolved absolute before handshake
    /// so a relative launch cwd cannot double-resolve inside the child.
    pub fn new(options: DeepSeekHarnessOptions) -> Self {
        let cwd = resolve_workspace_cwd(options.cwd.as_deref(), options.launch.cwd.as_deref());
        Self {
            launch: options.launch,
            cwd,
            provider: options
                .provider
                .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
            model: options.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_tokens: options.max_tokens,
            closed: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                client: None,
                initialized: false,
                parents: SessionParents::new(),
            }),
        }
    }

    /// Absolute workspace cwd sent on `initialize`.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Start the subprocess and perform the `initialize` handshake once.
    pub async fn start(&self) -> Result<(), SdkError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SdkError::closed());
        }
        let mut inner = self.inner.lock().await;
        if inner.initialized {
            return Ok(());
        }
        match self.handshake(&mut inner).await {
            Ok(()) => {
                inner.initialized = true;
                Ok(())
            }
            Err(error) => {
                if let Some(mut client) = inner.client.take() {
                    let _ = client.shutdown().await;
                }
                inner.parents = SessionParents::new();
                inner.initialized = false;
                Err(error)
            }
        }
    }

    /// Open a session handle (no wire traffic; the runtime creates the session
    /// on its first prompt).
    pub fn session(&self, session_id: Option<&str>) -> HarnessSession<'_> {
        HarnessSession {
            harness: self,
            id: session_id
                .map(str::to_string)
                .unwrap_or_else(mint_session_id),
        }
    }

    /// Run one prompt on a fresh (or named) session.
    pub async fn run(
        &self,
        input: impl Into<RunInput>,
        mut options: RunOptions<'_>,
    ) -> Result<RunResult, SdkError> {
        let session_id = options.session_id.take().unwrap_or_else(mint_session_id);
        self.run_session(&session_id, input.into(), options.on_notification)
            .await
    }

    /// Shut down and reap the runtime subprocess. Terminal: a closed harness
    /// no longer retries a failed handshake.
    pub async fn close(&self) -> Result<(), SdkError> {
        self.closed.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock().await;
        inner.initialized = false;
        inner.parents = SessionParents::new();
        if let Some(mut client) = inner.client.take() {
            client.shutdown().await?;
        }
        Ok(())
    }

    async fn run_session(
        &self,
        session_id: &str,
        input: RunInput,
        mut on_notification: Option<&mut dyn FnMut(&HarnessNotification)>,
    ) -> Result<RunResult, SdkError> {
        self.start().await?;
        if self.closed.load(Ordering::SeqCst) {
            return Err(SdkError::closed());
        }
        let mut inner = self.inner.lock().await;
        let blocks = normalize_input(&input);
        let prompt_result = {
            let client = inner.client.as_mut().ok_or_else(SdkError::closed)?;
            client.prompt_blocks(session_id, &blocks).await?
        };
        let message_id = message_id_from_prompt(&prompt_result)?;
        let mut collector = RunCollector::new(session_id, message_id);
        let buffered = {
            let client = inner.client.as_mut().ok_or_else(SdkError::closed)?;
            client.take_notifications()
        };
        for frame in buffered {
            if apply_frame(
                &mut collector,
                &mut inner.parents,
                frame,
                &mut on_notification,
            )? {
                return Ok(collector.finish());
            }
        }
        loop {
            let frame = {
                let client = inner.client.as_mut().ok_or_else(SdkError::closed)?;
                client.next_notification().await?
            };
            if apply_frame(
                &mut collector,
                &mut inner.parents,
                frame,
                &mut on_notification,
            )? {
                return Ok(collector.finish());
            }
        }
    }

    async fn handshake(&self, inner: &mut Inner) -> Result<(), SdkError> {
        let mut client = JsonRpcClient::spawn_with(
            &self.launch.command,
            &self.launch.args,
            self.launch.cwd.as_deref(),
            self.launch.env.as_ref(),
        )
        .await?;
        let result = client
            .initialize(
                &self.cwd.to_string_lossy(),
                &self.provider,
                &self.model,
                self.max_tokens,
            )
            .await?;
        validate_initialize(&result)?;
        inner.parents = SessionParents::new();
        inner.client = Some(client);
        Ok(())
    }
}

/// One SDK session: a stable id plus owned activity intervals.
pub struct HarnessSession<'a> {
    harness: &'a DeepSeekHarness,
    /// Wire session id this handle runs on.
    pub id: String,
}

impl HarnessSession<'_> {
    /// Queue one prompt, then observe the session through its next idle.
    pub async fn run(
        &self,
        input: impl Into<RunInput>,
        options: RunOptions<'_>,
    ) -> Result<RunResult, SdkError> {
        self.harness
            .run_session(&self.id, input.into(), options.on_notification)
            .await
    }
}

/// Mint `session-{uuid}` with the TypeScript hyphen-stripped UUID.
pub fn mint_session_id() -> String {
    format!("session-{}", uuid::Uuid::new_v4().simple())
}

/// Resolve workspace cwd lexically (no canonicalize) against this process cwd.
pub fn resolve_workspace_cwd(cwd: Option<&Path>, launch_cwd: Option<&Path>) -> PathBuf {
    let raw = cwd
        .map(Path::to_path_buf)
        .or_else(|| launch_cwd.map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().expect("current_dir"));
    if raw.is_absolute() {
        raw
    } else {
        std::env::current_dir().expect("current_dir").join(raw)
    }
}

fn apply_frame(
    collector: &mut RunCollector,
    tree: &mut SessionParents,
    frame: Value,
    on_notification: &mut Option<&mut dyn FnMut(&HarnessNotification)>,
) -> Result<bool, SdkProtocolError> {
    let Some(notification) = HarnessNotification::from_frame(&frame) else {
        return Ok(false);
    };
    let before = collector.collected_len();
    let done = collector.push(tree, notification)?;
    if collector.collected_len() > before {
        if let Some(callback) = on_notification.as_mut() {
            callback(collector.last_collected().expect("just collected"));
        }
    }
    Ok(done)
}

fn validate_initialize(result: &Value) -> Result<(), SdkProtocolError> {
    let server_info = result.get("serverInfo");
    let valid = server_info.is_some_and(|info| {
        info.is_object()
            && info.get("name").and_then(Value::as_str).is_some()
            && info.get("version").and_then(Value::as_str).is_some()
    });
    if !valid {
        return Err(SdkProtocolError::new(format!(
            "initialize returned no server identity: {result}"
        )));
    }
    Ok(())
}

fn message_id_from_prompt(result: &Value) -> Result<String, SdkProtocolError> {
    match result.get("messageId").and_then(Value::as_str) {
        Some(message_id) => Ok(message_id.to_string()),
        None => Err(SdkProtocolError::new(format!(
            "session/prompt returned no message id: {result}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mints_hyphen_stripped_session_ids() {
        let id = mint_session_id();
        assert!(id.starts_with("session-"));
        assert!(!id[8..].contains('-'));
        assert_eq!(id.len(), "session-".len() + 32);
    }

    #[test]
    fn resolve_cwd_joins_relative_paths_without_canonicalizing() {
        let relative = Path::new("worker");
        let resolved = resolve_workspace_cwd(Some(relative), None);
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("worker"));
        assert_eq!(
            resolve_workspace_cwd(Some(Path::new("/abs")), Some(Path::new("/launch"))),
            PathBuf::from("/abs")
        );
    }

    #[test]
    fn initialize_and_prompt_results_validate() {
        assert!(validate_initialize(&json!({
            "serverInfo": { "name": "deepseek-harness-sdk-runtime", "version": "0.0.1" }
        }))
        .is_ok());
        assert!(validate_initialize(&json!({ "serverInfo": { "name": 1 } })).is_err());
        assert_eq!(
            message_id_from_prompt(&json!({ "messageId": "m1" })).unwrap(),
            "m1"
        );
        assert!(message_id_from_prompt(&json!({})).is_err());
    }
}
