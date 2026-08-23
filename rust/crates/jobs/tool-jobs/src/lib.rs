//! Model-facing `job_output`, `job_list`, and `job_kill` over `ctx.jobs`.
//!
//! Loading attaches the controller producers require and delivers unreported
//! completions to the owning agent.

use async_trait::async_trait;
use dsh_agent::{AgentRegistry, AgentStatus};
use dsh_cordis::{Context, CordisError, Result};
use dsh_jobs::{status_line, JobRegistry, JobSnapshot};
use dsh_llm::{bound_context_summary, ContentBlock, MessageSource, UserMessage};
use dsh_session::session_id;
use dsh_system_prompt::{PromptContext, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const JOBS_SECTION: &str = "Track every background job id you start. You are notified in-session when a job finishes — do not busy-poll or sleep on one; keep working on independent steps and do not duplicate a running job's work. Before giving a final answer, collect every still-relevant job with job_output (set wait: true only when you are genuinely blocked on it), and job_kill jobs that stopped mattering.";

/// How an unreported completion reaches an idle owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDelivery {
    /// Open a turn on an idle owner.
    Wakeup,
    /// Leave the notice pending until something else wakes the owner.
    Quiet,
}

/// Bounded waits and completion-notice delivery.
#[derive(Debug, Clone)]
pub struct Config {
    /// Wait applied when `job_output` sets `wait` without `timeout_ms`.
    pub wait_timeout_ms: u64,
    /// Hard cap on any single wait.
    pub max_wait_timeout_ms: u64,
    /// Whether a completion opens a turn on an idle owner.
    pub completion_delivery: CompletionDelivery,
    /// Turns one owner may open by completion wakes before injection.
    pub max_consecutive_wakes: u32,
}

impl Config {
    /// Validate raw cordis.yml config. Omitted fields take TypeScript defaults.
    ///
    /// # Errors
    /// Non-positive timeouts, `waitTimeoutMs` above the cap, unknown
    /// `completionDelivery`, or a non-integer wake budget.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let wait_timeout_ms = positive(config, "waitTimeoutMs", 30_000)?;
        let max_wait_timeout_ms = positive(config, "maxWaitTimeoutMs", 600_000)?;
        if wait_timeout_ms > max_wait_timeout_ms {
            return Err(format!(
                "tool-jobs: waitTimeoutMs ({wait_timeout_ms}) exceeds maxWaitTimeoutMs ({max_wait_timeout_ms})"
            ));
        }
        let completion_delivery = match config
            .and_then(|value| value.get("completionDelivery"))
            .and_then(Value::as_str)
        {
            None | Some("wakeup") => CompletionDelivery::Wakeup,
            Some("quiet") => CompletionDelivery::Quiet,
            Some(other) => {
                return Err(format!(
                    "tool-jobs: completionDelivery must be \"wakeup\" or \"quiet\", got {other}"
                ));
            }
        };
        let max_consecutive_wakes = positive(config, "maxConsecutiveWakes", 3)? as u32;
        Ok(Self {
            wait_timeout_ms,
            max_wait_timeout_ms,
            completion_delivery,
            max_consecutive_wakes,
        })
    }
}

fn positive(config: Option<&Value>, key: &str, default: u64) -> std::result::Result<u64, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("tool-jobs: {key} must be a positive integer")),
    }
}

/// Install the three tools, the jobs prompt section, and completion delivery.
///
/// # Errors
/// Invalid Config, or a required service is missing.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let resolved = Config::resolve(config).map_err(CordisError::Validation)?;
    let jobs = ctx.service::<JobRegistry>()?;
    let tools = ctx.service::<ToolRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    let prompt = ctx.service::<SystemPrompt>()?;
    std::mem::forget(jobs.attach_controller());
    jobs.bind_agents(Arc::clone(&agents));
    prompt.register_context(PromptContext {
        name: "tool:jobs".into(),
        text: JOBS_SECTION.into(),
        order: 106,
    });
    let spent = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    if resolved.completion_delivery == CompletionDelivery::Wakeup {
        let spent = Arc::clone(&spent);
        ctx.on("agent/inbox/claimed", move |payload| {
            let kind = payload
                .pointer("/message/source/kind")
                .and_then(Value::as_str);
            let Some(agent_id) = payload.get("agentId").and_then(Value::as_str) else {
                return;
            };
            if kind == Some("user") {
                spent.lock().expect("wakes").remove(agent_id);
            }
        })?;
    }
    let delivery = resolved.completion_delivery;
    let wake_budget = resolved.max_consecutive_wakes;
    let agents_for_done = Arc::clone(&agents);
    let spent_for_done = Arc::clone(&spent);
    std::mem::forget(jobs.on_job_done(Arc::new(move |snapshot, owner| {
        if snapshot.reported {
            return;
        }
        let Some(owner_id) = owner else {
            return;
        };
        let Some(agent) = agents_for_done.get(&session_id(&owner_id)) else {
            return;
        };
        let message = UserMessage::from_parts(
            vec![ContentBlock::text(fit_completion_notice(&snapshot))],
            MessageSource::notice("tool-jobs", completion_summary(&snapshot)),
        );
        let spent = {
            let mut map = spent_for_done.lock().expect("wakes");
            *map.entry(owner_id.clone()).or_insert(0)
        };
        if delivery == CompletionDelivery::Wakeup
            && agent.status() == AgentStatus::Idle
            && spent < wake_budget
        {
            spent_for_done
                .lock()
                .expect("wakes")
                .insert(owner_id, spent + 1);
            agent.followup(message);
            return;
        }
        agent.inject(message);
    })));
    tools.insert(Arc::new(JobOutputTool {
        jobs: Arc::clone(&jobs),
        wait_default: resolved.wait_timeout_ms,
        wait_cap: resolved.max_wait_timeout_ms,
    }));
    tools.insert(Arc::new(JobListTool {
        jobs: Arc::clone(&jobs),
    }));
    tools.insert(Arc::new(JobKillTool {
        jobs: Arc::clone(&jobs),
    }));
    Ok(())
}

fn public_job(snapshot: &JobSnapshot) -> Value {
    let mut value = json!({
        "id": snapshot.id,
        "kind": snapshot.kind,
        "label": snapshot.label,
        "status": snapshot.status.to_string(),
        "startedAt": snapshot.started_at,
    });
    if let Some(detail) = &snapshot.detail {
        value["detail"] = json!(detail);
    }
    if let Some(finished) = snapshot.finished_at {
        value["finishedAt"] = json!(finished);
    }
    value
}

fn completion_summary(snapshot: &JobSnapshot) -> String {
    bound_context_summary(&format!(
        "{} {} {}",
        snapshot.kind,
        snapshot.label,
        status_line(snapshot.status, snapshot.detail.as_deref())
    ))
}

fn fit_completion_notice(snapshot: &JobSnapshot) -> String {
    format!(
        "background job {} ({}: {}) finished {}. Read its output with job_output.",
        snapshot.id,
        snapshot.kind,
        snapshot.label,
        status_line(snapshot.status, snapshot.detail.as_deref())
    )
}

fn validate_job_id(value: &Value) -> std::result::Result<String, ToolError> {
    let Some(id) = value.as_str() else {
        return Err(ToolError::Body(format!(
            "invalid job_id: expected a non-empty string, got {value}"
        )));
    };
    if id.is_empty() {
        return Err(ToolError::Body(format!(
            "invalid job_id: expected a non-empty string, got {}",
            json!(id)
        )));
    }
    Ok(id.to_string())
}

struct JobOutputTool {
    jobs: Arc<JobRegistry>,
    wait_default: u64,
    wait_cap: u64,
}

#[async_trait]
impl Tool for JobOutputTool {
    fn name(&self) -> &str {
        "job_output"
    }

    fn description(&self) -> &str {
        "Read a background job. Stream jobs return only output since the previous read; \
final-output jobs return their result after settlement. Every response ends with \
`[status: ...]`. Reads are non-blocking unless `wait: true`, which waits up to the configured cap."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Job id returned by the tool that started the background work."
                },
                "wait": {
                    "type": "boolean",
                    "description": "Block until the job reaches a terminal status or the timeout expires. A timed-out wait returns [status: running] and leaves the job alive."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Max wait in milliseconds (only meaningful with wait: true). Defaults to the configured wait timeout; capped by the configured maximum."
                }
            },
            "required": ["job_id"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let id = validate_job_id(call.args.get("job_id").unwrap_or(&Value::Null))?;
        let caller = call.agent_id.as_deref();
        if call.args.get("wait").and_then(Value::as_bool) == Some(true) {
            let timeout = call
                .args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(self.wait_default)
                .min(self.wait_cap);
            self.jobs
                .wait(&id, timeout, caller)
                .await
                .map_err(ToolError::Body)?;
        }
        let read = self.jobs.read(&id, caller).map_err(ToolError::Body)?;
        Ok(render_output(&read.text, &read.snapshot))
    }
}

fn render_output(text: &str, snapshot: &JobSnapshot) -> ToolOutcome {
    let body = if text.is_empty() {
        "(no new output)"
    } else {
        text
    };
    let separator = if body.ends_with('\n') { "" } else { "\n" };
    ToolOutcome::text(format!(
        "{body}{separator}{}",
        status_line(snapshot.status, snapshot.detail.as_deref())
    ))
}

struct JobListTool {
    jobs: Arc<JobRegistry>,
}

#[async_trait]
impl Tool for JobListTool {
    fn name(&self) -> &str {
        "job_list"
    }

    fn description(&self) -> &str {
        "List your background jobs (running and finished) with their ids, kinds, and statuses."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let jobs = self.jobs.list(call.agent_id.as_deref());
        let text = if jobs.is_empty() {
            "(no background jobs)".into()
        } else {
            jobs.iter()
                .map(|job| format!("{} [{}] {} — {}", job.id, job.kind, job.status, job.label))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let _ = jobs.iter().map(public_job).collect::<Vec<_>>();
        Ok(ToolOutcome::text(text))
    }
}

struct JobKillTool {
    jobs: Arc<JobRegistry>,
}

#[async_trait]
impl Tool for JobKillTool {
    fn name(&self) -> &str {
        "job_kill"
    }

    fn description(&self) -> &str {
        "Request cancellation of a running background job by job id. Returns immediately; the job settles as killed once its work actually stops."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Job id returned by the tool that started the background work."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional short reason, recorded in the log and forwarded to the job."
                }
            },
            "required": ["job_id"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let id = validate_job_id(call.args.get("job_id").unwrap_or(&Value::Null))?;
        let caller = call.agent_id.as_deref();
        let reason = call.args.get("reason").and_then(Value::as_str);
        let result = self
            .jobs
            .kill(&id, caller, reason)
            .map_err(ToolError::Body)?;
        let snapshot = self.jobs.get(&id, caller).map_err(ToolError::Body)?;
        let text = if result == "already-finished" {
            format!(
                "job {} had already finished {}",
                snapshot.id,
                status_line(snapshot.status, snapshot.detail.as_deref())
            )
        } else {
            format!("requested cancellation of job {}", snapshot.id)
        };
        Ok(ToolOutcome::text(text))
    }
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-tool-jobs"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_jobs::JobStatus;

    #[test]
    fn resolve_rejects_wait_above_cap() {
        let error = Config::resolve(Some(&json!({
            "waitTimeoutMs": 10,
            "maxWaitTimeoutMs": 5
        })))
        .unwrap_err();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn status_line_includes_detail() {
        assert_eq!(
            status_line(JobStatus::Completed, Some("exit code: 0")),
            "[status: completed, exit code: 0]"
        );
    }

    #[test]
    fn completion_notice_names_the_job() {
        let snapshot = JobSnapshot {
            id: "bash-1".into(),
            kind: "bash".into(),
            label: "echo hi".into(),
            output_limit_bytes: None,
            owner_session: None,
            status: JobStatus::Completed,
            detail: Some("exit code: 0".into()),
            started_at: 1,
            finished_at: Some(2),
            reported: false,
        };
        assert_eq!(
            fit_completion_notice(&snapshot),
            "background job bash-1 (bash: echo hi) finished [status: completed, exit code: 0]. Read its output with job_output."
        );
    }
}
