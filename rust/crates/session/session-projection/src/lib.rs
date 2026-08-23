//! Session projection registry (`ctx.sessionProjections`).
//!
//! Domain plugins register pure fold units. `snapshot` folds the in-memory
//! log of one session over every registered unit. Identity for `list_agents`
//! comes from the `subagent` unit, not from ad-hoc descriptor scans.

use dsh_cordis::{Context, Service};
use dsh_session::{event_type_name, Session, SessionEvent};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One registered fold unit.
#[derive(Clone)]
pub struct ProjectionUnit {
    /// Client-visible key (`subagent`, `goal`, `todos`, …).
    pub key: String,
    /// Non-negative schema generation; a mismatch refuses to share the key.
    pub state_version: u32,
    /// Initial fold state.
    pub init: Arc<dyn Fn() -> Value + Send + Sync>,
    /// Apply one committed event. Return the same value when nothing changed.
    pub apply: Arc<dyn Fn(&Value, &SessionEvent) -> Value + Send + Sync>,
    /// Project host state to the client-visible view (`null` when empty).
    pub view: Arc<dyn Fn(&Value) -> Value + Send + Sync>,
}

/// One consistent cut over every registered unit.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionSnapshot {
    /// Last folded event seq, or `-1` as `None` when the log is empty.
    pub as_of_seq: Option<u64>,
    /// Client-visible views keyed by unit name.
    pub values: HashMap<String, Value>,
}

/// `ctx.sessionProjections`.
pub struct SessionProjectionRegistry {
    units: Mutex<HashMap<String, ProjectionUnit>>,
}

impl SessionProjectionRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self {
            units: Mutex::new(HashMap::new()),
        }
    }

    /// Install as `ctx.sessionProjections`.
    ///
    /// # Errors
    /// Service already provided.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<Self>> {
        let registry = Arc::new(Self::new());
        ctx.provide(Arc::clone(&registry))?;
        Ok(registry)
    }

    /// Register one unit. Duplicate keys must share `state_version`.
    ///
    /// # Errors
    /// Same key already registered at a different `state_version`.
    pub fn register(&self, unit: ProjectionUnit) -> Result<(), String> {
        let mut units = self.units.lock().expect("sessionProjections");
        if let Some(existing) = units.get(&unit.key) {
            if existing.state_version != unit.state_version {
                return Err(format!(
                    "session projection key \"{}\" is already registered at stateVersion {}; refusing to share it with stateVersion {}",
                    unit.key, existing.state_version, unit.state_version
                ));
            }
            return Ok(());
        }
        units.insert(unit.key.clone(), unit);
        Ok(())
    }

    /// Fold `session`'s log over every registered unit.
    pub fn snapshot(&self, session: &Session) -> ProjectionSnapshot {
        let events = session.events();
        let as_of_seq = events.last().map(|event| event.seq);
        let units = self.units.lock().expect("sessionProjections");
        let mut values = HashMap::new();
        for unit in units.values() {
            let mut state = (unit.init)();
            for event in &events {
                state = (unit.apply)(&state, event);
            }
            values.insert(unit.key.clone(), (unit.view)(&state));
        }
        ProjectionSnapshot { as_of_seq, values }
    }

    /// Host fold state for one key, or `None` when the key is unregistered.
    pub fn state_of(&self, session: &Session, key: &str) -> Option<Value> {
        let units = self.units.lock().expect("sessionProjections");
        let unit = units.get(key)?;
        let mut state = (unit.init)();
        for event in session.events() {
            state = (unit.apply)(&state, &event);
        }
        Some(state)
    }
}

impl Default for SessionProjectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Service for SessionProjectionRegistry {
    const KEY: &'static str = "sessionProjections";
}

/// Last-wins `subagent` identity from `subagent/descriptor` events.
///
/// A malformed or unknown-version payload resets to JSON `null` so a later
/// healthy descriptor can replace it. `stateVersion` is 2, matching TypeScript.
pub fn subagent_identity_unit() -> ProjectionUnit {
    ProjectionUnit {
        key: "subagent".into(),
        state_version: 2,
        init: Arc::new(|| Value::Null),
        apply: Arc::new(|state, event| {
            if event_type_name(&event.data) != "subagent/descriptor" {
                return state.clone();
            }
            let SessionEvent {
                data: dsh_session::SessionEventData::Extension { data, .. },
                seq,
                ..
            } = event
            else {
                return Value::Null;
            };
            if data.get("version").and_then(Value::as_u64) != Some(2) {
                return Value::Null;
            }
            let Some(mode) = data.get("mode").and_then(Value::as_str) else {
                return Value::Null;
            };
            let mut identity = serde_json::Map::new();
            identity.insert("mode".into(), Value::String(mode.to_string()));
            identity.insert("seq".into(), Value::from(*seq));
            if let Some(label) = data.get("label").and_then(Value::as_str) {
                identity.insert("label".into(), Value::String(label.to_string()));
            } else if mode == "continuable" {
                return Value::Null;
            }
            Value::Object(identity)
        }),
        view: Arc::new(|state| state.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionEventData};

    #[test]
    fn provide_and_snapshot_subagent_identity() {
        let ctx = Context::new();
        let registry = SessionProjectionRegistry::install(&ctx).unwrap();
        registry.register(subagent_identity_unit()).unwrap();
        assert!(ctx.has_service("sessionProjections"));
        let session = Session::new(session_id("child"));
        session
            .append(
                SessionEventData::Extension {
                    type_name: "subagent/descriptor".into(),
                    data: serde_json::json!({
                        "version": 2,
                        "mode": "continuable",
                        "provider": "spawn",
                        "label": "echo probe"
                    }),
                },
                None,
            )
            .unwrap();
        let snap = registry.snapshot(&session);
        assert_eq!(snap.values["subagent"]["mode"], "continuable");
        assert_eq!(snap.values["subagent"]["label"], "echo probe");
        let wire = serde_json::to_value(&session.events()[0]).unwrap();
        assert_eq!(wire["data"]["mode"], "continuable");
        ctx.dispose();
        assert!(!ctx.has_service("sessionProjections"));
    }

    #[test]
    fn mismatched_state_version_fails_loud() {
        let registry = SessionProjectionRegistry::new();
        registry.register(subagent_identity_unit()).unwrap();
        let mut other = subagent_identity_unit();
        other.state_version = 1;
        let error = registry.register(other).unwrap_err();
        assert!(error.contains("stateVersion 2"));
    }
}
