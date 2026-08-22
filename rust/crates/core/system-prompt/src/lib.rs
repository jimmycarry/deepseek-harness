//! Prompt-section and tool-schema assembly (`ctx.systemPrompt`).

use dsh_cordis::Service;
use dsh_llm::ToolSchema;
use std::sync::Mutex;

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

/// `ctx.systemPrompt`.
#[derive(Default)]
pub struct SystemPrompt {
    sections: Mutex<Vec<PromptSection>>,
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
}
