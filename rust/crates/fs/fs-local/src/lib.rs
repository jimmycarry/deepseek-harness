//! Local filesystem provider.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{
    DirEntry, FsError, FsInfo, FsKind, FsProvider, FsRuntime, FsTarget, FsWriteIntent,
    FsWriteOutcome,
};
use dsh_sandbox::{SandboxPolicy, SandboxRuntime};
use std::sync::Arc;
use tokio::fs;

/// Host filesystem backend, optionally confined by a [`SandboxPolicy`].
pub struct LocalFs {
    sandbox: Option<Arc<dyn SandboxPolicy>>,
    write_lock: tokio::sync::Mutex<()>,
}

impl LocalFs {
    /// Unconfined host filesystem.
    pub fn new() -> Self {
        Self {
            sandbox: None,
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Confine every read and write through `sandbox`.
    pub fn with_sandbox(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self {
            sandbox: Some(sandbox),
            write_lock: tokio::sync::Mutex::new(()),
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

    async fn resolve(&self, path: &str) -> Result<FsTarget, FsError> {
        resolve_local_target(path)
    }

    async fn version_of(&self, target: &FsTarget) -> Result<Option<String>, FsError> {
        self.deny_if_needed(&target.target_key)?;
        Ok(probe_version(std::path::Path::new(&target.target_key)))
    }

    async fn write_intended(
        &self,
        target: &FsTarget,
        content: &str,
        intent: Option<FsWriteIntent>,
    ) -> Result<FsWriteOutcome, FsError> {
        self.deny_if_needed(&target.target_key)?;
        let _guard = self.write_lock.lock().await;
        let path = std::path::Path::new(&target.target_key);
        let existing = probe_file(path)?;
        if let Some(kind) = existing.as_ref().map(|info| info.0) {
            if kind != FsKind::File {
                return Err(FsError::not_regular(format!(
                    "cannot write \"{}\": not a regular file",
                    target.display_path
                )));
            }
        }
        match intent {
            Some(FsWriteIntent::ReplaceIfVersion { version }) => {
                let Some((_, current)) = existing.as_ref() else {
                    return Err(FsError::stale(format!(
                        "cannot write \"{}\": file no longer exists",
                        target.display_path
                    )));
                };
                if current != &version {
                    return Err(FsError::stale(format!(
                        "cannot write \"{}\": file changed since it was read",
                        target.display_path
                    )));
                }
            }
            Some(FsWriteIntent::CreateIfAbsent) if existing.is_some() => {
                return Err(FsError::not_observed(format!(
                    "cannot overwrite existing \"{}\" without reading it first",
                    target.display_path
                )));
            }
            _ => {}
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| FsError::Io(error.to_string()))?;
        }
        fs::write(path, content)
            .await
            .map_err(|error| FsError::Io(error.to_string()))?;
        let version = probe_version(path).ok_or_else(|| {
            FsError::Io(format!(
                "write succeeded but \"{}\" has no version",
                target.display_path
            ))
        })?;
        Ok(FsWriteOutcome {
            operation: if existing.is_some() {
                "update"
            } else {
                "create"
            },
            version,
        })
    }
}

fn resolve_local_target(path: &str) -> Result<FsTarget, FsError> {
    if path.trim().is_empty() {
        return Err(FsError::not_found("file_path must be a non-empty string"));
    }
    let display = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    match std::fs::canonicalize(&display) {
        Ok(real) => {
            return Ok(FsTarget::new(
                real.to_string_lossy().into_owned(),
                display.to_string_lossy().into_owned(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.raw_os_error() == Some(libc_enotdir()) => {
            return Err(FsError::not_found(format!(
                "cannot resolve \"{}\": a parent path segment is not a directory",
                display.display()
            )));
        }
        Err(error) => return Err(FsError::Io(error.to_string())),
    }
    let mut missing = vec![file_name(&display)];
    let mut ancestor = display
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| display.clone());
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(real) => {
                if !real.is_dir() {
                    return Err(FsError::not_found(format!(
                        "cannot resolve \"{}\": a parent path segment is not a directory",
                        display.display()
                    )));
                }
                let mut key = real;
                for part in missing {
                    key.push(part);
                }
                return Ok(FsTarget::new(
                    key.to_string_lossy().into_owned(),
                    display.to_string_lossy().into_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = ancestor
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| ancestor.clone());
                if parent == ancestor {
                    return Ok(FsTarget::new(
                        display.to_string_lossy().into_owned(),
                        display.to_string_lossy().into_owned(),
                    ));
                }
                missing.insert(0, file_name(&ancestor));
                ancestor = parent;
            }
            Err(error) if error.raw_os_error() == Some(libc_enotdir()) => {
                return Err(FsError::not_found(format!(
                    "cannot resolve \"{}\": a parent path segment is not a directory",
                    display.display()
                )));
            }
            Err(error) => return Err(FsError::Io(error.to_string())),
        }
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn libc_enotdir() -> i32 {
    20
}

fn probe_file(path: &std::path::Path) -> Result<Option<(FsKind, String)>, FsError> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let kind = if meta.is_dir() {
                FsKind::Directory
            } else if meta.is_file() {
                FsKind::File
            } else {
                FsKind::Other
            };
            Ok(Some((kind, version_of(&meta))))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FsError::Io(error.to_string())),
    }
}

fn probe_version(path: &std::path::Path) -> Option<String> {
    std::fs::metadata(path).ok().map(|meta| version_of(&meta))
}

fn version_of(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mtime = (meta.mtime() as i128) * 1_000_000_000 + i128::from(meta.mtime_nsec());
        let ctime = (meta.ctime() as i128) * 1_000_000_000 + i128::from(meta.ctime_nsec());
        format!(
            "{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.len(),
            mtime,
            ctime
        )
    }
    #[cfg(not(unix))]
    {
        format!(
            "{}:{}",
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        )
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

    #[tokio::test]
    async fn guarded_write_rejects_unobserved_overwrite() {
        let dir = std::env::temp_dir().join(format!("dsh-fs-cas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, "old").unwrap();
        let fs = LocalFs::new();
        let target = fs.resolve(path.to_str().unwrap()).await.unwrap();
        let err = fs
            .write_intended(&target, "new", Some(FsWriteIntent::CreateIfAbsent))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(dsh_fs::FsErrorCode::NotObserved));
        let version = fs.version_of(&target).await.unwrap().unwrap();
        fs.write_intended(
            &target,
            "new",
            Some(FsWriteIntent::ReplaceIfVersion { version }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let stale = fs
            .write_intended(
                &target,
                "newer",
                Some(FsWriteIntent::ReplaceIfVersion {
                    version: "not-current".into(),
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(stale.code(), Some(dsh_fs::FsErrorCode::StaleVersion));
        let _ = std::fs::remove_dir_all(&dir);
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
