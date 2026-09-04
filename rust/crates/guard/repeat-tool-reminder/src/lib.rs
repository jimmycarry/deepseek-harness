//! Advisory per-agent repeat-call detector (`@deepseek-ai/dsh-repeat-tool-reminder`).
//!
//! Enriches `tools/post-execute` with a logged user-role notice. It never
//! vetoes or rewrites the call. A human `source.kind === "user"` message on
//! `agent/pre-step` resets that agent's chain.

use dsh_cordis::{Context, Result};
use dsh_llm::UserMessage;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-repeat-tool-reminder"
}

/// First-threshold reminder. Keyed to `thresholds[0]`, not a literal count.
pub const GENTLE_REMINDER: &str = "You are repeating the exact same tool call with identical arguments. Carefully analyze the previous result before calling again: if the task is not complete, try a different approach or different arguments instead of repeating the call.";

/// Resolved guard policy. Every field is supplied at construction.
#[derive(Debug, Clone)]
pub struct Config {
    /// Consecutive-repeat counts that trigger a reminder, sorted ascending.
    pub thresholds: Vec<u32>,
    /// Tool-name patterns to track; empty means every tool is tracked.
    pub include: Vec<String>,
    /// Tool-name patterns transparent to the chain.
    pub exclude: Vec<String>,
    /// Maximum characters of canonical arguments quoted in the detailed reminder.
    pub arguments_preview_chars: usize,
}

impl Config {
    /// Resolve plugin config. Missing optional fields take the TypeScript defaults.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let thresholds = match value.and_then(|value| value.get("thresholds")) {
            None => vec![3, 5, 8],
            Some(Value::Array(items)) => {
                let mut parsed = Vec::new();
                for item in items {
                    let number = item.as_u64().ok_or_else(|| {
                        format!(
                            "repeat-tool-reminder: invalid threshold {item} — every threshold must be an integer >= 2"
                        )
                    })?;
                    parsed.push(number as u32);
                }
                parsed
            }
            Some(_) => {
                return Err(
                    "repeat-tool-reminder: `thresholds` must be an array of integers".into(),
                )
            }
        };
        let thresholds = validate_thresholds(thresholds)?;
        let include = string_list(value, "include")?;
        let exclude = string_list(value, "exclude")?;
        let arguments_preview_chars = match value.and_then(|value| value.get("argumentsPreviewChars"))
        {
            None => 500,
            Some(item) => {
                let number = item.as_u64().ok_or_else(|| {
                    format!(
                        "repeat-tool-reminder: invalid argumentsPreviewChars {item} — must be an integer >= 1"
                    )
                })?;
                if number < 1 {
                    return Err(format!(
                        "repeat-tool-reminder: invalid argumentsPreviewChars {number} — must be an integer >= 1"
                    ));
                }
                number as usize
            }
        };
        Ok(Self {
            thresholds,
            include,
            exclude,
            arguments_preview_chars,
        })
    }
}

fn string_list(value: Option<&Value>, field: &str) -> std::result::Result<Vec<String>, String> {
    match value.and_then(|value| value.get(field)) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("repeat-tool-reminder: `{field}` entries must be strings"))
            })
            .collect(),
        Some(_) => Err(format!("repeat-tool-reminder: `{field}` must be an array")),
    }
}

fn validate_thresholds(values: Vec<u32>) -> std::result::Result<Vec<u32>, String> {
    if values.is_empty() {
        return Err("repeat-tool-reminder: `thresholds` must not be empty".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for value in &values {
        if *value < 2 {
            return Err(format!(
                "repeat-tool-reminder: invalid threshold {value} — every threshold must be an integer >= 2"
            ));
        }
        if !seen.insert(*value) {
            return Err("repeat-tool-reminder: `thresholds` must not contain duplicates".into());
        }
    }
    let mut sorted = values;
    sorted.sort_unstable();
    Ok(sorted)
}

/// Deep key-sort then stringify so argument objects that differ only in
/// property order canonicalize identically.
pub fn canonicalize(value: &Value) -> String {
    serde_json::to_string(&sort_json(value.clone())).unwrap_or_else(|_| "null".into())
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json).collect()),
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(item) = map.get(&key) {
                    sorted.insert(key, sort_json(item.clone()));
                }
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

fn wildcard_match(pattern: &str, name: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), name.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => {
            wildcard_match_bytes(rest, name)
                || (!name.is_empty() && wildcard_match_bytes(pattern, &name[1..]))
        }
        (Some((pat, prest)), Some((ch, nrest))) if pat == ch => {
            wildcard_match_bytes(prest, nrest)
        }
        _ => false,
    }
}

/// Head-truncate canonical arguments for the detailed reminder.
pub fn preview_arguments(canonical: &str, cap: usize) -> String {
    if canonical.len() <= cap {
        return canonical.to_string();
    }
    format!(
        "{}… (+{} more chars)",
        &canonical[..cap],
        canonical.len() - cap
    )
}

/// Later-threshold reminder naming the tool, run length, and arguments.
pub fn detailed_reminder(tool_name: &str, count: u32, canonical_arguments: &str) -> String {
    format!(
        "Repeated tool call detected:\n- tool: {tool_name}\n- consecutive_calls: {count}\n- arguments: {canonical_arguments}\nThe repeated calls are not making progress. Do not call this tool with these exact arguments again. Inspect the latest result and choose a different action, different arguments, or finish the task if enough evidence has been gathered."
    )
}

struct Chain {
    key: String,
    count: u32,
}

/// Install the guard's listeners. Misconfiguration fails at load.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let thresholds = Arc::new(config.thresholds.clone());
    let threshold_set: Arc<std::collections::HashSet<u32>> =
        Arc::new(config.thresholds.iter().copied().collect());
    let include = Arc::new(config.include);
    let exclude = Arc::new(config.exclude);
    let preview_chars = config.arguments_preview_chars;
    let first = config.thresholds[0];
    let chains: Arc<Mutex<HashMap<String, Chain>>> = Arc::new(Mutex::new(HashMap::new()));

    let observe_chains = Arc::clone(&chains);
    let observe_thresholds = Arc::clone(&thresholds);
    let observe_set = Arc::clone(&threshold_set);
    let observe_include = Arc::clone(&include);
    let observe_exclude = Arc::clone(&exclude);
    ctx.on_waterfall("tools/post-execute", move |payload, next| {
        let reminder = observe(
            &payload,
            &observe_chains,
            &observe_include,
            &observe_exclude,
            &observe_set,
            &observe_thresholds,
            first,
            preview_chars,
        );
        let mut downstream = next.call(payload);
        if let Some(reminder) = reminder {
            if let Value::Object(map) = &mut downstream {
                let mut contexts = map
                    .get("additionalContexts")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                if let Value::Array(items) = &mut contexts {
                    items.insert(0, serde_json::to_value(&reminder).unwrap_or(json!({})));
                }
                map.insert("additionalContexts".into(), contexts);
            }
        }
        downstream
    })?;

    let reset_chains = Arc::clone(&chains);
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        if let Some(agent_id) = payload.get("agentId").and_then(Value::as_str) {
            let has_user = payload
                .get("messages")
                .and_then(Value::as_array)
                .map(|messages| {
                    messages.iter().any(|message| {
                        message
                            .get("source")
                            .and_then(|source| source.get("kind"))
                            .and_then(Value::as_str)
                            == Some("user")
                    })
                })
                .unwrap_or(false);
            if has_user {
                reset_chains.lock().expect("chains").remove(agent_id);
            }
        }
        next.call(payload)
    })?;
    Ok(())
}

fn tracked(name: &str, include: &[String], exclude: &[String]) -> bool {
    if !include.is_empty() && !include.iter().any(|pattern| wildcard_match(pattern, name)) {
        return false;
    }
    !exclude.iter().any(|pattern| wildcard_match(pattern, name))
}

#[allow(clippy::too_many_arguments)]
fn observe(
    payload: &Value,
    chains: &Mutex<HashMap<String, Chain>>,
    include: &[String],
    exclude: &[String],
    threshold_set: &std::collections::HashSet<u32>,
    _thresholds: &[u32],
    first: u32,
    preview_chars: usize,
) -> Option<UserMessage> {
    let agent_id = payload.get("agentId").and_then(Value::as_str)?;
    let tool_name = payload.get("name").and_then(Value::as_str)?;
    if !tracked(tool_name, include, exclude) {
        return None;
    }
    let args = payload.get("args").cloned().unwrap_or(Value::Null);
    let canonical = canonicalize(&args);
    let key = serde_json::to_string(&json!([tool_name, canonical])).unwrap_or_default();
    let count = {
        let mut map = chains.lock().expect("chains");
        let count = match map.get(agent_id) {
            Some(chain) if chain.key == key => chain.count + 1,
            _ => 1,
        };
        map.insert(
            agent_id.to_string(),
            Chain {
                key,
                count,
            },
        );
        count
    };
    if !threshold_set.contains(&count) {
        return None;
    }
    let text = if count == first {
        GENTLE_REMINDER.to_string()
    } else {
        detailed_reminder(tool_name, count, &preview_arguments(&canonical, preview_chars))
    };
    Some(UserMessage::notice(
        "repeat-tool-reminder",
        text,
        format!("{tool_name} × {count}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_tools::{ScriptTool, ToolRuntime};
    use std::sync::Arc;

    #[test]
    fn resolve_rejects_empty_thresholds() {
        let err = Config::resolve(Some(&json!({ "thresholds": [] }))).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn canonicalize_sorts_object_keys() {
        assert_eq!(
            canonicalize(&json!({"b": 1, "a": 2})),
            canonicalize(&json!({"a": 2, "b": 1}))
        );
    }

    #[tokio::test]
    async fn third_identical_call_prepends_gentle_reminder() {
        let ctx = Context::new();
        let tools = Arc::new(ToolRuntime::new());
        tools.insert(Arc::new(ScriptTool::new("probe", "probe", |_| {
            dsh_tools::ToolOutcome::text("ok")
        })));
        ctx.provide(Arc::clone(&tools)).unwrap();
        install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let args = json!({ "q": "same" });
        for _ in 0..2 {
            let result = tools
                .execute_for(&ctx, "probe", args.clone(), Some("agent-1"))
                .await
                .unwrap();
            assert!(result.additional_contexts.is_empty());
        }
        let third = tools
            .execute_for(&ctx, "probe", args, Some("agent-1"))
            .await
            .unwrap();
        assert_eq!(third.additional_contexts.len(), 1);
        let reminder = &third.additional_contexts[0];
        let text = match &reminder.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.as_str(),
            _ => panic!("text"),
        };
        assert_eq!(text, GENTLE_REMINDER);
        match &reminder.source {
            dsh_llm::MessageSource::Plugin {
                plugin,
                form,
                summary,
                ..
            } => {
                assert_eq!(plugin, "repeat-tool-reminder");
                assert_eq!(form.as_deref(), Some("notice"));
                assert_eq!(summary.as_deref(), Some("probe × 3"));
            }
            other => panic!("expected plugin notice, got {other:?}"),
        }
    }
}
