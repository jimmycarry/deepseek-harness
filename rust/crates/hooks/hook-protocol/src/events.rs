//! Append helpers for durable, log-only hook events.

use crate::types::{HookDialect, HookOutput};
use dsh_session::{Session, SessionEventData};
use serde_json::json;

/// Default character cap for `hook/result.stderrSummary`.
pub const DEFAULT_STDERR_SUMMARY_MAX_CHARS: usize = 500;

/// Identity of one hook invocation across its invoked/result pair.
#[derive(Debug, Clone)]
pub struct HookInvocation {
    /// Open turn the invocation lives inside.
    pub turn: u32,
    /// Hook point (`PreToolUse`, `Stop`, …).
    pub point: String,
    /// Bridge dialect that ran it.
    pub dialect: HookDialect,
    /// Stable id correlating invoked with result.
    pub handler_id: String,
    /// Matcher-group pattern; omitted for match-all.
    pub matcher: Option<String>,
}

/// Decided outcome half of the pair.
#[derive(Debug, Clone)]
pub struct HookResultRecord {
    /// Open turn.
    pub turn: u32,
    /// Hook point.
    pub point: String,
    /// Correlating handler id.
    pub handler_id: String,
    /// Decoded outcome.
    pub output: HookOutput,
    /// Character cap for `stderrSummary`.
    pub stderr_summary_max_chars: usize,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
}

/// Trim and cap stderr for the durable `stderrSummary` field.
pub fn summarize_stderr(stderr: &str, max_chars: usize) -> Option<String> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > max_chars {
        let cut: String = trimmed.chars().take(max_chars).collect();
        Some(format!("{cut}…"))
    } else {
        Some(trimmed.to_string())
    }
}

/// Append a log-only `hook/invoked` event. An absent matcher is omitted.
pub fn append_hook_invoked(session: &Session, invocation: HookInvocation) {
    let mut data = json!({
        "turn": invocation.turn,
        "point": invocation.point,
        "dialect": invocation.dialect.as_str(),
        "handlerId": invocation.handler_id,
    });
    if let Some(matcher) = invocation.matcher {
        data["matcher"] = json!(matcher);
    }
    let _ = session.append(
        SessionEventData::Extension {
            type_name: "hook/invoked".into(),
            data,
        },
        None,
    );
}

/// Append the durable result paired with `hook/invoked`.
pub fn append_hook_result(session: &Session, record: HookResultRecord) {
    let output = &record.output;
    let decision = output
        .decision
        .map(crate::HookDecision::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if output.continue_run == Some(false) {
                "stop".into()
            } else {
                "pass".into()
            }
        });
    let mut data = json!({
        "turn": record.turn,
        "point": record.point,
        "handlerId": record.handler_id,
        "decision": decision,
        "durationMs": record.duration_ms,
    });
    if let Some(exit) = output.exit_code {
        data["exitCode"] = json!(exit);
    }
    if let Some(summary) = summarize_stderr(&output.stderr, record.stderr_summary_max_chars) {
        data["stderrSummary"] = json!(summary);
    }
    let _ = session.append(
        SessionEventData::Extension {
            type_name: "hook/result".into(),
            data,
        },
        None,
    );
}
