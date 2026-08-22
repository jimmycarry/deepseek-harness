//! Model-facing job_* tools.

use async_trait::async_trait;
use dsh_jobs::JobsRuntime;
use dsh_tools::{Tool, ToolError, ToolOutcome};
use serde_json::Value;
use std::sync::Arc;

/// `job_start` over [`JobsRuntime`].
pub struct JobsStartTool {
    jobs: Arc<JobsRuntime>,
}

impl JobsStartTool {
    /// Bind to `ctx.jobs`.
    pub fn new(jobs: Arc<JobsRuntime>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobsStartTool {
    fn name(&self) -> &str {
        "job_start"
    }

    fn description(&self) -> &str {
        "Start a background job and return its id."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("command required".into()))?;
        let job = self.jobs.start(command);
        Ok(ToolOutcome::text(job.id))
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-jobs"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_a_job() {
        let jobs = Arc::new(JobsRuntime::new());
        let tool = JobsStartTool::new(Arc::clone(&jobs));
        let outcome = tool
            .execute(serde_json::json!({ "command": "echo hi" }))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert_eq!(jobs.list().len(), 1);
    }
}
