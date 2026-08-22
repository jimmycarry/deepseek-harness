//! dsh-base patch layer. The YAML is the TypeScript bundle file.

use dsh_cordis_loader::{parse_patch_list, EntryPatch};

/// Shipped bundle identity.
pub fn name() -> &'static str {
    "dsh-bundle-base"
}

/// Embedded `cordis.patch.yml` text.
pub fn patch_yaml() -> &'static str {
    include_str!("../cordis.patch.yml")
}

/// Patches from the shipped file: one root insert of every base row.
pub fn patches() -> Vec<EntryPatch> {
    parse_patch_list(patch_yaml()).expect("shipped dsh-base patch")
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
        assert!(super::patch_yaml().contains("id: llm"));
        let patches = super::patches();
        assert_eq!(patches.len(), 1);
        let inserted = patches[0].insert.as_ref().expect("base is one insert");
        assert!(inserted.iter().any(|entry| entry.id.as_deref() == Some("llm")));
        assert!(inserted
            .iter()
            .any(|entry| entry.name == "@deepseek-ai/dsh-llm"));
    }
}
