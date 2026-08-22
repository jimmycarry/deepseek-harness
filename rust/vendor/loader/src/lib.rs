//! YAML composition for the Rust Cordis port.
//!
//! Layers apply to an empty entry list: each bundle patch, then the profile
//! patch, then the home-level one, then any `--patch` overlay. A patch targets
//! a row by id and replaces its whole config, or inserts new rows.

use dsh_cordis::{Context, CordisError, FnPlugin, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// One config row in a `cordis.yml` entry list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Stable row id used by patches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Plugin name resolved through the loader registry.
    pub name: String,
    /// Plugin config object. Replaced wholesale by a patch of the same id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// When true, the row is present but not mounted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A patch against one entry id: replace config, insert, or disable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntryPatch {
    /// Target row id. Required for replace; optional for insert-at-end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Plugin name when inserting a new row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replacement config. `null` clears config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// When set, overrides the row's disabled flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// When true, insert this row instead of replacing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub insert: bool,
}

/// Errors from composing or loading an entry tree.
#[derive(Debug, Error)]
pub enum LoaderError {
    /// YAML or JSON parse failure.
    #[error("invalid entry list: {0}")]
    Parse(String),
    /// A patch named a row that is not in the tree.
    #[error("patch target `{0}` is not in the entry list")]
    MissingTarget(String),
    /// Plugin name is not in the loader registry.
    #[error("unknown plugin `{0}`")]
    UnknownPlugin(String),
    /// Cordis mount failure.
    #[error(transparent)]
    Cordis(#[from] CordisError),
}

/// Apply patches to a clone of `entries` and return the composed list.
///
/// Inserted rows are indexed as they are added, so a later patch in the same
/// list can configure a row an earlier patch inserted.
pub fn apply_entry_patches(
    mut entries: Vec<Entry>,
    patches: &[EntryPatch],
) -> std::result::Result<Vec<Entry>, LoaderError> {
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(id) = &entry.id {
            index.insert(id.clone(), i);
        }
    }
    for patch in patches {
        if patch.insert {
            let name = patch
                .name
                .clone()
                .ok_or_else(|| LoaderError::Parse("insert patch requires name".into()))?;
            let entry = Entry {
                id: patch.id.clone(),
                name,
                config: patch.config.clone(),
                disabled: patch.disabled.unwrap_or(false),
            };
            if let Some(id) = &entry.id {
                index.insert(id.clone(), entries.len());
            }
            entries.push(entry);
            continue;
        }
        let id = patch
            .id
            .clone()
            .ok_or_else(|| LoaderError::Parse("replace patch requires id".into()))?;
        let Some(&i) = index.get(&id) else {
            return Err(LoaderError::MissingTarget(id));
        };
        if let Some(name) = &patch.name {
            entries[i].name = name.clone();
        }
        if patch.config.is_some() {
            entries[i].config = patch.config.clone();
        }
        if let Some(disabled) = patch.disabled {
            entries[i].disabled = disabled;
        }
    }
    Ok(entries)
}

/// Parse a YAML entry list. A non-array document is invalid.
pub fn parse_entry_list(yaml: &str) -> std::result::Result<Vec<Entry>, LoaderError> {
    let value: Value = parse_yaml_value(yaml)?;
    if !value.is_array() && value != Value::Null {
        return Err(LoaderError::Parse("entry list must be an array".into()));
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(value).map_err(|error| LoaderError::Parse(error.to_string()))
}

/// Factory that mounts a named plugin onto a context.
pub type PluginFactory = Arc<dyn Fn(&Context, Option<Value>) -> Result<()> + Send + Sync>;

/// Resolves plugin names to factories and mounts a composed tree.
#[derive(Default)]
pub struct Loader {
    factories: Mutex<HashMap<String, PluginFactory>>,
}

impl Loader {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory under the plugin name used in YAML.
    pub fn register<F>(&self, name: impl Into<String>, factory: F)
    where
        F: Fn(&Context, Option<Value>) -> Result<()> + Send + Sync + 'static,
    {
        self.factories
            .lock()
            .expect("factories")
            .insert(name.into(), Arc::new(factory));
    }

    /// Register a function plugin that ignores config.
    pub fn register_fn<P>(&self, name: &'static str, plugin: P)
    where
        P: Fn(&Context) -> Result<()> + Send + Sync + Clone + 'static,
    {
        self.register(name, move |ctx, _config| plugin(ctx));
    }

    /// Mount every enabled entry. Unknown names fail loud.
    pub fn mount(&self, ctx: &Context, entries: &[Entry]) -> std::result::Result<(), LoaderError> {
        let factories = self.factories.lock().expect("factories").clone();
        for entry in entries {
            if entry.disabled {
                continue;
            }
            let factory = factories
                .get(&entry.name)
                .ok_or_else(|| LoaderError::UnknownPlugin(entry.name.clone()))?;
            let config = entry.config.clone();
            let factory = Arc::clone(factory);
            ctx.plugin(FnPlugin::new("loader-row", move |child| {
                factory(child, config.clone())
            }))?;
        }
        Ok(())
    }

    /// Render the composed tree exactly as Include would mount it.
    pub fn dump_config(entries: &[Entry]) -> String {
        dump_entries(entries)
    }
}

/// Parse a YAML subset used by cordis.yml entry lists into JSON.
fn parse_yaml_value(yaml: &str) -> std::result::Result<Value, LoaderError> {
    if yaml.trim().is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    if yaml.trim_start().starts_with('{') || yaml.trim_start().starts_with('[') {
        return serde_json::from_str(yaml).map_err(|error| LoaderError::Parse(error.to_string()));
    }
    // Mapping document without a list is invalid (matches Include).
    let trimmed = yaml.trim();
    if !trimmed.starts_with('-') && trimmed.contains(':') && !trimmed.contains("\n-") {
        return Err(LoaderError::Parse("entry list must be an array".into()));
    }
    let mut items: Vec<Value> = Vec::new();
    let mut current = serde_json::Map::new();
    for raw in yaml.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim().starts_with('#') {
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("- ") {
            if !current.is_empty() {
                items.push(Value::Object(std::mem::take(&mut current)));
            }
            if let Some((key, value)) = rest.split_once(':') {
                current.insert(key.trim().to_string(), yaml_scalar(value.trim()));
            }
        } else if let Some((key, value)) = line.trim().split_once(':') {
            current.insert(key.trim().to_string(), yaml_scalar(value.trim()));
        } else {
            return Err(LoaderError::Parse(format!("invalid row: {line}")));
        }
    }
    if !current.is_empty() {
        items.push(Value::Object(current));
    }
    Ok(Value::Array(items))
}

fn yaml_scalar(value: &str) -> Value {
    if value.is_empty() {
        return Value::Null;
    }
    if value == "true" {
        return Value::Bool(true);
    }
    if value == "false" {
        return Value::Bool(false);
    }
    if let Ok(number) = value.parse::<i64>() {
        return Value::Number(number.into());
    }
    Value::String(value.trim_matches('"').to_string())
}

fn dump_entries(entries: &[Entry]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str("- ");
        if let Some(id) = &entry.id {
            out.push_str(&format!("id: {id}\n"));
        } else {
            out.push_str(&format!("name: {}\n", entry.name));
            continue;
        }
        out.push_str(&format!("  name: {}\n", entry.name));
        if entry.disabled {
            out.push_str("  disabled: true\n");
        }
    }
    if out.is_empty() {
        out.push_str("[]\n");
    }
    out
}

/// Compose an empty list with ordered patch layers (bundle, profile, home, overlay).
pub fn compose_layers(layers: &[Vec<EntryPatch>]) -> std::result::Result<Vec<Entry>, LoaderError> {
    let mut entries = Vec::new();
    for layer in layers {
        entries = apply_entry_patches(entries, layer)?;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Service;
    use std::sync::Arc;

    struct Ping;
    impl Service for Ping {
        const KEY: &'static str = "ping";
    }

    #[test]
    fn non_array_parse_is_invalid() {
        let err = parse_entry_list("name: nope\n").unwrap_err();
        assert!(matches!(err, LoaderError::Parse(_)));
    }

    #[test]
    fn later_patch_can_configure_an_inserted_row() {
        let patches = vec![
            EntryPatch {
                id: Some("ping".into()),
                name: Some("ping".into()),
                config: Some(serde_json::json!({"v": 1})),
                disabled: None,
                insert: true,
            },
            EntryPatch {
                id: Some("ping".into()),
                name: None,
                config: Some(serde_json::json!({"v": 2})),
                disabled: None,
                insert: false,
            },
        ];
        let entries = apply_entry_patches(Vec::new(), &patches).unwrap();
        assert_eq!(entries[0].config, Some(serde_json::json!({"v": 2})));
    }

    #[test]
    fn missing_patch_target_fails_loud() {
        let err = apply_entry_patches(
            Vec::new(),
            &[EntryPatch {
                id: Some("missing".into()),
                name: None,
                config: None,
                disabled: None,
                insert: false,
            }],
        )
        .unwrap_err();
        assert!(matches!(err, LoaderError::MissingTarget(id) if id == "missing"));
    }

    #[test]
    fn mount_unknown_plugin_fails_loud() {
        let loader = Loader::new();
        let ctx = Context::new();
        let err = loader
            .mount(
                &ctx,
                &[Entry {
                    id: Some("x".into()),
                    name: "nope".into(),
                    config: None,
                    disabled: false,
                }],
            )
            .unwrap_err();
        assert!(matches!(err, LoaderError::UnknownPlugin(name) if name == "nope"));
    }

    #[test]
    fn dump_config_prints_composed_tree() {
        let entries = compose_layers(&[vec![EntryPatch {
            id: Some("ping".into()),
            name: Some("dsh-ping".into()),
            config: None,
            disabled: None,
            insert: true,
        }]])
        .unwrap();
        let dump = Loader::dump_config(&entries);
        assert!(dump.contains("dsh-ping"));
        assert!(dump.contains("id: ping"));
    }

    #[test]
    fn yaml_mounts_provider_then_consumer() {
        let loader = Loader::new();
        loader.register("dsh-ping", |ctx, _| ctx.provide(Arc::new(Ping)));
        loader.register("dsh-pong", |ctx, _| {
            assert!(ctx.has_service("ping"));
            Ok(())
        });
        let yaml = r#"
- id: ping
  name: dsh-ping
- id: pong
  name: dsh-pong
"#;
        let entries = parse_entry_list(yaml).unwrap();
        let ctx = Context::new();
        loader.mount(&ctx, &entries).unwrap();
        assert!(ctx.has_service("ping"));
    }
}
