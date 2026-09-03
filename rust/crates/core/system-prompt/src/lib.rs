//! Prompt-section and tool-schema assembly (`ctx.systemPrompt`).

use dsh_cordis::Service;
use dsh_llm::{SnapshotSection, ToolSchema};
use dsh_session::Session;
use std::sync::{Arc, Mutex};

/// One prompt section contributed by a plugin.
#[derive(Debug, Clone)]
pub struct PromptSection {
    /// Stable section id used for ordering and replacement.
    pub id: String,
    /// Section body.
    pub text: String,
    /// Sort key; lower first.
    pub order: i32,
}

/// Assembled request prefix.
#[derive(Debug, Clone, Default)]
pub struct PromptAssembly {
    /// Rendered system prompt text.
    pub system: String,
    /// Tool schemas in advertised order.
    pub tools: Vec<ToolSchema>,
}

/// Body of one runtime-context contribution.
#[derive(Clone)]
pub enum PromptContextText {
    /// Fixed text, independent of the calling session.
    Static(String),
    /// Text computed from the current session, or none for a bare assembly.
    Dynamic(Arc<dyn Fn(Option<&Session>) -> String + Send + Sync>),
}

impl std::fmt::Debug for PromptContextText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(text) => formatter.debug_tuple("Static").field(text).finish(),
            Self::Dynamic(_) => formatter.write_str("Dynamic(..)"),
        }
    }
}

impl PromptContextText {
    /// Materialize the contribution for `session`.
    pub fn render(&self, session: Option<&Session>) -> String {
        match self {
            Self::Static(text) => text.clone(),
            Self::Dynamic(render) => render(session),
        }
    }
}

impl From<&str> for PromptContextText {
    fn from(text: &str) -> Self {
        Self::Static(text.to_string())
    }
}

impl From<String> for PromptContextText {
    fn from(text: String) -> Self {
        Self::Static(text)
    }
}

/// Dynamic runtime-context contribution materialized as a user-role snapshot.
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// Unique name (`sandbox:policy`, `approval:policy`).
    pub name: String,
    /// Model-facing text. Empty text contributes nothing.
    pub text: PromptContextText,
    /// Sort key; lower first.
    pub order: i32,
}

/// `ctx.systemPrompt`.
#[derive(Default)]
pub struct SystemPrompt {
    sections: Mutex<Vec<PromptSection>>,
    contexts: Mutex<Vec<PromptContext>>,
    persona: Mutex<String>,
}

impl SystemPrompt {
    /// Create an empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the persona section (id `persona`).
    pub fn set_persona(&self, text: impl Into<String>) {
        *self.persona.lock().expect("persona") = text.into();
    }

    /// Register a runtime-context contribution. Later registrations with the same name replace.
    pub fn register_context(&self, context: PromptContext) {
        let mut contexts = self.contexts.lock().expect("contexts");
        if let Some(existing) = contexts.iter_mut().find(|item| item.name == context.name) {
            *existing = context;
        } else {
            contexts.push(context);
        }
    }

    /// Named snapshot sections with non-empty text, in order.
    pub fn context_sections(&self, session: Option<&Session>) -> Vec<SnapshotSection> {
        let mut contexts = self.contexts.lock().expect("contexts").clone();
        contexts.sort_by_key(|context| context.order);
        contexts
            .into_iter()
            .filter_map(|context| {
                let text = context.text.render(session);
                if text.is_empty() {
                    None
                } else {
                    Some(SnapshotSection {
                        name: context.name,
                        text,
                    })
                }
            })
            .collect()
    }

    /// Full snapshot text, or empty when no context is active.
    pub fn render_context_snapshot(&self, session: Option<&Session>) -> String {
        join_context_sections(&self.context_sections(session))
    }

    /// Register a section. Later registrations with the same id replace.
    pub fn register_section(&self, section: PromptSection) {
        let mut sections = self.sections.lock().expect("sections");
        if let Some(existing) = sections.iter_mut().find(|item| item.id == section.id) {
            *existing = section;
        } else {
            sections.push(section);
        }
    }

    /// Assemble persona + sections + tool schemas.
    pub fn assemble(&self, tools: Vec<ToolSchema>) -> PromptAssembly {
        let persona = self.persona.lock().expect("persona").clone();
        let mut sections = self.sections.lock().expect("sections").clone();
        sections.sort_by_key(|section| section.order);
        let mut parts = Vec::new();
        if !persona.is_empty() {
            parts.push(persona);
        }
        for section in sections {
            if !section.text.is_empty() {
                parts.push(section.text);
            }
        }
        PromptAssembly {
            system: parts.join("\n\n"),
            tools,
        }
    }
}

impl Service for SystemPrompt {
    const KEY: &'static str = "systemPrompt";
}

/// Render an assembly to the system string the loop sends.
pub fn render_prompt(assembly: &PromptAssembly) -> String {
    assembly.system.clone()
}

/// Join named snapshot sections with the TypeScript runtime-context prefix.
pub fn join_context_sections(sections: &[SnapshotSection]) -> String {
    let body = sections
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        String::new()
    } else {
        format!(
            "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n{body}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_orders_sections_after_persona() {
        let prompt = SystemPrompt::new();
        prompt.set_persona("You are dsh.");
        prompt.register_section(PromptSection {
            id: "b".into(),
            text: "second".into(),
            order: 20,
        });
        prompt.register_section(PromptSection {
            id: "a".into(),
            text: "first".into(),
            order: 10,
        });
        let assembly = prompt.assemble(Vec::new());
        assert_eq!(assembly.system, "You are dsh.\n\nfirst\n\nsecond");
    }

    #[test]
    fn dynamic_context_is_empty_without_a_session() {
        let prompt = SystemPrompt::new();
        prompt.register_context(PromptContext {
            name: "sandbox:policy".into(),
            text: PromptContextText::Dynamic(std::sync::Arc::new(|session| {
                session
                    .map(|item| item.id().as_str().to_string())
                    .unwrap_or_default()
            })),
            order: 110,
        });
        assert!(prompt.context_sections(None).is_empty());
    }
}
