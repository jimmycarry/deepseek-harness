//! YAML composition for the Rust Cordis port.
//!
//! Layers apply to an empty entry list as one flattened `apply_entry_patches`
//! call: each bundle patch, then the profile patch, then the home-level one,
//! then any `--patch` overlay. A patch either inserts a list of rows or
//! replaces named fields of a row by id. `!!js` stays an unevaluated
//! `{ "__jsExpr" }` node so dump-config and boot share the same tree.

use dsh_cordis::{Context, CordisError, FnPlugin, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

mod eval;
mod yaml;

pub use eval::{eval_disabled, eval_js, eval_value, EvalHost};
pub use yaml::{as_js_expr, js_expr_value, parse_yaml_document};

/// One config row in a `cordis.yml` entry list.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Stable row id used by patches.
    pub id: Option<String>,
    /// Plugin name resolved through the loader registry (`@deepseek-ai/dsh-*`).
    pub name: String,
    /// Plugin config object. Replaced wholesale by a patch of the same id.
    pub config: Option<Value>,
    /// `true`, `false`, or a `!!js` node. Absent means enabled.
    pub disabled: Option<Value>,
    /// Service names that must exist on `ctx` before this row applies.
    pub inject: Vec<String>,
    /// When true, `config` is a child entry list.
    pub group: bool,
}

impl Entry {
    /// Named row with no config, inject, or disable flag.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            name: name.into(),
            config: None,
            disabled: None,
            inject: Vec::new(),
            group: false,
        }
    }

    /// `disabled: true` is the only skip-at-mount flag until `!!js` is evaluated.
    pub fn is_statically_disabled(&self) -> bool {
        matches!(self.disabled, Some(Value::Bool(true)))
    }
}

/// A patch against the composed entry list: insert rows or replace fields by id.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryPatch {
    /// Target row id. Required for replace; optional for a root insert list.
    pub id: Option<String>,
    /// Expected plugin name on a replace. A mismatch fails loud.
    pub name: Option<String>,
    /// Replacement config. Present means the whole object is replaced.
    pub config: Option<Value>,
    /// Replacement disable flag or `!!js` node.
    pub disabled: Option<Value>,
    /// Replacement inject list.
    pub inject: Option<Vec<String>>,
    /// Rows appended to the root when this patch is an insert.
    pub insert: Option<Vec<Entry>>,
}

impl EntryPatch {
    /// Append one row to the root.
    pub fn insert_row(entry: Entry) -> Self {
        Self {
            id: None,
            name: None,
            config: None,
            disabled: None,
            inject: None,
            insert: Some(vec![entry]),
        }
    }

    /// Replace fields of the row with this id.
    pub fn replace(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            name: None,
            config: None,
            disabled: None,
            inject: None,
            insert: None,
        }
    }
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
/// list can configure a row an earlier patch inserted. A missing target or a
/// name mismatch fails loud. Insert-into-group (`insert` plus `id`) is not
/// applied: headless composition never uses it.
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
        if let Some(insert) = &patch.insert {
            if let Some(id) = &patch.id {
                return Err(LoaderError::Parse(format!(
                    "patch insert into group `{id}` is not supported"
                )));
            }
            for entry in insert {
                if let Some(id) = &entry.id {
                    index.insert(id.clone(), entries.len());
                }
                entries.push(entry.clone());
            }
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
            if name != &entries[i].name {
                return Err(LoaderError::Parse(format!(
                    "patch name mismatch for `{id}`: expected {}, got {name}",
                    entries[i].name
                )));
            }
        }
        if let Some(config) = &patch.config {
            entries[i].config = Some(config.clone());
        }
        if let Some(disabled) = &patch.disabled {
            entries[i].disabled = Some(disabled.clone());
        }
        if let Some(inject) = &patch.inject {
            entries[i].inject = inject.clone();
        }
    }
    Ok(entries)
}

/// Parse a YAML entry list. A non-array document is invalid.
pub fn parse_entry_list(yaml: &str) -> std::result::Result<Vec<Entry>, LoaderError> {
    let value = parse_yaml_document(yaml)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(entry_from_value).collect(),
        _ => Err(LoaderError::Parse("entry list must be an array".into())),
    }
}

/// Parse a YAML patch list (`insert` lists and id-targeted replacements).
pub fn parse_patch_list(yaml: &str) -> std::result::Result<Vec<EntryPatch>, LoaderError> {
    let value = parse_yaml_document(yaml)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(patch_from_value).collect(),
        _ => Err(LoaderError::Parse("patch list must be an array".into())),
    }
}

fn entry_from_value(value: &Value) -> std::result::Result<Entry, LoaderError> {
    let object = mapping(value, "entry")?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LoaderError::Parse("entry requires name".into()))?
        .to_string();
    Ok(Entry {
        id: string_field(object, "id"),
        name,
        config: object.get("config").cloned(),
        disabled: object.get("disabled").cloned(),
        inject: string_list(object.get("inject"), "inject")?,
        group: object
            .get("group")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn patch_from_value(value: &Value) -> std::result::Result<EntryPatch, LoaderError> {
    let object = mapping(value, "patch")?;
    let insert = match object.get("insert") {
        None => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(entry_from_value)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(LoaderError::Parse("insert must be an array".into())),
    };
    Ok(EntryPatch {
        id: string_field(object, "id"),
        name: string_field(object, "name"),
        config: object.get("config").cloned(),
        disabled: object.get("disabled").cloned(),
        inject: match object.get("inject") {
            None => None,
            Some(value) => Some(string_list(Some(value), "inject")?),
        },
        insert,
    })
}

fn mapping<'a>(value: &'a Value, what: &str) -> std::result::Result<&'a Map<String, Value>, LoaderError> {
    value
        .as_object()
        .ok_or_else(|| LoaderError::Parse(format!("{what} must be a mapping")))
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

fn string_list(value: Option<&Value>, field: &str) -> std::result::Result<Vec<String>, LoaderError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| LoaderError::Parse(format!("{field} items must be strings")))
            })
            .collect(),
        Some(_) => Err(LoaderError::Parse(format!("{field} must be an array"))),
    }
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
        self.mount_with(ctx, entries, &EvalHost::from_process())
    }

    /// Mount with an explicit `!!js` host (tests and launchers).
    pub fn mount_with(
        &self,
        ctx: &Context,
        entries: &[Entry],
        host: &EvalHost,
    ) -> std::result::Result<(), LoaderError> {
        let factories = self.factories.lock().expect("factories").clone();
        for entry in entries {
            if eval_disabled(entry.disabled.as_ref(), host)? {
                continue;
            }
            let factory = factories
                .get(&entry.name)
                .ok_or_else(|| LoaderError::UnknownPlugin(entry.name.clone()))?;
            let config = match &entry.config {
                Some(value) => Some(eval_value(value, host)?),
                None => None,
            };
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

fn dump_entries(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "[]\n".into();
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str("- ");
        let mut first = true;
        if let Some(id) = &entry.id {
            write_field(&mut out, &mut first, 0, "id", &Value::String(id.clone()));
        }
        write_field(
            &mut out,
            &mut first,
            0,
            "name",
            &Value::String(entry.name.clone()),
        );
        if let Some(disabled) = &entry.disabled {
            if disabled != &Value::Bool(false) {
                write_field(&mut out, &mut first, 0, "disabled", disabled);
            }
        }
        if !entry.inject.is_empty() {
            let inject = Value::Array(
                entry
                    .inject
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            );
            write_field(&mut out, &mut first, 0, "inject", &inject);
        }
        if entry.group {
            write_field(&mut out, &mut first, 0, "group", &Value::Bool(true));
        }
        if let Some(config) = &entry.config {
            write_field(&mut out, &mut first, 0, "config", config);
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn write_field(out: &mut String, first: &mut bool, indent: usize, key: &str, value: &Value) {
    if *first {
        *first = false;
    } else {
        out.push_str(&" ".repeat(indent + 2));
    }
    out.push_str(key);
    out.push(':');
    dump_field_value(out, indent + 2, value);
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

fn dump_field_value(out: &mut String, indent: usize, value: &Value) {
    if let Some(expr) = as_js_expr(value) {
        out.push_str(" !!js ");
        out.push_str(&quote_js_expr(expr));
        return;
    }
    match value {
        Value::Null => out.push_str(" null"),
        Value::Bool(flag) => out.push_str(if *flag { " true" } else { " false" }),
        Value::Number(number) => {
            out.push(' ');
            out.push_str(&number.to_string());
        }
        Value::String(text) if text.contains('\n') => {
            out.push_str(" |\n");
            let body = text.strip_suffix('\n').unwrap_or(text);
            for line in body.split('\n') {
                out.push_str(&" ".repeat(indent));
                out.push_str(line);
                out.push('\n');
            }
        }
        Value::String(text) => {
            out.push(' ');
            out.push_str(&quote_plain(text));
        }
        Value::Array(items) if items.is_empty() => out.push_str(" []"),
        Value::Array(items) => {
            out.push('\n');
            for item in items {
                out.push_str(&" ".repeat(indent));
                out.push_str("- ");
                if let Some(expr) = as_js_expr(item) {
                    out.push_str("!!js ");
                    out.push_str(&quote_js_expr(expr));
                    out.push('\n');
                } else if let Some(object) = item.as_object() {
                    let mut first = true;
                    for (key, child) in object {
                        write_field(out, &mut first, indent, key, child);
                    }
                } else {
                    dump_inline_scalar(out, item);
                    out.push('\n');
                }
            }
        }
        Value::Object(map) if map.is_empty() => out.push_str(" {}"),
        Value::Object(map) => {
            out.push('\n');
            for (key, child) in map {
                out.push_str(&" ".repeat(indent));
                let mut first = true;
                write_field(out, &mut first, indent, key, child);
            }
        }
    }
}

fn dump_inline_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&quote_plain(text)),
        other => out.push_str(&other.to_string()),
    }
}

fn quote_plain(text: &str) -> String {
    if needs_quotes(text) {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        text.to_string()
    }
}

fn quote_js_expr(expr: &str) -> String {
    if expr.contains(": ")
        || expr.contains(" #")
        || expr.starts_with(|ch: char| "[{&*!|>%@`".contains(ch))
    {
        format!("\"{}\"", expr.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        expr.to_string()
    }
}

fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text == "true"
        || text == "false"
        || text == "null"
        || text == "~"
        || text.parse::<i64>().is_ok()
        || text.contains(": ")
        || text.contains(" #")
        || text.starts_with(|ch: char| {
            matches!(
                ch,
                ' ' | '\''
                    | '"'
                    | '&'
                    | '*'
                    | '!'
                    | '%'
                    | '@'
                    | '`'
                    | '|'
                    | '>'
                    | '{'
                    | '['
                    | '?'
                    | ':'
                    | '#'
                    | '-'
            )
        })
}

/// Compose an empty list with ordered patch layers as one flattened apply.
pub fn compose_layers(layers: &[Vec<EntryPatch>]) -> std::result::Result<Vec<Entry>, LoaderError> {
    let patches: Vec<EntryPatch> = layers.iter().flatten().cloned().collect();
    apply_entry_patches(Vec::new(), &patches)
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
            EntryPatch::insert_row({
                let mut entry = Entry::new("ping", "ping");
                entry.config = Some(serde_json::json!({"v": 1}));
                entry
            }),
            {
                let mut patch = EntryPatch::replace("ping");
                patch.config = Some(serde_json::json!({"v": 2}));
                patch
            },
        ];
        let entries = apply_entry_patches(Vec::new(), &patches).unwrap();
        assert_eq!(entries[0].config, Some(serde_json::json!({"v": 2})));
    }

    #[test]
    fn missing_patch_target_fails_loud() {
        let err = apply_entry_patches(Vec::new(), &[EntryPatch::replace("missing")]).unwrap_err();
        assert!(matches!(err, LoaderError::MissingTarget(id) if id == "missing"));
    }

    #[test]
    fn mount_unknown_plugin_fails_loud() {
        let loader = Loader::new();
        let ctx = Context::new();
        let err = loader
            .mount(&ctx, &[Entry::new("x", "nope")])
            .unwrap_err();
        assert!(matches!(err, LoaderError::UnknownPlugin(name) if name == "nope"));
    }

    #[test]
    fn dump_config_prints_composed_tree() {
        let entries = compose_layers(&[vec![EntryPatch::insert_row(Entry::new(
            "ping",
            "dsh-ping",
        ))]])
        .unwrap();
        let dump = Loader::dump_config(&entries);
        assert!(dump.contains("dsh-ping"));
        assert!(dump.contains("id: ping"));
    }

    #[test]
    fn disabled_js_skips_win32_only_row_on_linux() {
        let loader = Loader::new();
        loader.register("win-only", |_ctx, _| {
            panic!("disabled row must not apply");
        });
        let mut entry = Entry::new("pwsh", "win-only");
        entry.disabled = Some(js_expr_value("process.platform !== 'win32'"));
        let mut host = EvalHost::from_process();
        host.platform = "linux".into();
        let ctx = Context::new();
        loader.mount_with(&ctx, &[entry], &host).unwrap();
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

    #[test]
    fn insert_list_then_replace_keeps_later_config() {
        let patches = parse_patch_list(
            r#"
- insert:
    - id: tools
      name: '@deepseek-ai/dsh-tools'
      config:
        mode: native
- id: tools
  config:
    mode: !!js process.env.DSH_TOOLS_MODE
"#,
        )
        .unwrap();
        let entries = apply_entry_patches(Vec::new(), &patches).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "@deepseek-ai/dsh-tools");
        assert_eq!(
            as_js_expr(entries[0].config.as_ref().unwrap().get("mode").unwrap()).unwrap(),
            "process.env.DSH_TOOLS_MODE"
        );
    }

    #[test]
    fn dump_prints_js_expression_verbatim() {
        let mut entry = Entry::new("tools", "@deepseek-ai/dsh-tools");
        entry.config = Some(serde_json::json!({
            "mode": { "__jsExpr": "process.env.DSH_TOOLS_MODE" }
        }));
        let dump = Loader::dump_config(&[entry]);
        assert!(dump.contains("!!js process.env.DSH_TOOLS_MODE"));
        assert!(!dump.contains("__jsExpr"));
    }

    #[test]
    fn dump_prints_inject_and_plugin_package_name() {
        let mut entry = Entry::new("headless-runner", "@deepseek-ai/dsh-headless");
        entry.inject = vec!["headlessStartup".into()];
        let dump = Loader::dump_config(&[entry]);
        assert!(dump.contains("name: '@deepseek-ai/dsh-headless'"));
        assert!(dump.contains("headlessStartup"));
    }
}
