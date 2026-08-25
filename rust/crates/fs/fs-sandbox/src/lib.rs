//! Sandbox-enforcing filesystem provider. Writes are fenced by the standing
//! file policy: `read-only` denies, `workspace-write` requires containment
//! under the workspace root, and `danger-full-access` is unfenced.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_fs::{
    DirEntry, FsError, FsInfo, FsProvider, FsRuntime, FsTarget, FsWriteIntent, FsWriteOutcome,
    FsWritePolicy,
};
use dsh_fs_local::LocalFs;
use dsh_sandbox_local::allow_path;
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "fs-sandbox"
}

/// Standing file-effect policy used for every mutation.
#[derive(Debug, Clone)]
pub struct Config {
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    pub mode: String,
    /// Absolute workspace root `workspace-write` may write under.
    pub workspace_root: String,
}

/// Local backend wrapped by the standing file policy.
pub struct SandboxedFs {
    inner: LocalFs,
    mode: String,
    workspace_root: String,
}

impl SandboxedFs {
    /// Wrap an unconfined local backend.
    pub fn new(mode: impl Into<String>, workspace_root: impl Into<String>) -> Self {
        Self {
            inner: LocalFs::new(),
            mode: mode.into(),
            workspace_root: workspace_root.into(),
        }
    }

    fn check_write(&self, target: &FsTarget) -> std::result::Result<(), FsError> {
        self.check_write_with(&self.mode, &self.workspace_root, target)
    }

    fn check_write_with(
        &self,
        mode: &str,
        workspace_root: &str,
        target: &FsTarget,
    ) -> std::result::Result<(), FsError> {
        match mode {
            "danger-full-access" => Ok(()),
            "read-only" => Err(FsError::Denied(format!(
                "cannot write \"{}\": file access denied under read-only mode",
                target.display_path
            ))),
            _ => {
                if allow_path(workspace_root, &target.target_key) {
                    Ok(())
                } else {
                    Err(FsError::Denied(format!(
                        "cannot write \"{}\": file access denied under workspace-write mode",
                        target.display_path
                    )))
                }
            }
        }
    }
}

#[async_trait]
impl FsProvider for SandboxedFs {
    async fn read_text(&self, path: &str) -> std::result::Result<String, FsError> {
        self.inner.read_text(path).await
    }

    async fn write_text(&self, path: &str, content: &str) -> std::result::Result<(), FsError> {
        let target = self.inner.resolve(path).await?;
        self.check_write(&target)?;
        self.inner.write_text(path, content).await
    }

    async fn exists(&self, path: &str) -> std::result::Result<bool, FsError> {
        self.inner.exists(path).await
    }

    async fn stat(&self, path: &str) -> std::result::Result<Option<FsInfo>, FsError> {
        self.inner.stat(path).await
    }

    async fn list_dir(&self, path: &str) -> std::result::Result<Vec<DirEntry>, FsError> {
        self.inner.list_dir(path).await
    }

    async fn resolve(&self, path: &str) -> std::result::Result<FsTarget, FsError> {
        self.inner.resolve(path).await
    }

    async fn version_of(&self, target: &FsTarget) -> std::result::Result<Option<String>, FsError> {
        self.inner.version_of(target).await
    }

    async fn write_intended(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
    ) -> std::result::Result<FsWriteOutcome, FsError> {
        self.check_write(target)?;
        self.inner.write_intended(target, content, intent).await
    }

    async fn write_intended_with_policy(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
        policy: Option<&FsWritePolicy>,
    ) -> std::result::Result<FsWriteOutcome, FsError> {
        match policy {
            Some(policy) => {
                self.check_write_with(&policy.mode, &policy.workspace_root, target)?;
                self.inner.write_intended(target, content, intent).await
            }
            None => self.write_intended(target, content, intent).await,
        }
    }
}

/// Provide `ctx.fs` as the sandboxed local backend, replacing a prior mount.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let runtime = Arc::new(FsRuntime::new(Arc::new(SandboxedFs::new(
        config.mode,
        config.workspace_root,
    ))));
    ctx.provide(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-fs-sandbox-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn read_only_denies_write() {
        let dir = unique_dir("ro");
        let path = dir.join("a.txt");
        std::fs::write(&path, "old").unwrap();
        let fs = SandboxedFs::new("read-only", dir.to_string_lossy().into_owned());
        let target = fs.resolve(path.to_str().unwrap()).await.unwrap();
        let err = fs
            .write_intended(&target, "new", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read-only mode"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_write_denies_escape() {
        let dir = unique_dir("ww");
        let outside = std::env::temp_dir().join(format!(
            "dsh-fs-sandbox-escape-{}",
            std::process::id()
        ));
        std::fs::write(&outside, "keep").unwrap();
        let fs = SandboxedFs::new("workspace-write", dir.to_string_lossy().into_owned());
        let target = fs.resolve(outside.to_str().unwrap()).await.unwrap();
        let err = fs
            .write_intended(&target, "stolen", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace-write mode"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_write_allows_inside() {
        let dir = unique_dir("ok");
        let path = dir.join("note.txt");
        let fs = SandboxedFs::new("workspace-write", dir.to_string_lossy().into_owned());
        let target = fs.resolve(path.to_str().unwrap()).await.unwrap();
        fs.write_intended(&target, "hello", Some(FsWriteIntent::CreateIfAbsent))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn danger_full_access_is_unfenced() {
        let dir = unique_dir("full");
        let outside = std::env::temp_dir().join(format!(
            "dsh-fs-sandbox-full-{}",
            std::process::id()
        ));
        let fs = SandboxedFs::new("danger-full-access", dir.to_string_lossy().into_owned());
        let target = fs.resolve(outside.to_str().unwrap()).await.unwrap();
        fs.write_intended(&target, "ok", Some(FsWriteIntent::CreateIfAbsent))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "ok");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
