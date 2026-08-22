//! Skill registry (`ctx.skills`).

use dsh_cordis::Service;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One named skill body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Kebab-case identifier used to address the skill.
    pub name: String,
    /// One-line catalog description from frontmatter.
    pub description: String,
    /// Markdown body loaded for the model.
    pub body: String,
    /// Whether the model may load this skill (`disable-model-invocation` off).
    pub model_invocable: bool,
    /// Sibling resource paths shipped with a directory-bundle skill.
    pub resources: Vec<String>,
}

impl Skill {
    /// Flat model-invocable skill with no resources.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            body: body.into(),
            model_invocable: true,
            resources: Vec::new(),
        }
    }
}

/// `ctx.skills`.
#[derive(Default)]
pub struct SkillRuntime {
    skills: Arc<Mutex<HashMap<String, Skill>>>,
}

impl SkillRuntime {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace one skill.
    pub fn register(&self, skill: Skill) {
        self.skills
            .lock()
            .expect("skills")
            .insert(skill.name.clone(), skill);
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.skills.lock().expect("skills").get(name).cloned()
    }

    /// Registered skill names in sorted order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .skills
            .lock()
            .expect("skills")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Model-invocable skills in name order (the catalog view).
    pub fn catalog(&self) -> Vec<Skill> {
        let mut entries: Vec<_> = self
            .skills
            .lock()
            .expect("skills")
            .values()
            .filter(|skill| skill.model_invocable)
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

impl Service for SkillRuntime {
    const KEY: &'static str = "skills";
}

/// Render one loaded skill in the model-facing `<skill_content>` format.
pub fn render_skill_content(skill: &Skill) -> String {
    let resources = skill.resources.join("\n");
    format!(
        "<skill_content name=\"{}\">\n<skill_resources>\n{}\n</skill_resources>\n\n<skill_instructions>\n{}\n</skill_instructions>\n</skill_content>",
        skill.name, resources, skill.body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn register_get_names_catalog() {
        let skills = SkillRuntime::new();
        skills.register(Skill::new("review", "do reviews", "do a review"));
        let mut hidden = Skill::new("internal", "hidden", "x");
        hidden.model_invocable = false;
        skills.register(hidden);
        assert_eq!(skills.get("review").unwrap().body, "do a review");
        assert_eq!(
            skills.names(),
            vec!["internal".to_string(), "review".to_string()]
        );
        let catalog = skills.catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "review");
    }

    #[test]
    fn renders_skill_content_frame() {
        let mut skill = Skill::new("review", "d", "instructions");
        skill.resources = vec!["checklist.md".into()];
        let rendered = render_skill_content(&skill);
        assert!(rendered.starts_with("<skill_content name=\"review\">"));
        assert!(rendered.contains("<skill_resources>\nchecklist.md\n</skill_resources>"));
        assert!(rendered.contains("<skill_instructions>\ninstructions\n</skill_instructions>"));
        assert!(rendered.ends_with("</skill_content>"));
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SkillRuntime::new())).unwrap();
        assert!(ctx.has_service("skills"));
        ctx.dispose();
        assert!(!ctx.has_service("skills"));
    }
}
