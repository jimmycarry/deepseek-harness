//! File-backed `ctx.settings`. Loads `$DSH_HOME/settings.yaml` (or an explicit
//! path) at mount. A missing file is an empty document. Invalid YAML at mount
//! fails loud. When `watch` is on (the default), later reads re-load the
//! document after the debounce window; an unreadable or invalid live edit
//! keeps the last good document. `update` / `replace` persist one namespace
//! as a comment-preserving leaf-level YAML diff.

mod yaml_patch;

use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_home_paths::resolve_dsh_home;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "settings-file"
}

struct DocumentState {
    document: Value,
    text: Option<String>,
}

/// `ctx.settings`: one raw document of per-namespace sections.
pub struct SettingsRuntime {
    /// Absolute path of the settings document.
    pub path: PathBuf,
    watch: bool,
    debounce_ms: u64,
    last_probe: Mutex<Option<Instant>>,
    state: Mutex<DocumentState>,
    namespaces: Mutex<HashSet<String>>,
    revision: Mutex<u64>,
}

impl Service for SettingsRuntime {
    const KEY: &'static str = "settings";
}

impl SettingsRuntime {
    /// One top-level section, when present. Reloads a watched document first.
    pub fn section(&self, name: &str) -> Option<Value> {
        self.refresh();
        self.state
            .lock()
            .expect("settings")
            .document
            .get(name)
            .cloned()
    }

    /// Current document object. Reloads a watched document first.
    pub fn document(&self) -> Value {
        self.refresh();
        self.state.lock().expect("settings").document.clone()
    }

    /// Re-read the file when watching is on. Unchanged text is a no-op.
    /// A missing file becomes `{}`. A live parse or I/O failure keeps the last
    /// good document.
    pub fn refresh(&self) {
        if !self.watch {
            return;
        }
        {
            let mut last = self.last_probe.lock().expect("settings probe");
            if let Some(previous) = *last {
                if self.debounce_ms > 0
                    && previous.elapsed() < Duration::from_millis(self.debounce_ms)
                {
                    return;
                }
            }
            *last = Some(Instant::now());
        }
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return,
        };
        let mut state = self.state.lock().expect("settings");
        if text.as_deref() == state.text.as_deref() {
            return;
        }
        match text {
            None => {
                state.text = None;
                state.document = Value::Object(Map::new());
            }
            Some(text) => match parse_settings_yaml(&text) {
                Ok(document) => {
                    state.text = Some(text);
                    state.document = document;
                }
                Err(_) => {}
            },
        }
    }

    /// Whether this backend writes `update` / `replace` to disk.
    pub fn writable(&self) -> bool {
        true
    }

    /// Register a namespace so later `update` / `replace` calls can persist it.
    ///
    /// @param namespace Settings namespace id (`^[a-z][a-z0-9-]*$`).
    pub fn register(&self, namespace: &str) -> Result<()> {
        if !is_settings_namespace(namespace) {
            return Err(CordisError::Validation(format!(
                "invalid settings namespace \"{namespace}\""
            )));
        }
        self.namespaces
            .lock()
            .expect("settings")
            .insert(namespace.to_string());
        Ok(())
    }

    /// Merge `patch` into `namespace` and persist when the document changes.
    ///
    /// @param namespace Registered settings namespace.
    /// @param patch Object merged into the existing section.
    /// @returns The next revision after a write, or the current revision when unchanged.
    pub fn update(&self, namespace: &str, patch: &Value) -> Result<u64> {
        self.write_section(namespace, |current| merge_objects(current, patch))
    }

    /// Replace `namespace` with `next` and persist when the document changes.
    ///
    /// @param namespace Registered settings namespace.
    /// @param next Replacement section (must be a JSON object).
    /// @returns The next revision after a write, or the current revision when unchanged.
    pub fn replace(&self, namespace: &str, next: &Value) -> Result<u64> {
        if !next.is_object() {
            return Err(CordisError::Validation(
                "settings replace value must be an object".into(),
            ));
        }
        self.write_section(namespace, |_| next.clone())
    }

    fn write_section(
        &self,
        namespace: &str,
        compute_next: impl FnOnce(&Value) -> Value,
    ) -> Result<u64> {
        if !self
            .namespaces
            .lock()
            .expect("settings")
            .contains(namespace)
        {
            return Err(CordisError::Validation(format!(
                "settings namespace \"{namespace}\" is not registered"
            )));
        }
        self.refresh();
        let mut state = self.state.lock().expect("settings");
        let current = state
            .document
            .get(namespace)
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let next = compute_next(&current);
        if current == next {
            return Ok(*self.revision.lock().expect("settings"));
        }
        match &mut state.document {
            Value::Object(map) => {
                map.insert(namespace.to_string(), next.clone());
            }
            other => {
                let mut map = Map::new();
                map.insert(namespace.to_string(), next.clone());
                *other = Value::Object(map);
            }
        }
        let rendered = render_persist(&self.path, state.text.as_deref(), &state.document, namespace, &next)?;
        write_document_atomic(&self.path, &rendered)?;
        state.text = Some(rendered);
        drop(state);
        *self.last_probe.lock().expect("settings probe") = Some(Instant::now());
        let mut revision = self.revision.lock().expect("settings");
        *revision += 1;
        Ok(*revision)
    }
}

fn is_settings_namespace(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
        _ => false,
    }
}

fn merge_objects(current: &Value, patch: &Value) -> Value {
    match (current, patch) {
        (Value::Object(cur), Value::Object(p)) => {
            let mut out = cur.clone();
            for (k, v) in p {
                match (out.get(k), v) {
                    (Some(existing), Value::Object(_)) if existing.is_object() => {
                        out.insert(k.clone(), merge_objects(existing, v));
                    }
                    _ => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            Value::Object(out)
        }
        (_, patch) => patch.clone(),
    }
}

fn render_persist(
    path: &Path,
    source: Option<&str>,
    document: &Value,
    namespace: &str,
    next: &Value,
) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("yaml")
        .to_ascii_lowercase();
    if ext == "json" {
        let mut text = serde_json::to_string_pretty(document)
            .map_err(|e| CordisError::Validation(format!("settings JSON persist: {e}")))?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        return Ok(text);
    }
    Ok(yaml_patch::patch_namespace(source, namespace, next))
}

fn write_document_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CordisError::plugin(format!("settings persist mkdir: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("settings.yaml"),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, contents)
        .map_err(|e| CordisError::plugin(format!("settings persist write: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| CordisError::plugin(format!("settings persist rename: {e}")))?;
    Ok(())
}

/// Resolve the document path from plugin config.
///
/// # Errors
/// An extension other than `.yaml`, `.yml`, or `.json`.
pub fn resolve_path(config: Option<&Value>) -> Result<PathBuf> {
    let path = if let Some(path) = config
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
    {
        PathBuf::from(path)
    } else {
        let home = config
            .and_then(|value| value.get("dshHome"))
            .and_then(Value::as_str);
        resolve_dsh_home(home).join("settings.yaml")
    };
    assert_supported_extension(&path)?;
    Ok(path)
}

fn assert_supported_extension(path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "yaml" | "yml" | "json") {
        return Ok(());
    }
    Err(CordisError::Validation(format!(
        "settings-file: extension \"{}\" is not supported (use .yaml, .yml, or .json)",
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{ext}"))
            .unwrap_or_default()
    )))
}

fn resolve_watch(config: Option<&Value>) -> Result<(bool, u64)> {
    let watch = match config.and_then(|value| value.get("watch")) {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(CordisError::Validation(
                "settings-file: watch must be a boolean".into(),
            ))
        }
    };
    let debounce_ms = match config.and_then(|value| value.get("debounceMs")) {
        None => 100,
        Some(value) => value.as_u64().ok_or_else(|| {
            CordisError::Validation("settings-file: debounceMs must be an integer".into())
        })?,
    };
    Ok((watch, debounce_ms))
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
    let path = resolve_path(config)?;
    let (watch, debounce_ms) = resolve_watch(config)?;
    let text = if path.exists() {
        Some(
            std::fs::read_to_string(&path)
                .map_err(|error| CordisError::plugin(format!("settings-file: {error}")))?,
        )
    } else {
        None
    };
    let document = match &text {
        None => Value::Object(Map::new()),
        Some(text) => parse_settings_yaml(text)?,
    };
    ctx.provide(Arc::new(SettingsRuntime {
        path,
        watch,
        debounce_ms,
        last_probe: Mutex::new(None),
        state: Mutex::new(DocumentState { document, text }),
        namespaces: Mutex::new(HashSet::new()),
        revision: Mutex::new(0),
    }))
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
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://example.test"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_sequence_document() {
        assert!(parse_settings_yaml("- not a mapping\n").is_err());
    }

    #[test]
    fn rejects_unsupported_extension() {
        assert!(resolve_path(Some(&serde_json::json!({ "path": "/tmp/settings.txt" }))).is_err());
    }

    #[test]
    fn watched_document_reloads_after_external_edit() {
        let dir = stamp_dir("reload");
        let path = dir.join("settings.yaml");
        std::fs::write(&path, "llm-deepseek:\n  baseURL: https://first.test\n").unwrap();
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "path": path.to_string_lossy(),
                "debounceMs": 0
            })),
        )
        .unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://first.test"))
        );
        std::fs::write(&path, "llm-deepseek:\n  baseURL: https://second.test\n").unwrap();
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://second.test"))
        );
        std::fs::write(&path, "- not a mapping\n").unwrap();
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://second.test"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_false_keeps_the_mount_document() {
        let dir = stamp_dir("nowatch");
        let path = dir.join("settings.yaml");
        std::fs::write(&path, "llm-deepseek:\n  baseURL: https://first.test\n").unwrap();
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "path": path.to_string_lossy(),
                "watch": false
            })),
        )
        .unwrap();
        std::fs::write(&path, "llm-deepseek:\n  baseURL: https://second.test\n").unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://first.test"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_persists_a_registered_namespace_and_keeps_comments() {
        let dir = stamp_dir("persist");
        let path = dir.join("settings.yaml");
        std::fs::write(
            &path,
            "# personal settings\nllm-deepseek:\n  baseURL: https://first.test  # lab gateway\n",
        )
        .unwrap();
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "path": path.to_string_lossy(),
                "watch": false
            })),
        )
        .unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        assert!(settings.writable());
        let missing = settings
            .update("llm-deepseek", &serde_json::json!({ "baseURL": "https://second.test" }))
            .unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("settings namespace \"llm-deepseek\" is not registered"),
            "{missing}"
        );
        settings.register("llm-deepseek").unwrap();
        let revision = settings
            .update(
                "llm-deepseek",
                &serde_json::json!({ "baseURL": "https://second.test" }),
            )
            .unwrap();
        assert_eq!(revision, 1);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# personal settings"), "{written}");
        assert!(written.contains("# lab gateway"), "{written}");
        assert!(written.contains("https://second.test"), "{written}");
        assert_eq!(
            settings
                .section("llm-deepseek")
                .and_then(|value| value.get("baseURL").cloned()),
            Some(serde_json::json!("https://second.test"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_an_invalid_namespace_id() {
        let dir = stamp_dir("badns");
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "path": dir.join("settings.yaml").to_string_lossy(),
                "watch": false
            })),
        )
        .unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        let err = settings.register("LLM_DeepSeek").unwrap_err();
        assert!(err.to_string().contains("invalid settings namespace"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_document_persists_pretty() {
        let dir = stamp_dir("json");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "path": path.to_string_lossy(),
                "watch": false
            })),
        )
        .unwrap();
        let settings = ctx.service::<SettingsRuntime>().unwrap();
        settings.register("llm-deepseek").unwrap();
        settings
            .update(
                "llm-deepseek",
                &serde_json::json!({ "baseURL": "https://json.test" }),
            )
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"baseURL\": \"https://json.test\""), "{written}");
        assert!(written.ends_with('\n'), "{written:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
