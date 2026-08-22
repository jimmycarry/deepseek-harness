//! Local filesystem provider.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{FsError, FsProvider, FsRuntime};
use dsh_sandbox::{SandboxPolicy, SandboxRuntime};
use std::sync::Arc;
use tokio::fs;

/// Host filesystem backend, optionally confined by a [`SandboxPolicy`].
pub struct LocalFs {
    sandbox: Option<Arc<dyn SandboxPolicy>>,
}

impl LocalFs {
    /// Unconfined host filesystem.
    pub fn new() -> Self {
        Self { sandbox: None }
    }

    /// Confine every read and write through `sandbox`.
    pub fn with_sandbox(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self {
            sandbox: Some(sandbox),
        }
    }

    fn deny_if_needed(&self, path: &str) -> Result<(), FsError> {
        if let Some(sandbox) = &self.sandbox {
            if !sandbox.allow_path(path) {
                return Err(FsError::Denied(path.to_string()));
            }
        }
        Ok(())
    }
}

impl Default for LocalFs {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FsProvider for LocalFs {
    async fn read_text(&self, path: &str) -> Result<String, FsError> {
        self.deny_if_needed(path)?;
        fs::read_to_string(path)
            .await
            .map_err(|error| FsError::Io(error.to_string()))
    }

    async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.deny_if_needed(path)?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| FsError::Io(error.to_string()))?;
        }
        fs::write(path, content)
            .await
            .map_err(|error| FsError::Io(error.to_string()))
    }
}

/// Provide `ctx.fs`, wrapping the optional `ctx.sandbox` policy when present.
pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<FsRuntime>> {
    let backend = match ctx.get::<SandboxRuntime>() {
        Some(sandbox) => Arc::new(LocalFs::with_sandbox(sandbox.policy())),
        None => Arc::new(LocalFs::new()),
    };
    let runtime = Arc::new(FsRuntime::new(backend));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sandbox_local::CwdSandbox;

    #[tokio::test]
    async fn write_then_external_read() {
        let path = std::env::temp_dir().join(format!("dsh-fs-{}.txt", std::process::id()));
        LocalFs::new()
            .write_text(path.to_str().unwrap(), "hello")
            .await
            .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "hello");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn sandbox_rejects_etc_passwd() {
        let fs = LocalFs::with_sandbox(Arc::new(CwdSandbox::new("/tmp/workspace")));
        let err = fs.read_text("/etc/passwd").await.unwrap_err();
        assert!(matches!(err, FsError::Denied(path) if path == "/etc/passwd"));
        let err = fs.write_text("/etc/passwd", "nope").await.unwrap_err();
        assert!(matches!(err, FsError::Denied(path) if path == "/etc/passwd"));
    }

    #[test]
    fn install_provides_fs() {
        let ctx = Context::new();
        install(&ctx).unwrap();
        assert!(ctx.has_service("fs"));
        ctx.dispose();
        assert!(!ctx.has_service("fs"));
    }
}
