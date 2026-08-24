//! Tool-independent `ctx.shellEnv` registry of trusted per-execution `DSH_*`
//! variables. Built-in shell facts stay owned by the registry; plugins register
//! additional enumerable facts with disposer-scoped ownership.

use dsh_cordis::{Context, CordisError, Result, Service};
use dsh_home_paths::resolve_dsh_home;
use dsh_session::session_id;
use dsh_session_persistence::{PersistenceRuntime, SessionLocation};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Prefix of every managed environment key.
pub const DSH_ENV_PREFIX: &str = "DSH_";
/// Harness home exposed to model shell calls.
pub const DSH_HOME_KEY: &str = "DSH_HOME";
/// Marker that this process is a DeepSeek Harness shell.
pub const DSH_SHELL_KEY: &str = "DSH_SHELL";
/// Calling agent's session id, when one is present.
pub const DSH_SESSION_ID_KEY: &str = "DSH_SESSION_ID";
/// Absolute JSONL transcript path when the persistence backend locates one.
pub const DSH_SESSION_JSONL_KEY: &str = "DSH_SESSION_JSONL";

const RESERVED_KEYS: &[&str] = &[DSH_HOME_KEY, DSH_SHELL_KEY, DSH_SESSION_ID_KEY];

/// One plugin contribution to the managed environment.
pub struct BashEnvContributor {
    /// Stable contributor name used in diagnostics and duplicate detection.
    pub name: String,
    /// Declared `DSH_*` keys this contributor may return, with descriptions.
    pub variables: BTreeMap<String, String>,
    /// Resolve available values for one tool execution's optional session id.
    pub resolve: Arc<dyn Fn(Option<&str>) -> BTreeMap<String, String> + Send + Sync>,
}

/// Enumerable declaration returned by [`ShellEnvRegistry::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashEnvVariableInfo {
    /// Contributor that owns the variable.
    pub contributor: String,
    /// Concise description of the environment fact.
    pub description: String,
    /// Declared `DSH_*` environment variable name.
    pub key: String,
}

struct RegistryState {
    contributors: BTreeMap<String, BashEnvContributor>,
    key_owners: BTreeMap<String, String>,
}

/// `ctx.shellEnv`.
pub struct ShellEnvRegistry {
    dsh_home: PathBuf,
    state: Arc<Mutex<RegistryState>>,
}

impl Service for ShellEnvRegistry {
    const KEY: &'static str = "shellEnv";
}

impl ShellEnvRegistry {
    /// Build a registry whose `DSH_HOME` is the resolved harness home.
    pub fn new(dsh_home: PathBuf) -> Self {
        Self {
            dsh_home,
            state: Arc::new(Mutex::new(RegistryState {
                contributors: BTreeMap::new(),
                key_owners: BTreeMap::new(),
            })),
        }
    }

    /// Register one environment contributor. Names and keys are unique;
    /// built-in keys are reserved.
    ///
    /// # Errors
    /// Empty name, duplicate name, invalid or reserved key, empty description,
    /// or a key already owned by another contributor.
    pub fn register(
        &self,
        contributor: BashEnvContributor,
    ) -> Result<Box<dyn FnOnce() + Send>> {
        let mut state = self.state.lock().expect("shellEnv");
        if contributor.name.trim().is_empty() {
            return Err(CordisError::Validation(
                "bash env contributor name must be non-empty".into(),
            ));
        }
        if state.contributors.contains_key(&contributor.name) {
            return Err(CordisError::Validation(format!(
                "bash env contributor \"{}\" is already registered",
                contributor.name
            )));
        }
        for (key, description) in &contributor.variables {
            if !is_managed_key(key) {
                return Err(CordisError::Validation(format!(
                    "bash env contributor \"{}\" declared invalid key \"{key}\"",
                    contributor.name
                )));
            }
            if RESERVED_KEYS.contains(&key.as_str()) {
                return Err(CordisError::Validation(format!(
                    "bash env contributor \"{}\" cannot own reserved key \"{key}\"",
                    contributor.name
                )));
            }
            if description.trim().is_empty() {
                return Err(CordisError::Validation(format!(
                    "bash env contributor \"{}\" must describe \"{key}\"",
                    contributor.name
                )));
            }
            if let Some(owner) = state.key_owners.get(key) {
                return Err(CordisError::Validation(format!(
                    "bash env key \"{key}\" is already owned by contributor \"{owner}\"; contributor \"{}\" cannot also own it",
                    contributor.name
                )));
            }
        }
        let name = contributor.name.clone();
        let keys: Vec<String> = contributor.variables.keys().cloned().collect();
        for key in &keys {
            state.key_owners.insert(key.clone(), name.clone());
        }
        state.contributors.insert(name.clone(), contributor);
        drop(state);
        let state = Arc::clone(&self.state);
        Ok(Box::new(move || {
            let mut state = state.lock().expect("shellEnv");
            state.contributors.remove(&name);
            for key in keys {
                state.key_owners.remove(&key);
            }
        }))
    }

    /// Build the trusted `DSH_*` snapshot for one shell tool execution.
    ///
    /// # Errors
    /// A contributor returned a key it did not declare.
    pub fn collect(&self, session_id: Option<&str>) -> Result<BTreeMap<String, String>> {
        let mut values = BTreeMap::new();
        values.insert(
            DSH_HOME_KEY.to_string(),
            self.dsh_home.to_string_lossy().into_owned(),
        );
        values.insert(DSH_SHELL_KEY.to_string(), "1".into());
        if let Some(session_id) = session_id {
            values.insert(DSH_SESSION_ID_KEY.to_string(), session_id.to_string());
        }
        let contributors = {
            let state = self.state.lock().expect("shellEnv");
            state
                .contributors
                .values()
                .map(|contributor| {
                    (
                        contributor.name.clone(),
                        contributor.variables.clone(),
                        Arc::clone(&contributor.resolve),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (name, variables, resolve) in contributors {
            let resolved = resolve(session_id);
            for (key, value) in resolved {
                if !variables.contains_key(&key) {
                    return Err(CordisError::Validation(format!(
                        "bash env contributor \"{name}\" returned undeclared key \"{key}\""
                    )));
                }
                values.insert(key, value);
            }
        }
        Ok(values)
    }

    /// Enumerate plugin-contributed variables without executing their resolvers.
    pub fn list(&self) -> Vec<BashEnvVariableInfo> {
        let state = self.state.lock().expect("shellEnv");
        let mut listed = state
            .contributors
            .values()
            .flat_map(|contributor| {
                contributor.variables.iter().map(|(key, description)| {
                    BashEnvVariableInfo {
                        contributor: contributor.name.clone(),
                        description: description.clone(),
                        key: key.clone(),
                    }
                })
            })
            .collect::<Vec<_>>();
        listed.sort_by(|left, right| left.key.cmp(&right.key));
        listed
    }
}

fn is_managed_key(key: &str) -> bool {
    let Some(suffix) = key.strip_prefix(DSH_ENV_PREFIX) else {
        return false;
    };
    let mut chars = suffix.chars();
    matches!(chars.next(), Some('A'..='Z'))
        && chars.all(|ch| matches!(ch, 'A'..='Z' | '0'..='9' | '_'))
}

/// Provide `ctx.shellEnv` and the session-persistence JSONL contributor.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let dsh_home = config
        .and_then(|value| value.get("dshHome"))
        .and_then(Value::as_str);
    let registry = Arc::new(ShellEnvRegistry::new(resolve_dsh_home(dsh_home)));
    let lookup = ctx.clone();
    registry.register(BashEnvContributor {
        name: "session-persistence".into(),
        variables: BTreeMap::from([(
            DSH_SESSION_JSONL_KEY.into(),
            "Absolute target path of the current session JSONL when the active persistence backend provides one.".into(),
        )]),
        resolve: Arc::new(move |session| {
            let Some(session) = session else {
                return BTreeMap::new();
            };
            let Some(persistence) = lookup.get::<PersistenceRuntime>() else {
                return BTreeMap::new();
            };
            match persistence.locate(&session_id(session)) {
                Some(SessionLocation::Jsonl { path }) => BTreeMap::from([(
                    DSH_SESSION_JSONL_KEY.to_string(),
                    path.to_string_lossy().into_owned(),
                )]),
                _ => BTreeMap::new(),
            }
        }),
    })?;
    ctx.provide(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn registry() -> ShellEnvRegistry {
        ShellEnvRegistry::new(PathBuf::from("/tmp/test-dsh-home"))
    }

    #[test]
    fn collects_unconditional_facts_and_session_id() {
        let registry = registry();
        let bare = registry.collect(None).unwrap();
        assert_eq!(bare.get(DSH_HOME_KEY).map(String::as_str), Some("/tmp/test-dsh-home"));
        assert_eq!(bare.get(DSH_SHELL_KEY).map(String::as_str), Some("1"));
        assert!(!bare.contains_key(DSH_SESSION_ID_KEY));
        let with_session = registry.collect(Some("session-a")).unwrap();
        assert_eq!(
            with_session.get(DSH_SESSION_ID_KEY).map(String::as_str),
            Some("session-a")
        );
    }

    #[test]
    fn collects_declared_contributor_variables() {
        let registry = registry();
        registry
            .register(BashEnvContributor {
                name: "always-available-fact".into(),
                variables: BTreeMap::from([(
                    "DSH_ALWAYS_AVAILABLE".into(),
                    "Always-available test fact.".into(),
                )]),
                resolve: Arc::new(|_| {
                    BTreeMap::from([("DSH_ALWAYS_AVAILABLE".into(), "yes".into())])
                }),
            })
            .unwrap();
        assert_eq!(
            registry
                .collect(None)
                .unwrap()
                .get("DSH_ALWAYS_AVAILABLE")
                .map(String::as_str),
            Some("yes")
        );
    }

    #[test]
    fn rejects_duplicate_and_reserved_keys() {
        let registry = registry();
        registry
            .register(BashEnvContributor {
                name: "first".into(),
                variables: BTreeMap::from([("DSH_SHARED".into(), "First owner.".into())]),
                resolve: Arc::new(|_| BTreeMap::new()),
            })
            .unwrap();
        let duplicate = registry.register(BashEnvContributor {
            name: "second".into(),
            variables: BTreeMap::from([("DSH_SHARED".into(), "Second owner.".into())]),
            resolve: Arc::new(|_| BTreeMap::new()),
        });
        assert!(duplicate.unwrap_err().to_string().contains("DSH_SHARED"));
        let reserved = registry.register(BashEnvContributor {
            name: "reserved-key".into(),
            variables: BTreeMap::from([("DSH_HOME".into(), "Reserved key.".into())]),
            resolve: Arc::new(|_| BTreeMap::new()),
        });
        assert!(reserved.unwrap_err().to_string().contains("reserved key"));
    }

    #[test]
    fn disposer_removes_the_contribution() {
        let registry = registry();
        let dispose = registry
            .register(BashEnvContributor {
                name: "temporary".into(),
                variables: BTreeMap::from([(
                    "DSH_TEMPORARY".into(),
                    "Temporary fact.".into(),
                )]),
                resolve: Arc::new(|_| BTreeMap::from([("DSH_TEMPORARY".into(), "present".into())])),
            })
            .unwrap();
        assert_eq!(
            registry.collect(None).unwrap().get("DSH_TEMPORARY").map(String::as_str),
            Some("present")
        );
        dispose();
        assert!(!registry.collect(None).unwrap().contains_key("DSH_TEMPORARY"));
    }
}
