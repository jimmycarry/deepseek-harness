//! Skill registry (`ctx.skills`).

use dsh_cordis::Service;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One named skill body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Kebab-case identifier used to address the skill.
    pub name: String,
    /// Markdown body loaded for the model.
    pub body: String,
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
        let mut names: Vec<_> = self.skills.lock().expect("skills").keys().cloned().collect();
        names.sort();
        names
    }
}

impl Service for SkillRuntime {
    const KEY: &'static str = "skills";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn register_get_names() {
        let skills = SkillRuntime::new();
        skills.register(Skill {
            name: "review".into(),
            body: "do a review".into(),
        });
        assert_eq!(skills.get("review").unwrap().body, "do a review");
        assert_eq!(skills.names(), vec!["review".to_string()]);
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
