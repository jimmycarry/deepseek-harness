//! Event-only filesystem observation policy; it registers no service.
//!
//! A per-owner map records every authoritative presence/absence observation.
//! `fs/write-intent` and `fs/edit-intent` occupy the single decision slot and
//! do not call `next()`. Without this plugin, tools keep the bare provider's
//! unconditional mutation behavior.

use dsh_cordis::{Context, Result};
use dsh_fs::{
    error_to_event, fs_event_payload, FsError, FsObservation, FsObservationActor, FsTarget,
    FsWriteIntent, FS_EDIT_INTENT, FS_OBSERVED, FS_WRITE_INTENT,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "fs-observation-policy"
}

/// Register the three `fs/*` listeners. Reads no services.
pub fn install(ctx: &Context) -> Result<()> {
    let gate = Arc::new(ObservedStateGate::default());
    let observed = Arc::clone(&gate);
    ctx.on(FS_OBSERVED, move |payload| {
        observed.observe_payload(&payload);
    })?;
    let writes = Arc::clone(&gate);
    ctx.on_waterfall(FS_WRITE_INTENT, move |payload, _next| {
        writes.write_intent_payload(&payload)
    })?;
    let edits = Arc::clone(&gate);
    ctx.on_waterfall(FS_EDIT_INTENT, move |payload, _next| {
        edits.edit_intent_payload(&payload)
    })?;
    let cleared = Arc::clone(&gate);
    ctx.effect("fs-observation-policy observed-state teardown", move || {
        move || {
            cleared.clear();
        }
    })?;
    Ok(())
}

#[derive(Default)]
struct ObservedStateGate {
    observed: Mutex<HashMap<String, HashMap<String, FsObservation>>>,
}

impl ObservedStateGate {
    fn clear(&self) {
        self.observed.lock().expect("fs-observation").clear();
    }

    fn get(&self, owner: &str, target_key: &str) -> Option<FsObservation> {
        self.observed
            .lock()
            .expect("fs-observation")
            .get(owner)
            .and_then(|by_target| by_target.get(target_key).cloned())
    }

    fn set(&self, owner: &str, target_key: &str, observation: FsObservation) {
        self.observed
            .lock()
            .expect("fs-observation")
            .entry(owner.to_string())
            .or_default()
            .insert(target_key.to_string(), observation);
    }

    fn write_intent(&self, target: &FsTarget, actor: &FsObservationActor) -> FsWriteIntent {
        let prior = actor
            .owner()
            .and_then(|owner| self.get(owner, &target.target_key));
        match prior {
            Some(FsObservation::Present { version }) => FsWriteIntent::ReplaceIfVersion { version },
            Some(FsObservation::Absent) | None => FsWriteIntent::CreateIfAbsent,
        }
    }

    fn edit_intent(
        &self,
        target: &FsTarget,
        actor: &FsObservationActor,
    ) -> std::result::Result<String, FsError> {
        let prior = actor
            .owner()
            .and_then(|owner| self.get(owner, &target.target_key));
        match prior {
            None => Err(FsError::not_observed(format!(
                "edit requires reading \"{}\" first",
                target.display_path
            ))),
            Some(FsObservation::Absent) => Err(FsError::not_found(format!(
                "cannot edit \"{}\": not found",
                target.display_path
            ))),
            Some(FsObservation::Present { version }) => Ok(version),
        }
    }

    fn observe(&self, target: &FsTarget, observation: FsObservation, actor: &FsObservationActor) {
        if let Some(owner) = actor.owner() {
            self.set(owner, &target.target_key, observation);
        }
    }

    fn write_intent_payload(&self, payload: &Value) -> Value {
        let Some(target) = payload.get("target").and_then(FsTarget::from_value) else {
            return json!(null);
        };
        let actor = FsObservationActor::from_value(payload.get("actor"));
        self.write_intent(&target, &actor).to_value()
    }

    fn edit_intent_payload(&self, payload: &Value) -> Value {
        let Some(target) = payload.get("target").and_then(FsTarget::from_value) else {
            return json!(null);
        };
        let actor = FsObservationActor::from_value(payload.get("actor"));
        match self.edit_intent(&target, &actor) {
            Ok(version) => json!({ "version": version }),
            Err(error) => error_to_event(&error),
        }
    }

    fn observe_payload(&self, payload: &Value) {
        let Some(target) = payload.get("target").and_then(FsTarget::from_value) else {
            return;
        };
        let Some(observation) = payload
            .get("observation")
            .and_then(FsObservation::from_value)
        else {
            return;
        };
        let actor = FsObservationActor::from_value(payload.get("actor"));
        self.observe(&target, observation, &actor);
    }
}

/// Test helper: dispatch `fs/write-intent` with the bare default thunk.
pub fn write_intent(
    ctx: &Context,
    target: &FsTarget,
    actor: &FsObservationActor,
) -> Option<FsWriteIntent> {
    let payload = fs_event_payload(target, actor, None);
    ctx.waterfall(FS_WRITE_INTENT, payload, |_| json!(null))
        .ok()
        .and_then(|value| FsWriteIntent::from_value(&value))
}

/// Test helper: dispatch `fs/edit-intent`.
pub fn edit_intent(
    ctx: &Context,
    target: &FsTarget,
    actor: &FsObservationActor,
) -> std::result::Result<Option<String>, FsError> {
    let payload = fs_event_payload(target, actor, None);
    let value = ctx
        .waterfall(FS_EDIT_INTENT, payload, |_| json!(null))
        .map_err(|error| FsError::Io(error.to_string()))?;
    if let Some(error) = dsh_fs::error_from_event(&value) {
        return Err(error);
    }
    Ok(value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Test helper: emit `fs/observed`.
pub fn observe(
    ctx: &Context,
    target: &FsTarget,
    observation: FsObservation,
    actor: &FsObservationActor,
) {
    ctx.emit(
        FS_OBSERVED,
        fs_event_payload(target, actor, Some(&observation)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> FsTarget {
        FsTarget::new(path, path)
    }

    fn owner(id: &str) -> FsObservationActor {
        FsObservationActor::from_agent_id(Some(id))
    }

    fn setup() -> Context {
        let ctx = Context::new();
        install(&ctx).unwrap();
        ctx
    }

    #[test]
    fn registers_no_service() {
        let ctx = setup();
        assert!(!ctx.has_service("fsPolicy"));
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &FsObservationActor::from_agent_id(None)),
            Some(FsWriteIntent::CreateIfAbsent)
        );
    }

    #[test]
    fn unobserved_and_no_owner_write_create_if_absent() {
        let ctx = setup();
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &owner("a")),
            Some(FsWriteIntent::CreateIfAbsent)
        );
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &FsObservationActor::from_agent_id(None)),
            Some(FsWriteIntent::CreateIfAbsent)
        );
    }

    #[test]
    fn present_observation_authorizes_replace() {
        let ctx = setup();
        let actor = owner("a");
        observe(
            &ctx,
            &target("a.txt"),
            FsObservation::Present {
                version: "v7".into(),
            },
            &actor,
        );
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &actor),
            Some(FsWriteIntent::ReplaceIfVersion {
                version: "v7".into()
            })
        );
    }

    #[test]
    fn absent_observation_keeps_create() {
        let ctx = setup();
        let actor = owner("a");
        observe(&ctx, &target("a.txt"), FsObservation::Absent, &actor);
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &actor),
            Some(FsWriteIntent::CreateIfAbsent)
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &actor)
                .unwrap_err()
                .code(),
            Some(dsh_fs::FsErrorCode::NotFound)
        );
    }

    #[test]
    fn unread_edit_is_not_observed() {
        let ctx = setup();
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &owner("a"))
                .unwrap_err()
                .code(),
            Some(dsh_fs::FsErrorCode::NotObserved)
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &FsObservationActor::from_agent_id(None))
                .unwrap_err()
                .code(),
            Some(dsh_fs::FsErrorCode::NotObserved)
        );
    }

    #[test]
    fn write_observation_refreshes_edit_basis() {
        let ctx = setup();
        let actor = owner("a");
        observe(
            &ctx,
            &target("a.txt"),
            FsObservation::Present {
                version: "v1".into(),
            },
            &actor,
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &actor).unwrap(),
            Some("v1".into())
        );
        observe(
            &ctx,
            &target("a.txt"),
            FsObservation::Present {
                version: "v2".into(),
            },
            &actor,
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &actor).unwrap(),
            Some("v2".into())
        );
    }

    #[test]
    fn no_owner_observation_records_nothing() {
        let ctx = setup();
        observe(
            &ctx,
            &target("a.txt"),
            FsObservation::Present {
                version: "v0".into(),
            },
            &FsObservationActor::from_agent_id(None),
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &owner("a"))
                .unwrap_err()
                .code(),
            Some(dsh_fs::FsErrorCode::NotObserved)
        );
    }

    #[test]
    fn owners_are_isolated() {
        let ctx = setup();
        let a = owner("a");
        let b = owner("b");
        observe(
            &ctx,
            &target("a.txt"),
            FsObservation::Present {
                version: "v0".into(),
            },
            &a,
        );
        assert_eq!(
            edit_intent(&ctx, &target("a.txt"), &b)
                .unwrap_err()
                .code(),
            Some(dsh_fs::FsErrorCode::NotObserved)
        );
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &b),
            Some(FsWriteIntent::CreateIfAbsent)
        );
        assert_eq!(
            write_intent(&ctx, &target("a.txt"), &a),
            Some(FsWriteIntent::ReplaceIfVersion {
                version: "v0".into()
            })
        );
    }

    #[test]
    fn first_wins_without_calling_next() {
        let ctx = setup();
        let default_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&default_ran);
        let value = ctx
            .waterfall(
                FS_WRITE_INTENT,
                fs_event_payload(
                    &target("a.txt"),
                    &owner("a"),
                    None,
                ),
                move |_| {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    json!({ "kind": "should-not-run" })
                },
            )
            .unwrap();
        assert!(!default_ran.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            FsWriteIntent::from_value(&value),
            Some(FsWriteIntent::CreateIfAbsent)
        );
    }
}
