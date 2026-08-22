//! Host-filesystem implementation of `ctx.spillStore`.
//!
//! Files land under `<root>/session-<hash>/…` with an exclusive owner-only
//! (`0o600`) write. A missing `root` uses a private (`0o700`) per-process
//! directory under the OS temp dir.

use dsh_cordis::{Context, Result};
use dsh_spill::{SaveTextSpill, SpillBackend, SpillError, SpillLocator, SpillRef, SpillStore};
use sha2::{Digest, Sha256};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

/// Model-facing retrieval guidance for a local path locator.
pub const RETRIEVAL_HINT: &str =
    "Use read with offset/limit, or grep this path to search within it.";

static DEFAULT_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Plugin construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Spill root. `None` uses the private per-process default.
    pub root: Option<PathBuf>,
}

impl Config {
    /// Resolve plugin config. `root` is optional.
    pub fn resolve(value: Option<&serde_json::Value>) -> std::result::Result<Self, String> {
        let root = value
            .and_then(|value| value.get("root"))
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        Ok(Self { root })
    }
}

/// The default spill root: a private per-process directory under the OS tmpdir.
pub fn private_root() -> PathBuf {
    DEFAULT_ROOT
        .get_or_init(|| {
            let path = std::env::temp_dir().join(format!("dsh-spill-{}", Uuid::new_v4().simple()));
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path).expect("private spill root");
            path
        })
        .clone()
}

/// Encode an arbitrary string as one safe path segment.
///
/// Empty becomes `~`. `.` / `..` and any code unit outside `[A-Za-z0-9._-]`
/// (minus `~`) is escaped as `~XXXX` so the mapping is injective.
pub fn encode_segment(raw: &str) -> String {
    if raw.is_empty() {
        return "~".into();
    }
    if raw == "." {
        return "~002E".into();
    }
    if raw == ".." {
        return "~002E~002E".into();
    }
    let mut out = String::new();
    for ch in raw.chars() {
        if ch != '~' && matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-') {
            out.push(ch);
        } else {
            let code = ch as u32;
            out.push('~');
            out.push_str(&format!("{code:04X}"));
        }
    }
    out
}

/// Session-scoped directory: `<root>/session-<12 hex of sha256(sessionId)>`.
pub fn session_dir(root: &Path, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(session_id.as_bytes());
    let hash = hex_encode(&digest[..6]);
    root.join(format!("session-{hash}"))
}

/// Write `content` under the session directory and return path + byte length.
pub fn save_text_file(
    root: &Path,
    session_id: &str,
    suggested_name: &str,
    content: &str,
) -> std::io::Result<(PathBuf, usize)> {
    let dir = session_dir(root, session_id);
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    let safe_name = encode_segment(suggested_name);
    let prefix = hex_encode(&Uuid::new_v4().as_bytes()[..6]);
    let path = dir.join(format!("{prefix}-{safe_name}"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(content.as_bytes())?;
    Ok((path, content.len()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Local-filesystem spill backend.
pub struct LocalSpillStore {
    root: PathBuf,
}

impl LocalSpillStore {
    /// Persist under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SpillBackend for LocalSpillStore {
    fn save_text(&self, input: SaveTextSpill) -> std::result::Result<SpillRef, SpillError> {
        let saved = save_text_file(
            &self.root,
            &input.owner.session_id,
            &input.suggested_name,
            &input.content,
        )
        .map_err(|error| SpillError::Storage(error.to_string()))?;
        Ok(SpillRef {
            locator: SpillLocator(saved.0.display().to_string()),
            bytes: saved.1,
            retrieval_hint: RETRIEVAL_HINT.into(),
        })
    }
}

/// Provide `ctx.spillStore` over a local root.
pub fn install(ctx: &Context, config: Config) -> Result<Arc<SpillStore>> {
    let root = config.root.unwrap_or_else(private_root);
    let store = Arc::new(SpillStore::new(Arc::new(LocalSpillStore::new(root))));
    ctx.provide(Arc::clone(&store))?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_spill::{SpillOwner, SpillSource};

    fn tmp_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-spill-local-{name}-{nanos}"))
    }

    #[test]
    fn encode_segment_is_injective_and_escapes_traversal() {
        assert_eq!(encode_segment(""), "~");
        assert_eq!(encode_segment("."), "~002E");
        assert_eq!(encode_segment(".."), "~002E~002E");
        assert_eq!(encode_segment("bash.txt"), "bash.txt");
        assert!(encode_segment("../etc/passwd").starts_with("~002E~002E"));
        assert_ne!(encode_segment("a/b"), encode_segment("a~b"));
    }

    #[test]
    fn save_then_reread_from_host() {
        let root = tmp_root("save");
        let ctx = Context::new();
        install(&ctx, Config { root: Some(root.clone()) }).unwrap();
        let store = ctx.service::<SpillStore>().unwrap();
        let saved = store
            .save_text(SaveTextSpill {
                owner: SpillOwner {
                    session_id: "s1".into(),
                },
                source: SpillSource {
                    tool_name: "bash".into(),
                    call_id: "c1".into(),
                    label: "result".into(),
                },
                suggested_name: "bash.txt".into(),
                content: "full-result".into(),
            })
            .unwrap();
        assert_eq!(saved.bytes, 11);
        assert_eq!(saved.retrieval_hint, RETRIEVAL_HINT);
        let body = std::fs::read_to_string(&saved.locator.0).unwrap();
        assert_eq!(body, "full-result");
        let meta = std::fs::metadata(&saved.locator.0).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        ctx.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }
}
