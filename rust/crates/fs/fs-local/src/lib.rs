//! Local filesystem provider.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{DirEntry, FsError, FsInfo, FsKind, FsProvider, FsRuntime};
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

    async fn exists(&self, path: &str) -> Result<bool, FsError> {
        Ok(self.stat(path).await?.is_some())
    }

    async fn stat(&self, path: &str) -> Result<Option<FsInfo>, FsError> {
        self.deny_if_needed(path)?;
        match fs::metadata(path).await {
            Ok(meta) => Ok(Some(FsInfo {
                kind: if meta.is_dir() {
                    FsKind::Directory
                } else if meta.is_file() {
                    FsKind::File
                } else {
                    FsKind::Other
                },
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FsError::Io(error.to_string())),
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        self.deny_if_needed(path)?;
        let mut entries = fs::read_dir(path)
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;
        let mut out = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => return Err(FsError::Io(error.to_string())),
            };
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| FsError::Io(error.to_string()))?;
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: if file_type.is_dir() {
                    FsKind::Directory
                } else if file_type.is_file() {
                    FsKind::File
                } else {
                    FsKind::Other
                },
            });
        }
        Ok(out)
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
        let err = fs.stat("/etc/passwd").await.unwrap_err();
        assert!(matches!(err, FsError::Denied(_)));
        let err = fs.list_dir("/etc").await.unwrap_err();
        assert!(matches!(err, FsError::Denied(_)));
    }

    #[tokio::test]
    async fn stat_and_list_round_trip() {
        let root = std::env::temp_dir().join(format!("dsh-fs-stat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let fs = LocalFs::new();
        let path = root.to_str().unwrap();
        assert!(fs.exists(path).await.unwrap());
        assert_eq!(
            fs.stat(path).await.unwrap().map(|info| info.kind),
            Some(FsKind::Directory)
        );
        assert_eq!(
            fs.stat(root.join("a.txt").to_str().unwrap())
                .await
                .unwrap()
                .map(|info| info.kind),
            Some(FsKind::File)
        );
        assert!(fs
            .stat(root.join("missing").to_str().unwrap())
            .await
            .unwrap()
            .is_none());
        let mut names: Vec<_> = fs
            .list_dir(path)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        assert_eq!(names, ["a.txt", "sub"]);
        let _ = std::fs::remove_dir_all(&root);
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
