//! Plugin-name registry for a composed profile tree.
//!
//! Every `@deepseek-ai/dsh-*` / `cordis-plugin-*` name in the shipped headless
//! composition is registered. Rows without a Rust apply fail at mount with the
//! plugin name; dump-config never mounts and is unaffected.

use dsh_cordis_loader::Loader;
use std::collections::BTreeSet;

use crate::plugins::apply_named;
use crate::{compose_profile, shipped_bundles};

/// Register factories for every plugin name the headless profile composes.
///
/// `@deepseek-ai/dsh-llm-replay` is also registered so a no-key overlay can
/// insert that row. Unknown names stay unregistered and fail as `UnknownPlugin`.
pub fn register_profile_plugins(loader: &Loader) {
    for name in shipped_plugin_names() {
        register_one(loader, &name);
    }
    register_one(loader, "@deepseek-ai/dsh-llm-replay");
}

/// Plugin names in the shipped headless composition, plus overlay extras.
pub fn shipped_plugin_names() -> BTreeSet<String> {
    let Ok(layers) = shipped_bundles("headless") else {
        return BTreeSet::new();
    };
    let Ok(entries) = compose_profile(&layers, &[], &[], &[]) else {
        return BTreeSet::new();
    };
    entries.into_iter().map(|entry| entry.name).collect()
}

fn register_one(loader: &Loader, name: &str) {
    let owned = name.to_string();
    loader.register(name, move |ctx, config| apply_named(&owned, ctx, config));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shipped_bundles;
    use dsh_cordis::Context;

    #[test]
    fn headless_tree_mounts() {
        let loader = Loader::new();
        register_profile_plugins(&loader);
        let entries = compose_profile(&shipped_bundles("headless").unwrap(), &[], &[], &[]).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(dsh_bundle_headless::HeadlessStartup {
            task: "ping".into(),
        }))
        .unwrap();
        loader.mount(&ctx, &entries).unwrap();
        assert!(ctx.has_service("timer"));
        assert!(ctx.has_service("sessions"));
        assert!(ctx.has_service("agents"));
        assert!(ctx.has_service("agentDefaultModel"));
        assert!(ctx.has_service("llm"));
        assert!(!ctx.has_service("hmr"));
    }

    #[test]
    fn unknown_plugin_name_is_not_silently_skipped() {
        let loader = Loader::new();
        register_profile_plugins(&loader);
        let err = loader
            .mount(
                &Context::new(),
                &[dsh_cordis_loader::Entry::new("x", "@deepseek-ai/dsh-nope")],
            )
            .unwrap_err();
        assert!(err.to_string().contains("@deepseek-ai/dsh-nope"));
    }
}
