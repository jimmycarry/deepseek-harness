//! Decode hook process outcomes for both dialects.

use crate::types::{HookDecision, HookOutput};
use serde_json::Value;

const BLOCKING_EXIT_CODE: i32 = 2;

fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bool_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(Value::as_bool)
}

fn top_level_decision(value: Option<&str>) -> Option<HookDecision> {
    match value {
        Some("approve") => Some(HookDecision::Approve),
        Some("block") => Some(HookDecision::Block),
        _ => None,
    }
}

fn permission_decision(value: Option<&str>) -> Option<HookDecision> {
    match value {
        Some("allow") => Some(HookDecision::Allow),
        Some("deny") => Some(HookDecision::Deny),
        Some("ask") => Some(HookDecision::Ask),
        _ => None,
    }
}

/// Decode process output into a dialect-neutral hook outcome.
///
/// This function is total: malformed JSON remains plain stdout. When
/// `expected_event_name` is set, a missing or different
/// `hookSpecificOutput.hookEventName` discards only its event-scoped fields.
pub fn parse_hook_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    expected_event_name: Option<&str>,
) -> HookOutput {
    let trimmed_err = stderr.trim();
    let trimmed_out = stdout.trim();
    let mut output = HookOutput {
        exit_code,
        stderr: trimmed_err.to_string(),
        stdout: trimmed_out.to_string(),
        ..HookOutput::default()
    };

    if exit_code == Some(BLOCKING_EXIT_CODE) {
        output.decision = Some(HookDecision::Block);
        if !trimmed_err.is_empty() {
            output.reason = Some(trimmed_err.to_string());
        }
    }

    if exit_code == Some(0) && trimmed_out.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed_out) {
            if let Some(obj) = as_object(&parsed) {
                apply_structured(&mut output, obj, expected_event_name);
            }
        }
    }

    output
}

fn apply_structured(
    output: &mut HookOutput,
    parsed: &serde_json::Map<String, Value>,
    expected_event_name: Option<&str>,
) {
    if let Some(cont) = bool_field(parsed, "continue") {
        output.continue_run = Some(cont);
    }
    if let Some(stop_reason) = str_field(parsed, "stopReason") {
        output.stop_reason = Some(stop_reason);
    }
    if let Some(sys_msg) = str_field(parsed, "systemMessage") {
        output.system_message = Some(sys_msg);
    }
    if let Some(decision) = top_level_decision(parsed.get("decision").and_then(Value::as_str)) {
        output.decision = Some(decision);
    }
    if let Some(reason) = str_field(parsed, "reason") {
        output.reason = Some(reason);
    }

    let Some(hso) = parsed.get("hookSpecificOutput").and_then(as_object) else {
        return;
    };
    let event_name = str_field(hso, "hookEventName");
    if let Some(name) = event_name.clone() {
        output.hook_event_name = Some(name);
    }
    if expected_event_name.is_some() && event_name.as_deref() != expected_event_name {
        return;
    }
    if let Some(permission) =
        permission_decision(hso.get("permissionDecision").and_then(Value::as_str))
    {
        output.decision = Some(permission);
    }
    if let Some(permission_reason) = str_field(hso, "permissionDecisionReason") {
        output.reason = Some(permission_reason);
    }
    if let Some(add_ctx) = str_field(hso, "additionalContext") {
        output.additional_context = Some(add_ctx);
    }
    if let Some(updated) = hso.get("updatedInput").and_then(as_object) {
        output.updated_input = Some(updated.clone());
    }
}
