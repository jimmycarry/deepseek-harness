//! Discovery roots and one-shot catalog scans.

use super::Config;
use dsh_skill::Skill;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One filesystem skill root in rank order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRoot {
    pub path: PathBuf,
    pub skip_system: bool,
    pub project_root: Option<PathBuf>,
}

/// Parse `---` frontmatter requiring `name` and `description`.
pub(crate) fn parse_frontmatter(text: &str) -> Option<(String, String, bool, String)> {
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
    load_dir_filtered(dir.as_ref(), false)
}

fn load_dir_filtered(dir: &Path, skip_system: bool) -> std::io::Result<Vec<Skill>> {
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
        if skip_system && entry.file_name() == ".system" {
            continue;
        }
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

/// Nearest ancestor that contains `.git`, or `cwd` when none exists.
pub(crate) fn find_project_root(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return cwd.to_path_buf(),
        }
    }
}

/// Discovery roots in rank order for `config`'s current project root.
pub(crate) fn roots(config: &Config) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    if config.include_default_roots {
        let project = find_project_root(&config.project_root);
        roots.push(SkillRoot {
            path: project.join(".dsh").join("skills"),
            skip_system: false,
            project_root: Some(project.clone()),
        });
        roots.push(SkillRoot {
            path: project.join(".agents").join("skills"),
            skip_system: false,
            project_root: Some(project),
        });
    }
    roots.extend(config.custom_skill_dirs.iter().map(|path| SkillRoot {
        path: PathBuf::from(path),
        skip_system: false,
        project_root: None,
    }));
    if config.include_default_roots {
        roots.push(SkillRoot {
            path: config.dsh_home.join("skills"),
            skip_system: true,
            project_root: None,
        });
        roots.push(SkillRoot {
            path: config.agents_home.join("skills"),
            skip_system: false,
            project_root: None,
        });
    }
    if let Some(bundled) = &config.bundled_skill_dir {
        roots.push(SkillRoot {
            path: bundled.clone(),
            skip_system: false,
            project_root: None,
        });
    }
    roots
}

/// Scan `config` roots. The first (lowest-rank) registration of a name wins.
pub fn scan(config: &Config) -> Vec<Skill> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots(config) {
        let Ok(loaded) = load_dir_filtered(&root.path, root.skip_system) else {
            continue;
        };
        for skill in loaded {
            if !seen.insert(skill.name.clone()) {
                continue;
            }
            out.push(skill);
        }
    }
    out
}

/// Replace this provider's previous registrations with a fresh scan.
pub fn apply_scan(
    skills: &dsh_skill::SkillRuntime,
    config: &Config,
    owned: &Mutex<HashSet<String>>,
) {
    let loaded = scan(config);
    let mut previous = owned.lock().expect("skill-filesystem owned");
    for name in previous.iter() {
        skills.unregister(name);
    }
    previous.clear();
    for skill in loaded {
        previous.insert(skill.name.clone());
        skills.register(skill);
    }
}

/// Relative path segments when `path` is inside `root`.
pub(crate) fn contained_segments(root: &Path, path: &Path) -> Option<Vec<String>> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return Some(Vec::new());
    }
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect(),
    )
}

/// Whether `path` is a catalog-relevant skill entry under `root`.
pub(crate) fn is_potential_skill_path(root: &SkillRoot, path: &Path) -> bool {
    let Some(segments) = contained_segments(&root.path, path) else {
        return false;
    };
    if segments.is_empty() || segments.len() > 2 {
        return false;
    }
    if root.skip_system && segments[0] == ".system" {
        return false;
    }
    if segments.len() == 1 {
        segments[0].ends_with(".md")
    } else {
        segments[1] == "SKILL.md"
    }
}

/// First-party `write` / `edit` tool name, matching TypeScript `mutationToolName`.
pub(crate) fn mutation_tool_name(actor: Option<&serde_json::Value>) -> Option<&'static str> {
    match actor
        .and_then(|value| value.get("name"))
        .and_then(|value| value.as_str())
    {
        Some("write") => Some("write"),
        Some("edit") => Some("edit"),
        _ => None,
    }
}
