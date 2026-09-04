//! Parse Codex's five-event hook subset into shared matcher groups.

use dsh_hook_protocol::{matcher_diagnostic, CommandHook, MatcherGroup, MatcherMode};
use serde_json::Value;
use std::collections::BTreeMap;

/// The five Codex hook points this bridge supports.
pub const CODEX_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
];

/// A parsed Codex config: event name → its matcher groups.
pub type CodexHookConfig = BTreeMap<String, Vec<MatcherGroup>>;

/// A skipped non-command or async hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedHook {
    /// Event that listed the unsupported hook.
    pub event: String,
    /// Why it was skipped.
    pub reason: String,
}

/// The outcome of parsing one Codex config file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCodexConfig {
    /// Runnable per-event groups.
    pub config: CodexHookConfig,
    /// Skipped hooks with reasons.
    pub skipped: Vec<SkippedHook>,
}

fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

/// Parse a wrapped or bare Codex event map.
///
/// # Errors
/// A matcher-bearing runnable group with an invalid regex.
pub fn parse_codex_config(raw: &Value) -> Result<ParsedCodexConfig, String> {
    let mut config = BTreeMap::new();
    let mut skipped = Vec::new();
    let Some(root) = as_object(raw) else {
        return Ok(ParsedCodexConfig { config, skipped });
    };
    let hooks_map = root.get("hooks").and_then(as_object).unwrap_or(root);

    for event in CODEX_EVENTS {
        let Some(raw_groups) = hooks_map.get(*event).and_then(Value::as_array) else {
            continue;
        };
        let mut groups = Vec::new();
        for raw_group in raw_groups {
            let Some(group) = as_object(raw_group) else {
                continue;
            };
            let Some(raw_hooks) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            let mut commands = Vec::new();
            for raw_hook in raw_hooks {
                let Some(hook) = as_object(raw_hook) else {
                    continue;
                };
                let type_name = hook
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                if type_name != "command" {
                    skipped.push(SkippedHook {
                        event: (*event).to_string(),
                        reason: format!("unsupported \"{type_name}\" hook"),
                    });
                    continue;
                }
                if hook.get("async").and_then(Value::as_bool) == Some(true) {
                    skipped.push(SkippedHook {
                        event: (*event).to_string(),
                        reason: "async hook".into(),
                    });
                    continue;
                }
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let timeout = hook
                    .get("timeout")
                    .and_then(Value::as_f64)
                    .or_else(|| hook.get("timeoutSec").and_then(Value::as_f64));
                commands.push(CommandHook {
                    command: command.to_string(),
                    timeout_sec: timeout,
                });
            }
            if commands.is_empty() {
                continue;
            }
            let matcher = if *event == "UserPromptSubmit" || *event == "Stop" {
                None
            } else {
                group
                    .get("matcher")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            if let Some(diagnostic) = matcher_diagnostic(matcher.as_deref(), MatcherMode::Codex) {
                return Err(format!(
                    "{diagnostic} on event {}",
                    serde_json::to_string(event).unwrap_or_else(|_| format!("\"{event}\""))
                ));
            }
            groups.push(MatcherGroup {
                matcher,
                hooks: commands,
            });
        }
        if !groups.is_empty() {
            config.insert((*event).to_string(), groups);
        }
    }
    Ok(ParsedCodexConfig { config, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn honors_only_five_events() {
        let parsed = parse_codex_config(&json!({
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": "a.sh" }] }],
            "SubagentStop": [{ "hooks": [{ "type": "command", "command": "b.sh" }] }],
            "Notification": [{ "hooks": [{ "type": "command", "command": "c.sh" }] }],
        }))
        .unwrap();
        assert_eq!(parsed.config.keys().collect::<Vec<_>>(), vec!["PreToolUse"]);
        assert!(CODEX_EVENTS.contains(&"PreToolUse"));
        assert!(!CODEX_EVENTS.contains(&"SubagentStop"));
    }

    #[test]
    fn accepts_timeout_aliases_without_substitution() {
        let parsed = parse_codex_config(&json!({
            "Stop": [{ "hooks": [{ "type": "command", "command": "${NOT_SUBSTITUTED}/s.sh", "timeout": 10 }] }],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "u.sh", "timeoutSec": 20 }] }],
        }))
        .unwrap();
        assert_eq!(
            parsed.config["Stop"][0].hooks[0].command,
            "${NOT_SUBSTITUTED}/s.sh"
        );
        assert_eq!(parsed.config["Stop"][0].hooks[0].timeout_sec, Some(10.0));
        assert_eq!(
            parsed.config["UserPromptSubmit"][0].hooks[0].timeout_sec,
            Some(20.0)
        );
    }

    #[test]
    fn skips_non_command_and_async() {
        let parsed = parse_codex_config(&json!({
            "PreToolUse": [{ "hooks": [
                { "type": "prompt" },
                { "type": "command", "command": "sync.sh" },
                { "type": "command", "command": "bg.sh", "async": true },
            ] }]
        }))
        .unwrap();
        assert_eq!(parsed.config["PreToolUse"][0].hooks.len(), 1);
        assert_eq!(parsed.skipped[0].reason, "unsupported \"prompt\" hook");
        assert_eq!(parsed.skipped[1].reason, "async hook");
    }

    #[test]
    fn parses_wrapper_and_bare_identically() {
        let groups = json!({ "Stop": [{ "hooks": [{ "type": "command", "command": "s.sh" }] }] });
        assert_eq!(
            parse_codex_config(&groups).unwrap().config,
            parse_codex_config(&json!({ "hooks": groups }))
                .unwrap()
                .config
        );
    }

    #[test]
    fn drops_malformed_entries() {
        assert!(parse_codex_config(&json!(null)).unwrap().config.is_empty());
        assert!(parse_codex_config(&json!({ "PreToolUse": "no" }))
            .unwrap()
            .config
            .is_empty());
    }

    #[test]
    fn rejects_invalid_regex() {
        let error = parse_codex_config(&json!({
            "PreToolUse": [{ "matcher": "[", "hooks": [{ "type": "command", "command": "s.sh" }] }]
        }))
        .unwrap_err();
        assert_eq!(
            error,
            r#"invalid codex regex matcher "[" on event "PreToolUse""#
        );
    }

    #[test]
    fn discards_matcher_on_events_without_subjects() {
        let parsed = parse_codex_config(&json!({
            "UserPromptSubmit": [{ "matcher": "[", "hooks": [{ "type": "command", "command": "prompt.sh" }] }],
            "Stop": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "stop.sh" }] }],
        }))
        .unwrap();
        assert!(parsed.config["UserPromptSubmit"][0].matcher.is_none());
        assert!(parsed.config["Stop"][0].matcher.is_none());
    }
}
