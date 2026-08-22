//! dsh-base patch layer.

use dsh_cordis_loader::{parse_entry_list, EntryPatch};

/// Shipped bundle identity.
pub fn name() -> &'static str {
    "dsh-bundle-base"
}

/// Embedded `cordis.patch.yml` text.
pub fn patch_yaml() -> &'static str {
    include_str!("../cordis.patch.yml")
}

/// Insert patches for every shipped row.
pub fn patches() -> Vec<EntryPatch> {
    parse_entry_list(patch_yaml())
        .expect("shipped dsh-base patch")
        .into_iter()
        .map(|entry| EntryPatch {
            id: entry.id,
            name: Some(entry.name),
            config: entry.config,
            disabled: if entry.disabled { Some(true) } else { None },
            insert: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
        assert!(super::patch_yaml().contains("id: llm"));
        assert!(super::patches().iter().any(|patch| patch.id.as_deref() == Some("llm")));
    }
}
