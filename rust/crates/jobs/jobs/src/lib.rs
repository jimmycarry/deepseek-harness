//! Jobs seam (`ctx.jobs`).

use dsh_cordis::Service;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One registered background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Registry-issued id.
    pub id: String,
    /// Command or label supplied at start.
    pub command: String,
    /// Lifecycle status (`running`, `completed`).
    pub status: String,
}

/// `ctx.jobs`.
#[derive(Default)]
pub struct JobsRuntime {
    next: AtomicU64,
    jobs: Arc<Mutex<HashMap<String, Job>>>,
}

impl JobsRuntime {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a job and return its snapshot.
    pub fn start(&self, command: impl Into<String>) -> Job {
        let id = format!("job-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let job = Job {
            id: id.clone(),
            command: command.into(),
            status: "running".into(),
        };
        self.jobs.lock().expect("jobs").insert(id, job.clone());
        job
    }

    /// Look up a job by id.
    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.lock().expect("jobs").get(id).cloned()
    }

    /// Every registered job, newest last.
    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<_> = self.jobs.lock().expect("jobs").values().cloned().collect();
        jobs.sort_by(|left, right| left.id.cmp(&right.id));
        jobs
    }
}

impl Service for JobsRuntime {
    const KEY: &'static str = "jobs";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn start_then_get() {
        let jobs = JobsRuntime::new();
        let started = jobs.start("echo hi");
        assert_eq!(jobs.get(&started.id).unwrap().command, "echo hi");
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(JobsRuntime::new())).unwrap();
        assert!(ctx.has_service("jobs"));
        ctx.dispose();
        assert!(!ctx.has_service("jobs"));
    }
}
