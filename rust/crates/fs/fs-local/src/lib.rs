//! Local filesystem provider.

use async_trait::async_trait;
use dsh_fs::{FsError, FsProvider};
use tokio::fs;

/// Host filesystem backend.
pub struct LocalFs;

#[async_trait]
impl FsProvider for LocalFs {
    async fn read_text(&self, path: &str) -> Result<String, FsError> {
        fs::read_to_string(path)
            .await
            .map_err(|error| FsError::Io(error.to_string()))
    }

    async fn write_text(&self, path: &str, content: &str) -> Result<(), FsError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_external_read() {
        let path = std::env::temp_dir().join(format!("dsh-fs-{}.txt", std::process::id()));
        LocalFs
            .write_text(path.to_str().unwrap(), "hello")
            .await
            .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "hello");
        let _ = std::fs::remove_file(path);
    }
}
