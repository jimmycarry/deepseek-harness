//! Session feedback event plus the human-facing `/feedback` producer.
//!
//! Recording appends one authoritative log-only `feedback/record` and does
//! not start model work. The append is eager but unflushed.

use async_trait::async_trait;
use dsh_anonymous_user_id::get_or_create_anonymous_user_id;
use dsh_commands::{Command, CommandHandler, CommandInvocation, CommandRegistry, CommandResult};
use dsh_cordis::{Context, Result, Service};
use dsh_session::{Session, SessionEventData};
use serde_json::json;
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "command-feedback"
}

const USAGE: &str = "Usage: /feedback <text>";

/// Disclosed session-sharing policy when `ctx.sessionTelemetry` is mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharingStatus {
    /// Full session sharing.
    Full,
    /// Sharing is gated on recorded feedback.
    FeedbackOnly,
    /// Sharing is off.
    Disabled,
}

/// Optional `ctx.sessionTelemetry` used only for the acknowledgement sentence.
#[derive(Debug, Clone)]
pub struct SessionTelemetry {
    /// Disclosed policy.
    pub sharing: SharingStatus,
}

impl Service for SessionTelemetry {
    const KEY: &'static str = "sessionTelemetry";
}

fn sharing_sentence(sharing: SharingStatus) -> &'static str {
    match sharing {
        SharingStatus::Full => "Session sharing is enabled.",
        SharingStatus::FeedbackOnly => {
            "Session sharing is feedback-gated; recording feedback releases the session prefix for sharing."
        }
        SharingStatus::Disabled => "Session sharing is disabled.",
    }
}

fn sharing_disclosure(telemetry: Option<&SessionTelemetry>) -> &'static str {
    match telemetry {
        None => "Session sharing is not configured.",
        Some(backend) => sharing_sentence(backend.sharing),
    }
}

/// Record feedback independently of any UI trigger.
///
/// # Errors
/// Empty normalized text.
pub fn record_feedback(session: &Session, text: &str) -> std::result::Result<(), String> {
    let normalized = text.trim();
    if normalized.is_empty() {
        return Err("feedback text must not be empty".into());
    }
    session
        .append(
            SessionEventData::Extension {
                type_name: "feedback/record".into(),
                data: json!({ "text": normalized }),
            },
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Register the global `/feedback` command.
pub fn install(ctx: &Context) -> Result<()> {
    let commands = ctx.service::<CommandRegistry>()?;
    let lookup = ctx.clone();
    commands.register(
        ctx,
        Command {
            name: "feedback".into(),
            description: "record feedback about this session".into(),
            model_visible: false,
            record_input: false,
            handler: Arc::new(FeedbackCommand { lookup }),
        },
    )
}

struct FeedbackCommand {
    lookup: Context,
}

#[async_trait]
impl CommandHandler for FeedbackCommand {
    async fn handle(&self, _args: &str) -> std::result::Result<String, String> {
        Err("feedback command requires a calling session".into())
    }

    async fn handle_invocation(
        &self,
        invocation: CommandInvocation<'_>,
    ) -> std::result::Result<CommandResult, String> {
        if invocation.raw_input.trim().is_empty() {
            return Err(format!("Feedback text is required. {USAGE}"));
        }
        record_feedback(invocation.session, invocation.raw_input)?;
        let user = get_or_create_anonymous_user_id();
        let telemetry = self.lookup.get::<SessionTelemetry>();
        Ok(CommandResult::text(format!(
            "Feedback recorded for session {}\nAnonymous user: {user}. {}",
            invocation.session.id(),
            sharing_disclosure(telemetry.as_deref())
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{event_type_name, SessionStore};

    fn setup() -> (Context, Arc<Session>) {
        let ctx = Context::new();
        ctx.provide(Arc::new(CommandRegistry::new())).unwrap();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        (ctx, session)
    }

    #[tokio::test]
    async fn acknowledges_and_records_once() {
        let home = std::env::temp_dir().join(format!(
            "dsh-feedback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("DSH_HOME", &home);
        std::fs::write(
            home.join(".anonymous-user-id"),
            "01234567-89ab-4cde-8f01-23456789abcd\n",
        )
        .unwrap();
        let (ctx, session) = setup();
        let outcome = ctx
            .service::<CommandRegistry>()
            .unwrap()
            .execute(session.as_ref(), "/feedback  the diff view is unreadable")
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.success);
        assert_eq!(
            outcome.text,
            format!(
                "Feedback recorded for session {}\nAnonymous user: 01234567-89ab-4cde-8f01-23456789abcd. Session sharing is not configured.",
                session.id()
            )
        );
        let types: Vec<_> = session
            .events()
            .iter()
            .map(|event| event_type_name(&event.data).to_string())
            .collect();
        assert_eq!(
            types,
            vec![
                "command/run".to_string(),
                "feedback/record".to_string(),
                "command/done".to_string()
            ]
        );
        let run = serde_json::to_value(&session.events()[0]).unwrap();
        assert!(run["data"].get("args").is_none());
        let record = serde_json::to_value(&session.events()[1]).unwrap();
        assert_eq!(record["data"]["text"], "the diff view is unreadable");
        assert!(session.derive_messages().is_empty());
    }

    #[tokio::test]
    async fn rejects_empty_without_recording() {
        let (ctx, session) = setup();
        let outcome = ctx
            .service::<CommandRegistry>()
            .unwrap()
            .execute(session.as_ref(), "/feedback")
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            outcome,
            "Feedback text is required. Usage: /feedback <text>"
        );
        let types: Vec<_> = session
            .events()
            .iter()
            .map(|event| event_type_name(&event.data).to_string())
            .collect();
        assert_eq!(types, vec!["command/run".to_string(), "command/done".to_string()]);
        assert!(record_feedback(session.as_ref(), "   ").is_err());
    }

    #[tokio::test]
    async fn rejects_whitespace_only_without_reading_anonymous_id() {
        let (ctx, session) = setup();
        let outcome = ctx
            .service::<CommandRegistry>()
            .unwrap()
            .execute(session.as_ref(), "/feedback   \n\t ")
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            outcome,
            "Feedback text is required. Usage: /feedback <text>"
        );
        let types: Vec<_> = session
            .events()
            .iter()
            .map(|event| event_type_name(&event.data).to_string())
            .collect();
        assert_eq!(types, vec!["command/run".to_string(), "command/done".to_string()]);
    }

    async fn disclose(sharing: SharingStatus, line: &str) -> String {
        let ctx = Context::new();
        ctx.provide(Arc::new(CommandRegistry::new())).unwrap();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(SessionTelemetry { sharing }))
            .unwrap();
        install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        ctx.service::<CommandRegistry>()
            .unwrap()
            .execute(session.as_ref(), line)
            .await
            .unwrap()
            .unwrap()
            .text
    }

    #[tokio::test]
    async fn discloses_full_sharing() {
        let text = disclose(SharingStatus::Full, "/feedback everything shared").await;
        assert!(text.contains("Session sharing is enabled."));
    }

    #[tokio::test]
    async fn discloses_feedback_only_sharing() {
        let text = disclose(SharingStatus::FeedbackOnly, "/feedback gated sharing").await;
        assert!(text.contains(
            "Session sharing is feedback-gated; recording feedback releases the session prefix for sharing."
        ));
    }

    #[tokio::test]
    async fn discloses_disabled_sharing() {
        let text = disclose(SharingStatus::Disabled, "/feedback local only").await;
        assert!(text.contains("Session sharing is disabled."));
    }
}
