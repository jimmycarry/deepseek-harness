//! Background-job Service Definition (`ctx.jobs`).
//!
//! The registry owns ids, owner fencing, lifecycle, and completion
//! listeners. Producers retain their execution resources and supply
//! [`JobHooks`] from a synchronous starter.

use dsh_agent::AgentRegistry;
use dsh_cordis::Service;
use dsh_session::session_id;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Timeout code that distinguishes a bounded wait from caller cancellation.
pub const TASK_WAIT_TIMEOUT: &str = "TASK_WAIT_TIMEOUT";

/// Task lifecycle: `running`, optionally `stopping`, then one terminal status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    /// Work is live.
    Running,
    /// Cancellation requested; producer has not settled.
    Stopping,
    /// Producer finished.
    Completed,
    /// Producer was cancelled.
    Killed,
    /// Producer broke.
    Failed,
}

impl JobStatus {
    /// Whether this status is one of the three terminals.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::Failed => "failed",
        })
    }
}

/// Terminal result supplied by a producer through [`JobHooks::wait_done`].
#[derive(Debug, Clone)]
pub struct JobOutcome {
    /// `completed`, `killed`, or `failed`.
    pub status: JobStatus,
    /// Kind-specific detail (`exit code: 0`, `max-tokens`).
    pub detail: Option<String>,
    /// Final output for jobs without `read_output`.
    pub output: Option<String>,
}

/// Hooks through which the runtime controls and observes producer work.
pub struct JobHooks {
    /// Request termination. Must be synchronous and idempotent.
    pub cancel: Arc<dyn Fn(Option<String>) + Send + Sync>,
    /// Block until the producer releases its resources. Must not panic.
    pub wait_done: Arc<dyn Fn() -> JobOutcome + Send + Sync>,
    /// Consume output since the previous call. Absence marks a final-output job.
    pub read_output: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

/// Producer declaration passed to [`JobRegistry::start`].
pub struct JobStart {
    /// Producer kind — also the id prefix (`bash`, `subagent`).
    pub kind: String,
    /// One-line model-facing label.
    pub label: String,
    /// Optional UTF-8 byte cap for model-facing notices and output reads.
    pub output_limit_bytes: Option<usize>,
    /// Owner session id; omitted for an unowned job.
    pub owner_session: Option<String>,
    /// Start work after preflight. A throw leaves nothing registered.
    pub run: Box<dyn FnOnce() -> Result<JobHooks, String> + Send>,
}

/// Read-only projection of one job. A fresh object per call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSnapshot {
    /// Registry-issued id (`<kind>-N`).
    pub id: String,
    /// Producer kind.
    pub kind: String,
    /// Producer-supplied label.
    pub label: String,
    /// Producer-owned cap, when declared.
    #[serde(rename = "outputLimitBytes", skip_serializing_if = "Option::is_none")]
    pub output_limit_bytes: Option<usize>,
    /// Owner session id; absent for unowned jobs.
    #[serde(rename = "ownerSession", skip_serializing_if = "Option::is_none")]
    pub owner_session: Option<String>,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Kind-specific status detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Epoch ms when the job was registered.
    #[serde(rename = "startedAt")]
    pub started_at: u64,
    /// Epoch ms when the job settled.
    #[serde(rename = "finishedAt", skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Whether a reader has already been told the terminal state.
    pub reported: bool,
}

/// Output and post-read state returned by [`JobRegistry::read`].
#[derive(Debug, Clone)]
pub struct JobRead {
    /// Stream delta, or the idempotent final output after settlement.
    pub text: String,
    /// Job state at read time.
    pub snapshot: JobSnapshot,
}

/// `ctx.jobs`.
pub struct JobRegistry {
    max_concurrent: usize,
    inner: Arc<Mutex<Inner>>,
    settled: Arc<Condvar>,
    agents: Mutex<Option<Arc<AgentRegistry>>>,
}

struct Inner {
    store: HashMap<String, Tracked>,
    counters: HashMap<String, u32>,
    controllers: usize,
    done: Vec<Arc<dyn Fn(JobSnapshot, Option<String>) + Send + Sync>>,
    changed: Vec<Arc<dyn Fn(Option<String>) + Send + Sync>>,
    listeners_closed: bool,
}

struct Tracked {
    id: String,
    kind: String,
    label: String,
    output_limit_bytes: Option<usize>,
    owner_session: Option<String>,
    cancel: Arc<dyn Fn(Option<String>) + Send + Sync>,
    read_output: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    status: JobStatus,
    detail: Option<String>,
    output: Option<String>,
    started_at: u64,
    finished_at: Option<u64>,
    reported: bool,
    waiters: usize,
}

impl JobRegistry {
    /// Empty registry with a positive per-owner active-job cap.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            inner: Arc::new(Mutex::new(Inner {
                store: HashMap::new(),
                counters: HashMap::new(),
                controllers: 0,
                done: Vec::new(),
                changed: Vec::new(),
                listeners_closed: false,
            })),
            settled: Arc::new(Condvar::new()),
            agents: Mutex::new(None),
        }
    }

    /// Bind the live agent registry used to fence owned starts.
    pub fn bind_agents(&self, agents: Arc<AgentRegistry>) {
        *self.agents.lock().expect("jobs agents") = Some(agents);
    }

    /// Attach a controller. `start` refuses work while none is attached.
    pub fn attach_controller(&self) -> impl FnOnce() + Send + 'static {
        let inner = Arc::clone(&self.inner);
        inner.lock().expect("jobs").controllers += 1;
        move || {
            let mut guard = inner.lock().expect("jobs");
            guard.controllers = guard.controllers.saturating_sub(1);
        }
    }

    /// Register a completion listener. Contained; never awaited.
    pub fn on_job_done(
        &self,
        listener: Arc<dyn Fn(JobSnapshot, Option<String>) + Send + Sync>,
    ) -> impl FnOnce() + Send + 'static {
        let inner = Arc::clone(&self.inner);
        inner.lock().expect("jobs").done.push(Arc::clone(&listener));
        move || {
            let mut guard = inner.lock().expect("jobs");
            guard.done.retain(|item| !Arc::ptr_eq(item, &listener));
        }
    }

    /// Register a visible-set observer.
    pub fn on_jobs_changed(
        &self,
        listener: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> impl FnOnce() + Send + 'static {
        let inner = Arc::clone(&self.inner);
        inner
            .lock()
            .expect("jobs")
            .changed
            .push(Arc::clone(&listener));
        move || {
            let mut guard = inner.lock().expect("jobs");
            guard.changed.retain(|item| !Arc::ptr_eq(item, &listener));
        }
    }

    /// Preflight, start, and register. A throwing starter leaves nothing.
    ///
    /// # Errors
    /// Missing controller, invalid spec, owner not live, concurrency cap,
    /// or a throwing starter.
    pub fn start(&self, spec: JobStart) -> Result<String, String> {
        if spec.kind.is_empty() {
            return Err("invalid job kind: expected a non-empty string".into());
        }
        if spec.label.is_empty() {
            return Err("invalid job label: expected a non-empty string".into());
        }
        if let Some(limit) = spec.output_limit_bytes {
            if limit == 0 {
                return Err(format!(
                    "invalid outputLimitBytes: expected a positive safe integer, got {limit}"
                ));
            }
        }
        {
            let guard = self.inner.lock().expect("jobs");
            if guard.controllers == 0 {
                return Err(
                    "background jobs unavailable: no job controller serves this agent (load @deepseek-ai/dsh-tool-jobs in its composition)"
                        .into(),
                );
            }
            let active = guard
                .store
                .values()
                .filter(|job| {
                    job.owner_session == spec.owner_session
                        && (job.status == JobStatus::Running || job.status == JobStatus::Stopping)
                })
                .count();
            if active >= self.max_concurrent {
                return Err(format!(
                    "background job limit reached for this owner (limit: {}); use job_kill to stop an unneeded job, wait for it to finish, then retry",
                    self.max_concurrent
                ));
            }
        }
        if let Some(owner) = spec.owner_session.as_ref() {
            let agents = self.agents.lock().expect("jobs agents");
            let Some(registry) = agents.as_ref() else {
                return Err(
                    "background job ownership requires the agent registry (load @deepseek-ai/dsh-agent)"
                        .into(),
                );
            };
            if registry.get(&session_id(owner.as_str())).is_none() {
                return Err(format!(
                    "agent \"{owner}\" is not the registered agent instance (background job owner must be live)"
                ));
            }
        }
        let hooks = (spec.run)()?;
        let id = {
            let mut guard = self.inner.lock().expect("jobs");
            let count = guard.counters.get(&spec.kind).copied().unwrap_or(0) + 1;
            guard.counters.insert(spec.kind.clone(), count);
            format!("{}-{count}", spec.kind)
        };
        let owner = spec.owner_session.clone();
        {
            let mut guard = self.inner.lock().expect("jobs");
            guard.store.insert(
                id.clone(),
                Tracked {
                    id: id.clone(),
                    kind: spec.kind,
                    label: spec.label,
                    output_limit_bytes: spec.output_limit_bytes,
                    owner_session: spec.owner_session,
                    cancel: hooks.cancel,
                    read_output: hooks.read_output,
                    status: JobStatus::Running,
                    detail: None,
                    output: None,
                    started_at: now_ms(),
                    finished_at: None,
                    reported: false,
                    waiters: 0,
                },
            );
        }
        self.notify_changed(owner.clone());
        let wait_done = hooks.wait_done;
        let registry = Arc::clone(&self.inner);
        let settled = Arc::clone(&self.settled);
        let job_id = id.clone();
        std::thread::Builder::new()
            .name(format!("dsh-job-{job_id}"))
            .spawn(move || {
                let outcome = wait_done();
                settle(&registry, &settled, &job_id, outcome);
            })
            .map_err(|error| error.to_string())?;
        Ok(id)
    }

    /// Caller-owned and unowned jobs in registration order.
    pub fn list(&self, caller: Option<&str>) -> Vec<JobSnapshot> {
        let guard = self.inner.lock().expect("jobs");
        let mut jobs: Vec<_> = guard
            .store
            .values()
            .filter(|job| job.owner_session.is_none() || job.owner_session.as_deref() == caller)
            .collect();
        jobs.sort_by_key(|job| job.started_at);
        jobs.iter().map(|job| snapshot(job)).collect()
    }

    /// Non-consuming snapshot.
    ///
    /// # Errors
    /// Unknown or foreign job.
    pub fn get(&self, id: &str, caller: Option<&str>) -> Result<JobSnapshot, String> {
        let guard = self.inner.lock().expect("jobs");
        let job = expect(&guard, id)?;
        assert_access(job, caller)?;
        Ok(snapshot(job))
    }

    /// Read the next stream delta, or the idempotent final output.
    ///
    /// # Errors
    /// Unknown or foreign job.
    pub fn read(&self, id: &str, caller: Option<&str>) -> Result<JobRead, String> {
        let mut guard = self.inner.lock().expect("jobs");
        let job = expect_mut(&mut guard, id)?;
        assert_access(job, caller)?;
        let text = if let Some(read) = &job.read_output {
            read()
        } else if job.status.is_terminal() {
            job.output.clone().unwrap_or_default()
        } else {
            String::new()
        };
        if job.status.is_terminal() {
            job.reported = true;
        }
        Ok(JobRead {
            text,
            snapshot: snapshot(job),
        })
    }

    /// Request cancellation. Returns `requested` or `already-finished`.
    ///
    /// # Errors
    /// Unknown or foreign job. A throwing cancel leaves state unchanged.
    pub fn kill(
        &self,
        id: &str,
        caller: Option<&str>,
        reason: Option<&str>,
    ) -> Result<&'static str, String> {
        let cancel;
        let owner;
        {
            let mut guard = self.inner.lock().expect("jobs");
            let job = expect_mut(&mut guard, id)?;
            assert_access(job, caller)?;
            if job.status.is_terminal() {
                job.reported = true;
                return Ok("already-finished");
            }
            cancel = Arc::clone(&job.cancel);
            owner = job.owner_session.clone();
        }
        cancel(reason.map(str::to_string));
        {
            let mut guard = self.inner.lock().expect("jobs");
            if let Some(job) = guard.store.get_mut(id) {
                if !job.status.is_terminal() {
                    job.status = JobStatus::Stopping;
                    job.reported = true;
                }
            }
        }
        self.notify_changed(owner);
        Ok("requested")
    }

    /// Wait for settlement or timeout without cancelling the job.
    ///
    /// # Errors
    /// Invalid timeout, unknown job, or foreign job.
    pub async fn wait(
        &self,
        id: &str,
        timeout_ms: u64,
        caller: Option<&str>,
    ) -> Result<JobSnapshot, String> {
        if timeout_ms == 0 {
            return Err(format!(
                "invalid wait timeout: expected a positive number of milliseconds, got {timeout_ms}"
            ));
        }
        {
            let mut guard = self.inner.lock().expect("jobs");
            let job = expect_mut(&mut guard, id)?;
            assert_access(job, caller)?;
            if job.status.is_terminal() {
                job.reported = true;
                return Ok(snapshot(job));
            }
            job.waiters += 1;
        }
        let inner = Arc::clone(&self.inner);
        let settled = Arc::clone(&self.settled);
        let job_id = id.to_string();
        let timeout = Duration::from_millis(timeout_ms);
        let joined = tokio::task::spawn_blocking(move || {
            let guard = inner.lock().expect("jobs");
            let (guard, _) = settled
                .wait_timeout_while(guard, timeout, |inner| {
                    inner
                        .store
                        .get(&job_id)
                        .is_some_and(|job| !job.status.is_terminal())
                })
                .expect("jobs wait");
            drop(guard);
        });
        let _ = joined.await;
        let mut guard = self.inner.lock().expect("jobs");
        let job = expect_mut(&mut guard, id)?;
        job.waiters = job.waiters.saturating_sub(1);
        if job.status.is_terminal() {
            job.reported = true;
        }
        Ok(snapshot(job))
    }

    fn notify_changed(&self, owner: Option<String>) {
        let listeners = {
            let guard = self.inner.lock().expect("jobs");
            if guard.listeners_closed {
                return;
            }
            guard.changed.clone()
        };
        for listener in listeners {
            listener(owner.clone());
        }
    }
}

impl Service for JobRegistry {
    const KEY: &'static str = "jobs";
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn expect<'a>(inner: &'a Inner, id: &str) -> Result<&'a Tracked, String> {
    inner
        .store
        .get(id)
        .ok_or_else(|| format!("unknown job {id}"))
}

fn expect_mut<'a>(inner: &'a mut Inner, id: &str) -> Result<&'a mut Tracked, String> {
    inner
        .store
        .get_mut(id)
        .ok_or_else(|| format!("unknown job {id}"))
}

fn assert_access(job: &Tracked, caller: Option<&str>) -> Result<(), String> {
    if job.owner_session.is_some() && job.owner_session.as_deref() != caller {
        return Err(format!("job {} belongs to another session", job.id));
    }
    Ok(())
}

fn snapshot(job: &Tracked) -> JobSnapshot {
    JobSnapshot {
        id: job.id.clone(),
        kind: job.kind.clone(),
        label: job.label.clone(),
        output_limit_bytes: job.output_limit_bytes,
        owner_session: job.owner_session.clone(),
        status: job.status,
        detail: job.detail.clone(),
        started_at: job.started_at,
        finished_at: job.finished_at,
        reported: job.reported,
    }
}

fn settle(inner: &Mutex<Inner>, settled: &Condvar, id: &str, outcome: JobOutcome) {
    let (snapshot_value, owner, listeners) = {
        let mut guard = inner.lock().expect("jobs");
        let Some(job) = guard.store.get_mut(id) else {
            return;
        };
        if job.status.is_terminal() {
            return;
        }
        let status = match outcome.status {
            JobStatus::Completed | JobStatus::Killed | JobStatus::Failed => outcome.status,
            _ => JobStatus::Failed,
        };
        job.status = status;
        job.detail = outcome.detail;
        job.output = outcome.output;
        job.finished_at = Some(now_ms());
        if job.waiters > 0 {
            job.reported = true;
        }
        let snapshot_value = snapshot(job);
        let owner = job.owner_session.clone();
        let listeners = if guard.listeners_closed {
            Vec::new()
        } else {
            guard.done.clone()
        };
        settled.notify_all();
        (snapshot_value, owner, listeners)
    };
    {
        let changed = inner.lock().expect("jobs").changed.clone();
        for listener in changed {
            listener(owner.clone());
        }
    }
    for listener in listeners {
        listener(snapshot_value.clone(), owner.clone());
    }
}

/// Render generic status with optional producer detail.
pub fn status_line(status: JobStatus, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("[status: {status}, {detail}]"),
        None => format!("[status: {status}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    fn hooks_completed(output: &str) -> JobHooks {
        let text = output.to_string();
        JobHooks {
            cancel: Arc::new(|_| {}),
            wait_done: Arc::new(move || JobOutcome {
                status: JobStatus::Completed,
                detail: Some("exit code: 0".into()),
                output: Some(text.clone()),
            }),
            read_output: None,
        }
    }

    #[test]
    fn start_requires_a_controller() {
        let jobs = JobRegistry::new(10);
        let error = jobs
            .start(JobStart {
                kind: "bash".into(),
                label: "echo".into(),
                output_limit_bytes: None,
                owner_session: None,
                run: Box::new(|| Ok(hooks_completed("hi"))),
            })
            .unwrap_err();
        assert!(error.contains("no job controller"));
    }

    #[test]
    fn start_issues_kind_prefixed_ids() {
        let jobs = JobRegistry::new(10);
        let _detach = jobs.attach_controller();
        let first = jobs
            .start(JobStart {
                kind: "bash".into(),
                label: "echo a".into(),
                output_limit_bytes: None,
                owner_session: None,
                run: Box::new(|| Ok(hooks_completed("a"))),
            })
            .unwrap();
        let second = jobs
            .start(JobStart {
                kind: "bash".into(),
                label: "echo b".into(),
                output_limit_bytes: None,
                owner_session: None,
                run: Box::new(|| Ok(hooks_completed("b"))),
            })
            .unwrap();
        assert_eq!(first, "bash-1");
        assert_eq!(second, "bash-2");
    }

    #[test]
    fn unknown_job_fails_loud() {
        let jobs = JobRegistry::new(10);
        let error = jobs.get("bash-9", None).unwrap_err();
        assert_eq!(error, "unknown job bash-9");
    }

    #[test]
    fn owned_job_is_fenced() {
        let jobs = JobRegistry::new(10);
        let _detach = jobs.attach_controller();
        let ctx = Context::new();
        let agents = Arc::new(AgentRegistry::new());
        ctx.provide(Arc::clone(&agents)).unwrap();
        jobs.bind_agents(agents);
        let error = jobs
            .start(JobStart {
                kind: "bash".into(),
                label: "echo".into(),
                output_limit_bytes: None,
                owner_session: Some("missing".into()),
                run: Box::new(|| Ok(hooks_completed("x"))),
            })
            .unwrap_err();
        assert!(error.contains("is not the registered agent instance"));
    }

    #[tokio::test]
    async fn wait_observes_settlement() {
        let jobs = JobRegistry::new(10);
        let _detach = jobs.attach_controller();
        let id = jobs
            .start(JobStart {
                kind: "bash".into(),
                label: "echo".into(),
                output_limit_bytes: None,
                owner_session: None,
                run: Box::new(|| Ok(hooks_completed("hi"))),
            })
            .unwrap();
        let snapshot = jobs.wait(&id, 5_000, None).await.unwrap();
        assert_eq!(snapshot.status, JobStatus::Completed);
        let read = jobs.read(&id, None).unwrap();
        assert_eq!(read.text, "hi");
        assert_eq!(
            status_line(read.snapshot.status, read.snapshot.detail.as_deref()),
            "[status: completed, exit code: 0]"
        );
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(JobRegistry::new(10))).unwrap();
        assert!(ctx.has_service("jobs"));
        ctx.dispose();
        assert!(!ctx.has_service("jobs"));
    }
}
