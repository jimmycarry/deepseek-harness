//! Filesystem skill provider.

use dsh_skill::Skill;
use std::path::Path;

/// Load every `*.md` file in `dir` as a skill named by the file stem.
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
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let body = std::fs::read_to_string(&path)?;
        skills.push(Skill {
            name: stem.to_string(),
            body,
        });
    }
    Ok(skills)
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-skill-filesystem"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_dir_reads_markdown() {
        let dir = std::env::temp_dir().join(format!("dsh-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review.md"), "review body").unwrap();
        std::fs::write(dir.join("skip.txt"), "no").unwrap();
        let skills = load_dir(&dir).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].body, "review body");
        let _ = std::fs::remove_dir_all(dir);
    }
}
