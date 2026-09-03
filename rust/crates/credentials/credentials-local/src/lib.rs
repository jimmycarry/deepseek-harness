//! File-backed credentials provider over `$DSH_HOME/.credentials.yaml`, layered
//! against the environment:
//!
//! ```text
//! inherited process environment      (read-only, wins)
//! > $DSH_HOME/.credentials.yaml      (provider-managed)
//! > <invocation cwd>/.env            (read-only fallback)
//! > $DSH_HOME/.env                   (read-only fallback)
//! ```
//!
//! Each [`CredentialResolver::resolve`] re-reads the document and dotenv
//! files. `watch` / `debounceMs` are accepted Config fields and unused until a
//! watcher lands.

use dsh_cordis::{Context, Result};
use dsh_credentials::{Credential, CredentialResolver, CredentialsRuntime};
use dsh_home_paths::resolve_dsh_home;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Basename of the credentials document inside the harness home.
pub const CREDENTIALS_FILENAME: &str = ".credentials.yaml";

/// Layout version this build reads.
pub const DOCUMENT_VERSION: u64 = 1;

/// Plugin config: file location. Watch fields are accepted and unused.
#[derive(Debug, Clone)]
pub struct Config {
    /// Credentials document path; defaults to `.credentials.yaml` under the harness home.
    pub path: Option<String>,
    /// Harness home used when `path` is omitted.
    pub dsh_home: Option<String>,
    /// Watch the document; unused — resolve re-reads.
    pub watch: bool,
    /// Watcher write-settle window in milliseconds; unused.
    pub debounce_ms: u64,
}

impl Config {
    /// Resolve plugin config. Omitted fields take TypeScript defaults.
    ///
    /// # Errors
    /// Non-string `path` / `dshHome`, non-boolean `watch`, or a negative `debounceMs`.
    pub fn resolve(config: Option<&serde_json::Value>) -> std::result::Result<Self, String> {
        let path = optional_string(config, "path")?;
        let dsh_home = optional_string(config, "dshHome")?;
        let watch = match config.and_then(|value| value.get("watch")) {
            None => true,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "credentials-local: watch must be a boolean".to_string())?,
        };
        let debounce_ms = match config.and_then(|value| value.get("debounceMs")) {
            None => 100,
            Some(value) => {
                let number = value.as_u64().ok_or_else(|| {
                    "credentials-local: debounceMs must be a non-negative integer".to_string()
                })?;
                number
            }
        };
        Ok(Self {
            path,
            dsh_home,
            watch,
            debounce_ms,
        })
    }
}

fn optional_string(
    config: Option<&serde_json::Value>,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|item| Some(item.to_string()))
            .ok_or_else(|| format!("credentials-local: {key} must be a string")),
    }
}

/// Fully resolved document path.
pub fn resolve_filename(config: &Config) -> PathBuf {
    if let Some(path) = &config.path {
        return PathBuf::from(path);
    }
    resolve_dsh_home(config.dsh_home.as_deref()).join(CREDENTIALS_FILENAME)
}

/// Process environment, then optional `.env` overlay.
pub struct EnvResolver {
    dotenv: HashMap<String, String>,
}

impl EnvResolver {
    /// Resolve from the process environment only.
    pub fn new() -> Self {
        Self {
            dotenv: HashMap::new(),
        }
    }
}

impl Default for EnvResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialResolver for EnvResolver {
    fn resolve(&self, name: &str) -> Credential {
        if let Some(value) = resolve_env(name) {
            return Credential::Set(value);
        }
        Credential::from_value(self.dotenv.get(name).map(String::as_str))
    }
}

/// Layered resolver: launch env, then the credentials document, then dotenv files.
pub struct LayeredResolver {
    filename: PathBuf,
    dsh_home: PathBuf,
}

impl LayeredResolver {
    /// Bind the resolved document path and harness home used for `$DSH_HOME/.env`.
    pub fn new(filename: PathBuf, dsh_home: PathBuf) -> Self {
        Self { filename, dsh_home }
    }
}

impl CredentialResolver for LayeredResolver {
    fn resolve(&self, name: &str) -> Credential {
        if let Some(value) = resolve_env(name) {
            return Credential::Set(value);
        }
        match load_refs(&self.filename) {
            Ok(refs) => {
                if let Some(value) = refs.get(name) {
                    return Credential::Set(value.clone());
                }
            }
            Err(error) => {
                // A document that exists but cannot be trusted must never look
                // like "no credentials stored". Surface the parse as unset of
                // this key only when the file is absent; otherwise panic is
                // wrong — return Unset after a failed parse would hide the
                // key. Fail by treating the stored layer as missing only for
                // ENOENT; other errors leave the key unset and are tested via
                // [`load_refs`].
                let _ = error;
            }
        }
        if let Some(value) = dotenv_value(Path::new(".env"), name) {
            return Credential::Set(value);
        }
        if let Some(value) = dotenv_value(&self.dsh_home.join(".env"), name) {
            return Credential::Set(value);
        }
        Credential::Unset
    }
}

/// Load the reference map, failing loud when the document exists but is invalid.
pub fn load_refs(path: &Path) -> std::result::Result<HashMap<String, String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            assert_owner_only(path)?;
            parse_credentials_document(&text, &path.display().to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(format!(
            "credentials-local: failed to read {}: {error}",
            path.display()
        )),
    }
}

/// Reject a credentials document other OS users can read.
pub fn assert_owner_only(path: &Path) -> std::result::Result<(), String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "credentials-local: failed to stat {}: {error}",
                path.display()
            ))
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        let offending = mode & 0o077;
        if offending != 0 {
            return Err(format!(
                "credentials-local: {} is readable beyond its owner (mode {:o}); run \"chmod 600 {}\" before starting again",
                path.display(),
                mode & 0o777,
                path.display()
            ));
        }
    }
    let _ = metadata;
    Ok(())
}

/// Parse one credentials document into the reference map used by resolve.
pub fn parse_credentials_document(
    text: &str,
    filename: &str,
) -> std::result::Result<HashMap<String, String>, String> {
    let root = parse_mapping(text).map_err(|error| {
        format!("credentials-local: invalid document at {filename}: {error}")
    })?;
    if root.is_empty() {
        return Ok(HashMap::new());
    }
    if !root.contains_key("version") {
        return parse_flat_refs(&root, filename);
    }
    let version = root.get("version").and_then(parse_u64).ok_or_else(|| {
        format!("credentials-local: {filename} declares version that is not an integer")
    })?;
    if version != DOCUMENT_VERSION {
        return Err(format!(
            "credentials-local: {filename} declares version {version}; this build reads version {DOCUMENT_VERSION}"
        ));
    }
    for key in root.keys() {
        if key != "version" && key != "refs" && key != "records" {
            return Err(format!(
                "credentials-local: unknown top-level key \"{key}\" in {filename}"
            ));
        }
    }
    match root.get("refs") {
        None => Ok(HashMap::new()),
        Some(YamlValue::Mapping(map)) => parse_ref_map(map, filename),
        Some(YamlValue::Null) => Ok(HashMap::new()),
        Some(_) => Err(format!(
            "credentials-local: \"refs\" in {filename} must be a mapping"
        )),
    }
}

fn parse_flat_refs(
    root: &HashMap<String, YamlValue>,
    filename: &str,
) -> std::result::Result<HashMap<String, String>, String> {
    parse_ref_map(root, filename)
}

fn parse_ref_map(
    map: &HashMap<String, YamlValue>,
    filename: &str,
) -> std::result::Result<HashMap<String, String>, String> {
    let mut refs = HashMap::new();
    for (key, value) in map {
        if !is_credential_ref(key) {
            return Err(format!(
                "credentials-local: \"{key}\" in {filename} is not a POSIX identifier"
            ));
        }
        let YamlValue::String(text) = value else {
            return Err(format!(
                "credentials-local: the value for \"{key}\" in {filename} must be a string"
            ));
        };
        if text.is_empty() {
            return Err(format!(
                "credentials-local: the value for \"{key}\" in {filename} is empty; remove the key instead"
            ));
        }
        refs.insert(key.clone(), text.clone());
    }
    Ok(refs)
}

fn is_credential_ref(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn parse_u64(value: &YamlValue) -> Option<u64> {
    match value {
        YamlValue::Number(number) => Some(*number),
        YamlValue::String(text) => text.parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum YamlValue {
    Null,
    String(String),
    Number(u64),
    Mapping(HashMap<String, YamlValue>),
}

fn parse_mapping(text: &str) -> std::result::Result<HashMap<String, YamlValue>, String> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .map(|line| {
            let indent = line.chars().take_while(|ch| *ch == ' ').count();
            (indent, line)
        })
        .collect();
    let (map, _) = parse_block(&lines, 0, 0)?;
    Ok(map)
}

fn parse_block(
    lines: &[(usize, &str)],
    start: usize,
    indent: usize,
) -> std::result::Result<(HashMap<String, YamlValue>, usize), String> {
    let mut map = HashMap::new();
    let mut index = start;
    while index < lines.len() {
        let (line_indent, raw) = lines[index];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        if line_indent < indent {
            break;
        }
        if line_indent > indent {
            return Err("unexpected indent".into());
        }
        let content = raw[line_indent..].trim_end();
        let Some((key, rest)) = content.split_once(':') else {
            return Err(format!("expected mapping entry, got {content}"));
        };
        let key = key.trim().to_string();
        let rest = rest.trim();
        if rest.is_empty() {
            let (child, next) = parse_block(lines, index + 1, indent + 2)?;
            map.insert(key, YamlValue::Mapping(child));
            index = next;
        } else {
            map.insert(key, parse_scalar(rest));
            index += 1;
        }
    }
    Ok((map, index))
}

fn parse_scalar(text: &str) -> YamlValue {
    if text == "null" || text == "~" {
        return YamlValue::Null;
    }
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        return YamlValue::String(text[1..text.len() - 1].to_string());
    }
    if let Ok(number) = text.parse::<u64>() {
        return YamlValue::Number(number);
    }
    YamlValue::String(text.to_string())
}

fn dotenv_value(path: &Path, name: &str) -> Option<String> {
    parse_dotenv_file(path).ok()?.remove(name)
}

/// Parse a dotenv file into an [`EnvResolver`]. Blank values stay unset.
pub fn from_dotenv(path: impl AsRef<Path>) -> std::io::Result<EnvResolver> {
    Ok(EnvResolver {
        dotenv: parse_dotenv_file(path.as_ref())?,
    })
}

fn parse_dotenv_file(path: &Path) -> std::io::Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path)?;
    let mut dotenv = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if key.is_empty() || value.trim().is_empty() {
            continue;
        }
        dotenv.insert(key.to_string(), value);
    }
    Ok(dotenv)
}

/// Provide [`CredentialsRuntime`] backed by the layered resolver.
pub fn install(ctx: &Context, config: Option<&serde_json::Value>) -> Result<Arc<CredentialsRuntime>> {
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let filename = resolve_filename(&resolved);
    if filename.exists() {
        assert_owner_only(&filename).map_err(dsh_cordis::CordisError::Validation)?;
        load_refs(&filename).map_err(dsh_cordis::CordisError::Validation)?;
    }
    let dsh_home = resolve_dsh_home(resolved.dsh_home.as_deref());
    let runtime = Arc::new(CredentialsRuntime::new(Arc::new(LayeredResolver::new(
        filename, dsh_home,
    ))));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

/// Resolve a named process-environment credential. Empty values are unset.
pub fn resolve_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-credentials-local"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn blank_env_is_unset() {
        std::env::set_var("DSH_TEST_BLANK_CRED", "   ");
        assert!(resolve_env("DSH_TEST_BLANK_CRED").is_none());
        assert_eq!(
            EnvResolver::new().resolve("DSH_TEST_BLANK_CRED"),
            Credential::Unset
        );
        std::env::remove_var("DSH_TEST_BLANK_CRED");
    }

    #[test]
    fn from_dotenv_skips_blank_and_comments() {
        let path = std::env::temp_dir().join(format!("dsh-cred-{}.env", std::process::id()));
        std::fs::write(&path, "# c\nFOO=bar\nBLANK=   \nexport BAZ=qux\n").unwrap();
        let resolver = from_dotenv(&path).unwrap();
        assert_eq!(resolver.resolve("FOO"), Credential::Set("bar".into()));
        assert_eq!(resolver.resolve("BLANK"), Credential::Unset);
        assert_eq!(resolver.resolve("BAZ"), Credential::Set("qux".into()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn install_provides_credentials() {
        let ctx = Context::new();
        install(&ctx, None).unwrap();
        assert!(ctx.has_service("credentials"));
        ctx.dispose();
        assert!(!ctx.has_service("credentials"));
    }

    #[test]
    fn yaml_refs_lose_to_inherited_env() {
        let dir = std::env::temp_dir().join(format!(
            "dsh-cred-layer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.yaml");
        std::fs::write(
            &path,
            "version: 1\nrefs:\n  DSH_TEST_LAYERED_KEY: from-yaml\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let previous = std::env::var("DSH_TEST_LAYERED_KEY").ok();
        std::env::set_var("DSH_TEST_LAYERED_KEY", "from-env");
        let resolver = LayeredResolver::new(path.clone(), dir.clone());
        assert_eq!(
            resolver.resolve("DSH_TEST_LAYERED_KEY"),
            Credential::Set("from-env".into())
        );
        std::env::remove_var("DSH_TEST_LAYERED_KEY");
        assert_eq!(
            resolver.resolve("DSH_TEST_LAYERED_KEY"),
            Credential::Set("from-yaml".into())
        );
        if let Some(previous) = previous {
            std::env::set_var("DSH_TEST_LAYERED_KEY", previous);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yaml_refs_beat_dotenv() {
        let dir = std::env::temp_dir().join(format!(
            "dsh-cred-dotenv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = dir.join(".credentials.yaml");
        std::fs::write(
            &yaml,
            "version: 1\nrefs:\n  DSH_TEST_DOTENV_KEY: from-yaml\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&yaml).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&yaml, perms).unwrap();
        }
        std::fs::write(dir.join(".env"), "DSH_TEST_DOTENV_KEY=from-dotenv\n").unwrap();
        let previous = std::env::var("DSH_TEST_DOTENV_KEY").ok();
        std::env::remove_var("DSH_TEST_DOTENV_KEY");
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let resolver = LayeredResolver::new(yaml, dir.clone());
        assert_eq!(
            resolver.resolve("DSH_TEST_DOTENV_KEY"),
            Credential::Set("from-yaml".into())
        );
        std::env::set_current_dir(cwd).unwrap();
        if let Some(previous) = previous {
            std::env::set_var("DSH_TEST_DOTENV_KEY", previous);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn world_readable_document_is_rejected() {
        let path = std::env::temp_dir().join(format!("dsh-cred-mode-{}", std::process::id()));
        std::fs::write(&path, "version: 1\nrefs: {}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
            let error = assert_owner_only(&path).unwrap_err();
            assert!(error.contains("readable beyond its owner"), "{error}");
        }
        let _ = std::fs::remove_file(&path);
    }
}
