//! Filesystem seam (`ctx.fs`).

use async_trait::async_trait;
use dsh_cordis::Service;
use thiserror::Error;

/// Filesystem failures.
#[derive(Debug, Error)]
pub enum FsError {
    /// Host I/O failure.
    #[error("{0}")]
    Io(String),
    /// Sandbox policy denied this path.
    #[error("denied: {0}")]
    Denied(String),
}

/// Provider interface.
#[async_trait]
pub trait FsProvider: Send + Sync {
    /// Read a UTF-8 file.
    async fn read_text(&self, path: &str) -> Result<String, FsError>;
    /// Write a UTF-8 file, creating parents.
    async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError>;
}

/// `ctx.fs`.
pub struct FsRuntime {
    backend: std::sync::Arc<dyn FsProvider>,
}

impl FsRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn FsProvider>) -> Self {
        Self { backend }
    }

    /// Read text.
    pub async fn read_text(&self, path: &str) -> Result<String, FsError> {
        self.backend.read_text(path).await
    }

    /// Write text.
    pub async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.backend.write_text(path, content).await
    }
}

impl Service for FsRuntime {
    const KEY: &'static str = "fs";
}
