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

/// File or directory kind returned by [`FsProvider::stat`] and [`FsProvider::list_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Neither a regular file nor a directory (symlink to a special, socket, …).
    Other,
}

/// Stat result for an existing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInfo {
    /// File, directory, or other.
    pub kind: FsKind,
}

/// One directory entry from [`FsProvider::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Basename, not a path.
    pub name: String,
    /// Entry kind.
    pub kind: FsKind,
}

/// Provider interface.
#[async_trait]
pub trait FsProvider: Send + Sync {
    /// Read a UTF-8 file.
    async fn read_text(&self, path: &str) -> Result<String, FsError>;
    /// Write a UTF-8 file, creating parents.
    async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError>;
    /// Whether `path` exists. Denied paths are [`FsError::Denied`], not `false`.
    async fn exists(&self, path: &str) -> Result<bool, FsError>;
    /// Stat `path`. `Ok(None)` means the path is absent; denied paths are [`FsError::Denied`].
    async fn stat(&self, path: &str) -> Result<Option<FsInfo>, FsError>;
    /// List immediate children of a directory.
    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError>;
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

    /// Whether `path` exists.
    pub async fn exists(&self, path: &str) -> Result<bool, FsError> {
        self.backend.exists(path).await
    }

    /// Stat `path`.
    pub async fn stat(&self, path: &str) -> Result<Option<FsInfo>, FsError> {
        self.backend.stat(path).await
    }

    /// List immediate children of a directory.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        self.backend.list_dir(path).await
    }
}

impl Service for FsRuntime {
    const KEY: &'static str = "fs";
}
