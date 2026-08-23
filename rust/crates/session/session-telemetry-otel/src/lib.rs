//! Session telemetry backend. The shipped default is `DISABLED`: no records
//! leave the process, and `ctx.sessionTelemetry.sharing` discloses `disabled`
//! so `/feedback` can report the standing policy.

use dsh_command_feedback::{SessionTelemetry, SharingStatus};
use dsh_cordis::{Context, CordisError, Result};
use serde_json::Value;
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "session-telemetry-otel"
}

/// Resolved sharing policy from plugin config or `DSH_TELEMETRY_MODE`.
pub fn resolve_sharing(config: Option<&Value>) -> Result<SharingStatus> {
    let raw = config
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::var("DSH_TELEMETRY_MODE").ok())
        .unwrap_or_else(|| "DISABLED".into());
    match raw.as_str() {
        "FULL" => Ok(SharingStatus::Full),
        "FEEDBACK_ONLY" => Ok(SharingStatus::FeedbackOnly),
        "DISABLED" => Ok(SharingStatus::Disabled),
        other => Err(CordisError::Validation(format!(
            "session-telemetry-otel: unknown mode {other:?}"
        ))),
    }
}

/// Provide `ctx.sessionTelemetry` with the disclosed sharing status.
///
/// `DISABLED` constructs no exporter. `FULL` and `FEEDBACK_ONLY` currently
/// disclose the same status without an OTLP pipeline.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let sharing = resolve_sharing(config)?;
    ctx.provide(Arc::new(SessionTelemetry { sharing }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_disabled() {
        assert_eq!(resolve_sharing(None).unwrap(), SharingStatus::Disabled);
        assert_eq!(
            resolve_sharing(Some(&serde_json::json!({ "mode": "DISABLED" }))).unwrap(),
            SharingStatus::Disabled
        );
    }

    #[test]
    fn accepts_upload_modes_and_rejects_unknown() {
        assert_eq!(
            resolve_sharing(Some(&serde_json::json!({ "mode": "FULL" }))).unwrap(),
            SharingStatus::Full
        );
        assert_eq!(
            resolve_sharing(Some(&serde_json::json!({ "mode": "FEEDBACK_ONLY" }))).unwrap(),
            SharingStatus::FeedbackOnly
        );
        assert!(resolve_sharing(Some(&serde_json::json!({ "mode": "maybe" }))).is_err());
    }

    #[test]
    fn mounts_disabled_disclosure() {
        let ctx = Context::new();
        install(&ctx, Some(&serde_json::json!({ "mode": "DISABLED" }))).unwrap();
        assert_eq!(
            ctx.service::<SessionTelemetry>().unwrap().sharing,
            SharingStatus::Disabled
        );
    }
}
