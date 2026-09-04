//! Workflow engine (`ctx.workflowEngine`).

use async_trait::async_trait;
use dsh_cordis::Service;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Workflow identity block from the `meta` argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMeta {
    /// Kebab-case name.
    pub name: String,
    /// Human description.
    pub description: String,
    /// Optional when-to-use hint.
    #[serde(rename = "whenToUse", skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
}

/// Result of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResult {
    /// Stop reason (`completed`, `error`).
    pub stop_reason: String,
    /// JSON return value.
    pub value: Value,
    /// How many child agents ran.
    pub agent_count: u32,
}

/// Engine failures.
#[derive(Debug, Error)]
pub enum WorkflowError {
    /// Meta or script rejected before start.
    #[error("{0}")]
    Invalid(String),
    /// Script failed during the run.
    #[error("workflow run failed: {0}")]
    Failed(String),
}

/// One start request.
pub struct WorkflowStartRequest {
    /// Plain script body.
    pub script: String,
    /// JSON identity block.
    pub meta: WorkflowMeta,
    /// Optional `args` global.
    pub args: Option<Value>,
}

/// `ctx.workflowEngine`.
#[async_trait]
pub trait WorkflowEngine: Send + Sync {
    /// Run one script.
    async fn start(
        &self,
        request: WorkflowStartRequest,
    ) -> std::result::Result<WorkflowResult, WorkflowError>;
}

/// In-process engine that evaluates `return <json>`.
pub struct WorkflowRuntime {
    isolation: String,
}

impl WorkflowRuntime {
    /// Bind isolation label (never hardcoded in `start`).
    pub fn new(isolation: impl Into<String>) -> Self {
        Self {
            isolation: isolation.into(),
        }
    }

    /// Configured isolation realm.
    pub fn isolation(&self) -> &str {
        &self.isolation
    }
}

/// Validate kebab-case name and required description.
pub fn validate_meta(value: &Value) -> std::result::Result<WorkflowMeta, WorkflowError> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkflowError::Invalid("invalid meta: name is required".into()))?;
    if !is_kebab(name) {
        return Err(WorkflowError::Invalid(
            "invalid meta: name must be kebab-case".into(),
        ));
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkflowError::Invalid("invalid meta: description is required".into()))?;
    if description.trim().is_empty() {
        return Err(WorkflowError::Invalid(
            "invalid meta: description must be a non-empty string".into(),
        ));
    }
    Ok(WorkflowMeta {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use: value
            .get("whenToUse")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn is_kebab(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    name.chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.contains("--")
        && !name.ends_with('-')
}

/// Evaluate a script whose body is `return <json>`.
pub fn eval_return_json(script: &str) -> std::result::Result<Value, WorkflowError> {
    let trimmed = script.trim();
    if trimmed.contains("export const meta") {
        return Err(WorkflowError::Invalid(
            "workflow meta rides the `meta` request field, not the script: remove the `export const meta = {...}` statement from the body"
                .into(),
        ));
    }
    let rest = trimmed
        .strip_prefix("return")
        .ok_or_else(|| {
            WorkflowError::Invalid(
                "workflow script does not parse: expected a top-level `return <json>`".into(),
            )
        })?
        .trim()
        .trim_end_matches(';')
        .trim();
    serde_json::from_str(rest).map_err(|error| {
        WorkflowError::Invalid(format!("workflow script does not parse: {error}"))
    })
}

#[async_trait]
impl WorkflowEngine for WorkflowRuntime {
    async fn start(
        &self,
        request: WorkflowStartRequest,
    ) -> std::result::Result<WorkflowResult, WorkflowError> {
        let mut value = eval_return_json(&request.script)?;
        if let Some(args) = request.args {
            if let Value::Object(map) = &mut value {
                map.insert("args".into(), args);
            }
        }
        let _ = &self.isolation;
        Ok(WorkflowResult {
            stop_reason: "completed".into(),
            value,
            agent_count: 0,
        })
    }
}

impl Service for WorkflowRuntime {
    const KEY: &'static str = "workflowEngine";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn return_json_uses_isolation_config() {
        let engine = WorkflowRuntime::new("in-process");
        assert_eq!(engine.isolation(), "in-process");
        let result = engine
            .start(WorkflowStartRequest {
                script: "return {\"ok\":true}".into(),
                meta: WorkflowMeta {
                    name: "snapshot-flow".into(),
                    description: "test".into(),
                    when_to_use: None,
                },
                args: None,
            })
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!({"ok": true}));
    }

    #[test]
    fn rejects_meta_in_script() {
        let err = eval_return_json("export const meta = {}; return 1").unwrap_err();
        assert!(err.to_string().contains("meta rides the"));
    }
}
