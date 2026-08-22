//! Shell executor seam (`ctx.shell`).

use async_trait::async_trait;
use dsh_cordis::Service;
use thiserror::Error;

/// Resolved shell spec.
#[derive(Debug, Clone)]
pub struct ShellSpec {
    /// Command string.
    pub command: String,
    /// Working directory.
    pub cwd: Option<String>,
}

/// Shell request before resolve.
#[derive(Debug, Clone)]
pub struct ShellRequest {
    /// Command string.
    pub command: String,
    /// Optional cwd.
    pub cwd: Option<String>,
}

/// Explicit resolve.
pub fn resolve(request: ShellRequest) -> ShellSpec {
    ShellSpec {
        command: request.command,
        cwd: request.cwd,
    }
}

/// Shell failures.
#[derive(Debug, Error)]
pub enum ShellError {
    /// Backend failure.
    #[error("{0}")]
    Failed(String),
    /// A confined mode was requested and no sandbox backend is usable.
    #[error("{0}")]
    Unavailable(String),
}

/// Provider interface.
#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Run one resolved spec.
    async fn run(&self, spec: ShellSpec) -> Result<String, ShellError>;
}

/// `ctx.shell`.
pub struct ShellRuntime {
    backend: std::sync::Arc<dyn ShellExecutor>,
}

impl ShellRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn ShellExecutor>) -> Self {
        Self { backend }
    }

    /// Run a resolved spec.
    pub async fn run(&self, spec: ShellSpec) -> Result<String, ShellError> {
        self.backend.run(spec).await
    }
}

impl Service for ShellRuntime {
    const KEY: &'static str = "shell";
}
