//! Subprocess seam (`ctx.subprocess`).

use async_trait::async_trait;
use dsh_cordis::Service;
use thiserror::Error;

/// Resolved spawn request. Defaulting happens in `resolve`, never inside `run`.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Program path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<String>,
}

/// Caller-facing request before resolve.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Program path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Optional cwd.
    pub cwd: Option<String>,
}

/// Completed process output.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Exit status.
    pub status: i32,
}

/// Spawn failures.
#[derive(Debug, Error)]
pub enum SubprocessError {
    /// OS spawn or wait failed.
    #[error("{0}")]
    Spawn(String),
}

/// Explicit resolve step.
pub fn resolve(request: SpawnRequest) -> SpawnSpec {
    SpawnSpec {
        program: request.program,
        args: request.args,
        cwd: request.cwd,
    }
}

/// Provider interface.
#[async_trait]
pub trait SubprocessExecutor: Send + Sync {
    /// Run one resolved spec.
    async fn run(&self, spec: SpawnSpec) -> Result<ProcessOutput, SubprocessError>;
}

/// `ctx.subprocess`.
pub struct SubprocessRuntime {
    backend: std::sync::Arc<dyn SubprocessExecutor>,
}

impl SubprocessRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn SubprocessExecutor>) -> Self {
        Self { backend }
    }

    /// Run a resolved spec.
    pub async fn run(&self, spec: SpawnSpec) -> Result<ProcessOutput, SubprocessError> {
        self.backend.run(spec).await
    }
}

impl Service for SubprocessRuntime {
    const KEY: &'static str = "subprocess";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_does_not_invent_a_cwd() {
        let spec = resolve(SpawnRequest {
            program: "echo".into(),
            args: vec![],
            cwd: None,
        });
        assert!(spec.cwd.is_none());
    }
}
