//! Filesystem skill provider: discovers `name/SKILL.md` directory bundles and
//! flat `*.md` skills under the default roots (project `.dsh/skills`, project
//! `.agents/skills`, custom dirs, `{dshHome}/skills`, `{agentsHome}/skills`,
//! and an optional bundled root) and registers them on `ctx.skills`. Later
//! (higher-rank) roots do not override earlier ones.
//!
//! Catalog changes are observed by interval polling of existing roots and of
//! the next missing ancestor segment. `watchUsePolling` is accepted and
//! stored so cordis.yml matches TypeScript; both values poll. First-party
//! `write` / `edit` `fs/observed` events rescan immediately. `agent/pre-step`
//! always rescans so the next catalog publication sees the current roots.

mod scan;
mod watch;

use dsh_cordis::Context;
use dsh_home_paths::{expand_home_path, resolve_dsh_home};
use dsh_skill::SkillRuntime;
use scan::{apply_scan, find_project_root, mutation_tool_name, roots};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use watch::WatchManager;

pub use scan::{load_dir, scan};

const DEFAULT_WATCH_STABILITY_THRESHOLD_MS: u64 = 200;
const DEFAULT_WATCH_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_WATCH_MAX_PROJECTS: u64 = 128;

/// Resolved discovery and watch policy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether the default project/user roots are scanned.
    pub include_default_roots: bool,
    /// Extra skill directories scanned between project and user roots.
    pub custom_skill_dirs: Vec<String>,
    /// Starting directory used to locate the nearest `.git` project root.
    pub project_root: PathBuf,
    /// Resolved DeepSeek Harness home; `{dshHome}/skills` is scanned.
    pub dsh_home: PathBuf,
    /// Resolved shared agent home; `{agentsHome}/skills` is scanned.
    pub agents_home: PathBuf,
    /// Optional bundled skill root (`bundledSkillDir` or `$DSH_BUNDLED_SKILL_DIR`).
    pub bundled_skill_dir: Option<PathBuf>,
    /// Whether host skill roots are polled for catalog changes.
    pub watch: bool,
    /// TypeScript Chokidar polling switch; Rust polls either way.
    pub watch_use_polling: bool,
    /// Milliseconds a changed catalog entry must remain stable before a rescan.
    pub watch_stability_threshold_ms: u64,
    /// Milliseconds between existing-root samples and missing-path probes.
    pub watch_poll_interval_ms: u64,
    /// Maximum distinct project roots retained in the watcher LRU.
    pub watch_max_projects: usize,
    /// Whether catalog snapshots follow symbolic links.
    pub watch_follow_symlinks: bool,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// A non-array `customSkillDirs`, a non-string entry, or a non-positive
    /// watch integer.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let include_default_roots = optional_bool(config, "includeDefaultRoots", true)?;
        let custom_skill_dirs = match config.and_then(|value| value.get("customSkillDirs")) {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|value| resolve_configured_path(value))
                        .ok_or_else(|| {
                            "skill-filesystem: customSkillDirs entries must be strings".to_string()
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err("skill-filesystem: customSkillDirs must be an array".into());
            }
        };
        let project_root = config
            .and_then(|value| value.get("projectRoot"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let dsh_home = resolve_dsh_home(
            config
                .and_then(|value| value.get("dshHome"))
                .and_then(Value::as_str),
        );
        let agents_home = resolve_agents_home(
            config
                .and_then(|value| value.get("agentsHome"))
                .and_then(Value::as_str),
        );
        let bundled_skill_dir = match config
            .and_then(|value| value.get("bundledSkillDir"))
            .and_then(Value::as_str)
        {
            Some(path) => Some(resolve_configured_path(path).into()),
            None if include_default_roots => std::env::var_os("DSH_BUNDLED_SKILL_DIR")
                .map(|path| absolute_path(&PathBuf::from(path))),
            None => None,
        };
        if let Some(Value::String(name)) = config.and_then(|value| value.get("providerName")) {
            if name.is_empty() {
                return Err("skill-filesystem: providerName must be a non-empty string".into());
            }
        }
        Ok(Self {
            include_default_roots,
            custom_skill_dirs,
            project_root,
            dsh_home,
            agents_home,
            bundled_skill_dir,
            watch: optional_bool(config, "watch", true)?,
            watch_use_polling: optional_bool(config, "watchUsePolling", false)?,
            watch_stability_threshold_ms: positive_integer(
                "watchStabilityThresholdMs",
                config.and_then(|value| value.get("watchStabilityThresholdMs")),
                DEFAULT_WATCH_STABILITY_THRESHOLD_MS,
            )?,
            watch_poll_interval_ms: positive_integer(
                "watchPollIntervalMs",
                config.and_then(|value| value.get("watchPollIntervalMs")),
                DEFAULT_WATCH_POLL_INTERVAL_MS,
            )?,
            watch_max_projects: usize::try_from(positive_integer(
                "watchMaxProjects",
                config.and_then(|value| value.get("watchMaxProjects")),
                DEFAULT_WATCH_MAX_PROJECTS,
            )?)
            .map_err(|_| {
                "skill-filesystem: watchMaxProjects must be a positive integer".to_string()
            })?,
            watch_follow_symlinks: optional_bool(config, "watchFollowSymlinks", true)?,
        })
    }
}

fn optional_bool(config: Option<&Value>, field: &str, default: bool) -> Result<bool, String> {
    match config.and_then(|value| value.get(field)) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("skill-filesystem: {field} must be a boolean")),
    }
}

fn positive_integer(field: &str, value: Option<&Value>, default: u64) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(default);
    };
    if let Some(number) = value.as_u64() {
        if number >= 1 {
            return Ok(number);
        }
    }
    Err(format!(
        "skill-filesystem: {field} must be a positive integer"
    ))
}

fn resolve_agents_home(configured: Option<&str>) -> PathBuf {
    let selected = if let Some(configured) = configured {
        configured.to_string()
    } else if let Ok(from_env) = std::env::var("DSH_AGENTS_HOME") {
        if from_env.trim().is_empty() {
            default_agents_home().to_string_lossy().into_owned()
        } else {
            from_env
        }
    } else {
        default_agents_home().to_string_lossy().into_owned()
    };
    absolute_path(&expand_home_path(&selected))
}

fn default_agents_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".agents")
}

fn resolve_configured_path(path: &str) -> String {
    absolute_path(&expand_home_path(path))
        .to_string_lossy()
        .into_owned()
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn config_for_cwd(config: &Config, cwd: Option<&str>) -> Config {
    let mut next = config.clone();
    if let Some(cwd) = cwd.filter(|value| !value.is_empty()) {
        next.project_root = PathBuf::from(cwd);
    }
    next
}

/// Scan the discovery roots and register every skill on `ctx.skills`.
/// Later `agent/pre-step`, catalog polling, and `write`/`edit` `fs/observed`
/// events rescan the same roots.
///
/// # Errors
/// Missing `ctx.skills`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let skills = ctx.service::<SkillRuntime>()?;
    let owned = Arc::new(Mutex::new(HashSet::new()));
    apply_scan(&skills, &config, &owned);
    let live = Arc::new(Mutex::new(config.clone()));
    let invalidate_skills = Arc::clone(&skills);
    let invalidate_owned = Arc::clone(&owned);
    let invalidate_live = Arc::clone(&live);
    let invalidate: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let current = invalidate_live
            .lock()
            .expect("skill-filesystem config")
            .clone();
        apply_scan(&invalidate_skills, &current, &invalidate_owned);
    });
    let watcher = Arc::new(WatchManager::start(&config, Arc::clone(&invalidate)));
    watcher.observe_roots(&roots(&config));
    let pre_skills = Arc::clone(&skills);
    let pre_owned = Arc::clone(&owned);
    let pre_live = Arc::clone(&live);
    let pre_watch = Arc::clone(&watcher);
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        let cwd = payload.get("cwd").and_then(Value::as_str);
        let current = {
            let mut guard = pre_live.lock().expect("skill-filesystem config");
            *guard = config_for_cwd(&guard.clone(), cwd);
            guard.clone()
        };
        apply_scan(&pre_skills, &current, &pre_owned);
        pre_watch.observe_roots(&roots(&current));
        next.call(payload)
    })?;
    let observed_watch = Arc::clone(&watcher);
    let observed_live = Arc::clone(&live);
    ctx.on("fs/observed", move |payload| {
        if mutation_tool_name(payload.get("actor")).is_none() {
            return;
        }
        let path = payload
            .pointer("/target/displayPath")
            .and_then(Value::as_str)
            .unwrap_or("");
        if path.is_empty() {
            return;
        }
        let current = observed_live
            .lock()
            .expect("skill-filesystem config")
            .clone();
        let path = Path::new(path);
        if WatchManager::is_skill_path(&current, path) {
            observed_watch.observe_host_mutation(path);
        }
    })?;
    ctx.effect("skill-filesystem watcher", move || {
        move || watcher.dispose()
    })?;
    Ok(())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-skill-filesystem"
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan::{is_potential_skill_path, parse_frontmatter, SkillRoot};
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-skillfs-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(project: &Path) -> Config {
        Config {
            include_default_roots: true,
            custom_skill_dirs: vec![],
            project_root: project.to_path_buf(),
            dsh_home: project.join(".unused-dsh"),
            agents_home: project.join(".unused-agents"),
            bundled_skill_dir: None,
            watch: false,
            watch_use_polling: false,
            watch_stability_threshold_ms: 20,
            watch_poll_interval_ms: 10,
            watch_max_projects: 128,
            watch_follow_symlinks: true,
        }
    }

    fn write_bundle(root: &Path, name: &str, description: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) {
        let start = Instant::now();
        while !pred() {
            if start.elapsed() > timeout {
                panic!("timed out waiting for skill-filesystem watch");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn load_dir_reads_bundles_and_flat_markdown() {
        let dir = scratch("load");
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(
            dir.join("review").join("SKILL.md"),
            "---\nname: review\ndescription: do reviews\n---\nreview body",
        )
        .unwrap();
        std::fs::write(dir.join("review").join("checklist.md"), "c").unwrap();
        std::fs::write(dir.join("flat.md"), "flat body").unwrap();
        std::fs::write(dir.join("skip.txt"), "no").unwrap();
        let skills = load_dir(&dir).unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "flat");
        assert_eq!(skills[1].name, "review");
        assert_eq!(skills[1].description, "do reviews");
        assert_eq!(skills[1].body, "review body");
        assert_eq!(skills[1].resources, vec!["checklist.md".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn frontmatter_disable_model_invocation() {
        let parsed = parse_frontmatter(
            "---\nname: hidden\ndescription: d\ndisable-model-invocation: true\n---\nbody",
        )
        .unwrap();
        assert!(!parsed.2);
        assert_eq!(parsed.3, "body");
        assert!(parse_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn resolve_rejects_non_positive_watch_integers() {
        let err = Config::resolve(Some(&json!({ "watchPollIntervalMs": 0 }))).unwrap_err();
        assert_eq!(
            err,
            "skill-filesystem: watchPollIntervalMs must be a positive integer"
        );
        let err = Config::resolve(Some(&json!({ "watchStabilityThresholdMs": 1.5 }))).unwrap_err();
        assert_eq!(
            err,
            "skill-filesystem: watchStabilityThresholdMs must be a positive integer"
        );
        let err = Config::resolve(Some(&json!({ "watchMaxProjects": -1 }))).unwrap_err();
        assert_eq!(
            err,
            "skill-filesystem: watchMaxProjects must be a positive integer"
        );
    }

    #[test]
    fn resolve_defaults_watch_policy() {
        let resolved = Config::resolve(Some(&json!({
            "dshHome": "/tmp/dsh-home",
            "agentsHome": "/tmp/agents-home",
            "includeDefaultRoots": false,
        })))
        .unwrap();
        assert!(resolved.watch);
        assert!(!resolved.watch_use_polling);
        assert_eq!(resolved.watch_stability_threshold_ms, 200);
        assert_eq!(resolved.watch_poll_interval_ms, 100);
        assert_eq!(resolved.watch_max_projects, 128);
        assert!(resolved.watch_follow_symlinks);
        assert_eq!(resolved.dsh_home, PathBuf::from("/tmp/dsh-home"));
        assert_eq!(resolved.agents_home, PathBuf::from("/tmp/agents-home"));
        assert!(resolved.bundled_skill_dir.is_none());
    }

    #[test]
    fn user_dsh_skips_system_child() {
        let project = scratch("system");
        let dsh = project.join("home-dsh");
        write_bundle(&dsh.join("skills"), "user-ok", "ok", "user");
        write_bundle(&dsh.join("skills").join(".system"), "hidden", "no", "sys");
        let mut config = test_config(&project);
        config.dsh_home = dsh;
        let names: Vec<_> = scan(&config).into_iter().map(|skill| skill.name).collect();
        assert_eq!(names, vec!["user-ok".to_string()]);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn agents_home_root_is_scanned() {
        let project = scratch("agents-home");
        let agents = project.join("home-agents");
        write_bundle(&agents.join("skills"), "shared", "from agents", "body");
        let mut config = test_config(&project);
        config.agents_home = agents;
        let skills = scan(&config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "shared");
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn project_root_walks_to_git() {
        let project = scratch("git-root");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let nested = project.join("pkg").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        write_bundle(&project.join(".dsh").join("skills"), "from-git", "g", "b");
        let mut config = test_config(&nested);
        config.project_root = nested;
        let skills = scan(&config);
        assert_eq!(skills[0].name, "from-git");
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn install_scans_project_roots_first() {
        let project = scratch("roots");
        std::fs::create_dir_all(project.join(".agents").join("skills")).unwrap();
        std::fs::write(
            project.join(".agents").join("skills").join("only.md"),
            "---\nname: only\ndescription: project skill\n---\nproject body",
        )
        .unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        install(&ctx, test_config(&project)).unwrap();
        let skills = ctx.service::<SkillRuntime>().unwrap();
        assert_eq!(skills.get("only").unwrap().body, "project body");
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn pre_step_rescans_a_new_skill_file() {
        let project = scratch("rescan");
        let skills_dir = project.join(".dsh").join("skills");
        std::fs::create_dir_all(skills_dir.join("first")).unwrap();
        std::fs::write(
            skills_dir.join("first").join("SKILL.md"),
            "---\nname: first\ndescription: first skill\n---\nfirst body",
        )
        .unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        install(&ctx, test_config(&project)).unwrap();
        assert!(ctx
            .service::<SkillRuntime>()
            .unwrap()
            .get("second")
            .is_none());
        std::fs::create_dir_all(skills_dir.join("second")).unwrap();
        std::fs::write(
            skills_dir.join("second").join("SKILL.md"),
            "---\nname: second\ndescription: second skill\n---\nsecond body",
        )
        .unwrap();
        ctx.waterfall(
            "agent/pre-step",
            serde_json::json!({ "cwd": project.to_string_lossy() }),
            |payload| payload,
        )
        .unwrap();
        assert_eq!(
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("second")
                .unwrap()
                .body,
            "second body"
        );
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_false_does_not_start_a_poll_thread() {
        let project = scratch("watch-off");
        let skills_dir = project.join(".dsh").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = false;
        install(&ctx, config.clone()).unwrap();
        write_bundle(&skills_dir, "late", "late", "body");
        std::thread::sleep(Duration::from_millis(80));
        assert!(ctx.service::<SkillRuntime>().unwrap().get("late").is_none());
        let watcher = WatchManager::start(&config, Arc::new(|| {}));
        assert!(!watcher.thread_alive());
        watcher.dispose();
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_picks_up_a_new_bundle_after_stability() {
        let project = scratch("watch-add");
        let skills_dir = project.join(".dsh").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = true;
        install(&ctx, config).unwrap();
        assert!(ctx
            .service::<SkillRuntime>()
            .unwrap()
            .get("watched")
            .is_none());
        write_bundle(&skills_dir, "watched", "from watch", "watch body");
        wait_until(Duration::from_secs(2), || {
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("watched")
                .is_some()
        });
        assert_eq!(
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("watched")
                .unwrap()
                .body,
            "watch body"
        );
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_ignores_bundle_resource_edits() {
        let project = scratch("watch-resource");
        let skills_dir = project.join(".dsh").join("skills");
        write_bundle(&skills_dir, "kept", "same", "original");
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = true;
        install(&ctx, config).unwrap();
        std::fs::write(skills_dir.join("kept").join("references.md"), "resource").unwrap();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("kept")
                .unwrap()
                .body,
            "original"
        );
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_sees_a_missing_root_appear() {
        let project = scratch("watch-missing");
        let agents = project.join("agents-home");
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = true;
        config.agents_home = agents.clone();
        install(&ctx, config).unwrap();
        assert!(ctx
            .service::<SkillRuntime>()
            .unwrap()
            .get("appeared")
            .is_none());
        write_bundle(&agents.join("skills"), "appeared", "now here", "hello");
        wait_until(Duration::from_secs(2), || {
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("appeared")
                .is_some()
        });
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_evicts_the_oldest_project() {
        let first = scratch("watch-lru-a");
        let second = scratch("watch-lru-b");
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(second.join(".git")).unwrap();
        let mut config = test_config(&first);
        config.watch = true;
        config.watch_max_projects = 1;
        let watcher = WatchManager::start(&config, Arc::new(|| {}));
        watcher.observe_roots(&roots(&config));
        assert_eq!(watcher.watched_projects(), vec![find_project_root(&first)]);
        config.project_root = second.clone();
        watcher.observe_roots(&roots(&config));
        assert_eq!(watcher.watched_projects(), vec![find_project_root(&second)]);
        let first_skills = first.join(".dsh").join("skills");
        assert!(!watcher
            .watched_root_paths()
            .iter()
            .any(|path| path.starts_with(&first_skills)));
        watcher.dispose();
        let _ = std::fs::remove_dir_all(first);
        let _ = std::fs::remove_dir_all(second);
    }

    #[test]
    fn dispose_stops_the_poll_thread() {
        let project = scratch("watch-dispose");
        let mut config = test_config(&project);
        config.watch = true;
        let watcher = WatchManager::start(&config, Arc::new(|| {}));
        watcher.observe_roots(&roots(&config));
        assert!(watcher.thread_alive());
        watcher.dispose();
        assert!(!watcher.thread_alive());
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn fs_observed_write_rescans_and_read_does_not() {
        let project = scratch("observed");
        let skills_dir = project.join(".dsh").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        install(&ctx, test_config(&project)).unwrap();
        write_bundle(&skills_dir, "from-write", "w", "written");
        let path = skills_dir.join("from-write").join("SKILL.md");
        ctx.emit(
            "fs/observed",
            json!({
                "target": { "displayPath": path.to_string_lossy() },
                "actor": { "name": "read" },
            }),
        );
        assert!(ctx
            .service::<SkillRuntime>()
            .unwrap()
            .get("from-write")
            .is_none());
        ctx.emit(
            "fs/observed",
            json!({
                "target": { "displayPath": path.to_string_lossy() },
                "actor": { "name": "write" },
            }),
        );
        assert_eq!(
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("from-write")
                .unwrap()
                .body,
            "written"
        );
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_refreshes_frontmatter_after_stability() {
        let project = scratch("watch-frontmatter");
        let skills_dir = project.join(".dsh").join("skills");
        write_bundle(&skills_dir, "named", "first", "body");
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = true;
        install(&ctx, config).unwrap();
        assert_eq!(
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("named")
                .unwrap()
                .description,
            "first"
        );
        write_bundle(&skills_dir, "named", "second", "body");
        wait_until(Duration::from_secs(2), || {
            ctx.service::<SkillRuntime>()
                .unwrap()
                .get("named")
                .is_some_and(|skill| skill.description == "second")
        });
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn watch_drops_a_removed_bundle() {
        let project = scratch("watch-remove");
        let skills_dir = project.join(".dsh").join("skills");
        write_bundle(&skills_dir, "gone", "g", "b");
        let ctx = Context::new();
        ctx.provide(std::sync::Arc::new(SkillRuntime::new()))
            .unwrap();
        let mut config = test_config(&project);
        config.watch = true;
        install(&ctx, config).unwrap();
        assert!(ctx.service::<SkillRuntime>().unwrap().get("gone").is_some());
        let _ = std::fs::remove_dir_all(skills_dir.join("gone"));
        wait_until(Duration::from_secs(2), || {
            ctx.service::<SkillRuntime>().unwrap().get("gone").is_none()
        });
        ctx.dispose();
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn include_default_roots_false_keeps_only_custom_and_bundled() {
        let project = scratch("isolated");
        write_bundle(&project.join(".dsh").join("skills"), "project", "p", "p");
        let custom = project.join("custom");
        write_bundle(&custom, "only-custom", "c", "c");
        let mut config = test_config(&project);
        config.include_default_roots = false;
        config.custom_skill_dirs = vec![custom.to_string_lossy().into_owned()];
        let names: Vec<_> = scan(&config).into_iter().map(|skill| skill.name).collect();
        assert_eq!(names, vec!["only-custom".to_string()]);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn potential_skill_path_skips_resources_and_system() {
        let root = SkillRoot {
            path: PathBuf::from("/home/.dsh/skills"),
            skip_system: true,
            project_root: None,
        };
        assert!(is_potential_skill_path(
            &root,
            Path::new("/home/.dsh/skills/flat.md")
        ));
        assert!(is_potential_skill_path(
            &root,
            Path::new("/home/.dsh/skills/review/SKILL.md")
        ));
        assert!(!is_potential_skill_path(
            &root,
            Path::new("/home/.dsh/skills/review/references/notes.md")
        ));
        assert!(!is_potential_skill_path(
            &root,
            Path::new("/home/.dsh/skills/.system/SKILL.md")
        ));
    }
}
