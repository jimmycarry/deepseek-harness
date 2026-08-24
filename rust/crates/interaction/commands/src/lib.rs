//! Human commands (`ctx.commands`).

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use dsh_session::{Session, SessionEventData};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// One registered slash command.
pub struct Command {
    /// Name without the leading slash.
    pub name: String,
    /// Human-readable summary used in discovery UI.
    pub description: String,
    /// Whether the command text is injected into the model request.
    pub model_visible: bool,
    /// Whether `command/run` records `args`. `false` when a domain event owns
    /// the payload (`/feedback`).
    pub record_input: bool,
    /// Handler invoked by [`CommandRegistry::dispatch`].
    pub handler: Arc<dyn CommandHandler>,
}

/// Session-aware invocation used by [`CommandRegistry::execute`].
pub struct CommandInvocation<'a> {
    /// Receiving session; lifecycle events are appended here.
    pub session: &'a Session,
    /// Remainder of the typed line after the command name.
    pub raw_input: &'a str,
    /// Pairing id already written on `command/run`.
    pub command_id: &'a str,
}

/// Body of one command.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Run the command with the remainder of the typed line.
    async fn handle(&self, args: &str) -> Result<String, String>;

    /// Session-aware entry used by [`CommandRegistry::execute`].
    async fn handle_invocation(
        &self,
        invocation: CommandInvocation<'_>,
    ) -> Result<CommandResult, String> {
        self.handle(invocation.raw_input)
            .await
            .map(CommandResult::text)
    }
}

/// Successful handler body returned to [`CommandRegistry::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// Handler text shown to the user.
    pub text: String,
    /// Earlier non-command event that owns a richer presentation.
    pub source_event_seq: Option<u64>,
}

impl CommandResult {
    /// Text-only success with no domain-event citation.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source_event_seq: None,
        }
    }
}

/// Settled outcome of [`CommandRegistry::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecution {
    /// Pairing id carried by `command/run` / `command/done`.
    pub command_id: String,
    /// Handler text.
    pub text: String,
    /// Whether the handler reported success.
    pub success: bool,
    /// `command/done.sourceEventSeq` when the handler cited one.
    pub source_event_seq: Option<u64>,
}

/// `ctx.commands`.
pub struct CommandRegistry {
    commands: Arc<Mutex<HashMap<String, Arc<Command>>>>,
    instance: String,
    seq: AtomicU64,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        let hex = Uuid::new_v4().as_simple().to_string();
        Self {
            commands: Arc::new(Mutex::new(HashMap::new())),
            instance: hex[..8].to_string(),
            seq: AtomicU64::new(0),
        }
    }

    /// Register a command. The registration is an effect on `ctx`.
    pub fn register(&self, ctx: &Context, command: Command) -> dsh_cordis::Result<()> {
        let name = command.name.clone();
        let command = Arc::new(command);
        let map = Arc::clone(&self.commands);
        ctx.effect(&format!("commands.register({name})"), || {
            map.lock().expect("commands").insert(name.clone(), command);
            let map = Arc::clone(&map);
            let name = name.clone();
            move || {
                map.lock().expect("commands").remove(&name);
            }
        })
    }

    /// Insert a command without an effect (tests / static composition).
    pub fn insert(&self, command: Command) {
        self.commands
            .lock()
            .expect("commands")
            .insert(command.name.clone(), Arc::new(command));
    }

    /// Look up a command by name (no slash).
    pub fn get(&self, name: &str) -> Option<Arc<Command>> {
        self.commands.lock().expect("commands").get(name).cloned()
    }

    /// Dispatch a typed line. Normal prompts return `None`.
    pub async fn dispatch(&self, prompt: &str) -> Option<Result<String, String>> {
        let trimmed = prompt.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let rest = &trimmed[1..];
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        let command = self.get(name)?;
        Some(command.handler.handle(args).await)
    }

    /// Parse and execute a known command, appending log-only `command/run`
    /// then `command/done`. Admission misses (not a slash line, unknown name)
    /// return `None` and write nothing. A handler `Err` settles as
    /// `success: false` with that text; `Err` on this `Result` is a lifecycle
    /// append failure after `command/run`.
    pub async fn execute(
        &self,
        session: &Session,
        line: &str,
    ) -> Option<std::result::Result<CommandExecution, String>> {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let rest = &trimmed[1..];
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        let command = self.get(name)?;
        let command_id = {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
            format!("cmd-{}-{seq}", self.instance)
        };
        let mut run = serde_json::Map::new();
        run.insert("commandId".into(), json!(command_id));
        run.insert("name".into(), json!(name));
        run.insert("source".into(), json!({ "kind": "user" }));
        if command.record_input {
            run.insert("args".into(), json!(args));
        }
        if let Err(error) = session.append(
            SessionEventData::Extension {
                type_name: "command/run".into(),
                data: Value::Object(run),
            },
            None,
        ) {
            return Some(Err(error.to_string()));
        }
        let outcome = command
            .handler
            .handle_invocation(CommandInvocation {
                session,
                raw_input: args,
                command_id: &command_id,
            })
            .await;
        let (success, text, source_event_seq) = match &outcome {
            Ok(result) => (true, result.text.clone(), result.source_event_seq),
            Err(text) => (false, text.clone(), None),
        };
        let mut done = serde_json::Map::new();
        done.insert("commandId".into(), json!(command_id));
        done.insert(
            "kind".into(),
            json!(if success { "success" } else { "error" }),
        );
        if !text.is_empty() {
            done.insert("text".into(), json!(text));
        }
        if success {
            if let Some(seq) = source_event_seq {
                done.insert("sourceEventSeq".into(), json!(seq));
            }
        }
        let done_error = session
            .append(
                SessionEventData::Extension {
                    type_name: "command/done".into(),
                    data: Value::Object(done),
                },
                None,
            )
            .err();
        if let Some(error) = done_error {
            if success {
                return Some(Err(error.to_string()));
            }
        }
        Some(Ok(CommandExecution {
            command_id,
            text,
            success,
            source_event_seq,
        }))
    }
}

impl Service for CommandRegistry {
    const KEY: &'static str = "commands";
}

/// Handler that returns a fixed string.
pub struct StaticHandler {
    /// Text returned from [`CommandHandler::handle`].
    pub text: String,
}

#[async_trait]
impl CommandHandler for StaticHandler {
    async fn handle(&self, _args: &str) -> Result<String, String> {
        Ok(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[tokio::test]
    async fn dispatch_returns_none_for_normal_prompts() {
        let registry = CommandRegistry::new();
        registry.insert(Command {
            name: "goal".into(),
            description: "Set a same-session goal".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(StaticHandler {
                text: "ok".into(),
            }),
        });
        assert!(registry.dispatch("please do this").await.is_none());
        assert_eq!(registry.dispatch("/goal").await.unwrap().unwrap(), "ok");
    }

    #[test]
    fn goal_is_not_model_visible() {
        let registry = CommandRegistry::new();
        registry.insert(Command {
            name: "goal".into(),
            description: "Set a same-session goal".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(StaticHandler {
                text: "ok".into(),
            }),
        });
        let goal = registry.get("goal").unwrap();
        assert!(!goal.model_visible);
    }

    #[tokio::test]
    async fn execute_logs_run_and_done_and_omits_args_when_unrecorded() {
        let ctx = Context::new();
        ctx.provide(Arc::new(dsh_session::SessionStore::new()))
            .unwrap();
        let registry = CommandRegistry::new();
        registry.insert(Command {
            name: "echo".into(),
            description: "echo".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(StaticHandler {
                text: "heard".into(),
            }),
        });
        registry.insert(Command {
            name: "quiet".into(),
            description: "quiet".into(),
            model_visible: false,
            record_input: false,
            handler: Arc::new(StaticHandler {
                text: "ok".into(),
            }),
        });
        let session = ctx
            .service::<dsh_session::SessionStore>()
            .unwrap()
            .create_fresh();
        let recorded = registry
            .execute(session.as_ref(), "/echo  please log this")
            .await
            .unwrap()
            .unwrap();
        assert!(recorded.success);
        assert_eq!(recorded.text, "heard");
        let run = serde_json::to_value(&session.events()[0]).unwrap();
        assert_eq!(run["type"], "command/run");
        assert_eq!(run["data"]["args"], "please log this");
        assert_eq!(run["data"]["source"]["kind"], "user");
        let hidden = registry
            .execute(session.as_ref(), "/quiet secret payload")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hidden.text, "ok");
        let quiet_run = serde_json::to_value(&session.events()[2]).unwrap();
        assert!(quiet_run["data"].get("args").is_none());
        assert!(registry.execute(session.as_ref(), "not a slash").await.is_none());
        assert!(registry.execute(session.as_ref(), "/missing").await.is_none());
    }

    struct FailHandler;

    #[async_trait]
    impl CommandHandler for FailHandler {
        async fn handle(&self, _args: &str) -> Result<String, String> {
            Err("nope".into())
        }
    }

    #[tokio::test]
    async fn execute_settles_handler_err_as_unsuccessful() {
        let ctx = Context::new();
        ctx.provide(Arc::new(dsh_session::SessionStore::new()))
            .unwrap();
        let registry = CommandRegistry::new();
        registry.insert(Command {
            name: "fail".into(),
            description: "fail".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(FailHandler),
        });
        let session = ctx
            .service::<dsh_session::SessionStore>()
            .unwrap()
            .create_fresh();
        let recorded = registry
            .execute(session.as_ref(), "/fail")
            .await
            .unwrap()
            .unwrap();
        assert!(!recorded.success);
        assert_eq!(recorded.text, "nope");
        let done = serde_json::to_value(&session.events()[1]).unwrap();
        assert_eq!(done["type"], "command/done");
        assert_eq!(done["data"]["kind"], "error");
        assert_eq!(done["data"]["text"], "nope");
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(CommandRegistry::new())).unwrap();
        assert!(ctx.has_service("commands"));
        ctx.dispose();
        assert!(!ctx.has_service("commands"));
    }
}
