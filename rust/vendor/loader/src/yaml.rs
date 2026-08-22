//! YAML subset used by `cordis.yml` and `cordis.patch.yml`.
//!
//! Covers the dialect Include mounts: indented maps and lists, `- insert:`
//! nested rows, `!!js` scalars stored as `{ "__jsExpr": "..." }`, `|` / `>-`
//! block scalars, flow sequences, and comments. Unknown tags and mapping
//! documents fail loud. Expressions are not evaluated here.

use serde_json::{Map, Number, Value};

use crate::LoaderError;

/// Parse a YAML document into JSON. An empty document is an empty array.
pub fn parse_yaml_document(yaml: &str) -> Result<Value, LoaderError> {
    if yaml.trim().is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let trimmed = yaml.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(yaml.trim()) {
            return Ok(value);
        }
    }
    let mut parser = Parser::new(yaml);
    parser.parse_document()
}

/// `{ "__jsExpr": expr }` node Include uses for an unevaluated `!!js` scalar.
pub fn js_expr_value(expr: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("__jsExpr".into(), Value::String(expr.into()));
    Value::Object(map)
}

/// Expression text when `value` is a `!!js` node.
pub fn as_js_expr(value: &Value) -> Option<&str> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.get("__jsExpr")?.as_str()
}

struct Line<'a> {
    indent: usize,
    content: &'a str,
    blank: bool,
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(yaml: &'a str) -> Self {
        let lines = yaml
            .lines()
            .map(|raw| {
                let indent = raw.chars().take_while(|ch| *ch == ' ').count();
                let content = raw.get(indent..).unwrap_or("");
                let blank = content.trim().is_empty();
                Line {
                    indent,
                    content,
                    blank,
                }
            })
            .collect();
        Self { lines, i: 0 }
    }

    fn parse_document(&mut self) -> Result<Value, LoaderError> {
        self.skip_noise();
        let Some(line) = self.peek() else {
            return Ok(Value::Array(Vec::new()));
        };
        if line.content.starts_with('-') {
            return self.parse_seq(line.indent);
        }
        Err(LoaderError::Parse(
            "entry list must be an array".into(),
        ))
    }

    fn skip_noise(&mut self) {
        while let Some(line) = self.lines.get(self.i) {
            if line.blank || line.content.starts_with('#') {
                self.i += 1;
                continue;
            }
            break;
        }
    }

    fn peek(&self) -> Option<&Line<'a>> {
        let mut i = self.i;
        while let Some(line) = self.lines.get(i) {
            if line.blank || line.content.starts_with('#') {
                i += 1;
                continue;
            }
            return Some(line);
        }
        None
    }

    fn parse_nested(&mut self, parent_indent: usize) -> Result<Value, LoaderError> {
        self.skip_noise();
        let Some(line) = self.peek() else {
            return Ok(Value::Null);
        };
        if line.indent <= parent_indent {
            return Ok(Value::Null);
        }
        if line.content.starts_with('-') {
            self.parse_seq(line.indent)
        } else {
            self.parse_map(line.indent)
        }
    }

    fn parse_seq(&mut self, seq_indent: usize) -> Result<Value, LoaderError> {
        let mut items = Vec::new();
        loop {
            self.skip_noise();
            let Some(line) = self.peek() else {
                break;
            };
            if line.indent != seq_indent || !line.content.starts_with('-') {
                break;
            }
            let content = line.content.to_string();
            self.i += 1;
            let rest = content
                .strip_prefix('-')
                .map(str::trim_start)
                .unwrap_or("");
            let item = if rest.is_empty() {
                self.parse_nested(seq_indent)?
            } else if split_pair(rest).is_some() {
                self.parse_compact_map(seq_indent, rest)?
            } else {
                parse_scalar(rest)?
            };
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn parse_map(&mut self, map_indent: usize) -> Result<Value, LoaderError> {
        let mut map = Map::new();
        loop {
            self.skip_noise();
            let Some(line) = self.peek() else {
                break;
            };
            if line.indent != map_indent || line.content.starts_with('-') {
                break;
            }
            let content = line.content.to_string();
            self.i += 1;
            self.push_pair(&mut map, &content, map_indent)?;
        }
        Ok(Value::Object(map))
    }

    fn parse_compact_map(&mut self, dash_indent: usize, first: &str) -> Result<Value, LoaderError> {
        let mut map = Map::new();
        self.push_pair(&mut map, first, dash_indent)?;
        let key_indent = dash_indent + 2;
        loop {
            self.skip_noise();
            let Some(line) = self.peek() else {
                break;
            };
            if line.content.starts_with('-') || line.indent != key_indent {
                break;
            }
            let content = line.content.to_string();
            self.i += 1;
            self.push_pair(&mut map, &content, key_indent)?;
        }
        Ok(Value::Object(map))
    }

    fn push_pair(
        &mut self,
        map: &mut Map<String, Value>,
        text: &str,
        key_indent: usize,
    ) -> Result<(), LoaderError> {
        let (key, value) = split_pair(text)
            .ok_or_else(|| LoaderError::Parse(format!("invalid mapping pair: {text}")))?;
        let parsed = self.parse_rhs(value, key_indent)?;
        map.insert(key.to_string(), parsed);
        Ok(())
    }

    fn parse_rhs(&mut self, raw: &str, key_indent: usize) -> Result<Value, LoaderError> {
        let value = raw.trim();
        if value.is_empty() {
            return self.parse_nested(key_indent);
        }
        if let Some(style) = block_style(value) {
            return self.parse_block(style, key_indent);
        }
        if value.starts_with("!!js") {
            return parse_js_tag(value);
        }
        if value.starts_with('[') {
            return parse_flow_seq(value);
        }
        parse_scalar(value)
    }

    fn parse_block(&mut self, style: BlockStyle, key_indent: usize) -> Result<Value, LoaderError> {
        let mut collected: Vec<(bool, String)> = Vec::new();
        let mut content_indent: Option<usize> = None;
        while self.i < self.lines.len() {
            let line = &self.lines[self.i];
            if line.blank {
                collected.push((true, String::new()));
                self.i += 1;
                continue;
            }
            if line.indent <= key_indent {
                break;
            }
            let indent = *content_indent.get_or_insert(line.indent);
            if line.indent < indent {
                break;
            }
            let extra = line.indent - indent;
            let mut text = " ".repeat(extra);
            text.push_str(line.content);
            collected.push((false, text));
            self.i += 1;
        }
        while collected.last().is_some_and(|(blank, _)| *blank) {
            collected.pop();
        }
        let body = match style.kind {
            BlockKind::Literal => collected
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n"),
            BlockKind::Folded => fold_block(&collected),
        };
        Ok(Value::String(match style.chomp {
            Chomp::Strip => body,
            Chomp::Clip | Chomp::Keep => {
                if body.is_empty() {
                    String::new()
                } else {
                    format!("{body}\n")
                }
            }
        }))
    }
}

#[derive(Clone, Copy)]
struct BlockStyle {
    kind: BlockKind,
    chomp: Chomp,
}

#[derive(Clone, Copy)]
enum BlockKind {
    Literal,
    Folded,
}

#[derive(Clone, Copy)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

fn block_style(value: &str) -> Option<BlockStyle> {
    let kind = match value.as_bytes().first()? {
        b'|' => BlockKind::Literal,
        b'>' => BlockKind::Folded,
        _ => return None,
    };
    let chomp = match value.as_bytes().get(1) {
        None => Chomp::Clip,
        Some(b'-') if value.len() == 2 => Chomp::Strip,
        Some(b'+') if value.len() == 2 => Chomp::Keep,
        _ => return None,
    };
    Some(BlockStyle { kind, chomp })
}

fn fold_block(lines: &[(bool, String)]) -> String {
    let mut out = String::new();
    let mut pending_blank = false;
    let mut started = false;
    for (blank, text) in lines {
        if *blank {
            pending_blank = started;
            continue;
        }
        if pending_blank {
            out.push('\n');
            pending_blank = false;
        } else if started {
            out.push(' ');
        }
        out.push_str(text);
        started = true;
    }
    out
}

fn split_pair(text: &str) -> Option<(&str, &str)> {
    let colon = text.find(':')?;
    let key = text[..colon].trim();
    if key.is_empty() {
        return None;
    }
    let after = text.get(colon + 1..)?;
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    Some((key, after.trim()))
}

fn parse_js_tag(value: &str) -> Result<Value, LoaderError> {
    let rest = value
        .strip_prefix("!!js")
        .ok_or_else(|| LoaderError::Parse("invalid !!js tag".into()))?
        .trim();
    if rest.is_empty() {
        return Err(LoaderError::Parse(
            "!!js tag with no expression body".into(),
        ));
    }
    let expr = if rest.starts_with('"') {
        parse_double_quoted(rest)?
    } else if rest.starts_with('\'') {
        parse_single_quoted(rest)?
    } else {
        rest.to_string()
    };
    Ok(js_expr_value(expr))
}

fn parse_flow_seq(value: &str) -> Result<Value, LoaderError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| LoaderError::Parse(format!("invalid flow sequence: {value}")))?;
    if inner.trim().is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let mut items = Vec::new();
    for part in split_flow_items(inner)? {
        items.push(parse_scalar(part.trim())?);
    }
    Ok(Value::Array(items))
}

fn split_flow_items(inner: &str) -> Result<Vec<&str>, LoaderError> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    for (i, ch) in inner.char_indices() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(q), ch) if ch == q => quote = None,
            (None, ',') => {
                items.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err(LoaderError::Parse("unclosed quote in flow sequence".into()));
    }
    items.push(&inner[start..]);
    Ok(items)
}

fn parse_scalar(raw: &str) -> Result<Value, LoaderError> {
    let value = strip_unquoted_comment(raw).trim();
    if value.is_empty() {
        return Ok(Value::Null);
    }
    if value.starts_with("!!js") {
        return parse_js_tag(value);
    }
    if value.starts_with('[') {
        return parse_flow_seq(value);
    }
    if value.starts_with('\'') {
        return Ok(Value::String(parse_single_quoted(value)?));
    }
    if value.starts_with('"') {
        return Ok(Value::String(parse_double_quoted(value)?));
    }
    if value == "true" {
        return Ok(Value::Bool(true));
    }
    if value == "false" {
        return Ok(Value::Bool(false));
    }
    if value == "null" || value == "~" {
        return Ok(Value::Null);
    }
    if let Ok(number) = value.parse::<i64>() {
        return Ok(Value::Number(Number::from(number)));
    }
    Ok(Value::String(value.to_string()))
}

fn strip_unquoted_comment(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') || trimmed.starts_with("!!js") {
        return raw;
    }
    raw.split_once(" #").map(|(left, _)| left).unwrap_or(raw)
}

fn parse_single_quoted(raw: &str) -> Result<String, LoaderError> {
    if !raw.starts_with('\'') {
        return Err(LoaderError::Parse("expected single-quoted scalar".into()));
    }
    let mut out = String::new();
    let mut chars = raw[1..].chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            return Ok(out);
        }
        out.push(ch);
    }
    Err(LoaderError::Parse("unclosed single-quoted scalar".into()))
}

fn parse_double_quoted(raw: &str) -> Result<String, LoaderError> {
    if !raw.starts_with('"') {
        return Err(LoaderError::Parse("expected double-quoted scalar".into()));
    }
    let mut out = String::new();
    let mut chars = raw[1..].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Ok(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {
                    return Err(LoaderError::Parse(
                        "unterminated escape in double-quoted scalar".into(),
                    ))
                }
            },
            other => out.push(other),
        }
    }
    Err(LoaderError::Parse("unclosed double-quoted scalar".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_list_and_js_round_trip_fields() {
        let yaml = r#"
- insert:
    - id: timer
      name: '@deepseek-ai/cordis-plugin-timer'
    - id: hmr
      name: '@deepseek-ai/cordis-plugin-hmr'
      config:
        root: ['.']
- id: tools
  config:
    mode: !!js process.env.DSH_TOOLS_MODE
- insert:
    - id: runner
      name: '@deepseek-ai/dsh-headless'
      inject: [headlessStartup]
      config:
        task: !!js ctx.headlessStartup.task
"#;
        let value = parse_yaml_document(yaml).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 3);
        let insert = items[0]["insert"].as_array().unwrap();
        assert_eq!(insert[0]["id"], "timer");
        assert_eq!(insert[1]["config"]["root"][0], ".");
        assert_eq!(
            as_js_expr(&items[1]["config"]["mode"]).unwrap(),
            "process.env.DSH_TOOLS_MODE"
        );
        assert_eq!(items[2]["insert"][0]["inject"][0], "headlessStartup");
        assert_eq!(
            as_js_expr(&items[2]["insert"][0]["config"]["task"]).unwrap(),
            "ctx.headlessStartup.task"
        );
    }

    #[test]
    fn quoted_js_drops_wrapping_quotes() {
        let yaml = "- id: approval\n  config:\n    policy: !!js \"(a ?? 'b') === 'c' ? 'never' : 'ask'\"\n";
        let value = parse_yaml_document(yaml).unwrap();
        assert_eq!(
            as_js_expr(&value[0]["config"]["policy"]).unwrap(),
            "(a ?? 'b') === 'c' ? 'never' : 'ask'"
        );
    }

    #[test]
    fn folded_and_literal_blocks() {
        let yaml = "- id: prompt\n  config:\n    persona: >-\n      hello world\n    section: |\n      line one\n\n      line two\n";
        let value = parse_yaml_document(yaml).unwrap();
        assert_eq!(value[0]["config"]["persona"], "hello world");
        assert_eq!(value[0]["config"]["section"], "line one\n\nline two\n");
    }

    #[test]
    fn empty_js_tag_fails() {
        let err = parse_yaml_document("- id: x\n  config:\n    a: !!js\n").unwrap_err();
        assert!(err.to_string().contains("!!js"));
    }
}
