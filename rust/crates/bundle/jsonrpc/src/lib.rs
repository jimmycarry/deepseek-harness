//! dsh-jsonrpc patch layer. The launcher serves SDK stdio after the tree is up.

use dsh_cordis_loader::{parse_patch_list, EntryPatch};

/// Shipped bundle identity.
pub fn name() -> &'static str {
    "dsh-bundle-jsonrpc"
}

/// Embedded `cordis.patch.yml` text.
pub fn patch_yaml() -> &'static str {
    include_str!("../cordis.patch.yml")
}

/// Patches from the shipped file: replace shared rows, then insert the server.
pub fn patches() -> Vec<EntryPatch> {
    parse_patch_list(patch_yaml()).expect("shipped dsh-jsonrpc patch")
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_role() {
        assert!(!super::name().is_empty());
        assert!(super::patch_yaml().contains("id: sdk-jsonrpc-server"));
        let patches = super::patches();
        assert!(patches.iter().any(|patch| {
            patch.insert.as_ref().is_some_and(|rows| {
                rows.iter()
                    .any(|entry| entry.id.as_deref() == Some("sdk-jsonrpc-server"))
            })
        }));
        assert!(patches.iter().any(|patch| {
            patch.id.as_deref() == Some("hmr")
                && patch.disabled.as_ref().and_then(|value| value.as_bool()) == Some(true)
        }));
    }
}
