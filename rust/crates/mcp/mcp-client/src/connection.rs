//! Connection supervisor: generations, reconnect backoff, and tool sync.

use crate::protocol::McpSession;
use crate::tools::{sync_tools, LiveTools};
use crate::{Config, RegistrationFailure, ResolvedReconnectPolicy, ToolBridgeOptions};
use dsh_cordis::Context;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const GENERATION_CLOSE_TIMEOUT_MS: u64 = 5_000;

/// Result from the initial connection attempt.
#[derive(Debug, Default)]
pub struct ConnectionOutcome {
    /// If the initial connection or tool sync failed, the error text.
    pub error: Option<String>,
}

/// Handle for one plugin instance's supervised connection.
pub struct ConnectionHandle {
    ready: tokio::sync::Mutex<Option<ConnectionOutcome>>,
    ready_signal: Notify,
    dispose: Notify,
    disposed: Arc<AtomicBool>,
}

impl ConnectionHandle {
    /// Settles when the first connection attempt completes.
    pub async fn ready(&self) -> ConnectionOutcome {
        loop {
            if let Some(outcome) = self.ready.lock().await.as_ref() {
                return ConnectionOutcome {
                    error: outcome.error.clone(),
                };
            }
            self.ready_signal.notified().await;
        }
    }

    /// Stop reconnection and unregister tools.
    pub async fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        self.dispose.notify_waiters();
    }
}

/// Start the supervised connection for one MCP server.
pub fn start_connection(
    ctx: Context,
    config: Config,
    policy: ResolvedReconnectPolicy,
) -> Arc<ConnectionHandle> {
    let label = format!("mcp-client({})", config.server_name);
    let handle = Arc::new(ConnectionHandle {
        ready: tokio::sync::Mutex::new(None),
        ready_signal: Notify::new(),
        dispose: Notify::new(),
        disposed: Arc::new(AtomicBool::new(false)),
    });
    let worker = Arc::clone(&handle);
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(run_supervisor(ctx, config, policy, worker, label));
    } else {
        tokio::spawn(run_supervisor(ctx, config, policy, worker, label));
    }
    handle
}

async fn run_supervisor(
    ctx: Context,
    config: Config,
    policy: ResolvedReconnectPolicy,
    handle: Arc<ConnectionHandle>,
    label: String,
) {
    let live = LiveTools::default();
    let alive = Arc::new(AtomicBool::new(true));
    let mut failed_attempts = 0u32;
    let mut connected_at: Option<Instant> = None;
    let mut first_error: Option<String> = None;
    let mut startup = true;
    let current: Arc<Mutex<Option<Arc<McpSession>>>> = Arc::new(Mutex::new(None));

    loop {
        if handle.disposed.load(Ordering::SeqCst) {
            break;
        }
        let opts = ToolBridgeOptions {
            registration_failure: if startup && config.fail_on_startup_error {
                RegistrationFailure::Throw
            } else {
                RegistrationFailure::Contain
            },
            server_name: config.server_name.clone(),
            tool_call_timeout_ms: config.tool_call_timeout_ms,
        };
        match connect_generation(&ctx, &config, &opts, &live, &alive).await {
            Ok(session) => {
                *current.lock().expect("current") = Some(Arc::clone(&session));
                connected_at = Some(Instant::now());
                if failed_attempts > 0 {
                    tracing::info!(
                        "{label}: reconnected and re-synced tools (attempt {failed_attempts}/{})",
                        policy.max_attempts
                    );
                }
                if startup {
                    *handle.ready.lock().await = Some(ConnectionOutcome { error: None });
                    handle.ready_signal.notify_waiters();
                    startup = false;
                }
                let changed = session.list_changed();
                tokio::select! {
                    _ = handle.dispose.notified() => break,
                    _ = changed.notified() => {
                        tracing::info!("{label}: tool list changed, re-syncing");
                        if let Err(error) = sync_tools(Arc::clone(&session), &ctx, &opts, &live, &alive).await {
                            tracing::error!("{label}: tool re-sync failed: {error}");
                        }
                    }
                }
                *current.lock().expect("current") = None;
                let _ = tokio::time::timeout(
                    Duration::from_millis(GENERATION_CLOSE_TIMEOUT_MS),
                    session.close(),
                )
                .await;
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error.clone());
                }
                if !handle.disposed.load(Ordering::SeqCst) {
                    tracing::warn!("{label}: connection attempt failed: {error}");
                }
                if startup {
                    *handle.ready.lock().await = Some(ConnectionOutcome {
                        error: Some(error.clone()),
                    });
                    handle.ready_signal.notify_waiters();
                    startup = false;
                }
                let lost = connected_at.is_some();
                if !policy.enabled {
                    let message = if lost {
                        "connection lost and reconnect is disabled — registered tools will fail until an HMR reload or Host restart"
                    } else {
                        "connection failed and reconnect is disabled — no tools were registered; reload the plugin or restart the Host to connect"
                    };
                    tracing::error!("{label}: {message}");
                    break;
                }
                if connected_at
                    .map(|started| started.elapsed().as_millis() as u64 >= policy.max_delay_ms)
                    .unwrap_or(false)
                {
                    failed_attempts = 0;
                }
                connected_at = None;
                failed_attempts += 1;
                if failed_attempts > policy.max_attempts {
                    live.replace_empty();
                    tracing::error!(
                        "{label}: giving up after {} consecutive failed reconnect attempts — tools unregistered; reload the plugin or restart the Host to reconnect",
                        policy.max_attempts
                    );
                    break;
                }
                let delay = policy.max_delay_ms.min(
                    policy
                        .initial_delay_ms
                        .saturating_mul(2u64.saturating_pow(failed_attempts - 1)),
                );
                let action = if lost {
                    "connection lost; reconnecting"
                } else {
                    "connection failed; retrying"
                };
                tracing::warn!(
                    "{label}: {action} in {delay}ms (attempt {failed_attempts}/{})",
                    policy.max_attempts
                );
                tokio::select! {
                    _ = handle.dispose.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                }
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    live.replace_empty();
    let session = current.lock().expect("current").take();
    if let Some(session) = session {
        let _ = tokio::time::timeout(
            Duration::from_millis(GENERATION_CLOSE_TIMEOUT_MS),
            session.close(),
        )
        .await;
    }
}

async fn connect_generation(
    ctx: &Context,
    config: &Config,
    opts: &ToolBridgeOptions,
    live: &LiveTools,
    alive: &Arc<AtomicBool>,
) -> Result<Arc<McpSession>, String> {
    let session = McpSession::connect(config).await?;
    sync_tools(Arc::clone(&session), ctx, opts, live, alive).await?;
    Ok(session)
}
