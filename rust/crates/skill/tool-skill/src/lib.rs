//! Model-facing `skill` tool plus the per-session skill-catalog publication.
//!
//! The catalog rides `agent/pre-step` as a `user/message` with
//! `source.kind: "skill-catalog"`. The first publication for an agent frames
//! the skills concept. A later scan whose name/description digest differs
//! publishes a replacement catalog with `update: true`.

use async_trait::async_trait;
use dsh_cordis::{Context, CordisError};
use dsh_llm::{MessageSource, SkillCatalogEntry, UserMessage};
use dsh_skill::{render_skill_content, SkillRuntime};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Catalog presentation policy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum characters of one catalog description before truncation.
    pub catalog_description_max_length: usize,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// A `catalogDescriptionMaxLength` below 3 or non-integer.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let max = match config.and_then(|value| value.get("catalogDescriptionMaxLength")) {
            None => 500,
            Some(value) => {
                let number = value.as_u64().ok_or_else(|| {
                    "tool-skill: catalogDescriptionMaxLength must be an integer".to_string()
                })?;
                if number < 3 {
                    return Err("tool-skill: catalogDescriptionMaxLength must be at least 3".into());
                }
                number as usize
            }
        };
        Ok(Self {
            catalog_description_max_length: max,
        })
    }
}

/// `skill` loader over `ctx.skills`.
pub struct LoadSkillTool {
    skills: Arc<SkillRuntime>,
}

impl LoadSkillTool {
    /// Bind to `ctx.skills`.
    pub fn new(skills: Arc<SkillRuntime>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for an available skill. Call this with the exact skill name from the session skill catalog before performing a task the skill covers."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(ToolError::Body(format!(
                "Error: invalid skill name {}",
                serde_json::to_string(&name).unwrap_or_default()
            )));
        }
        let Some(skill) = self.skills.get(&name) else {
            return Err(ToolError::Body(format!(
                "Error: skill \"{name}\" is unknown or no longer available"
            )));
        };
        if !skill.model_invocable {
            return Err(ToolError::Body(format!(
                "Error: skill \"{name}\" is not available for model invocation"
            )));
        }
        Ok(ToolOutcome::text(render_skill_content(&skill)))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        self.execute(call.args.clone()).await
    }
}

/// Truncate one catalog description to the configured cap.
fn truncate_description(description: &str, cap: usize) -> String {
    let chars: Vec<char> = description.chars().collect();
    if chars.len() <= cap {
        return description.to_string();
    }
    let head: String = chars[..cap.saturating_sub(1)].iter().collect();
    format!("{head}…")
}

fn catalog_lines(entries: &[SkillCatalogEntry]) -> String {
    entries
        .iter()
        .map(|entry| format!("- `{}`: {}", entry.name, entry.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// First-publication catalog body listing `entries`.
fn catalog_body(entries: &[SkillCatalogEntry]) -> String {
    let lines = catalog_lines(entries);
    format!(
        "<system-reminder>\nA skill is a reusable set of task-specific instructions. The following skills are available in this session:\n\n<available_skills>\n{lines}\n</available_skills>\n\nIf the user names a skill, or the task clearly matches a skill's description, call the `skill` tool with the exact skill name before taking task actions. Load all applicable skills, then follow their full instructions. This catalog contains summaries only; do not infer or follow a skill's instructions until it has been loaded.\nA user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.\n</system-reminder>"
    )
}

/// Replacement catalog body after the first publication.
fn catalog_update_body(entries: &[SkillCatalogEntry]) -> String {
    let lines = catalog_lines(entries);
    let availability = if entries.is_empty() {
        "No skills are currently available through the `skill` tool. Do not use names from earlier skill catalogs.\nA user may still invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool for it."
    } else {
        "Use only names in this replacement catalog. If the user names a listed skill, or the task clearly matches its description, call the `skill` tool with the exact name before acting.\nA user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill."
    };
    format!(
        "<system-reminder>\nThe available skill catalog changed. This complete catalog replaces every earlier available-skills list in this session:\n\n<available_skills>\n{lines}\n</available_skills>\n\n{availability}\n</system-reminder>"
    )
}

fn catalog_digest(entries: &[SkillCatalogEntry]) -> String {
    entries
        .iter()
        .map(|entry| format!("{}\n{}", entry.name, entry.description))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

/// Register the `skill` tool and the pre-step catalog publication.
///
/// # Errors
/// Missing `ctx.tools` or `ctx.skills`, or a failed listener registration.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let skills = ctx.service::<SkillRuntime>()?;
    tools.insert(Arc::new(LoadSkillTool::new(Arc::clone(&skills))));
    let published: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let cap = config.catalog_description_max_length;
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        let mut payload = next.call(payload);
        let agent_id = payload
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if agent_id.is_empty() {
            return payload;
        }
        let entries: Vec<SkillCatalogEntry> = skills
            .catalog()
            .into_iter()
            .map(|skill| SkillCatalogEntry {
                name: skill.name,
                description: truncate_description(&skill.description, cap),
            })
            .collect();
        let digest = catalog_digest(&entries);
        let previous = published
            .lock()
            .expect("skill catalog")
            .get(&agent_id)
            .cloned();
        if previous.as_deref() == Some(digest.as_str()) {
            return payload;
        }
        if previous.is_none() && entries.is_empty() {
            return payload;
        }
        let update = previous.is_some();
        let body = if update {
            catalog_update_body(&entries)
        } else {
            catalog_body(&entries)
        };
        if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            let catalog = UserMessage::from_parts(
                vec![dsh_llm::ContentBlock::text(body)],
                MessageSource::SkillCatalog {
                    form: "catalog".into(),
                    update: update.then_some(true),
                    entries,
                },
            );
            messages.push(serde_json::to_value(&catalog).unwrap_or_default());
            published
                .lock()
                .expect("skill catalog")
                .insert(agent_id, digest);
        }
        payload
    })
    .map_err(|error| CordisError::Plugin(error.to_string()))?;
    Ok(())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-skill"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_skill::Skill;

    fn runtime_with(skill: Skill) -> Arc<SkillRuntime> {
        let skills = Arc::new(SkillRuntime::new());
        skills.register(skill);
        skills
    }

    #[tokio::test]
    async fn loads_registered_skill_in_content_frame() {
        let tool = LoadSkillTool::new(runtime_with(Skill::new(
            "review",
            "do reviews",
            "do a review",
        )));
        let outcome = tool
            .execute(serde_json::json!({ "name": "review" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        let text = match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(
            text.starts_with("<skill_content name=\"review\">"),
            "{text}"
        );
        assert!(text.contains("<skill_instructions>\ndo a review\n</skill_instructions>"));
    }

    #[tokio::test]
    async fn failure_texts_match_the_typescript_tool() {
        let mut hidden = Skill::new("hidden", "d", "x");
        hidden.model_invocable = false;
        let tool = LoadSkillTool::new(runtime_with(hidden));
        let unknown = tool
            .execute(serde_json::json!({ "name": "missing" }))
            .await
            .unwrap_err();
        assert!(matches!(
            unknown,
            ToolError::Body(message)
                if message == "Error: skill \"missing\" is unknown or no longer available"
        ));
        let invalid = tool
            .execute(serde_json::json!({ "name": "  " }))
            .await
            .unwrap_err();
        assert!(matches!(
            invalid,
            ToolError::Body(message) if message == "Error: invalid skill name \"\""
        ));
        let blocked = tool
            .execute(serde_json::json!({ "name": "hidden" }))
            .await
            .unwrap_err();
        assert!(matches!(
            blocked,
            ToolError::Body(message)
                if message == "Error: skill \"hidden\" is not available for model invocation"
        ));
    }

    #[test]
    fn catalog_publishes_once_per_agent() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(runtime_with(Skill::new("review", "do reviews", "body")))
            .unwrap();
        install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let payload = serde_json::json!({
            "agentId": "a1",
            "messages": [],
            "turn": 1,
        });
        let first = ctx
            .waterfall("agent/pre-step", payload.clone(), |payload| payload)
            .unwrap();
        let messages = first["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["source"]["kind"], "skill-catalog");
        assert_eq!(messages[0]["source"]["form"], "catalog");
        assert_eq!(messages[0]["source"]["entries"][0]["name"], "review");
        let body = messages[0]["content"][0]["text"].as_str().unwrap();
        assert!(body.starts_with("<system-reminder>"), "{body}");
        assert!(body.contains("<available_skills>\n- `review`: do reviews\n</available_skills>"));
        let second = ctx
            .waterfall("agent/pre-step", payload, |payload| payload)
            .unwrap();
        assert_eq!(second["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn catalog_replaces_when_entries_change() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        let skills = runtime_with(Skill::new("review", "do reviews", "body"));
        ctx.provide(Arc::clone(&skills)).unwrap();
        install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let payload = serde_json::json!({
            "agentId": "a1",
            "messages": [],
            "turn": 1,
        });
        let first = ctx
            .waterfall("agent/pre-step", payload.clone(), |payload| payload)
            .unwrap();
        assert!(first["messages"][0]["source"].get("update").is_none());
        skills.register(Skill::new("extra", "another skill", "x"));
        let second = ctx
            .waterfall("agent/pre-step", payload, |payload| payload)
            .unwrap();
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["source"]["update"], true);
        let body = messages[0]["content"][0]["text"].as_str().unwrap();
        assert!(
            body.contains("This complete catalog replaces every earlier available-skills list"),
            "{body}"
        );
        assert!(body.contains("- `extra`: another skill"), "{body}");
    }

    #[test]
    fn config_bounds_description_length() {
        assert!(Config::resolve(Some(&serde_json::json!({
            "catalogDescriptionMaxLength": 2
        })))
        .is_err());
        let config = Config::resolve(None).unwrap();
        assert_eq!(config.catalog_description_max_length, 500);
        assert_eq!(truncate_description("abcdef", 4), "abc…");
        assert_eq!(truncate_description("ab", 4), "ab");
    }
}
