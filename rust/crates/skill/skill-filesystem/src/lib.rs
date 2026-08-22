//! Filesystem skill provider: discovers `name/SKILL.md` directory bundles and
//! flat `*.md` skills under the default roots (project `.dsh/skills`, project
//! `.agents/skills`, custom dirs, `{dshHome}/skills`, `{agentsHome}/skills`)
//! and registers them on `ctx.skills`. Later (higher-rank) roots do not
//! override earlier ones. File watching is not implemented; the scan runs at
//! install.

use dsh_cordis::Context;
use dsh_skill::{Skill, SkillRuntime};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Resolved discovery policy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether the default project/user roots are scanned.
    pub include_default_roots: bool,
    /// Extra skill directories scanned between project and user roots.
    pub custom_skill_dirs: Vec<String>,
    /// Project root used for `.dsh/skills` and `.agents/skills`.
    pub project_root: PathBuf,
    /// `$DSH_HOME` override; the `skills` subdirectory is scanned.
    pub dsh_home: Option<PathBuf>,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// A non-array `customSkillDirs` or non-string entry.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let include_default_roots = config
            .and_then(|value| value.get("includeDefaultRoots"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let custom_skill_dirs = match config.and_then(|value| value.get("customSkillDirs")) {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        "skill-filesystem: customSkillDirs entries must be strings".to_string()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err("skill-filesystem: customSkillDirs must be an array".into());
            }
        };
        Ok(Self {
            include_default_roots,
            custom_skill_dirs,
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            dsh_home: std::env::var_os("DSH_HOME").map(PathBuf::from),
        })
    }
}

/// Parse `---` frontmatter requiring `name` and `description`.
fn parse_frontmatter(text: &str) -> Option<(String, String, bool, String)> {
    let rest = text.strip_prefix("---")?;
    let (header, body) = rest.split_once("\n---")?;
    let mut name = None;
    let mut description = None;
    let mut model_invocable = true;
    for line in header.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "disable-model-invocation" => model_invocable = value != "true",
            _ => {}
        }
    }
    let body = body.strip_prefix('\n').unwrap_or(body).to_string();
    Some((name?, description?, model_invocable, body))
}

/// Load one `SKILL.md` bundle directory into a skill with resource listing.
fn load_bundle(dir: &Path) -> Option<Skill> {
    let manifest = dir.join("SKILL.md");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let (name, description, model_invocable, body) = parse_frontmatter(&text)?;
    let mut resources: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name() != "SKILL.md")
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    resources.sort();
    Some(Skill {
        name,
        description,
        body,
        model_invocable,
        resources,
    })
}

/// Load every skill under one root: bundles first, then flat `*.md`.
pub fn load_dir(dir: impl AsRef<Path>) -> std::io::Result<Vec<Skill>> {
    let mut skills = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect();
    entries.sort_by_key(|entry| {
        entry
            .as_ref()
            .map(|dir_entry| dir_entry.file_name())
            .unwrap_or_default()
    });
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(skill) = load_bundle(&path) {
                skills.push(skill);
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_frontmatter(&text) {
            Some((name, description, model_invocable, body)) => skills.push(Skill {
                name,
                description,
                body,
                model_invocable,
                resources: Vec::new(),
            }),
            None => skills.push(Skill::new(stem, "", text)),
        }
    }
    Ok(skills)
}

/// Discovery roots in rank order for `config`.
fn roots(config: &Config) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if config.include_default_roots {
        roots.push(config.project_root.join(".dsh").join("skills"));
        roots.push(config.project_root.join(".agents").join("skills"));
    }
    roots.extend(config.custom_skill_dirs.iter().map(PathBuf::from));
    if config.include_default_roots {
        if let Some(home) = &config.dsh_home {
            roots.push(home.join("skills"));
        }
    }
    roots
}

/// Scan the discovery roots and register every skill on `ctx.skills`.
/// The first (lowest-rank) registration of a name wins.
///
/// # Errors
/// Missing `ctx.skills`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let skills = ctx.service::<SkillRuntime>()?;
    let mut seen: Vec<String> = skills.names();
    for root in roots(&config) {
        let Ok(loaded) = load_dir(&root) else {
            continue;
        };
        for skill in loaded {
            if seen.contains(&skill.name) {
                continue;
            }
            seen.push(skill.name.clone());
            skills.register(skill);
        }
    }
    Ok(())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-skill-filesystem"
}

#[cfg(test)]
mod tests {
    use super::*;

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
        install(
            &ctx,
            Config {
                include_default_roots: true,
                custom_skill_dirs: vec![],
                project_root: project.clone(),
                dsh_home: None,
            },
        )
        .unwrap();
        let skills = ctx.service::<SkillRuntime>().unwrap();
        assert_eq!(skills.get("only").unwrap().body, "project body");
        let _ = std::fs::remove_dir_all(project);
    }
}
