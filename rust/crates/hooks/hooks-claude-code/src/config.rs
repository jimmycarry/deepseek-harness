//! Parse Claude Code's event-to-matcher-group hook format.

use dsh_hook_protocol::{matcher_diagnostic, CommandHook, MatcherGroup, MatcherMode};
use serde_json::Value;
use std::collections::BTreeMap;

const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

/// A parsed CC config: event name → its matcher groups (command hooks only).
pub type ClaudeCodeHookConfig = BTreeMap<String, Vec<MatcherGroup>>;

/// A skipped non-command hook, surfaced so the bridge can warn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedHook {
    /// Event that listed the unsupported hook.
    pub event: String,
    /// Declared hook type.
    pub type_name: String,
}

/// The outcome of parsing one config file.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedClaudeConfig {
    /// Runnable per-event groups.
    pub config: ClaudeCodeHookConfig,
    /// Unsupported non-command hooks.
    pub skipped: Vec<SkippedHook>,
}

/// Substitution variables applied to each `command` string at parse time.
#[derive(Debug, Clone, Default)]
pub struct SubstitutionVars {
    /// Replaces `${CLAUDE_PLUGIN_ROOT}`.
    pub plugin_root: Option<String>,
    /// Replaces `${CLAUDE_PROJECT_DIR}`.
    pub project_dir: Option<String>,
}

fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

/// Apply `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PROJECT_DIR}` substitution.
pub fn substitute_command(command: &str, vars: &SubstitutionVars) -> String {
    let mut out = command.to_string();
    if let Some(root) = &vars.plugin_root {
        out = out.replace("${CLAUDE_PLUGIN_ROOT}", root);
    }
    if let Some(dir) = &vars.project_dir {
        out = out.replace("${CLAUDE_PROJECT_DIR}", dir);
    }
    out
}

/// Parse either a settings `hooks` value or a bare `hooks.json` event map.
///
/// # Errors
/// A matcher-bearing supported runnable group with an invalid regex.
pub fn parse_claude_code_config(
    raw: &Value,
    vars: &SubstitutionVars,
) -> Result<ParsedClaudeConfig, String> {
    let mut config = BTreeMap::new();
    let mut skipped = Vec::new();
    let Some(root) = as_object(raw) else {
        return Ok(ParsedClaudeConfig { config, skipped });
    };
    let hooks_map = root.get("hooks").and_then(as_object).unwrap_or(root);

    for event in CLAUDE_EVENTS {
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
                        type_name: type_name.to_string(),
                    });
                    continue;
                }
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                commands.push(CommandHook {
                    command: substitute_command(command, vars),
                    timeout_sec: hook.get("timeout").and_then(Value::as_f64),
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
            if let Some(diagnostic) =
                matcher_diagnostic(matcher.as_deref(), MatcherMode::ClaudeCode)
            {
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
    Ok(ParsedClaudeConfig { config, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_all_occurrences() {
        assert_eq!(
            substitute_command(
                "${CLAUDE_PLUGIN_ROOT}/x.sh",
                &SubstitutionVars {
                    plugin_root: Some("/p".into()),
                    project_dir: None,
                }
            ),
            "/p/x.sh"
        );
        assert_eq!(
            substitute_command(
                "${CLAUDE_PROJECT_DIR}/a ${CLAUDE_PROJECT_DIR}/b",
                &SubstitutionVars {
                    plugin_root: None,
                    project_dir: Some("/proj".into()),
                },
            ),
            "/proj/a /proj/b"
        );
        assert_eq!(
            substitute_command("${CLAUDE_PLUGIN_ROOT}/x", &SubstitutionVars::default()),
            "${CLAUDE_PLUGIN_ROOT}/x"
        );
    }

    #[test]
    fn parses_bare_and_wrapped_identically() {
        let groups = json!({
            "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "x.sh" }] }]
        });
        let bare = parse_claude_code_config(&groups, &SubstitutionVars::default()).unwrap();
        let wrapped =
            parse_claude_code_config(&json!({ "hooks": groups }), &SubstitutionVars::default())
                .unwrap();
        assert_eq!(bare.config, wrapped.config);
        assert_eq!(
            bare.config["PreToolUse"][0].matcher.as_deref(),
            Some("Bash")
        );
        assert_eq!(bare.config["PreToolUse"][0].hooks[0].command, "x.sh");
    }

    #[test]
    fn carries_timeout_and_substitutes() {
        let parsed = parse_claude_code_config(
            &json!({
                "Stop": [{ "hooks": [{ "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/s.sh", "timeout": 30 }] }]
            }),
            &SubstitutionVars {
                plugin_root: Some("/p".into()),
                project_dir: None,
            },
        )
        .unwrap();
        assert_eq!(parsed.config["Stop"][0].hooks[0].command, "/p/s.sh");
        assert_eq!(parsed.config["Stop"][0].hooks[0].timeout_sec, Some(30.0));
        assert!(parsed.config["Stop"][0].matcher.is_none());
    }

    #[test]
    fn skips_non_command_hooks() {
        let parsed = parse_claude_code_config(
            &json!({
                "PreToolUse": [{ "hooks": [
                    { "type": "prompt", "prompt": "hi" },
                    { "type": "command", "command": "ok.sh" },
                    { "type": "http", "url": "http://x" },
                ] }]
            }),
            &SubstitutionVars::default(),
        )
        .unwrap();
        assert_eq!(parsed.config["PreToolUse"][0].hooks.len(), 1);
        assert_eq!(parsed.skipped.len(), 2);
        assert_eq!(parsed.skipped[0].type_name, "prompt");
        assert_eq!(parsed.skipped[1].type_name, "http");
    }

    #[test]
    fn treats_missing_type_as_command() {
        let parsed = parse_claude_code_config(
            &json!({ "Stop": [{ "hooks": [{ "command": "d.sh" }] }] }),
            &SubstitutionVars::default(),
        )
        .unwrap();
        assert_eq!(parsed.config["Stop"][0].hooks[0].command, "d.sh");
    }

    #[test]
    fn drops_malformed_entries() {
        assert!(parse_claude_code_config(
            &json!({ "PreToolUse": "nope" }),
            &SubstitutionVars::default()
        )
        .unwrap()
        .config
        .is_empty());
        assert!(
            parse_claude_code_config(&json!(null), &SubstitutionVars::default())
                .unwrap()
                .config
                .is_empty()
        );
        assert!(parse_claude_code_config(
            &json!({ "Stop": [{ "hooks": [{ "type": "command", "command": 5 }] }] }),
            &SubstitutionVars::default(),
        )
        .unwrap()
        .config
        .is_empty());
    }

    #[test]
    fn rejects_invalid_regex() {
        let error = parse_claude_code_config(
            &json!({
                "PreToolUse": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "x.sh" }] }]
            }),
            &SubstitutionVars::default(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            r#"invalid claude-code regex matcher "(" on event "PreToolUse""#
        );
    }

    #[test]
    fn discards_matcher_on_events_without_subjects() {
        let parsed = parse_claude_code_config(
            &json!({
                "UserPromptSubmit": [{ "matcher": "[", "hooks": [{ "type": "command", "command": "prompt.sh" }] }],
                "Stop": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "stop.sh" }] }],
            }),
            &SubstitutionVars::default(),
        )
        .unwrap();
        assert!(parsed.config["UserPromptSubmit"][0].matcher.is_none());
        assert!(parsed.config["Stop"][0].matcher.is_none());
    }
}
