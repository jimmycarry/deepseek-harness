//! Deterministic model-facing public names for MCP tools.

use sha2::{Digest, Sha256};

const MAX_PUBLIC_NAME_LENGTH: usize = 64;
const HASH_LENGTH: usize = 12;

/// Derive the model-facing public name for one MCP tool.
pub fn public_tool_name(server_name: &str, raw_name: &str) -> String {
    let joined = format!("mcp__{server_name}__{raw_name}");
    let normalized: String = joined
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if normalized == joined && normalized.len() <= MAX_PUBLIC_NAME_LENGTH {
        return normalized;
    }
    let mut hasher = Sha256::new();
    hasher.update(server_name.as_bytes());
    hasher.update([0u8]);
    hasher.update(raw_name.as_bytes());
    let digest = hasher.finalize();
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let hash = &hash[..HASH_LENGTH];
    let keep = MAX_PUBLIC_NAME_LENGTH - HASH_LENGTH - 1;
    let prefix: String = normalized.chars().take(keep).collect();
    format!("{prefix}_{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_clean_names_verbatim() {
        assert_eq!(
            public_tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(
            public_tool_name("everything", "get-sum"),
            "mcp__everything__get-sum"
        );
    }

    #[test]
    fn replaces_invalid_characters_and_appends_hash() {
        let name = public_tool_name("srv", "admin.reset");
        assert!(name.starts_with("mcp__srv__admin_reset_"));
        assert_eq!(name.len(), "mcp__srv__admin_reset_".len() + 12);
        assert!(name.len() <= 64);
        assert!(name.chars().rev().take(12).all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn truncates_overlong_names() {
        let name = public_tool_name("srv", &"a".repeat(80));
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("mcp__srv__aaa"));
        assert!(name.ends_with(
            &name
                .chars()
                .rev()
                .take(12)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }

    #[test]
    fn is_deterministic_and_collision_free() {
        let a = public_tool_name("srv", "admin.reset");
        let b = public_tool_name("srv", "admin_reset");
        assert_eq!(a, public_tool_name("srv", "admin.reset"));
        assert_ne!(a, b);
    }
}
