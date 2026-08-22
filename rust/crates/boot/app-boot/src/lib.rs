//! Profile and bundle composition. Layers: bundles, profile patch, home, overlay.

use dsh_cordis_loader::{compose_layers, parse_entry_list, Entry, EntryPatch, Loader, LoaderError};
use serde::Deserialize;

/// A named composition stored in the Harness home.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    /// Profile name (`web`, `headless`).
    pub name: String,
    /// Bundles stacked in order.
    pub bundles: Vec<String>,
}

/// Shipped profile templates.
pub fn profile_templates() -> Vec<Profile> {
    vec![
        Profile {
            name: "headless".into(),
            bundles: vec!["dsh-base".into(), "dsh-headless".into()],
        },
        Profile {
            name: "web".into(),
            bundles: vec!["dsh-base".into(), "dsh-web-app".into()],
        },
    ]
}

/// One bundle patch layer.
#[derive(Debug, Clone)]
pub struct BundleLayer {
    /// Bundle name.
    pub name: String,
    /// Patch rows.
    pub patches: Vec<EntryPatch>,
}

/// Compose an empty list with ordered layers.
pub fn compose_profile(
    bundles: &[BundleLayer],
    profile_patch: &[EntryPatch],
    home_patch: &[EntryPatch],
    overlay: &[EntryPatch],
) -> Result<Vec<Entry>, LoaderError> {
    let mut layers: Vec<Vec<EntryPatch>> = bundles.iter().map(|layer| layer.patches.clone()).collect();
    if !profile_patch.is_empty() {
        layers.push(profile_patch.to_vec());
    }
    if !home_patch.is_empty() {
        layers.push(home_patch.to_vec());
    }
    if !overlay.is_empty() {
        layers.push(overlay.to_vec());
    }
    compose_layers(&layers)
}

/// Render the tree `dsh --dump-config` would print.
pub fn dump_config(entries: &[Entry]) -> String {
    Loader::dump_config(entries)
}

/// Parse a YAML overlay.
pub fn parse_overlay(yaml: &str) -> Result<Vec<Entry>, LoaderError> {
    parse_entry_list(yaml)
}

/// Re-export patch apply for dump-config tooling.
pub use dsh_cordis_loader::apply_entry_patches;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_template_stacks_base_then_runner() {
        let profile = profile_templates()
            .into_iter()
            .find(|profile| profile.name == "headless")
            .unwrap();
        assert_eq!(profile.bundles, ["dsh-base", "dsh-headless"]);
    }
}
