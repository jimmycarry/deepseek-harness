//! Human commands (`ctx.commands`).

use async_trait::async_trait;
use dsh_cordis::{Context, Service};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One registered slash command.
pub struct Command {
    /// Name without the leading slash.
    pub name: String,
    /// Human-readable summary used in discovery UI.
    pub description: String,
    /// Whether the command text is injected into the model request.
    pub model_visible: bool,
    /// Handler invoked by [`CommandRegistry::dispatch`].
    pub handler: Arc<dyn CommandHandler>,
}

/// Body of one command.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Run the command with the remainder of the typed line.
    async fn handle(&self, args: &str) -> Result<String, String>;
}

/// `ctx.commands`.
#[derive(Default)]
pub struct CommandRegistry {
    commands: Arc<Mutex<HashMap<String, Arc<Command>>>>,
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
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
            handler: Arc::new(StaticHandler {
                text: "ok".into(),
            }),
        });
        let goal = registry.get("goal").unwrap();
        assert!(!goal.model_visible);
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
