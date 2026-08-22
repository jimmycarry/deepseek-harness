//! Model-facing `str_replace_editor` over the filesystem Service Definition.

use async_trait::async_trait;
use dsh_cordis::Context;
use dsh_fs::{FsError, FsKind, FsRuntime};
use dsh_tools::{Tool, ToolError, ToolOutcome, ToolRuntime};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-str-replace-editor"
}

const TRUNCATED_MESSAGE: &str = "<response clipped><NOTE>To save on context only part of this file has been shown to you. You should retry this tool after you have searched inside the file with `grep -n` in order to find the line numbers of what you are looking for.</NOTE>";

const DEFAULT_DESCRIPTION: &str = "\
Custom editing tool for viewing, creating and editing files
* State is persistent across command calls and discussions with the user
* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep
* The `create` command cannot be used if the specified `path` already exists as a file
* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`

Notes for using the `str_replace` command:
* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!
* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique
* The `new_str` parameter should contain the edited lines that should replace the `old_str`";

/// Resolved editor caps. Defaults match TypeScript schemastery.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum returned view characters before clipping.
    pub max_output_chars: usize,
    /// Model-facing tool description.
    pub description: String,
}

impl Config {
    /// Resolve plugin config. Absent fields take the TypeScript defaults.
    pub fn resolve(value: Option<&Value>) -> Result<Self, String> {
        let max_output_chars = match value.and_then(|value| value.get("maxOutputChars")) {
            None => 16_000,
            Some(item) => {
                let number = item.as_u64().ok_or_else(|| {
                    "tool-str-replace-editor: maxOutputChars must be a positive safe integer"
                        .to_string()
                })?;
                if number < 1 {
                    return Err(
                        "tool-str-replace-editor: maxOutputChars must be a positive safe integer"
                            .into(),
                    );
                }
                number as usize
            }
        };
        let description = match value.and_then(|value| value.get("description")) {
            None => DEFAULT_DESCRIPTION.to_string(),
            Some(item) => {
                let text = item.as_str().ok_or_else(|| {
                    "tool-str-replace-editor: description must be non-empty".to_string()
                })?;
                if text.trim().is_empty() {
                    return Err("tool-str-replace-editor: description must be non-empty".into());
                }
                text.to_string()
            }
        };
        Ok(Self {
            max_output_chars,
            description,
        })
    }
}

fn maybe_truncate(content: &str, max_output_chars: usize) -> String {
    if content.len() <= max_output_chars {
        return content.to_string();
    }
    let mut end = max_output_chars;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED_MESSAGE}", &content[..end])
}

fn require_absolute(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path must be a non-empty string".into());
    }
    if !Path::new(path).is_absolute() {
        return Err(format!(
            "The path {path} is not an absolute path, it should start with `/`. Maybe you meant /{path}?"
        ));
    }
    Ok(())
}

fn match_offsets(content: &str, search: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut start = 0;
    while let Some(index) = content[start..].find(search) {
        let offset = start + index;
        offsets.push(offset);
        start = offset + search.len();
        if search.is_empty() {
            break;
        }
    }
    offsets
}

fn line_numbers_at(content: &str, offsets: &[usize]) -> Vec<usize> {
    let mut line = 1usize;
    let mut cursor = 0usize;
    offsets
        .iter()
        .map(|&offset| {
            while cursor < offset {
                if content.as_bytes().get(cursor) == Some(&b'\n') {
                    line += 1;
                }
                cursor += 1;
            }
            line
        })
        .collect()
}

fn required_for_command(
    value: Option<&str>,
    parameter: &str,
    command: &str,
    allow_empty: bool,
) -> Result<String, String> {
    let Some(value) = value else {
        return Err(format!(
            "Parameter `{parameter}` is required for command: {command}"
        ));
    };
    if !allow_empty && value.is_empty() {
        return Err(format!(
            "Parameter `{parameter}` is empty for command: {command}"
        ));
    }
    Ok(value.to_string())
}

fn format_file_view(
    path: &str,
    content: &str,
    max_output_chars: usize,
    view_range: Option<&[i64]>,
) -> Result<String, String> {
    let all_lines: Vec<&str> = content.split('\n').collect();
    let mut initial_line = 1i64;
    let mut lines = all_lines.as_slice();
    let mut prompt = format!(
        "Here's the content of {path} with line numbers (which has a total of {} lines)",
        all_lines.len()
    );
    if let Some(range) = view_range {
        if range.len() != 2 {
            return Err("Invalid `view_range`. It should be a list of two integers.".into());
        }
        let requested_initial = range[0];
        let requested_final = range[1];
        initial_line = requested_initial;
        if initial_line < 1 || initial_line as usize > all_lines.len() {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its first element `{initial_line}` should be within the range of lines of the file: [1, {}]",
                range[0],
                range[1],
                all_lines.len()
            ));
        }
        if requested_final > all_lines.len() as i64 {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its second element `{requested_final}` should be smaller than the number of lines in the file: `{}`",
                range[0],
                range[1],
                all_lines.len()
            ));
        }
        if requested_final != -1 && requested_final < initial_line {
            return Err(format!(
                "Invalid `view_range`: [{}, {}]. Its second element `{requested_final}` should be larger or equal than its first `{initial_line}`",
                range[0],
                range[1]
            ));
        }
        lines = if requested_final == -1 {
            &all_lines[(initial_line as usize) - 1..]
        } else {
            &all_lines[(initial_line as usize) - 1..requested_final as usize]
        };
        prompt.push_str(&format!(
            " with view_range=[{initial_line}, {requested_final}]"
        ));
    }
    let numbered = lines
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>6}  {line}", initial_line + index as i64))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(maybe_truncate(
        &format!("{prompt}:\n{numbered}\n"),
        max_output_chars,
    ))
}

/// `str_replace_editor` tool.
pub struct StrReplaceEditorTool {
    fs: Arc<FsRuntime>,
    config: Config,
}

impl StrReplaceEditorTool {
    /// Bind to `ctx.fs`.
    pub fn new(fs: Arc<FsRuntime>, config: Config) -> Self {
        Self { fs, config }
    }

    async fn list_directory(&self, path: &str) -> Result<String, String> {
        async fn visit(fs: &FsRuntime, dir: &str, depth: u32) -> Result<Vec<String>, String> {
            let entries = fs.list_dir(dir).await.map_err(|error| error.to_string())?;
            let mut rows = Vec::new();
            for entry in entries {
                if entry.name.starts_with('.')
                    || entry.name == "node_modules"
                    || entry.name == "__pycache__"
                {
                    continue;
                }
                let child = format!("{dir}/{}", entry.name);
                let kind = match entry.kind {
                    FsKind::Directory => "d",
                    FsKind::File => "f",
                    FsKind::Other => "?",
                };
                rows.push(format!("{kind}\t{child}"));
                if entry.kind == FsKind::Directory && depth < 2 {
                    rows.extend(Box::pin(visit(fs, &child, depth + 1)).await?);
                }
            }
            Ok(rows)
        }
        let mut rows = vec![format!("d\t{path}")];
        rows.extend(visit(&self.fs, path, 1).await?);
        rows.sort_by(|left, right| {
            let left_path = left.split_once('\t').map(|(_, path)| path).unwrap_or(left);
            let right_path = right
                .split_once('\t')
                .map(|(_, path)| path)
                .unwrap_or(right);
            left_path.cmp(right_path)
        });
        let listing = maybe_truncate(
            &format!("{}\n", rows.join("\n")),
            self.config.max_output_chars,
        );
        Ok(format!(
            "Here're the files and directories up to 2 levels deep in {path}, excluding hidden items, node_modules, and Python cache directories:\n{listing}\n"
        ))
    }

    async fn view(&self, path: &str, view_range: Option<Vec<i64>>) -> Result<String, String> {
        require_absolute(path)?;
        let info = self.fs.stat(path).await.map_err(map_fs)?.ok_or_else(|| {
            format!("The path {path} does not exist. Please provide a valid path.")
        })?;
        match info.kind {
            FsKind::Directory => {
                if view_range.is_some() {
                    return Err(
                        "The `view_range` parameter is not allowed when `path` points to a directory."
                            .into(),
                    );
                }
                self.list_directory(path).await
            }
            FsKind::File => {
                let content = self.fs.read_text(path).await.map_err(map_fs)?;
                format_file_view(
                    path,
                    &content,
                    self.config.max_output_chars,
                    view_range.as_deref(),
                )
            }
            FsKind::Other => Err(format!(
                "cannot view \"{path}\": not a regular file or directory"
            )),
        }
    }

    async fn create(&self, path: &str, file_text: Option<&str>) -> Result<String, String> {
        require_absolute(path)?;
        let content = required_for_command(file_text, "file_text", "create", true)?;
        if self.fs.stat(path).await.map_err(map_fs)?.is_some() {
            return Err(format!(
                "File already exists at: {path}. Cannot overwrite files using command `create`."
            ));
        }
        self.fs.write_text(path, &content).await.map_err(map_fs)?;
        Ok(format!("New file created successfully at: {path}"))
    }

    async fn str_replace(
        &self,
        path: &str,
        old_str: Option<&str>,
        new_str: Option<&str>,
    ) -> Result<String, String> {
        require_absolute(path)?;
        let old_value = required_for_command(old_str, "old_str", "str_replace", false)?;
        let new_value = new_str.unwrap_or("");
        let info = self.fs.stat(path).await.map_err(map_fs)?.ok_or_else(|| {
            format!("The path {path} does not exist. Please provide a valid path.")
        })?;
        if info.kind == FsKind::Directory {
            return Err(format!(
                "The path {path} is a directory and only the `view` command can be used on directories"
            ));
        }
        if info.kind != FsKind::File {
            return Err(format!("cannot edit \"{path}\": not a regular file"));
        }
        let before = self.fs.read_text(path).await.map_err(map_fs)?;
        let offsets = match_offsets(&before, &old_value);
        match offsets.as_slice() {
            [] => Err(format!(
                "No replacement was performed, old_str `{old_value}` did not appear verbatim in {path}."
            )),
            [_] => {
                let after = before.replacen(&old_value, new_value, 1);
                self.fs.write_text(path, &after).await.map_err(map_fs)?;
                Ok(format!("The file {path} has been edited successfully."))
            }
            many => {
                let lines = line_numbers_at(&before, many);
                let list = lines
                    .iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "No replacement was performed. Multiple occurrences of old_str `{old_value}` in lines [{list}]. Please ensure it is unique"
                ))
            }
        }
    }

    async fn insert(
        &self,
        path: &str,
        insert_line: Option<i64>,
        new_str: Option<&str>,
    ) -> Result<String, String> {
        require_absolute(path)?;
        let insert_line = insert_line
            .ok_or_else(|| "Parameter `insert_line` is required for command: insert".to_string())?;
        let value = required_for_command(new_str, "new_str", "insert", true)?;
        let info = self.fs.stat(path).await.map_err(map_fs)?.ok_or_else(|| {
            format!("The path {path} does not exist. Please provide a valid path.")
        })?;
        if info.kind != FsKind::File {
            return Err(format!("cannot insert into \"{path}\": not a regular file"));
        }
        let before = self.fs.read_text(path).await.map_err(map_fs)?;
        let lines: Vec<&str> = before.split('\n').collect();
        if insert_line < 0 || insert_line as usize > lines.len() {
            return Err(format!(
                "Invalid `insert_line` parameter: {insert_line}. It should be within the range of lines of the file: [0, {}]",
                lines.len()
            ));
        }
        let at = insert_line as usize;
        let mut after: Vec<&str> = Vec::new();
        after.extend(&lines[..at]);
        after.extend(value.split('\n'));
        after.extend(&lines[at..]);
        self.fs
            .write_text(path, &after.join("\n"))
            .await
            .map_err(map_fs)?;
        Ok(format!("The file {path} has been edited successfully."))
    }
}

fn map_fs(error: FsError) -> String {
    error.to_string()
}

fn view_range_from(args: &Value) -> Result<Option<Vec<i64>>, String> {
    let Some(value) = args.get("view_range") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err("Invalid `view_range`. It should be a list of two integers.".into());
    };
    let mut out = Vec::new();
    for item in items {
        let Some(number) = item.as_i64() else {
            return Err("Invalid `view_range`. It should be a list of two integers.".into());
        };
        out.push(number);
    }
    Ok(Some(out))
}

#[async_trait]
impl Tool for StrReplaceEditorTool {
    fn name(&self) -> &str {
        "str_replace_editor"
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["view", "create", "str_replace", "insert"]
                },
                "path": { "type": "string" },
                "file_text": { "type": "string" },
                "insert_line": { "type": "integer" },
                "new_str": { "type": "string" },
                "old_str": { "type": "string" },
                "view_range": { "type": "array", "items": { "type": "integer" } }
            },
            "required": ["command", "path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("command required".into()))?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("path required".into()))?;
        let result = match command {
            "view" => {
                let range = view_range_from(&args).map_err(ToolError::Body)?;
                self.view(path, range).await
            }
            "create" => {
                self.create(path, args.get("file_text").and_then(Value::as_str))
                    .await
            }
            "str_replace" => {
                self.str_replace(
                    path,
                    args.get("old_str").and_then(Value::as_str),
                    args.get("new_str").and_then(Value::as_str),
                )
                .await
            }
            "insert" => {
                self.insert(
                    path,
                    args.get("insert_line").and_then(Value::as_i64),
                    args.get("new_str").and_then(Value::as_str),
                )
                .await
            }
            other => Err(format!("unknown command: {other}")),
        };
        match result {
            Ok(text) => Ok(ToolOutcome::text(text)),
            Err(error) => Ok(ToolOutcome::error(error)),
        }
    }
}

/// Register `str_replace_editor` on `ctx.tools`. Requires `ctx.fs`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<()> {
    let fs = ctx.service::<FsRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-str-replace-editor requires ctx.fs".into())
    })?;
    let tools = ctx.service::<ToolRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-str-replace-editor requires ctx.tools".into())
    })?;
    tools.insert(Arc::new(StrReplaceEditorTool::new(fs, config)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_fs_local::LocalFs;
    use dsh_tools::ToolRuntime;

    fn tool(root: &std::path::Path) -> StrReplaceEditorTool {
        let _ = root;
        StrReplaceEditorTool::new(
            Arc::new(FsRuntime::new(Arc::new(LocalFs::new()))),
            Config::resolve(None).unwrap(),
        )
    }

    fn text(outcome: &ToolOutcome) -> &str {
        match &outcome.content[0] {
            dsh_llm::ContentBlock::Text { text } => text,
            _ => panic!("text"),
        }
    }

    #[test]
    fn relative_path_is_rejected() {
        let err = require_absolute("rel/file.txt").unwrap_err();
        assert!(err.contains("not an absolute path"));
    }

    #[tokio::test]
    async fn create_replace_view_and_unique_match() {
        let root = std::env::temp_dir().join(format!(
            "dsh-editor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("note.txt");
        let path_str = path.to_str().unwrap().to_string();
        let editor = tool(&root);
        let created = editor
            .execute(serde_json::json!({
                "command": "create",
                "path": path_str,
                "file_text": "alpha\nbeta\nalpha\n"
            }))
            .await
            .unwrap();
        assert!(!created.is_error);
        assert!(text(&created).contains("New file created successfully"));
        let again = editor
            .execute(serde_json::json!({
                "command": "create",
                "path": path_str,
                "file_text": "nope"
            }))
            .await
            .unwrap();
        assert!(again.is_error);
        let ambiguous = editor
            .execute(serde_json::json!({
                "command": "str_replace",
                "path": path_str,
                "old_str": "alpha",
                "new_str": "gamma"
            }))
            .await
            .unwrap();
        assert!(ambiguous.is_error);
        assert!(text(&ambiguous).contains("Multiple occurrences"));
        let replaced = editor
            .execute(serde_json::json!({
                "command": "str_replace",
                "path": path_str,
                "old_str": "beta",
                "new_str": "delta"
            }))
            .await
            .unwrap();
        assert!(!replaced.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\ndelta\nalpha\n"
        );
        let viewed = editor
            .execute(serde_json::json!({
                "command": "view",
                "path": path_str
            }))
            .await
            .unwrap();
        assert!(text(&viewed).contains("delta"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_registers_the_tool() {
        let ctx = Context::new();
        ctx.provide(Arc::new(FsRuntime::new(Arc::new(LocalFs::new()))))
            .unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        install(&ctx, Config::resolve(None).unwrap()).unwrap();
        let tools = ctx.service::<ToolRuntime>().unwrap();
        assert!(tools.get("str_replace_editor").is_some());
    }
}
