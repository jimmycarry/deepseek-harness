//! `/compact` command consumer.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_commands::{Command, CommandHandler, CommandInvocation, CommandRegistry, CommandResult};
use dsh_compaction::{CompactionRuntime, ManualCompactionError};
use dsh_cordis::Context;
use dsh_session::session_id;
use futures::executor::block_on;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-command-compact"
}

const USAGE: &str = "Usage: /compact (no arguments)";

struct CompactHandler {
    compaction: Arc<CompactionRuntime>,
    lookup: Context,
}

#[async_trait]
impl CommandHandler for CompactHandler {
    async fn handle(&self, args: &str) -> Result<String, String> {
        self.run(args, None, None).map(|(text, _)| text)
    }

    async fn handle_invocation(
        &self,
        invocation: CommandInvocation<'_>,
    ) -> Result<CommandResult, String> {
        let (text, source_event_seq) = self.run(
            invocation.raw_input,
            Some(invocation.session.id().as_str()),
            Some(invocation.command_id),
        )?;
        Ok(CommandResult {
            text,
            source_event_seq,
        })
    }
}

impl CompactHandler {
    fn run(
        &self,
        args: &str,
        session_id_hint: Option<&str>,
        command_id: Option<&str>,
    ) -> Result<(String, Option<u64>), String> {
        if !args.trim().is_empty() {
            return Err(USAGE.to_string());
        }
        let Some(agents) = self.lookup.get::<AgentRegistry>() else {
            return Err("agents service is not provided".into());
        };
        let id = match session_id_hint {
            Some(id) => id.to_string(),
            None => self
                .lookup
                .serial("command/compact", serde_json::json!({}))
                .and_then(|value| {
                    value
                        .get("sessionId")
                        .and_then(|value| value.as_str().map(str::to_string))
                })
                .ok_or_else(|| "sessionId required".to_string())?,
        };
        let Some(agent) = agents.get(&session_id(id)) else {
            return Err("unknown session".into());
        };
        let generated = format!("cmd-{}", uuid::Uuid::new_v4());
        let command_id = command_id.unwrap_or(generated.as_str());
        match block_on(
            self.compaction
                .engine()
                .compact_now(agent.as_ref(), Some(command_id)),
        ) {
            Ok(None) => Ok(("No compactable history yet.".into(), None)),
            Ok(Some(result)) => Ok((
                format!(
                    "Compacted {} history items (~{} tokens).",
                    result.shadowed_seqs.len(),
                    result.shadowed_token_count
                ),
                Some(result.summary_seq),
            )),
            Err(ManualCompactionError::Busy) => Err(
                "Compaction is unavailable because this process has an active compaction, or the agent is not idle."
                    .into(),
            ),
            Err(ManualCompactionError::NoRange) => Ok(("No compactable history yet.".into(), None)),
            Err(ManualCompactionError::Summary) => Err(
                "Compaction could not produce a useful summary. The conversation is unchanged; the attempt is recorded in the session log."
                    .into(),
            ),
        }
    }
}

/// Register `/compact` on `ctx.commands` with `model_visible: false`.
pub fn install(ctx: &Context) -> dsh_cordis::Result<()> {
    let commands = ctx.service::<CommandRegistry>()?;
    let compaction = ctx.service::<CompactionRuntime>()?;
    commands.register(
        ctx,
        Command {
            name: "compact".into(),
            description: "Compact older conversation history".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(CompactHandler {
                compaction,
                lookup: ctx.clone(),
            }),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentRegistry, AgentStatus, Inbox,
        InboxTarget,
    };
    use dsh_compaction::{
        CompactionEngine, CompactionResult, CompactionRuntime, CompactionTrigger,
        ManualCompactionError,
    };
    use dsh_cordis::Context;
    use dsh_session::{session_id, Session};
    use std::sync::Arc;

    struct NullEngine;

    #[async_trait]
    impl CompactionEngine for NullEngine {
        async fn compact_if_needed(
            &self,
            _: &dyn Agent,
            _: CompactionTrigger,
        ) -> Result<Option<CompactionResult>, ManualCompactionError> {
            Ok(None)
        }

        async fn compact_now(
            &self,
            _: &dyn Agent,
            _: Option<&str>,
        ) -> Result<Option<CompactionResult>, ManualCompactionError> {
            Ok(None)
        }
    }

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn id(&self) -> &dsh_session::SessionId {
            self.session.id()
        }
        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.session)
        }
        fn inbox(&self) -> Arc<Inbox> {
            Arc::clone(&self.inbox)
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn send(&self, _: dsh_llm::UserMessage, _: InboxTarget, _: bool) {}
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    struct StubFactory;

    impl AgentFactory for StubFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(StubAgent {
                session,
                inbox: Arc::new(Inbox::default()),
            })
        }
    }

    #[test]
    fn names_the_role() {
        assert_eq!(name(), "dsh-command-compact");
    }

    #[tokio::test]
    async fn install_registers_compact_not_model_visible() {
        let ctx = Context::new();
        let commands = Arc::new(CommandRegistry::new());
        ctx.provide(Arc::clone(&commands)).unwrap();
        ctx.provide(Arc::new(CompactionRuntime::new(Arc::new(NullEngine))))
            .unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        ctx.provide(Arc::new(agents)).unwrap();
        install(&ctx).unwrap();

        let definition = commands.get("compact").unwrap();
        assert!(!definition.model_visible);
        assert_eq!(definition.name, "compact");

        let session = Arc::new(Session::new(session_id("cmd")));
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        ctx.on_serial("command/compact", {
            let id = handle.agent.id().as_str().to_string();
            move |_| Some(serde_json::json!({ "sessionId": id }))
        })
        .unwrap();

        let result = commands.dispatch("/compact").await.unwrap().unwrap();
        assert!(result.contains("No compactable history yet."));

        let err = commands
            .dispatch("/compact extra")
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(err, USAGE);
    }

    #[tokio::test]
    async fn execute_compacts_the_invocation_session() {
        let ctx = Context::new();
        let commands = Arc::new(CommandRegistry::new());
        ctx.provide(Arc::clone(&commands)).unwrap();
        ctx.provide(Arc::new(CompactionRuntime::new(Arc::new(NullEngine))))
            .unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        ctx.provide(Arc::new(agents)).unwrap();
        install(&ctx).unwrap();

        let session = Arc::new(Session::new(session_id("exec")));
        let _handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        let outcome = commands
            .execute(session.as_ref(), "/compact")
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.text, "No compactable history yet.");
        let types: Vec<String> = session
            .events()
            .into_iter()
            .map(|event| match event.data {
                dsh_session::SessionEventData::Extension { type_name, .. } => type_name,
                _ => String::new(),
            })
            .collect();
        assert_eq!(types, vec!["command/run".to_string(), "command/done".to_string()]);
    }
}
