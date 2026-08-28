//! Shared rendering and exit-status parse for the shell tools.

use super::{CollectedOutput, ShellRunResult, ShellSandboxInfo};
use dsh_sandbox::{escalation_hint_marker, sandbox_denial_marker};
use serde_json::{json, Value};

/// Exit status recovered from a rendered result, with the output body that
/// status was split off from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedExitStatus {
    /// Marker-free body. Timeout and sandbox markers stay in it.
    pub body: String,
    /// Process exit code when the run ended by exiting (including a clean 0).
    pub exit_code: Option<i32>,
    /// Signal name that killed the process. Mutually exclusive with a non-zero pill.
    pub signal: Option<String>,
}

/// Split a rendered shell-tool result string into its output body and the
/// structured exit status — the inverse of the `[exit code: N]` /
/// `[killed by signal: X]` markers. A killed marker yields `signal`;
/// otherwise a non-zero marker yields `exit_code`; absent both means a
/// clean exit 0.
///
/// The consumed marker is removed from `body` because a terminal presentation
/// shows the exit status as its own pill. Other markers (timeout, sandbox)
/// stay in the body. Requiring a leading newline and the end of the string
/// keeps ordinary output that merely ends with marker-like text from matching.
pub fn parse_exit_status(text: &str) -> ParsedExitStatus {
    if let Some(caps) = trailing_signal(text) {
        return ParsedExitStatus {
            body: text[..caps.index].to_string(),
            exit_code: None,
            signal: Some(caps.value),
        };
    }
    if let Some(caps) = trailing_exit(text) {
        return ParsedExitStatus {
            body: text[..caps.index].to_string(),
            exit_code: Some(caps.code),
            signal: None,
        };
    }
    ParsedExitStatus {
        body: text.to_string(),
        exit_code: Some(0),
        signal: None,
    }
}

struct SignalCap {
    index: usize,
    value: String,
}

struct ExitCap {
    index: usize,
    code: i32,
}

fn trailing_signal(text: &str) -> Option<SignalCap> {
    const PREFIX: &str = "\n[killed by signal: ";
    let start = text.rfind(PREFIX)?;
    if !text.ends_with(']') {
        return None;
    }
    let inner = &text[start + PREFIX.len()..text.len() - 1];
    if inner.is_empty() || inner.contains('\n') || inner.contains(']') {
        return None;
    }
    Some(SignalCap {
        index: start,
        value: inner.to_string(),
    })
}

fn trailing_exit(text: &str) -> Option<ExitCap> {
    const PREFIX: &str = "\n[exit code: ";
    let start = text.rfind(PREFIX)?;
    if !text.ends_with(']') {
        return None;
    }
    let inner = &text[start + PREFIX.len()..text.len() - 1];
    if inner.is_empty() || inner.contains('\n') || inner.contains(']') {
        return None;
    }
    if !inner.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let code = inner.parse::<i32>().ok()?;
    Some(ExitCap {
        index: start,
        code,
    })
}

fn stream_text(output: &CollectedOutput) -> String {
    if !output.truncated {
        return output.text.clone();
    }
    format!(
        "{}\n[output truncated; full output: {}]",
        output.text,
        output.spill_path.as_deref().unwrap_or("(unavailable)")
    )
}

/// Shape one finished run into the text the model sees.
pub fn render_shell_result(result: &ShellRunResult, advertises_escalation: bool) -> String {
    let out = stream_text(&result.stdout);
    let err = stream_text(&result.stderr);
    let mut body = out;
    if !err.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&err);
    }
    if body.is_empty() {
        body = "(no output)".into();
    }
    let mut markers = Vec::new();
    if result.sandbox.as_ref().is_some_and(|info| info.denied) {
        let mode = result.sandbox.as_ref().expect("denied implies sandbox").mode;
        markers.push(sandbox_denial_marker(mode));
        if advertises_escalation {
            markers.push(escalation_hint_marker("command"));
        }
    }
    if result.timed_out {
        markers.push(format!("[timed out after {}ms]", result.timeout_ms));
    }
    if let Some(signal) = &result.signal {
        markers.push(format!("[killed by signal: {signal}]"));
    } else if result.exit_code != Some(0) {
        let code = result
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "null".into());
        markers.push(format!("[exit code: {code}]"));
    }
    if markers.is_empty() {
        return body;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&markers.join("\n"));
    body
}

/// Shape one background-process read into the `job_output` delta.
pub fn render_process_read(
    delta: &str,
    sandbox: Option<&ShellSandboxInfo>,
    advertises_escalation: bool,
) -> String {
    let mut notices = Vec::new();
    if sandbox.is_some_and(|info| info.runner_failed == Some(true)) {
        let mode = sandbox.expect("runnerFailed implies sandbox").mode;
        notices.push(format!(
            "[sandbox: the sandbox runner itself failed under {} mode — the command did not run; this is a sandbox problem, not a command failure]",
            mode.as_str()
        ));
    } else if sandbox.is_some_and(|info| info.denied) {
        let mode = sandbox.expect("denied implies sandbox").mode;
        notices.push(sandbox_denial_marker(mode));
        if advertises_escalation {
            notices.push(escalation_hint_marker("command"));
        }
    }
    if notices.is_empty() {
        return delta.to_string();
    }
    let separator = if !delta.is_empty() && !delta.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!("{delta}{separator}{}", notices.join("\n"))
}

/// Canonical `output.schema` oneOf for bash and pwsh.
pub fn shell_tool_output_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": { "type": "string", "required": true, "const": "background" },
                    "jobId": { "type": "string", "required": true }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": { "type": "string", "required": true, "const": "foreground" },
                    "exitCode": { "required": true, "oneOf": [{ "type": "integer" }, { "type": "null" }] },
                    "signal": { "required": true, "oneOf": [{ "type": "string" }, { "type": "null" }] },
                    "timedOut": { "type": "boolean", "required": true },
                    "aborted": { "type": "boolean", "required": true },
                    "timeoutMs": { "type": "number", "required": true },
                    "stdout": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": true,
                        "properties": {
                            "text": { "type": "string", "required": true },
                            "truncated": { "type": "boolean", "required": true },
                            "spillPath": { "type": "string" }
                        }
                    },
                    "stderr": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": true,
                        "properties": {
                            "text": { "type": "string", "required": true },
                            "truncated": { "type": "boolean", "required": true },
                            "spillPath": { "type": "string" }
                        }
                    },
                    "sandbox": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "mode": { "type": "string", "required": true },
                            "denied": { "type": "boolean", "required": true },
                            "enforcement": { "type": "string" },
                            "runnerFailed": { "type": "boolean" }
                        }
                    }
                }
            }
        ]
    })
}

fn collected_json(output: &CollectedOutput) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("text".into(), Value::String(output.text.clone()));
    map.insert("truncated".into(), Value::Bool(output.truncated));
    if let Some(path) = &output.spill_path {
        map.insert("spillPath".into(), Value::String(path.clone()));
    }
    Value::Object(map)
}

/// Project a settled run into the foreground arm of the shell-tool output union.
pub fn canonical_foreground_json(result: &ShellRunResult) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), Value::String("foreground".into()));
    map.insert(
        "exitCode".into(),
        result
            .exit_code
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    map.insert(
        "signal".into(),
        result
            .signal
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    map.insert("timedOut".into(), Value::Bool(result.timed_out));
    map.insert("aborted".into(), Value::Bool(result.aborted));
    map.insert("timeoutMs".into(), json!(result.timeout_ms));
    map.insert("stdout".into(), collected_json(&result.stdout));
    map.insert("stderr".into(), collected_json(&result.stderr));
    if let Some(sandbox) = &result.sandbox {
        let mut facts = serde_json::Map::new();
        facts.insert("mode".into(), Value::String(sandbox.mode.as_str().into()));
        facts.insert("denied".into(), Value::Bool(sandbox.denied));
        if let Some(enforcement) = sandbox.enforcement {
            facts.insert(
                "enforcement".into(),
                Value::String(
                    match enforcement {
                        dsh_sandbox::SandboxEnforcement::Full => "full",
                        dsh_sandbox::SandboxEnforcement::Partial => "partial",
                    }
                    .into(),
                ),
            );
        }
        if let Some(runner_failed) = sandbox.runner_failed {
            facts.insert("runnerFailed".into(), Value::Bool(runner_failed));
        }
        map.insert("sandbox".into(), Value::Object(facts));
    }
    Value::Object(map)
}

/// Canonical bash/pwsh tool output: a background job handle or a foreground run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellToolOutput {
    /// `run_in_background` acknowledgement.
    Background {
        /// Job id from `ctx.jobs`.
        job_id: String,
    },
    /// Settled foreground run, projected then rendered.
    Foreground {
        /// Executor result this value was projected from.
        result: ShellRunResult,
    },
}

impl ShellToolOutput {
    /// Model-facing text for this canonical value.
    pub fn render(&self, advertises_escalation: bool) -> String {
        match self {
            Self::Background { job_id } => format!("started background job {job_id}"),
            Self::Foreground { result } => render_shell_result(result, advertises_escalation),
        }
    }

    /// JSON projection of the `output.schema` value.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Background { job_id } => json!({ "kind": "background", "jobId": job_id }),
            Self::Foreground { result } => canonical_foreground_json(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exit_status_round_trips_markers() {
        let zero = parse_exit_status("hi\n\n");
        assert_eq!(zero.body, "hi\n\n");
        assert_eq!(zero.exit_code, Some(0));
        assert!(zero.signal.is_none());

        let nonzero = parse_exit_status("oops\n[exit code: 3]");
        assert_eq!(nonzero.body, "oops");
        assert_eq!(nonzero.exit_code, Some(3));

        let killed = parse_exit_status("gone\n[killed by signal: SIGKILL]");
        assert_eq!(killed.body, "gone");
        assert_eq!(killed.signal.as_deref(), Some("SIGKILL"));
        assert!(killed.exit_code.is_none());
    }

    #[test]
    fn parse_exit_status_leaves_timeout_and_sandbox_markers() {
        let timed = parse_exit_status("slow\n[timed out after 100ms]\n[exit code: 143]");
        assert_eq!(timed.body, "slow\n[timed out after 100ms]");
        assert_eq!(timed.exit_code, Some(143));
    }

    #[test]
    fn parse_exit_status_does_not_eat_marker_like_body() {
        let out = parse_exit_status("[exit code: 5]");
        assert_eq!(out.body, "[exit code: 5]");
        assert_eq!(out.exit_code, Some(0));
        let sig = parse_exit_status("[killed by signal: SIGKILL]");
        assert_eq!(sig.body, "[killed by signal: SIGKILL]");
        assert_eq!(sig.exit_code, Some(0));
        let negative = parse_exit_status("oops\n[exit code: -1]");
        assert_eq!(negative.body, "oops\n[exit code: -1]");
        assert_eq!(negative.exit_code, Some(0));
    }

    #[test]
    fn output_schema_is_a_foreground_background_oneof() {
        let schema = shell_tool_output_schema();
        assert!(schema["oneOf"].as_array().unwrap().len() == 2);
        assert_eq!(schema["oneOf"][0]["properties"]["kind"]["const"], "background");
        assert_eq!(schema["oneOf"][1]["properties"]["kind"]["const"], "foreground");
    }

    #[test]
    fn process_read_prefers_runner_failed_over_denied() {
        use dsh_sandbox::{SandboxEnforcement, SandboxMode};
        let sandbox = ShellSandboxInfo {
            mode: SandboxMode::ReadOnly,
            denied: true,
            enforcement: Some(SandboxEnforcement::Full),
            runner_failed: Some(true),
        };
        let text = render_process_read("x", Some(&sandbox), true);
        assert_eq!(
            text,
            "x\n[sandbox: the sandbox runner itself failed under read-only mode — the command did not run; this is a sandbox problem, not a command failure]"
        );
        let denied_only = ShellSandboxInfo {
            runner_failed: None,
            ..sandbox
        };
        let denied = render_process_read("x", Some(&denied_only), true);
        assert!(denied.contains("[sandbox: file access denied under read-only mode]"));
        assert!(denied.contains("sandbox_permissions"));
        assert!(!denied.contains("the sandbox runner itself failed"));
    }
}
