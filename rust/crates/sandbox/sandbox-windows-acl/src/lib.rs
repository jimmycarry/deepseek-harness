//! Portable Windows ACL identities and path-boundary checks.
//!
//! [`workspace_write_sid`] and [`temp_write_sid`] are byte-identical to the
//! TypeScript helpers. [`assert_temp_root_outside_workspace`] and
//! [`assert_private_temp_disjoint`] keep the standing workspace capability
//! off the private temp tree. The Win32 restricted-token runner, grant
//! lifecycle, and koffi FFI are not mounted.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 30-bit SID subauthority modulus (`2**30 - 1`) used by the TypeScript helpers.
const SUBAUTHORITY_MOD: u32 = (1 << 30) - 1;

/// Path-boundary refusal with the TypeScript sentences, or a canonicalize failure.
#[derive(Debug, Error)]
pub enum WindowsAclPathError {
    /// Overlapping workspace and temp trees.
    #[error("{0}")]
    Overlap(String),
    /// `realpath` / canonicalize failed for a path that must already exist.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Derive the workspace write SID (`S-1-4-x-y`) from the canonical workspace path.
pub fn workspace_write_sid(workspace_root: &str) -> String {
    let digest = Sha256::digest(workspace_root.as_bytes());
    let (first, second) = two_subauthorities(&digest);
    format!("S-1-4-{first}-{second}")
}

/// Derive a private-temp write SID (`S-1-4-x-y-1`) domain-separated from workspace SIDs.
pub fn temp_write_sid(temp_dir: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"temp\0");
    hasher.update(temp_dir.as_bytes());
    let digest = hasher.finalize();
    let (first, second) = two_subauthorities(&digest);
    format!("S-1-4-{first}-{second}-1")
}

/// Reject a temp parent that is the workspace or a descendant of it.
///
/// # Errors
/// Overlap uses the TypeScript sentence. Missing paths surface the IO error
/// from canonicalize (the TypeScript helpers require existing directories).
pub fn assert_temp_root_outside_workspace(
    workspace_root: &str,
    temp_root: &str,
) -> Result<(), WindowsAclPathError> {
    if contains_directory(workspace_root, temp_root)? {
        return Err(WindowsAclPathError::Overlap(format!(
            "Windows ACL temp root must be outside the workspace: workspace={workspace_root}; temp={temp_root}"
        )));
    }
    Ok(())
}

/// Reject overlap between a private temp directory and any writable directory.
///
/// # Errors
/// Overlap uses the TypeScript sentence. Missing paths surface canonicalize IO.
pub fn assert_private_temp_disjoint(
    writable_dirs: &[&str],
    temp_dir: &str,
) -> Result<(), WindowsAclPathError> {
    for writable_dir in writable_dirs {
        if contains_directory(writable_dir, temp_dir)?
            || contains_directory(temp_dir, writable_dir)?
        {
            return Err(WindowsAclPathError::Overlap(format!(
                "AclSandbox private temp directory must be disjoint from writable directories: writable={writable_dir}; temp={temp_dir}"
            )));
        }
    }
    Ok(())
}

fn two_subauthorities(digest: &[u8]) -> (u32, u32) {
    let first = u32::from_le_bytes(digest[0..4].try_into().expect("sha256")) % SUBAUTHORITY_MOD + 1;
    let second =
        u32::from_le_bytes(digest[4..8].try_into().expect("sha256")) % SUBAUTHORITY_MOD + 1;
    (first, second)
}

fn contains_directory(root: &str, candidate: &str) -> Result<bool, WindowsAclPathError> {
    let root = canonicalize_dir(root)?;
    let candidate = canonicalize_dir(candidate)?;
    Ok(candidate == root || candidate.starts_with(&root))
}

fn canonicalize_dir(path: &str) -> Result<PathBuf, WindowsAclPathError> {
    Ok(Path::new(path).canonicalize()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn workspace_write_sid_matches_the_typescript_digest() {
        let sid = workspace_write_sid(r"C:\Users\agent\repo");
        assert_eq!(sid, "S-1-4-907248133-152761708");
        assert_eq!(sid, workspace_write_sid(r"C:\Users\agent\repo"));
        assert!(sid.starts_with("S-1-4-"));
        assert_eq!(sid.chars().filter(|c| *c == '-').count(), 3);
    }

    #[test]
    fn workspace_write_sid_is_byte_sensitive_and_distinct() {
        assert_ne!(
            workspace_write_sid(r"C:\Users\agent\repo-a"),
            workspace_write_sid(r"C:\Users\agent\repo-b")
        );
        assert_ne!(
            workspace_write_sid(r"C:\Repo"),
            workspace_write_sid(r"c:\repo")
        );
        assert_ne!(
            workspace_write_sid(r"C:\Repo\"),
            workspace_write_sid(r"C:\Repo")
        );
    }

    #[test]
    fn temp_write_sid_is_domain_separated_from_workspace() {
        let path = r"C:\Users\agent\AppData\Local\Temp\dsh-abc123";
        let sid = temp_write_sid(path);
        assert_eq!(sid, "S-1-4-174242848-241453763-1");
        assert_eq!(sid, temp_write_sid(path));
        assert!(sid.ends_with("-1"));
        assert_ne!(sid, workspace_write_sid(path));
        assert_ne!(
            temp_write_sid(r"C:\Temp\dsh-a"),
            temp_write_sid(r"C:\Temp\dsh-b")
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dsh-acl-boundary-{name}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn rejects_a_temp_root_equal_to_or_below_the_workspace() {
        let workspace = scratch("ws");
        let nested = workspace.join("temp");
        fs::create_dir_all(&nested).unwrap();
        let ws = workspace.to_str().expect("utf8");
        let nested = nested.to_str().expect("utf8");
        let equal = assert_temp_root_outside_workspace(ws, ws).unwrap_err();
        assert!(equal
            .to_string()
            .contains("temp root must be outside the workspace"));
        let below = assert_temp_root_outside_workspace(ws, nested).unwrap_err();
        assert!(below
            .to_string()
            .contains("temp root must be outside the workspace"));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn accepts_a_temp_parent_above_the_workspace() {
        let temp_root = scratch("parent");
        let workspace = temp_root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        assert_temp_root_outside_workspace(
            workspace.to_str().expect("utf8"),
            temp_root.to_str().expect("utf8"),
        )
        .unwrap();
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn requires_private_temp_to_be_disjoint_in_either_direction() {
        let root = scratch("disjoint");
        let workspace = root.join("workspace");
        let nested_temp = workspace.join("temp");
        let sibling = root.join("sibling-temp");
        fs::create_dir_all(&nested_temp).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let ws = workspace.to_str().expect("utf8");
        let nested = nested_temp.to_str().expect("utf8");
        let sibling = sibling.to_str().expect("utf8");
        assert!(assert_private_temp_disjoint(&[ws], nested)
            .unwrap_err()
            .to_string()
            .contains("must be disjoint"));
        assert!(assert_private_temp_disjoint(&[nested], ws)
            .unwrap_err()
            .to_string()
            .contains("must be disjoint"));
        assert_private_temp_disjoint(&[ws], sibling).unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
