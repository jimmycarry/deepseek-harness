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

/// Plugin names across every shipped composition, plus overlay extras.
pub fn shipped_plugin_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for profile in ["headless", "acp", "jsonrpc"] {
        let Ok(layers) = shipped_bundles(profile) else {
            continue;
        };
        let Ok(entries) = compose_profile(&layers, &[], &[], &[]) else {
            continue;
        };
        names.extend(entries.into_iter().map(|entry| entry.name));
    }
    names
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
        let entries =
            compose_profile(&shipped_bundles("headless").unwrap(), &[], &[], &[]).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(dsh_bundle_headless::HeadlessStartup {
            task: "ping".into(),
            cwd: None,
        }))
        .unwrap();
        loader.mount(&ctx, &entries).unwrap();
        assert!(ctx.has_service("timer"));
        assert!(ctx.has_service("sessions"));
        assert!(ctx.has_service("agents"));
        assert!(ctx.has_service("agentDefaultModel"));
        assert!(ctx.has_service("llm"));
        assert!(!ctx.has_service("hmr"));
        assert!(ctx.has_service("shell"));
        let tools = ctx.service::<dsh_tools::ToolRuntime>().unwrap();
        let names: Vec<_> = tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        assert!(names.contains(&"read".into()));
        assert!(names.contains(&"write".into()));
        assert!(names.contains(&"edit".into()));
        assert!(names.contains(&"glob".into()));
        assert!(ctx.has_service("settings"));
        assert!(ctx.has_service("sessionTelemetry"));
        assert!(names.contains(&"grep".into()));
        assert!(names.contains(&"str_replace_editor".into()));
        assert!(names.contains(&"bash".into()));
        assert!(names.contains(&"job_output".into()));
        assert!(names.contains(&"job_list".into()));
        assert!(names.contains(&"job_kill".into()));
        assert!(ctx.has_service("jobs"));
        assert!(names.contains(&"create_goal".into()));
        assert!(names.contains(&"get_goal".into()));
        assert!(names.contains(&"update_goal".into()));
        assert!(names.contains(&"web_search".into()));
        assert!(!names.contains(&"web_fetch".into()));
        assert!(names.contains(&"subagent".into()));
        assert!(names.contains(&"subagent_fork".into()));
        assert!(names.contains(&"workflow".into()));
        assert!(names.contains(&"ralph".into()));
        assert!(ctx.has_service("goals"));
        assert!(ctx.has_service("subagents"));
        assert!(ctx.has_service("web"));
        assert!(ctx.has_service("workflowEngine"));
        assert!(ctx.has_service("attachments"));
        assert!(ctx.has_service("sessionQuery"));
        assert!(ctx.has_service("spillStore"));
        assert!(ctx.has_service("sessionProjections"));
        assert!(ctx.has_service("sessionCheckpointPolicy"));
        let commands = ctx.service::<dsh_commands::CommandRegistry>().unwrap();
        assert!(
            commands.get("feedback").is_some(),
            "dsh-command-feedback must register /feedback"
        );
        assert_eq!(
            dsh_fs_observation_policy::write_intent(
                &ctx,
                &dsh_fs::FsTarget::new("a.txt", "a.txt"),
                &dsh_fs::FsObservationActor::from_agent_id(Some("s")),
            ),
            Some(dsh_fs::FsWriteIntent::CreateIfAbsent)
        );
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
