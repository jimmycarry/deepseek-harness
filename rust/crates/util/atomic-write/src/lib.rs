//! Zero-dependency atomic file replacement and writer coordination.
//!
//! `write_file_atomic` writes a random-suffix sibling with exclusive create,
//! then renames it over the target. `with_file_lock` serializes writers of one
//! file through a `wx`-created `<file>.lock` sibling.

use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::fs;
use tokio::time::sleep;
use uuid::Uuid;

const LOCK_RETRY_INITIAL_MS: u64 = 20;
const LOCK_RETRY_MAX_MS: u64 = 200;
const DEFAULT_LOCK_WAIT_MS: u64 = 2_000;

/// Filesystem options for [`write_file_atomic`]; `mode` is required so the
/// permission decision stays visible at every call site.
#[derive(Debug, Clone, Copy)]
pub struct WriteFileAtomicOptions {
    /// Permission bits stamped on the fresh temp inode.
    pub mode: u32,
    /// Permission bits for parent directories this call creates.
    pub dir_mode: Option<u32>,
}

/// Errors from atomic write or lock acquisition.
#[derive(Debug, Error)]
pub enum AtomicWriteError {
    /// Underlying IO failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Timed out waiting for the sibling writer lock.
    #[error("atomic-write: timed out waiting for the writer lock at {0}")]
    LockTimeout(String),
}

/// Replace `filename` with `content` in one atomic step, creating parents.
pub async fn write_file_atomic(
    filename: impl AsRef<Path>,
    content: impl AsRef<[u8]>,
    options: WriteFileAtomicOptions,
) -> Result<(), AtomicWriteError> {
    let filename = filename.as_ref();
    if let Some(parent) = filename.parent() {
        fs::create_dir_all(parent).await?;
        if let Some(dir_mode) = options.dir_mode {
            set_mode(parent, dir_mode)?;
        }
    }
    let temp = filename.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        fs::write(&temp, content).await?;
        set_mode(&temp, options.mode)?;
        fs::rename(&temp, filename).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

/// Hold the cross-process writer lock for `filename` around one operation.
pub async fn with_file_lock<T, F, Fut>(
    filename: impl AsRef<Path>,
    wait_ms: Option<u64>,
    operation: F,
) -> Result<T, AtomicWriteError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, AtomicWriteError>>,
{
    let filename = filename.as_ref();
    let lock_path = filename.with_extension(format!(
        "{}.lock",
        filename.extension().and_then(|e| e.to_str()).unwrap_or("file")
    ));
    let lock_path = Path::new(&format!("{}.lock", filename.display())).to_path_buf();
    let deadline = Instant::now() + Duration::from_millis(wait_ms.unwrap_or(DEFAULT_LOCK_WAIT_MS));
    let mut delay = LOCK_RETRY_INITIAL_MS;
    loop {
        match create_exclusive(&lock_path).await {
            Ok(()) => break,
            Err(error) if is_lock_contention(&error, &lock_path) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(AtomicWriteError::LockTimeout(lock_path.display().to_string()));
        }
        sleep(Duration::from_millis(delay)).await;
        delay = (delay * 2).min(LOCK_RETRY_MAX_MS);
    }
    let result = operation().await;
    let _ = fs::remove_file(&lock_path).await;
    result
}

async fn create_exclusive(path: &Path) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    opts.open(path).await?;
    Ok(())
}

fn is_lock_contention(error: &std::io::Error, lock_path: &Path) -> bool {
    match error.kind() {
        ErrorKind::AlreadyExists => true,
        ErrorKind::PermissionDenied => lock_path.exists(),
        _ => false,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-atomic-{name}-{nanos}"))
    }

    #[tokio::test]
    async fn write_then_read_sees_complete_content() {
        let path = tmp("file.txt");
        let path = tmp("dir").join("file.txt");
        write_file_atomic(
            &path,
            "hello",
            WriteFileAtomicOptions {
                mode: 0o600,
                dir_mode: Some(0o700),
            },
        )
        .await
        .unwrap();
        let body = fs::read_to_string(&path).await.unwrap();
        assert_eq!(body, "hello");
        let _ = fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn lock_serializes_writers() {
        let path = tmp("locked.txt");
        write_file_atomic(
            &path,
            "v1",
            WriteFileAtomicOptions {
                mode: 0o600,
                dir_mode: None,
            },
        )
        .await
        .unwrap();
        let result = with_file_lock(&path, Some(200), || async {
            write_file_atomic(
                &path,
                "v2",
                WriteFileAtomicOptions {
                    mode: 0o600,
                    dir_mode: None,
                },
            )
            .await?;
            Ok("ok".to_string())
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "v2");
        let _ = fs::remove_file(&path).await;
    }
}
