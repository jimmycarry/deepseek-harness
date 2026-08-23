//! File-backed `ctx.settings`. Loads `$DSH_HOME/settings.yaml` (or an explicit
//! path) at mount. A missing file is an empty document. Invalid YAML fails loud.

use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_home_paths::resolve_dsh_home;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "settings-file"
}

/// `ctx.settings`: one raw document of per-namespace sections.
#[derive(Debug, Clone)]
pub struct SettingsRuntime {
    /// Absolute path of the settings document.
    pub path: PathBuf,
    /// Parsed document object. Missing file yields `{}`.
    pub document: Value,
}

impl Service for SettingsRuntime {
    const KEY: &'static str = "settings";
}

impl SettingsRuntime {
    /// One top-level section, when present.
    pub fn section(&self, name: &str) -> Option<&Value> {
        self.document.get(name)
    }
}

/// Resolve the document path from plugin config.
pub fn resolve_path(config: Option<&Value>) -> PathBuf {
    if let Some(path) = config
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
    {
        return PathBuf::from(path);
    }
    let home = config
        .and_then(|value| value.get("dshHome"))
        .and_then(Value::as_str);
    resolve_dsh_home(home).join("settings.yaml")
}

/// Load the document. Missing file is `{}`; unreadable or invalid YAML fails.
pub fn load_document(path: &std::path::Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| CordisError::plugin(format!("settings-file: {error}")))?;
    parse_settings_yaml(&text)
}

/// Parse a settings mapping document (nested `key: value` only).
pub fn parse_settings_yaml(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|error| CordisError::Validation(format!("settings-file: {error}")));
    }
    if trimmed.starts_with('-') {
        return Err(CordisError::Validation(
            "settings-file: document must be a mapping".into(),
        ));
    }
    let lines: Vec<(usize, &str)> = text
        .lines()
        .filter_map(|raw| {
            let indent = raw.chars().take_while(|ch| *ch == ' ').count();
            let content = raw.get(indent..).unwrap_or("").trim_end();
            if content.is_empty() || content.starts_with('#') {
                None
            } else {
                Some((indent, content))
            }
        })
        .collect();
    parse_mapping(&lines, 0, 0).map(|(value, _)| value)
}

fn parse_mapping(lines: &[(usize, &str)], start: usize, indent: usize) -> Result<(Value, usize)> {
    let mut map = Map::new();
    let mut i = start;
    while i < lines.len() {
        let (line_indent, content) = lines[i];
        if line_indent < indent {
            break;
        }
        if line_indent > indent {
            return Err(CordisError::Validation(format!(
                "settings-file: unexpected indent at {content}"
            )));
        }
        let (key, rest) = split_key(content)?;
        i += 1;
        if rest.is_empty() {
            let child_indent = lines
                .get(i)
                .map(|(child, _)| *child)
                .filter(|child| *child > indent);
            if let Some(child_indent) = child_indent {
                let (child, next) = parse_mapping(lines, i, child_indent)?;
                map.insert(key, child);
                i = next;
            } else {
                map.insert(key, Value::Object(Map::new()));
            }
        } else {
            map.insert(key, scalar(&rest));
        }
    }
    Ok((Value::Object(map), i))
}

fn split_key(content: &str) -> Result<(String, String)> {
    let Some((key, rest)) = content.split_once(':') else {
        return Err(CordisError::Validation(format!(
            "settings-file: expected key: value, got {content}"
        )));
    };
    if key.trim().is_empty() {
        return Err(CordisError::Validation(
            "settings-file: empty key".into(),
        ));
    }
    Ok((key.trim().to_string(), rest.trim().to_string()))
}

fn scalar(text: &str) -> Value {
    if text == "true" {
        return Value::Bool(true);
    }
    if text == "false" {
        return Value::Bool(false);
    }
    if text == "null" || text == "~" {
        return Value::Null;
    }
    if let Ok(number) = text.parse::<i64>() {
        return Value::from(number);
    }
    let unquoted = text
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(text);
    Value::String(unquoted.to_string())
}

/// Provide `ctx.settings` from the resolved document.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let path = resolve_path(config);
    let document = load_document(&path)?;
    ctx.provide(Arc::new(SettingsRuntime { path, document }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-settings-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_empty_document() {
        let dir = stamp_dir("missing");
        let path = dir.join("settings.yaml");
        assert_eq!(load_document(&path).unwrap(), serde_json::json!({}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_llm_deepseek_section() {
        let dir = stamp_dir("present");
        let path = dir.join("settings.yaml");
        std::fs::write(
            &path,
            "llm-deepseek:\n  apiKeyEnv: DEEPSEEK_API_KEY\n  baseURL: https://example.test\n",
        )
        .unwrap();
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({ "path": path.to_string_lossy() })),
        )
        .unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL")),
            Some(&serde_json::json!("https://example.test"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_sequence_document() {
        assert!(parse_settings_yaml("- not a mapping\n").is_err());
    }
}
