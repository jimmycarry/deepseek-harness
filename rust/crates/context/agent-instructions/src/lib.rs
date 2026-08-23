//! Workspace instruction loader for AGENTS.md-compatible files.
//!
//! Baseline instructions enter durable context on `agent/pre-step` before
//! the first request. A later step whose discovery identity changed publishes
//! a replacement baseline; a file that appeared, changed, or disappeared
//! under the same identity publishes a non-baseline update. More specific
//! files take precedence; the byte budget is Config. Providerless products
//! that never reach `pre-step` observe nothing.

use dsh_cordis::Context;
use dsh_home_paths::{dsh_home_display, resolve_dsh_home};
use dsh_llm::{MessageSource, UserMessage};
use dsh_session::SessionEventData;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const ADDITIONAL_INSTRUCTIONS_GUIDANCE: &str = "These instructions apply to work under";
const UPDATED_INSTRUCTIONS_GUIDANCE: &str = "This file changed after it was loaded. Use the following content instead of the previously loaded instructions from this file.";
const REMOVED_INSTRUCTIONS_GUIDANCE: &str =
    "The previously loaded instructions from this file no longer apply.";

#[derive(Debug, Clone)]
struct PublishedFile {
    display_path: String,
    content: String,
}

#[derive(Debug, Clone)]
struct PublishedState {
    identity: String,
    emitted_baseline: bool,
    files: Vec<PublishedFile>,
}

#[derive(Debug, Clone)]
struct InstructionChange {
    action: &'static str,
    scope: String,
    path: String,
    content: String,
}

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

/// Install the `agent/pre-step` baseline and file-touch publisher.
///
/// # Errors
/// Invalid Config, or waterfall registration failure.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let published: Arc<Mutex<HashMap<String, PublishedState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        let mut payload = next.call(payload);
        if config.max_bytes == 0 {
            return payload;
        }
        let agent_id = payload
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if agent_id.is_empty() {
            return payload;
        }
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let files = discover(&config, &cwd);
        let identity = workspace_baseline_identity(&config, &cwd);
        let desired = desired_instruction_message(&config, &published, &agent_id, &identity, &files);
        let Some(message) = desired else {
            return payload;
        };
        if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
            messages.insert(0, serde_json::to_value(&message).unwrap_or_default());
        }
        payload
    })?;
    Ok(())
}

fn desired_instruction_message(
    config: &Config,
    published: &Mutex<HashMap<String, PublishedState>>,
    agent_id: &str,
    identity: &str,
    files: &[LoadedFile],
) -> Option<UserMessage> {
    let previous = published
        .lock()
        .expect("instructions")
        .get(agent_id)
        .cloned();
    let snapshot = published_files(files);
    let message = match previous {
        None => {
            let text = render_baseline(files, config.max_bytes, false);
            if text.is_empty() {
                None
            } else {
                Some(baseline_message(
                    &text,
                    set_changes(files),
                    true,
                    Some(identity.to_string()),
                ))
            }
        }
        Some(previous) if previous.identity != identity => {
            let text = render_baseline(files, config.max_bytes, true);
            Some(baseline_message(
                &text,
                replacement_changes(&previous.files, files),
                true,
                Some(identity.to_string()),
            ))
        }
        Some(previous) if !previous.emitted_baseline => {
            let text = render_baseline(files, config.max_bytes, false);
            if text.is_empty() {
                None
            } else {
                Some(baseline_message(
                    &text,
                    set_changes(files),
                    true,
                    Some(identity.to_string()),
                ))
            }
        }
        Some(previous) => {
            let changes = file_touch_changes(&previous.files, files);
            if changes.is_empty() {
                None
            } else {
                let text = render_updates(&changes, config.max_bytes);
                Some(baseline_message(
                    &text,
                    changes_to_values(&changes),
                    false,
                    None,
                ))
            }
        }
    };
    if message.is_some() || previous.is_none() {
        let emitted_baseline = match &message {
            Some(item) => matches!(
                item.source,
                MessageSource::AgentInstructions {
                    baseline: Some(true),
                    ..
                }
            ) || previous
                .as_ref()
                .is_some_and(|state| state.emitted_baseline),
            None => previous
                .as_ref()
                .is_some_and(|state| state.emitted_baseline),
        };
        published.lock().expect("instructions").insert(
            agent_id.to_string(),
            PublishedState {
                identity: identity.to_string(),
                emitted_baseline,
                files: snapshot,
            },
        );
    }
    message
}

fn published_files(files: &[LoadedFile]) -> Vec<PublishedFile> {
    files
        .iter()
        .map(|file| PublishedFile {
            display_path: file.display_path.clone(),
            content: file.content.clone(),
        })
        .collect()
}

fn set_changes(files: &[LoadedFile]) -> Vec<Value> {
    files
        .iter()
        .map(|file| {
            json!({
                "action": "set",
                "scope": instruction_scope_key(&file.display_path),
                "path": file.display_path,
            })
        })
        .collect()
}

fn replacement_changes(previous: &[PublishedFile], files: &[LoadedFile]) -> Vec<Value> {
    let current: HashSet<&str> = files
        .iter()
        .map(|file| file.display_path.as_str())
        .collect();
    let mut changes = Vec::new();
    for file in previous {
        if !current.contains(file.display_path.as_str()) {
            changes.push(json!({
                "action": "remove",
                "scope": instruction_scope_key(&file.display_path),
                "path": file.display_path,
            }));
        }
    }
    changes.extend(set_changes(files));
    changes
}

fn file_touch_changes(previous: &[PublishedFile], files: &[LoadedFile]) -> Vec<InstructionChange> {
    let mut previous_by_path: HashMap<&str, &PublishedFile> = HashMap::new();
    for file in previous {
        previous_by_path.insert(file.display_path.as_str(), file);
    }
    let mut current_paths = HashSet::new();
    let mut changes = Vec::new();
    for file in files {
        current_paths.insert(file.display_path.as_str());
        match previous_by_path.get(file.display_path.as_str()) {
            None => changes.push(InstructionChange {
                action: "set",
                scope: instruction_scope_key(&file.display_path),
                path: file.display_path.clone(),
                content: file.content.clone(),
            }),
            Some(previous) if previous.content != file.content => {
                changes.push(InstructionChange {
                    action: "replace",
                    scope: instruction_scope_key(&file.display_path),
                    path: file.display_path.clone(),
                    content: file.content.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for file in previous {
        if !current_paths.contains(file.display_path.as_str()) {
            changes.push(InstructionChange {
                action: "remove",
                scope: instruction_scope_key(&file.display_path),
                path: file.display_path.clone(),
                content: String::new(),
            });
        }
    }
    changes
}

fn changes_to_values(changes: &[InstructionChange]) -> Vec<Value> {
    changes
        .iter()
        .map(|change| {
            json!({
                "action": change.action,
                "scope": change.scope,
                "path": change.path,
            })
        })
        .collect()
}

fn baseline_message(
    text: &str,
    changes: Vec<Value>,
    baseline: bool,
    identity: Option<String>,
) -> UserMessage {
    UserMessage::from_parts(
        vec![dsh_llm::ContentBlock::text(text.to_string())],
        MessageSource::agent_instructions(changes, baseline, identity),
    )
}

fn render_updates(changes: &[InstructionChange], max_bytes: usize) -> String {
    let mut sections = Vec::new();
    for change in changes {
        let section = match change.action {
            "set" => {
                let scope = scope_for_display_path(&change.path);
                format!(
                    "Additional instructions from: {}\n\n{ADDITIONAL_INSTRUCTIONS_GUIDANCE} `{scope}`. Use them as guidance when relevant; more specific instructions take precedence. They do not override system, developer, or direct user instructions.\n\n{}",
                    change.path, change.content
                )
            }
            "remove" => format!(
                "Instructions removed: {}\n\n{REMOVED_INSTRUCTIONS_GUIDANCE}",
                change.path
            ),
            _ => format!(
                "Updated instructions from: {}\n\n{UPDATED_INSTRUCTIONS_GUIDANCE}\n\n{}",
                change.path, change.content
            ),
        };
        let trial = if sections.is_empty() {
            frame(&section)
        } else {
            frame(&format!("{}\n\n{section}", sections.join("\n\n")))
        };
        if trial.len() <= max_bytes || sections.is_empty() {
            sections.push(section);
            if trial.len() > max_bytes {
                break;
            }
        } else {
            break;
        }
    }
    frame(&sections.join("\n\n"))
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

fn instruction_scope_key(display_path: &str) -> String {
    let name = Path::new(display_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{}\0{name}", scope_for_display_path(display_path))
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

    fn test_config(dir: &Path) -> Config {
        Config {
            dsh_home: dir.join("home"),
            project_root_markers: vec![".git".into()],
            max_bytes: 65_536,
            max_source_bytes: 1_048_576,
            instruction_file_candidates: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            local_instruction_file_candidates: vec![
                "AGENTS.local.md".into(),
                "CLAUDE.local.md".into(),
            ],
        }
    }

    fn pre_step(ctx: &dsh_cordis::Context, cwd: &Path) -> Value {
        ctx.waterfall(
            "agent/pre-step",
            json!({
                "agentId": "a1",
                "cwd": cwd.to_string_lossy(),
                "messages": [],
                "turn": 1,
            }),
            |payload| payload,
        )
        .unwrap()
    }

    #[test]
    fn install_replaces_changed_file_on_later_pre_step() {
        let dir = std::env::temp_dir().join(format!("dsh-instr-touch-{}", uuid_like()));
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("AGENTS.md"), "old rule").unwrap();
        let ctx = dsh_cordis::Context::new();
        install(&ctx, test_config(&dir)).unwrap();
        let first = pre_step(&ctx, &dir);
        let first_text = first["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(first_text.contains("old rule"), "{first_text}");
        assert_eq!(first["messages"][0]["source"]["baseline"], true);
        fs::write(dir.join("AGENTS.md"), "new rule").unwrap();
        let second = pre_step(&ctx, &dir);
        let second_text = second["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert_eq!(second["messages"].as_array().unwrap().len(), 1);
        assert!(second["messages"][0]["source"].get("baseline").is_none());
        assert!(
            second_text.contains("Updated instructions from: AGENTS.md"),
            "{second_text}"
        );
        assert!(second_text.contains("new rule"), "{second_text}");
        assert!(!second_text.contains("old rule"), "{second_text}");
        let third = pre_step(&ctx, &dir);
        assert_eq!(third["messages"].as_array().unwrap().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_emits_replacement_baseline_when_identity_changes() {
        let dir = std::env::temp_dir().join(format!("dsh-instr-ident-{}", uuid_like()));
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("AGENTS.md"), "root rule").unwrap();
        let pkg = dir.join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("AGENTS.md"), "pkg rule").unwrap();
        let ctx = dsh_cordis::Context::new();
        install(&ctx, test_config(&dir)).unwrap();
        let first = pre_step(&ctx, &dir);
        assert!(first["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("root rule"));
        let second = pre_step(&ctx, &pkg);
        let text = second["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert_eq!(second["messages"][0]["source"]["baseline"], true);
        assert!(
            text.contains("replaces all earlier workspace instruction baselines"),
            "{text}"
        );
        assert!(text.contains("pkg rule"), "{text}");
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
