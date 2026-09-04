//! Execute command hooks and decode their outcomes.

use crate::codec::parse_hook_output;
use crate::types::{CommandHook, HookOutput};
use std::collections::BTreeMap;
use std::future::Future;

/// Reference default per-hook timeout, in ms (10 minutes).
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 600_000;

/// Request handed to the bridge's shell runner.
#[derive(Debug, Clone)]
pub struct HookShellRequest {
    /// Command line.
    pub command: String,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
    /// Serialized stdin payload.
    pub stdin: String,
    /// Working directory, when the bridge supplied one.
    pub cwd: Option<String>,
    /// Extra environment entries.
    pub env: Option<BTreeMap<String, String>>,
}

/// Captured process outcome, or an infrastructure failure string.
#[derive(Debug, Clone)]
pub struct HookShellResult {
    /// Exit code; `None` when the process died from a signal.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// Everything a single hook invocation needs beyond its command line.
#[derive(Debug, Clone)]
pub struct RunHookOptions {
    /// JSON payload written to stdin.
    pub payload: serde_json::Value,
    /// Extra env vars.
    pub env: Option<BTreeMap<String, String>>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Whether the shared abort has fired.
    pub aborted: bool,
    /// Whether to append a trailing newline (Claude Code yes, Codex no).
    pub trailing_newline: bool,
    /// Timeout when the hook sets none.
    pub default_timeout_ms: u64,
    /// Firing event used to guard hook-specific fields.
    pub expected_event_name: Option<String>,
}

/// Decoded output plus wall-clock duration.
#[derive(Debug, Clone)]
pub struct RunHookResult {
    /// Decoded outcome.
    pub output: HookOutput,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
}

/// Run `hook` with serialized stdin and decode its outcome.
///
/// Infrastructure rejection becomes an outcome with no exit code, so this
/// function never panics the calling turn.
pub async fn run_hook<F, Fut, N>(
    runner: F,
    hook: &CommandHook,
    options: RunHookOptions,
    now: N,
) -> RunHookResult
where
    F: FnOnce(HookShellRequest) -> Fut,
    Fut: Future<Output = Result<HookShellResult, String>>,
    N: Fn() -> u128,
{
    let started = now();
    if options.aborted {
        return RunHookResult {
            output: parse_hook_output(None, "", "hook bridge disposed", None),
            duration_ms: (now().saturating_sub(started)) as u64,
        };
    }
    let timeout_ms = hook
        .timeout_sec
        .map(|seconds| (seconds * 1000.0) as u64)
        .unwrap_or(options.default_timeout_ms);
    let mut stdin = serde_json::to_string(&options.payload).unwrap_or_else(|_| "{}".into());
    if options.trailing_newline {
        stdin.push('\n');
    }
    let request = HookShellRequest {
        command: hook.command.clone(),
        timeout_ms,
        stdin,
        cwd: options.cwd,
        env: options.env,
    };
    let expected = options.expected_event_name.as_deref();
    match runner(request).await {
        Ok(result) => RunHookResult {
            output: parse_hook_output(result.exit_code, &result.stdout, &result.stderr, expected),
            duration_ms: (now().saturating_sub(started)) as u64,
        },
        Err(message) => RunHookResult {
            output: parse_hook_output(None, "", &message, None),
            duration_ms: (now().saturating_sub(started)) as u64,
        },
    }
}
