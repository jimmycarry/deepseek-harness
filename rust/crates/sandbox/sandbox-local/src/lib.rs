//! Local cwd-rooted sandbox backend.

use dsh_cordis::Context;
use dsh_sandbox::{SandboxPolicy, SandboxRuntime};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-sandbox-local"
}

/// Lexically normalize `.` / `..` without touching the filesystem.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Whether `resolved` is `/etc` or a descendant — host-secret escape.
fn is_etc_escape(resolved: &Path) -> bool {
    resolved.starts_with(Path::new("/etc"))
}

/// Deny paths that escape `root` via `..` or land under `/etc`.
pub fn allow_path(root: &str, path: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let root_norm = normalize_lexical(Path::new(root));
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_norm.join(candidate)
    };
    let resolved = normalize_lexical(&joined);
    if is_etc_escape(&resolved) {
        return false;
    }
    resolved == root_norm || resolved.starts_with(&root_norm)
}

/// Workspace-rooted policy used by the local sandbox provider.
pub struct CwdSandbox {
    root: String,
}

impl CwdSandbox {
    /// Bind the policy to an explicit workspace root.
    pub fn new(root: impl Into<String>) -> Self {
        let root = root.into();
        if root.is_empty() {
            panic!("CwdSandbox: root must be a non-empty path");
        }
        Self { root }
    }

    /// Configured workspace root.
    pub fn root(&self) -> &str {
        &self.root
    }
}

impl SandboxPolicy for CwdSandbox {
    fn allow_path(&self, path: &str) -> bool {
        allow_path(&self.root, path)
    }
}

/// Provide `ctx.sandbox` as a cwd-rooted [`CwdSandbox`].
pub fn install(ctx: &Context, root: impl Into<String>) -> dsh_cordis::Result<Arc<SandboxRuntime>> {
    let runtime = Arc::new(SandboxRuntime::new(Arc::new(CwdSandbox::new(root))));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn denies_absolute_escape() {
        assert!(!allow_path("/tmp/workspace", "/etc/passwd"));
    }

    #[test]
    fn denies_parent_escape() {
        assert!(!allow_path("/tmp/workspace", "../etc/passwd"));
        assert!(!allow_path("/tmp/workspace", "foo/../../etc/passwd"));
        assert!(!allow_path(
            "/tmp/workspace",
            "/tmp/workspace/../etc/passwd"
        ));
    }

    #[test]
    fn allows_relative_child() {
        assert!(allow_path("/tmp/workspace", "ok.txt"));
        assert!(allow_path("/tmp/workspace", "foo/../ok.txt"));
    }

    #[test]
    fn cwd_sandbox_matches_allow_path() {
        let sandbox = CwdSandbox::new("/tmp/workspace");
        assert!(sandbox.allow_path("ok.txt"));
        assert!(!sandbox.allow_path("/etc/passwd"));
        assert!(!sandbox.allow_path(".."));
    }

    #[test]
    fn install_provides_sandbox() {
        let ctx = Context::new();
        install(&ctx, "/tmp/workspace").unwrap();
        assert!(ctx.has_service("sandbox"));
        let runtime = ctx.service::<SandboxRuntime>().unwrap();
        assert!(runtime.allow_path("ok.txt"));
        assert!(!runtime.allow_path("/etc/passwd"));
        ctx.dispose();
        assert!(!ctx.has_service("sandbox"));
    }

    #[test]
    fn seam_is_present() {
        assert_eq!(name(), "dsh-sandbox-local");
    }
}
