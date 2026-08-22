//! Local cwd-rooted sandbox backend.

use dsh_sandbox::Runtime;

pub fn name() -> &'static str {
    "dsh-sandbox-local"
}

/// Deny paths that escape `root`.
pub fn allow_path(root: &str, path: &str) -> bool {
    let root = std::path::Path::new(root);
    let candidate = std::path::Path::new(path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    resolved.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_absolute_escape() {
        assert!(!allow_path("/tmp/workspace", "/etc/passwd"));
    }

    #[test]
    fn allows_relative_child() {
        assert!(allow_path("/tmp/workspace", "ok.txt"));
    }

    #[test]
    fn seam_is_present() {
        let _ = Runtime::new();
        assert_eq!(name(), "dsh-sandbox-local");
    }
}
