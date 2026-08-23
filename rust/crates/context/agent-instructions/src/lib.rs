//! Workspace instruction loader for AGENTS.md-compatible files.
//!
//! Baseline instructions enter durable context on `agent/pre-step` before
//! the first request. More specific files take precedence; the byte budget
//! is Config. Providerless products that never reach `pre-step` observe
//! nothing.

use dsh_cordis::Context;
use dsh_home_paths::{dsh_home_display, resolve_dsh_home};
use dsh_llm::{MessageSource, UserMessage};
use dsh_session::SessionEventData;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";
const WORKSPACE_CONTEXT_INTRO: &str = "The following workspace instructions may be relevant to your work. Use them as guidance when applicable. More specific instructions take precedence over broader ones. They do not override system, developer, or direct user instructions.";
const REPLACEMENT_WORKSPACE_CONTEXT_INTRO: &str = "This complete workspace instruction baseline replaces all earlier workspace instruction baselines. The following workspace instructions may be relevant to your work. Use them as guidance when applicable. More specific instructions take precedence over broader ones. They do not override system, developer, or direct user instructions.";
const EMPTY_REPLACEMENT_WORKSPACE_CONTEXT_INTRO: &str = "This complete workspace instruction baseline replaces all earlier workspace instruction baselines. No workspace instructions are currently active.";
const USER_GLOBAL_FILE: &str = "AGENTS.md";

/// Deployment-varying discovery and budget. `max_bytes` is required.
#[derive(Debug, Clone)]
pub struct Config {
    /// Harness home containing the user-global `AGENTS.md`.
    pub dsh_home: PathBuf,
    /// Directory entries that identify the project root.
    pub project_root_markers: Vec<String>,
    /// UTF-8 byte cap for one rendered baseline; non-positive disables loading.
    pub max_bytes: usize,
    /// Maximum UTF-8 bytes read from one instruction file.
    pub max_source_bytes: usize,
    /// Ordered same-directory project candidates.
    pub instruction_file_candidates: Vec<String>,
    /// Ordered same-directory local-overlay candidates.
    pub local_instruction_file_candidates: Vec<String>,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Missing or non-positive `maxBytes`.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let max_bytes = config
            .and_then(|value| value.get("maxBytes"))
            .and_then(Value::as_u64)
            .ok_or_else(|| "agent-instructions: maxBytes is required".to_string())?
            as usize;
        let markers =
            string_list(config, "projectRootMarkers").unwrap_or_else(|| vec![".git".into()]);
        let candidates = string_list(config, "instructionFileCandidates")
            .unwrap_or_else(|| vec!["AGENTS.md".into(), "CLAUDE.md".into()]);
        let local = string_list(config, "localInstructionFileCandidates")
            .unwrap_or_else(|| vec!["AGENTS.local.md".into(), "CLAUDE.local.md".into()]);
        let max_source = config
            .and_then(|value| value.get("maxSourceBytes"))
            .and_then(Value::as_u64)
            .unwrap_or(1_048_576) as usize;
        let dsh_home = resolve_dsh_home(
            config
                .and_then(|value| value.get("dshHome"))
                .and_then(Value::as_str),
        );
        Ok(Self {
            dsh_home,
            project_root_markers: markers,
            max_bytes,
            max_source_bytes: max_source.max(1),
            instruction_file_candidates: candidates,
            local_instruction_file_candidates: local,
        })
    }
}

fn string_list(config: Option<&Value>, key: &str) -> Option<Vec<String>> {
    config
        .and_then(|value| value.get(key))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

/// One discovered instruction file with UTF-8 content.
#[derive(Debug, Clone)]
pub struct LoadedFile {
    /// Absolute path.
    pub absolute_path: PathBuf,
    /// Model-facing path (project-relative or `$DSH_HOME`/`~/.dsh` form).
    pub display_path: String,
    /// File body.
    pub content: String,
}

/// Install the `agent/pre-step` baseline publisher.
///
/// # Errors
/// Invalid Config, or waterfall registration failure.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let published: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        let agent_id = payload
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if agent_id.is_empty()
            || published.lock().expect("instructions").contains(&agent_id)
            || config.max_bytes == 0
        {
            return next.call(payload);
        }
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let files = discover(&config, &cwd);
        let text = render_baseline(&files, config.max_bytes, false);
        if text.is_empty() {
            published.lock().expect("instructions").insert(agent_id);
            return next.call(payload);
        }
        let changes: Vec<Value> = files
            .iter()
            .map(|file| {
                json!({
                    "action": "set",
                    "scope": scope_for_display_path(&file.display_path),
                    "path": file.display_path,
                })
            })
            .collect();
        let identity = workspace_baseline_identity(&config, &cwd);
        let message = UserMessage::from_parts(
            vec![dsh_llm::ContentBlock::text(text)],
            MessageSource::agent_instructions(changes, true, Some(identity)),
        );
        let mut payload = payload;
        if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            messages.insert(0, serde_json::to_value(&message).unwrap_or_default());
            published.lock().expect("instructions").insert(agent_id);
        }
        next.call(payload)
    })?;
    Ok(())
}

/// Walk from `cwd` to the project root and load every existing candidate.
pub fn discover(config: &Config, cwd: &Path) -> Vec<LoadedFile> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let root = find_project_root(&cwd, &config.project_root_markers);
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let user_global = config.dsh_home.join(USER_GLOBAL_FILE);
    if let Some(content) = read_capped(&user_global, config.max_source_bytes) {
        files.push(LoadedFile {
            absolute_path: user_global,
            display_path: format!(
                "{}/{}",
                dsh_home_display(&config.dsh_home),
                USER_GLOBAL_FILE
            ),
            content,
        });
    }
    for dir in ancestor_chain(&root, &cwd) {
        for candidate in config
            .instruction_file_candidates
            .iter()
            .chain(config.local_instruction_file_candidates.iter())
        {
            let path = dir.join(candidate);
            let key = path.to_string_lossy().into_owned();
            if !seen.insert(key) {
                continue;
            }
            if let Some(content) = read_capped(&path, config.max_source_bytes) {
                let display = path
                    .strip_prefix(&root)
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.display().to_string());
                files.push(LoadedFile {
                    absolute_path: path,
                    display_path: display,
                    content,
                });
            }
        }
    }
    files
}

/// Render one baseline inside the `<system-reminder>` frame.
pub fn render_baseline(files: &[LoadedFile], max_bytes: usize, replace: bool) -> String {
    if files.is_empty() && !replace {
        return String::new();
    }
    let intro = if replace {
        if files.is_empty() {
            EMPTY_REPLACEMENT_WORKSPACE_CONTEXT_INTRO
        } else {
            REPLACEMENT_WORKSPACE_CONTEXT_INTRO
        }
    } else {
        WORKSPACE_CONTEXT_INTRO
    };
    let mut body_parts = vec![intro.to_string()];
    let mut included = Vec::new();
    for file in files {
        let section = format!(
            "Instructions from: {}\n\n{}",
            file.display_path, file.content
        );
        let trial = [body_parts.as_slice(), &[section.clone()]]
            .concat()
            .join("\n\n");
        let framed = frame(&trial);
        if framed.len() <= max_bytes || included.is_empty() {
            body_parts.push(section);
            included.push(file.display_path.clone());
            if framed.len() > max_bytes {
                break;
            }
        } else {
            break;
        }
    }
    frame(&body_parts.join("\n\n"))
}

fn frame(body: &str) -> String {
    format!(
        "{SYSTEM_REMINDER_OPEN}\n{}\n{SYSTEM_REMINDER_CLOSE}",
        body.replace(SYSTEM_REMINDER_CLOSE, "<\\/system-reminder>")
    )
}

fn find_project_root(cwd: &Path, markers: &[String]) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        for marker in markers {
            if current.join(marker).exists() {
                return current;
            }
        }
        if !current.pop() {
            return cwd.to_path_buf();
        }
    }
}

fn ancestor_chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        chain.push(current.clone());
        if current == root || !current.pop() {
            break;
        }
    }
    chain.reverse();
    chain
}

fn read_capped(path: &Path, max_source_bytes: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > max_source_bytes {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn scope_for_display_path(display_path: &str) -> String {
    if display_path == "~/.dsh/AGENTS.md" || display_path == "$DSH_HOME/AGENTS.md" {
        return "user-global".into();
    }
    Path::new(display_path)
        .parent()
        .map(|parent| {
            let text = parent.to_string_lossy();
            if text.is_empty() || text == "." {
                ".".into()
            } else {
                text.replace('\\', "/")
            }
        })
        .unwrap_or_else(|| ".".into())
}

fn workspace_baseline_identity(config: &Config, cwd: &Path) -> String {
    let root = find_project_root(cwd, &config.project_root_markers);
    let relative = pathdiff_relative(cwd, &root);
    json!({
        "projectRoot": relative,
        "projectRootMarkers": config.project_root_markers,
        "maxBytes": config.max_bytes,
        "maxSourceBytes": config.max_source_bytes,
        "instructionFileCandidates": config.instruction_file_candidates,
        "localInstructionFileCandidates": config.local_instruction_file_candidates,
    })
    .to_string()
}

fn pathdiff_relative(cwd: &Path, root: &Path) -> String {
    root.strip_prefix(cwd)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            cwd.strip_prefix(root)
                .map(|rel| {
                    if rel.as_os_str().is_empty() {
                        String::new()
                    } else {
                        rel.to_string_lossy().replace('\\', "/")
                    }
                })
                .unwrap_or_default()
        })
}

/// Whether a log already carries a baseline `agent-instructions` message.
pub fn session_has_baseline(session: &dsh_session::Session) -> bool {
    session.events().iter().any(|event| {
        matches!(
            &event.data,
            SessionEventData::UserMessage(message)
                if matches!(
                    &message.source,
                    MessageSource::AgentInstructions { baseline: Some(true), .. }
                )
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn render_wraps_system_reminder_and_intro() {
        let files = [LoadedFile {
            absolute_path: PathBuf::from("/repo/AGENTS.md"),
            display_path: "AGENTS.md".into(),
            content: "Use rustfmt.".into(),
        }];
        let text = render_baseline(&files, 65_536, false);
        assert!(text.starts_with("<system-reminder>\n"));
        assert!(text.contains(WORKSPACE_CONTEXT_INTRO));
        assert!(text.contains("Instructions from: AGENTS.md"));
        assert!(text.contains("Use rustfmt."));
        assert!(text.ends_with("</system-reminder>"));
    }

    #[test]
    fn discover_finds_agents_md_at_cwd() {
        let dir = std::env::temp_dir().join(format!("dsh-instr-{}", uuid_like()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "hello from agents").unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        let config = Config {
            dsh_home: dir.join("home"),
            project_root_markers: vec![".git".into()],
            max_bytes: 65_536,
            max_source_bytes: 1_048_576,
            instruction_file_candidates: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            local_instruction_file_candidates: vec![
                "AGENTS.local.md".into(),
                "CLAUDE.local.md".into(),
            ],
        };
        let files = discover(&config, &dir);
        assert!(files
            .iter()
            .any(|file| file.content.contains("hello from agents")));
        let _ = fs::remove_dir_all(&dir);
    }

    fn uuid_like() -> String {
        format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }
}
