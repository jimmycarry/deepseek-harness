//! Model-facing `glob` and `grep` tools. Execution spawns ripgrep through
//! `ctx.subprocess` with a plain argv vector — never `ctx.shell`, never `ctx.fs`.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_subprocess::{resolve, SpawnRequest, SubprocessRuntime};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolError, ToolOutcome, ToolRuntime};
use serde_json::Value;
use std::path::{Path, MAIN_SEPARATOR};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-fs-search"
}

/// Default cap on inline `glob` paths (TypeScript `GLOB_MAX_RESULTS`).
pub const GLOB_MAX_RESULTS: usize = 100;

/// Default cap on inline `grep` matches (TypeScript `GREP_MAX_MATCHES`).
pub const GREP_MAX_MATCHES: usize = 250;

/// Default cap in bytes on one matched-line preview.
pub const GREP_MAX_LINE_BYTES: usize = 2000;

/// Default cap on raw `rg` stdout the tools will parse.
pub const RAW_OUTPUT_MAX_BYTES: usize = 20_000_000;

/// VCS directories ripgrep must not descend into under `--no-ignore --hidden`.
pub const GLOB_VCS_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Resolved search caps. `sample_over_cap_glob_results` is required; the rest
/// are filled by [`Config::resolve`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether an over-cap `glob` page is sampled across top-level entries.
    pub sample_over_cap_glob_results: bool,
    /// Max paths one `glob` call retains inline.
    pub glob_max_results: usize,
    /// Max flat matches one `grep` call retains inline.
    pub grep_max_matches: usize,
    /// Max bytes retained for one matched-line preview.
    pub grep_max_line_bytes: usize,
    /// Max complete raw `rg` stdout bytes a search will parse.
    pub raw_output_max_bytes: usize,
}

impl Config {
    /// Resolve plugin config. `sampleOverCapGlobResults` is required; missing
    /// optional caps take the TypeScript defaults. Non-positive integers fail.
    pub fn resolve(value: Option<&Value>) -> Result<Self, String> {
        let sample = value
            .and_then(|value| value.get("sampleOverCapGlobResults"))
            .and_then(Value::as_bool)
            .ok_or_else(|| "tool-fs-search: sampleOverCapGlobResults is required".to_string())?;
        let glob_max = optional_usize(value, "globMaxResults", GLOB_MAX_RESULTS)?;
        let grep_max = optional_usize(value, "grepMaxMatches", GREP_MAX_MATCHES)?;
        let line_bytes = optional_usize(value, "grepMaxLineBytes", GREP_MAX_LINE_BYTES)?;
        let raw_bytes = optional_usize(value, "rawOutputMaxBytes", RAW_OUTPUT_MAX_BYTES)?;
        Ok(Self {
            sample_over_cap_glob_results: sample,
            glob_max_results: glob_max,
            grep_max_matches: grep_max,
            grep_max_line_bytes: line_bytes,
            raw_output_max_bytes: raw_bytes,
        })
    }

    /// Caps with TypeScript defaults and an explicit sampling switch.
    pub fn with_sample_over_cap(sample_over_cap_glob_results: bool) -> Self {
        Self {
            sample_over_cap_glob_results,
            glob_max_results: GLOB_MAX_RESULTS,
            grep_max_matches: GREP_MAX_MATCHES,
            grep_max_line_bytes: GREP_MAX_LINE_BYTES,
            raw_output_max_bytes: RAW_OUTPUT_MAX_BYTES,
        }
    }
}

fn optional_usize(value: Option<&Value>, field: &str, default: usize) -> Result<usize, String> {
    match value.and_then(|value| value.get(field)) {
        None => Ok(default),
        Some(item) => {
            let number = item
                .as_u64()
                .ok_or_else(|| format!("tool-fs-search: {field} must be a positive integer"))?;
            if number < 1 {
                return Err(format!(
                    "tool-fs-search: {field} must be a positive integer"
                ));
            }
            Ok(number as usize)
        }
    }
}

/// Locate the ripgrep binary. Prefers `PATH`, then `/exec-daemon/rg`.
pub fn resolve_rg_path() -> Result<String, String> {
    if let Some(path) = find_on_path("rg") {
        return Ok(path);
    }
    if Path::new("/exec-daemon/rg").is_file() {
        return Ok("/exec-daemon/rg".into());
    }
    Err("rg binary not found on PATH".into())
}

fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then(|| candidate.display().to_string())
    })
}

/// Validated `glob` arguments.
#[derive(Debug, Clone)]
pub struct GlobInput {
    /// Path glob (ripgrep `--glob` syntax).
    pub pattern: String,
    /// Optional search root.
    pub path: Option<String>,
}

/// Validate value constraints the schema cannot express.
pub fn parse_glob_args(args: &Value) -> Result<GlobInput, String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| "pattern must be a non-empty string".to_string())?;
    if pattern.trim().is_empty() {
        return Err("pattern must be a non-empty string".into());
    }
    let path = match args.get("path").and_then(Value::as_str) {
        None => None,
        Some(path) if path.trim().is_empty() => {
            return Err("path must be a non-empty string when given".into());
        }
        Some(path) => Some(path.to_string()),
    };
    Ok(GlobInput {
        pattern: pattern.to_string(),
        path,
    })
}

/// Build `rg --files` argv (excluding the binary).
pub fn build_glob_command(input: &GlobInput) -> Vec<String> {
    let mut parts = vec![
        "--files".into(),
        format!("--glob={}", input.pattern),
        "--sort=modified".into(),
        "--no-ignore".into(),
        "--hidden".into(),
    ];
    for name in GLOB_VCS_EXCLUDES {
        parts.push(format!("--glob=!**/{name}"));
        parts.push(format!("--glob=!**/{name}/**"));
    }
    if let Some(path) = &input.path {
        parts.push("--".into());
        parts.push(path.clone());
    }
    parts
}

/// Validated `grep` arguments.
#[derive(Debug, Clone)]
pub struct GrepInput {
    /// Ripgrep regular expression.
    pub pattern: String,
    /// Optional file or directory target.
    pub path: Option<String>,
    /// Optional single positive include glob.
    pub include: Option<String>,
}

fn validate_include(include: &str) -> Result<(), String> {
    if include.trim().is_empty() {
        return Err("include must be a non-empty glob when given".into());
    }
    if include.starts_with('!') {
        return Err(
            "include must be a positive glob filter; negated patterns (\"!…\") are not supported"
                .into(),
        );
    }
    let mut brace_depth = 0i32;
    for ch in include.chars() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth = (brace_depth - 1).max(0),
            ',' if brace_depth == 0 => {
                return Err(
                    "include must be one glob, not a comma-separated list (use {a,b} alternation instead)"
                        .into(),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate `grep` arguments.
pub fn parse_grep_args(args: &Value) -> Result<GrepInput, String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| "pattern must be a non-empty string".to_string())?;
    if pattern.is_empty() {
        return Err("pattern must be a non-empty string".into());
    }
    let path = match args.get("path").and_then(Value::as_str) {
        None => None,
        Some(path) if path.trim().is_empty() => {
            return Err("path must be a non-empty string when given".into());
        }
        Some(path) => Some(path.to_string()),
    };
    let include = match args.get("include").and_then(Value::as_str) {
        None => None,
        Some(include) => {
            validate_include(include)?;
            Some(include.to_string())
        }
    };
    Ok(GrepInput {
        pattern: pattern.to_string(),
        path,
        include,
    })
}

/// Build `rg --json` argv (excluding the binary).
pub fn build_grep_command(input: &GrepInput) -> Vec<String> {
    let mut parts = vec!["--json".into(), format!("--regexp={}", input.pattern)];
    if let Some(include) = &input.include {
        parts.push(format!("--glob={include}"));
    }
    if let Some(path) = &input.path {
        parts.push("--".into());
        parts.push(path.clone());
    }
    parts
}

/// One `grep` match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// Display path relative to the workdir when possible.
    pub path: String,
    /// 1-based line number.
    pub line_number: u64,
    /// Matched line text without the trailing newline.
    pub line: String,
}

/// Parse complete `rg --json` stdout into flat matches.
pub fn parse_grep_matches(stdout: &str) -> Result<Vec<GrepMatch>, String> {
    let mut matches = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(item) = parse_grep_record(line)? {
            matches.push(item);
        }
    }
    Ok(matches)
}

fn parse_grep_record(line: &str) -> Result<Option<GrepMatch>, String> {
    let parsed: Value = serde_json::from_str(line).map_err(|_| {
        "grep received malformed ripgrep --json output (a line is not JSON)".to_string()
    })?;
    if !parsed.is_object() {
        return Err(
            "grep received malformed ripgrep --json output (a record is not an object)".into(),
        );
    }
    if parsed.get("type").and_then(Value::as_str) != Some("match") {
        return Ok(None);
    }
    let data = parsed
        .get("data")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            "grep received malformed ripgrep --json output (a match record has no data)".to_string()
        })?;
    let path = data
        .get("path")
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "grep received malformed ripgrep --json output (a match record has no path text)"
                .to_string()
        })?;
    let line_number = data
        .get("line_number")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "grep received malformed ripgrep --json output (a match record has no line number)"
                .to_string()
        })?;
    let lines = data
        .get("lines")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            "grep received malformed ripgrep --json output (a match record has no line content)"
                .to_string()
        })?;
    let line = if let Some(text) = lines.get("text").and_then(Value::as_str) {
        text.trim_end_matches(['\n', '\r']).to_string()
    } else if lines.get("bytes").and_then(Value::as_str).is_some() {
        "(line is not valid UTF-8)".into()
    } else {
        return Err(
            "grep received malformed ripgrep --json output (a match record has neither line text nor bytes)"
                .into(),
        );
    };
    Ok(Some(GrepMatch {
        path: path.to_string(),
        line_number,
        line,
    }))
}

/// Group matches by file for the model-facing body.
pub fn format_grep_matches(matches: &[GrepMatch]) -> String {
    let mut sections: Vec<(String, Vec<&GrepMatch>)> = Vec::new();
    for item in matches {
        if let Some((_, group)) = sections.iter_mut().find(|(path, _)| path == &item.path) {
            group.push(item);
        } else {
            sections.push((item.path.clone(), vec![item]));
        }
    }
    sections
        .into_iter()
        .map(|(path, group)| {
            let rows = group
                .into_iter()
                .map(|item| format!("Line {}: {}", item.line_number, item.line))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{path}\n{rows}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn preview_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_string()
}

fn format_grep_output(matches: &[GrepMatch], seen: usize, max_matches: usize) -> String {
    if seen == 0 {
        return "No matches found".into();
    }
    let truncated = seen > max_matches;
    let kept = matches.len();
    let header = if truncated {
        format!("Found {kept} of {seen} matches")
    } else if seen == 1 {
        "Found 1 match".into()
    } else {
        format!("Found {seen} matches")
    };
    let body = format_grep_matches(matches);
    if !truncated {
        return format!("{header}\n\n{body}");
    }
    format!(
        "{header}\n\n{body}\n\n(The complete result could not be saved; narrow pattern, path, or include to see more.)"
    )
}

fn strip_leading_separators(path: &str) -> &str {
    path.trim_start_matches(MAIN_SEPARATOR)
}

fn relative_to_search_root<'a>(path: &'a str, root: &str) -> &'a str {
    if root == "." {
        return path
            .strip_prefix(&format!(".{MAIN_SEPARATOR}"))
            .unwrap_or(path);
    }
    let trimmed = root.trim_end_matches(MAIN_SEPARATOR);
    if trimmed.is_empty() {
        return strip_leading_separators(path);
    }
    if path == trimmed {
        return "";
    }
    path.strip_prefix(&format!("{trimmed}{MAIN_SEPARATOR}"))
        .unwrap_or(path)
}

fn top_level_segment(path: &str) -> &str {
    let trimmed = strip_leading_separators(path);
    trimmed
        .split_once(MAIN_SEPARATOR)
        .map(|(head, _)| head)
        .unwrap_or(trimmed)
}

/// Sample an over-cap path list round-robin across top-level entries.
pub fn sample_across_top_level(paths: &[String], max_items: usize, root: &str) -> Vec<String> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for path in paths {
        let key = top_level_segment(relative_to_search_root(path, root)).to_string();
        if let Some((_, items)) = groups.iter_mut().find(|(name, _)| name == &key) {
            items.push(path.clone());
        } else {
            groups.push((key, vec![path.clone()]));
        }
    }
    let mut indexes = vec![0usize; groups.len()];
    let mut taken: Vec<Vec<String>> = vec![Vec::new(); groups.len()];
    let mut count = 0;
    while count < max_items {
        let mut progressed = false;
        for (index, (_, items)) in groups.iter().enumerate() {
            if count >= max_items {
                break;
            }
            if indexes[index] >= items.len() {
                continue;
            }
            taken[index].push(items[indexes[index]].clone());
            indexes[index] += 1;
            count += 1;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    taken.into_iter().flatten().collect()
}

fn render_glob_paths(paths: &[String], config: &Config, root: &str) -> String {
    if paths.is_empty() {
        return "No files found".into();
    }
    if paths.len() <= config.glob_max_results {
        return paths.join("\n");
    }
    let page = if config.sample_over_cap_glob_results {
        sample_across_top_level(paths, config.glob_max_results, root)
    } else {
        paths[..config.glob_max_results].to_vec()
    };
    format!(
        "{}\n\n(Showing {} of {} paths. The complete result could not be saved; narrow pattern or path to see more.)",
        page.join("\n"),
        page.len(),
        paths.len()
    )
}

fn to_workdir_relative(path: &str, workdir: &str) -> String {
    let workdir = workdir.trim_end_matches(MAIN_SEPARATOR);
    if path == workdir {
        return ".".into();
    }
    path.strip_prefix(&format!("{workdir}{MAIN_SEPARATOR}"))
        .unwrap_or(path)
        .to_string()
}

struct RipgrepRun {
    stdout: String,
    no_matches: bool,
    workdir: String,
}

async fn run_ripgrep(
    subprocess: &SubprocessRuntime,
    tool_name: &str,
    argv: Vec<String>,
    raw_output_max_bytes: usize,
    cwd: Option<String>,
) -> Result<RipgrepRun, String> {
    let program = resolve_rg_path()?;
    let workdir = cwd
        .clone()
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| ".".into());
    let mut args = vec!["--no-config".into()];
    args.extend(argv);
    let output = subprocess
        .run(resolve(SpawnRequest {
            program,
            args,
            cwd: Some(workdir.clone()),
        }))
        .await
        .map_err(|error| format!("{tool_name} search failed: {error}"))?;
    if output.stdout.len() > raw_output_max_bytes {
        return Err(format!(
            "{tool_name} produced {} bytes of raw output, over the {raw_output_max_bytes}-byte cap; narrow pattern, path, or include and retry",
            output.stdout.len()
        ));
    }
    if output.status == 1 {
        return Ok(RipgrepRun {
            stdout: output.stdout,
            no_matches: true,
            workdir,
        });
    }
    if output.status != 0 {
        let stderr = output.stderr.trim();
        if stderr.to_ascii_lowercase().contains("regex parse error")
            || stderr.to_ascii_lowercase().contains("error parsing glob")
        {
            return Err(format!("{tool_name} pattern rejected by ripgrep: {stderr}"));
        }
        return Err(if stderr.is_empty() {
            format!("{tool_name} search failed (exit {})", output.status)
        } else {
            format!(
                "{tool_name} search failed (exit {}): {stderr}",
                output.status
            )
        });
    }
    Ok(RipgrepRun {
        stdout: output.stdout,
        no_matches: false,
        workdir,
    })
}

/// `glob` tool.
pub struct GlobTool {
    subprocess: Arc<SubprocessRuntime>,
    config: Config,
}

impl GlobTool {
    /// Bind to `ctx.subprocess` with resolved caps.
    pub fn new(subprocess: Arc<SubprocessRuntime>, config: Config) -> Self {
        Self { subprocess, config }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files whose paths match a glob pattern. Returns matching file paths — never directories."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let input = parse_glob_args(&args).map_err(ToolError::Body)?;
        let cwd = input.path.clone();
        let run = run_ripgrep(
            &self.subprocess,
            "glob",
            build_glob_command(&input),
            self.config.raw_output_max_bytes,
            cwd,
        )
        .await
        .map_err(ToolError::Body)?;
        let root = input.path.as_deref().unwrap_or(".");
        if run.no_matches {
            return Ok(ToolOutcome::text("No files found"));
        }
        let paths: Vec<String> = run
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| to_workdir_relative(line, &run.workdir))
            .collect();
        Ok(ToolOutcome::text(render_glob_paths(
            &paths,
            &self.config,
            root,
        )))
    }
}

/// `grep` tool.
pub struct GrepTool {
    subprocess: Arc<SubprocessRuntime>,
    config: Config,
}

impl GrepTool {
    /// Bind to `ctx.subprocess` with resolved caps.
    pub fn new(subprocess: Arc<SubprocessRuntime>, config: Config) -> Self {
        Self { subprocess, config }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a ripgrep regular expression. Returns matching lines with line numbers, grouped by file."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "include": { "type": "string" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let input = parse_grep_args(&args).map_err(ToolError::Body)?;
        let cwd = input.path.clone();
        let run = run_ripgrep(
            &self.subprocess,
            "grep",
            build_grep_command(&input),
            self.config.raw_output_max_bytes,
            cwd,
        )
        .await
        .map_err(ToolError::Body)?;
        if run.no_matches {
            return Ok(ToolOutcome::text("No matches found"));
        }
        let all = parse_grep_matches(&run.stdout).map_err(ToolError::Body)?;
        let previewed: Vec<GrepMatch> = all
            .into_iter()
            .map(|item| GrepMatch {
                path: to_workdir_relative(&item.path, &run.workdir),
                line_number: item.line_number,
                line: preview_line(&item.line, self.config.grep_max_line_bytes),
            })
            .collect();
        let seen = previewed.len();
        let kept = if seen > self.config.grep_max_matches {
            &previewed[..self.config.grep_max_matches]
        } else {
            &previewed
        };
        Ok(ToolOutcome::text(format_grep_output(
            kept,
            seen,
            self.config.grep_max_matches,
        )))
    }
}

/// Register `glob` and `grep` on `ctx.tools`. Requires `ctx.subprocess`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let subprocess = ctx.service::<SubprocessRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-fs-search requires ctx.subprocess".into())
    })?;
    let tools = ctx.service::<ToolRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-fs-search requires ctx.tools".into())
    })?;
    tools.insert(Arc::new(GlobTool::new(
        Arc::clone(&subprocess),
        config.clone(),
    )));
    tools.insert(Arc::new(GrepTool::new(subprocess, config.clone())));
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        let over_cap = if config.sample_over_cap_glob_results {
            "while a larger one is sampled across top-level entries, so it spans the tree instead of one subtree."
        } else {
            "while a larger one keeps the modification-time-ordered head."
        };
        prompt.register_section(PromptSection {
            id: "tool:glob".into(),
            order: 103,
            text: format!(
                "Use the glob tool — not shell find — to discover files by path pattern. A pattern with no \"/\" matches basenames at any depth, so \"*\" matches every file in the tree rather than its top level. Results are files only, never directories, and include hidden and ignored files: a result that fits comes back in modification-time order, {over_cap}"
            ),
        });
        prompt.register_section(PromptSection {
            id: "tool:grep".into(),
            order: 104,
            text: "Use the grep tool — not shell grep or rg — to search file contents. Use read on a matched file when you need surrounding context.".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_subprocess_local::LocalSubprocess;
    use dsh_tools::ToolRuntime;

    fn subprocess() -> Arc<SubprocessRuntime> {
        Arc::new(SubprocessRuntime::new(Arc::new(LocalSubprocess)))
    }

    fn config() -> Config {
        Config::resolve(Some(&serde_json::json!({
            "sampleOverCapGlobResults": false
        })))
        .unwrap()
    }

    #[test]
    fn resolve_requires_sample_flag() {
        let err = Config::resolve(Some(&serde_json::json!({}))).unwrap_err();
        assert!(err.contains("sampleOverCapGlobResults"));
    }

    #[test]
    fn build_glob_command_excludes_vcs() {
        let argv = build_glob_command(&GlobInput {
            pattern: "**/*.rs".into(),
            path: Some("/tmp".into()),
        });
        assert_eq!(argv[0], "--files");
        assert_eq!(argv[1], "--glob=**/*.rs");
        assert!(argv.contains(&"--glob=!**/.git".into()));
        assert!(argv.ends_with(&["--".into(), "/tmp".into()]));
    }

    #[test]
    fn parse_grep_rejects_include_list() {
        let err = parse_grep_args(&serde_json::json!({
            "pattern": "foo",
            "include": "*.ts,*.js"
        }))
        .unwrap_err();
        assert!(err.contains("comma-separated"));
        assert!(parse_grep_args(&serde_json::json!({
            "pattern": "foo",
            "include": "*.{ts,tsx}"
        }))
        .is_ok());
    }

    #[test]
    fn parse_grep_json_match() {
        let stdout = r#"{"type":"begin","data":{}}
{"type":"match","data":{"path":{"text":"src/a.rs"},"line_number":3,"lines":{"text":"hello\n"}}}
{"type":"end","data":{}}
"#;
        let matches = parse_grep_matches(stdout).unwrap();
        assert_eq!(
            matches,
            [GrepMatch {
                path: "src/a.rs".into(),
                line_number: 3,
                line: "hello".into(),
            }]
        );
    }

    #[test]
    fn sample_round_robin() {
        let paths = vec![
            "a/1".into(),
            "a/2".into(),
            "b/1".into(),
            "b/2".into(),
            "c/1".into(),
        ];
        assert_eq!(
            sample_across_top_level(&paths, 3, "."),
            ["a/1", "b/1", "c/1"]
        );
    }

    #[tokio::test]
    async fn glob_and_grep_find_a_real_file() {
        let root = std::env::temp_dir().join(format!(
            "dsh-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "hello-search\n").unwrap();
        let subprocess = subprocess();
        let glob = GlobTool::new(Arc::clone(&subprocess), config());
        let grep = GrepTool::new(subprocess, config());
        let found = glob
            .execute(serde_json::json!({
                "pattern": "*.txt",
                "path": root.to_str().unwrap()
            }))
            .await
            .unwrap();
        let glob_text = match &found.content[0] {
            dsh_llm::ContentBlock::Text { text } => text,
            _ => panic!("text"),
        };
        assert!(glob_text.contains("note.txt"), "glob output: {glob_text}");
        let grepped = grep
            .execute(serde_json::json!({
                "pattern": "hello-search",
                "path": root.to_str().unwrap()
            }))
            .await
            .unwrap();
        let grep_text = match &grepped.content[0] {
            dsh_llm::ContentBlock::Text { text } => text,
            _ => panic!("text"),
        };
        assert!(
            grep_text.contains("hello-search"),
            "grep output: {grep_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_registers_both_tools() {
        let ctx = Context::new();
        ctx.provide(subprocess()).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        install(&ctx, config()).unwrap();
        let tools = ctx.service::<ToolRuntime>().unwrap();
        let names: Vec<_> = tools.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"glob".into()));
        assert!(names.contains(&"grep".into()));
    }
}
