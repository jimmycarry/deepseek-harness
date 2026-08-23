//! Profile and bundle composition. Layers: bundles, profile patch, home, overlay.

use dsh_cordis_loader::{compose_layers, parse_patch_list, Entry, EntryPatch, Loader, LoaderError};
use serde::Deserialize;

mod plugins;
mod registry;

pub use plugins::{ApprovalService, PermissionService, SandboxPolicyService};
pub use registry::{register_profile_plugins, shipped_plugin_names};

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

/// Shipped bundle layers for a named profile.
///
/// `headless` stacks base then the headless runner; `acp` and `jsonrpc` stack
/// base then the respective stdio server. Unknown names fail loud.
pub fn shipped_bundles(profile: &str) -> Result<Vec<BundleLayer>, LoaderError> {
    let base = BundleLayer {
        name: dsh_bundle_base::name().into(),
        patches: dsh_bundle_base::patches(),
    };
    match profile {
        "headless" => Ok(vec![
            base,
            BundleLayer {
                name: dsh_bundle_headless::name().into(),
                patches: dsh_bundle_headless::patches(),
            },
        ]),
        "acp" => Ok(vec![
            base,
            BundleLayer {
                name: dsh_bundle_acp::name().into(),
                patches: dsh_bundle_acp::patches(),
            },
        ]),
        "jsonrpc" => Ok(vec![
            base,
            BundleLayer {
                name: dsh_bundle_jsonrpc::name().into(),
                patches: dsh_bundle_jsonrpc::patches(),
            },
        ]),
        other => Err(LoaderError::Parse(format!(
            "unknown shipped profile `{other}`"
        ))),
    }
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

/// Parse a YAML overlay patch list.
pub fn parse_overlay(yaml: &str) -> Result<Vec<EntryPatch>, LoaderError> {
    parse_patch_list(yaml)
}

/// Re-export patch apply for dump-config tooling.
pub use dsh_cordis_loader::apply_entry_patches;

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn headless_template_stacks_base_then_runner() {
        let profile = profile_templates()
            .into_iter()
            .find(|profile| profile.name == "headless")
            .unwrap();
        assert_eq!(profile.bundles, ["dsh-base", "dsh-headless"]);
    }

    #[test]
    fn dump_config_includes_base_and_headless_ids() {
        let layers = shipped_bundles("headless").unwrap();
        let entries = compose_profile(&layers, &[], &[], &[]).unwrap();
        let dump = dump_config(&entries);
        assert!(dump.contains("id: llm"));
        assert!(dump.contains("id: headless-runner"));
        assert!(dump.contains("name: '@deepseek-ai/dsh-llm'"));
        assert!(dump.contains("!!js process.env.DSH_TOOLS_MODE"));
        assert!(dump.contains("!!js ctx.headlessStartup.task"));
    }

    /// Row ids `dsh --profile headless --dump-config` must emit, in order.
    const HEADLESS_IDS: &[&str] = &[
        "timer",
        "hmr",
        "llm",
        "session",
        "typert",
        "typert-loader",
        "typert-gateway",
        "session-title",
        "session-title-llm",
        "user-questions",
        "agent",
        "agent-default-model",
        "jobs",
        "llm-retry",
        "settings",
        "credentials",
        "llm-pi-ai",
        "session-persistence-jsonl",
        "attachment-local",
        "session-query-sqlite",
        "session-projection",
        "session-telemetry-otel",
        "subprocess",
        "sandbox",
        "sandbox-policy",
        "bash-sandbox",
        "pwsh-sandbox",
        "approval",
        "permission",
        "shell-env",
        "tool-bash",
        "tool-pwsh",
        "tool-jobs",
        "fs-observation-policy",
        "tool-fs",
        "tool-fs-search",
        "agent-instructions",
        "skill",
        "skill-filesystem",
        "skill-badge",
        "tool-skill",
        "commands",
        "command-feedback",
        "goal",
        "goal-round-driver",
        "command-goal",
        "plan-mode",
        "token-meter",
        "compaction-basic",
        "command-compact",
        "subagent",
        "subagent-spawn-in-process",
        "subagent-fork-in-process",
        "tool-subagent-control",
        "tool-subagent-list-agents",
        "tool-subagent",
        "tool-subagent-fork",
        "tool-subagent-report",
        "workflow-worker-thread",
        "tool-workflow",
        "timeout-policy",
        "spill-local",
        "spill-policy",
        "session-checkpoint-policy",
        "tool-result-pruner",
        "tool-todo",
        "tool-goal",
        "tool-ralph",
        "tool-str-replace-editor",
        "repeat-tool-reminder",
        "web",
        "web-search-deepseek",
        "tool-web",
        "tools",
        "system-prompt",
        "agent-loop",
        "fs-sandbox",
        "llm-deepseek",
        "code-runtime",
        "headless-startup",
        "headless-runner",
    ];

    #[test]
    fn headless_dump_id_sequence_matches_typescript_profile() {
        let layers = shipped_bundles("headless").unwrap();
        let entries = compose_profile(&layers, &[], &[], &[]).unwrap();
        let ids: Vec<&str> = entries
            .iter()
            .map(|entry| entry.id.as_deref().expect("composed row has id"))
            .collect();
        assert_eq!(ids, HEADLESS_IDS);
        let hmr = entries.iter().find(|entry| entry.id.as_deref() == Some("hmr")).unwrap();
        assert_eq!(
            hmr.disabled.as_ref().and_then(|value| value.as_bool()),
            Some(true)
        );
        let runner = entries
            .iter()
            .find(|entry| entry.id.as_deref() == Some("headless-runner"))
            .unwrap();
        assert_eq!(runner.name, "@deepseek-ai/dsh-headless");
        assert_eq!(runner.inject, ["headlessStartup"]);
    }

    fn replay_overlay(text: &str) -> Vec<EntryPatch> {
        let mut disable = EntryPatch::replace("llm-deepseek");
        disable.disabled = Some(serde_json::json!(true));
        let mut replay = Entry::new("llm-replay", "@deepseek-ai/dsh-llm-replay");
        replay.config = Some(serde_json::json!({ "text": text }));
        vec![disable, EntryPatch::insert_row(replay)]
    }

    #[tokio::test]
    async fn headless_replay_turn_flushes() {
        let dir = std::env::temp_dir().join(format!("dsh-wave-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("DSH_HOME", &dir);
        let layers = shipped_bundles("headless").unwrap();
        let entries = compose_profile(&layers, &[], &[], &replay_overlay("pong")).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(dsh_bundle_headless::HeadlessStartup {
            task: "ping".into(),
            cwd: None,
        }))
        .unwrap();
        let loader = Loader::new();
        register_profile_plugins(&loader);
        loader.mount(&ctx, &entries).unwrap();
        dsh_bundle_headless::run(&ctx).await.unwrap();
        let sessions = dir.join("sessions");
        assert!(sessions.exists());
    }
}
