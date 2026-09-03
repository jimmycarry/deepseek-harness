//! Comment-preserving YAML mapping patcher for settings documents.
//!
//! Nested maps recurse; arrays and scalars replace wholesale when unequal so
//! comments on untouched sibling keys survive.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Blank,
    Comment {
        indent: usize,
        text: String,
    },
    Pair {
        indent: usize,
        key: String,
        inline: Option<String>,
        children: Vec<Node>,
    },
    SequenceItem {
        indent: usize,
        inline: Option<String>,
        children: Vec<Node>,
    },
}

/// Patch one namespace section into `text`, preserving comments on untouched nodes.
pub fn patch_namespace(text: Option<&str>, ns: &str, section: &Value) -> String {
    let mut nodes = text
        .map(parse_document)
        .unwrap_or_default()
        .into_iter()
        .filter(|node| !matches!(node, Node::Blank) || text.is_some())
        .collect::<Vec<_>>();
    if nodes
        .iter()
        .all(|node| matches!(node, Node::Blank | Node::Comment { .. }))
        && text.map(str::trim).unwrap_or("").is_empty()
    {
        nodes.clear();
    }
    let current = json_of(&nodes, ns);
    patch_nodes(&mut nodes, 0, ns, current.as_ref(), section);
    let mut rendered = render(&nodes);
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn parse_document(text: &str) -> Vec<Node> {
    let lines: Vec<&str> = text.lines().collect();
    parse_block(&lines, 0, 0).0
}

fn parse_block<'a>(lines: &[&'a str], start: usize, indent: usize) -> (Vec<Node>, usize) {
    let mut nodes = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let raw = lines[i];
        let line_indent = raw.chars().take_while(|ch| *ch == ' ').count();
        let content = raw.get(line_indent..).unwrap_or("").trim_end();
        if content.is_empty() {
            if line_indent < indent && indent > 0 {
                break;
            }
            nodes.push(Node::Blank);
            i += 1;
            continue;
        }
        if line_indent < indent {
            break;
        }
        if line_indent > indent && indent > 0 {
            break;
        }
        if content.starts_with('#') {
            nodes.push(Node::Comment {
                indent: line_indent,
                text: content.to_string(),
            });
            i += 1;
            continue;
        }
        if let Some(item) = content.strip_prefix("- ") {
            let (children, next) = peek_children(lines, i + 1, line_indent);
            nodes.push(Node::SequenceItem {
                indent: line_indent,
                inline: Some(item.to_string()),
                children,
            });
            i = next;
            continue;
        }
        if content == "-" {
            let (children, next) = peek_children(lines, i + 1, line_indent);
            nodes.push(Node::SequenceItem {
                indent: line_indent,
                inline: None,
                children,
            });
            i = next;
            continue;
        }
        let Some((key, rest)) = split_pair(content) else {
            nodes.push(Node::Comment {
                indent: line_indent,
                text: content.to_string(),
            });
            i += 1;
            continue;
        };
        let (children, next) = if rest.is_empty() {
            peek_children(lines, i + 1, line_indent)
        } else {
            (Vec::new(), i + 1)
        };
        nodes.push(Node::Pair {
            indent: line_indent,
            key,
            inline: if rest.is_empty() { None } else { Some(rest) },
            children,
        });
        i = next;
    }
    (nodes, i)
}

fn peek_children<'a>(lines: &[&'a str], start: usize, parent_indent: usize) -> (Vec<Node>, usize) {
    if start >= lines.len() {
        return (Vec::new(), start);
    }
    let raw = lines[start];
    let line_indent = raw.chars().take_while(|ch| *ch == ' ').count();
    let content = raw.get(line_indent..).unwrap_or("").trim_end();
    if content.is_empty() || content.starts_with('#') {
        if line_indent > parent_indent || content.starts_with('#') || content.is_empty() {
            return parse_block(lines, start, parent_indent + 2);
        }
    }
    if line_indent > parent_indent {
        return parse_block(lines, start, line_indent);
    }
    (Vec::new(), start)
}

fn split_pair(content: &str) -> Option<(String, String)> {
    let (key, rest) = content.split_once(':')?;
    if key.trim().is_empty() || key.starts_with('-') {
        return None;
    }
    Some((key.trim().to_string(), rest.trim().to_string()))
}

fn json_of(nodes: &[Node], key: &str) -> Option<Value> {
    nodes.iter().find_map(|node| match node {
        Node::Pair {
            key: found,
            inline,
            children,
            ..
        } if found == key => Some(node_value(inline.as_deref(), children)),
        _ => None,
    })
}

fn node_value(inline: Option<&str>, children: &[Node]) -> Value {
    if let Some(inline) = inline {
        return scalar(inline);
    }
    if children
        .iter()
        .any(|node| matches!(node, Node::SequenceItem { .. }))
    {
        return Value::Array(
            children
                .iter()
                .filter_map(|node| match node {
                    Node::SequenceItem {
                        inline, children, ..
                    } => Some(node_value(inline.as_deref(), children)),
                    _ => None,
                })
                .collect(),
        );
    }
    let mut map = Map::new();
    for node in children {
        if let Node::Pair {
            key,
            inline,
            children,
            ..
        } = node
        {
            map.insert(key.clone(), node_value(inline.as_deref(), children));
        }
    }
    Value::Object(map)
}

fn scalar(text: &str) -> Value {
    let (value, _) = split_trailing_comment(text);
    let trimmed = value.trim();
    if trimmed == "true" {
        return Value::Bool(true);
    }
    if trimmed == "false" {
        return Value::Bool(false);
    }
    if trimmed == "null" || trimmed == "~" || trimmed.is_empty() {
        return Value::Null;
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return Value::from(number);
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    Value::String(unquoted.to_string())
}

fn split_trailing_comment(text: &str) -> (String, String) {
    let mut in_single = false;
    let mut in_double = false;
    for (index, ch) in text.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => {
                return (text[..index].to_string(), text[index..].to_string());
            }
            _ => {}
        }
    }
    (text.to_string(), String::new())
}

fn patch_nodes(
    nodes: &mut Vec<Node>,
    indent: usize,
    key: &str,
    current: Option<&Value>,
    next: &Value,
) {
    let position = nodes
        .iter()
        .position(|node| matches!(node, Node::Pair { key: found, .. } if found == key));
    if next.is_null() {
        if let Some(index) = position {
            remove_pair(nodes, index);
        }
        return;
    }
    match (current, next) {
        (Some(Value::Object(current_map)), Value::Object(next_map)) if position.is_some() => {
            let index = position.expect("pair");
            if let Node::Pair { children, .. } = &mut nodes[index] {
                for child_key in current_map.keys() {
                    if !next_map.contains_key(child_key) {
                        if let Some(child_index) = children.iter().position(
                            |node| matches!(node, Node::Pair { key, .. } if key == child_key),
                        ) {
                            remove_pair(children, child_index);
                        }
                    }
                }
                for (child_key, child_next) in next_map {
                    patch_nodes(
                        children,
                        indent + 2,
                        child_key,
                        current_map.get(child_key),
                        child_next,
                    );
                }
            }
        }
        _ => {
            if position.is_some() && current.is_some_and(|value| json_equal(value, next)) {
                return;
            }
            if let Some(index) = position {
                if !next.is_object() && !next.is_array() {
                    if let Node::Pair {
                        indent: pair_indent,
                        key: pair_key,
                        inline,
                        ..
                    } = &nodes[index]
                    {
                        let pair_indent = *pair_indent;
                        let pair_key = pair_key.clone();
                        let existing = inline.clone();
                        nodes[index] = Node::Pair {
                            indent: pair_indent,
                            key: pair_key,
                            inline: Some(keep_or_emit(existing.as_deref(), next)),
                            children: Vec::new(),
                        };
                        return;
                    }
                }
                let replacement = value_to_nodes(indent, key, next);
                replace_pair_keeping_leading_comments(nodes, index, replacement);
            } else {
                append_pair(nodes, value_to_nodes(indent, key, next));
            }
        }
    }
}

fn json_equal(left: &Value, right: &Value) -> bool {
    left == right
}

fn remove_pair(nodes: &mut Vec<Node>, index: usize) {
    let mut start = index;
    while start > 0 {
        match &nodes[start - 1] {
            Node::Comment { .. } => start -= 1,
            _ => break,
        }
    }
    nodes.drain(start..=index);
}

fn replace_pair_keeping_leading_comments(nodes: &mut Vec<Node>, index: usize, replacement: Node) {
    if let (
        Node::Pair {
            inline, children, ..
        },
        Node::Pair {
            inline: next_inline,
            children: next_children,
            ..
        },
    ) = (&nodes[index], &replacement)
    {
        let _ = (inline, children, next_inline, next_children);
    }
    nodes[index] = replacement;
}

fn append_pair(nodes: &mut Vec<Node>, pair: Node) {
    while matches!(nodes.last(), Some(Node::Blank)) {
        nodes.pop();
    }
    nodes.push(pair);
}

fn value_to_nodes(indent: usize, key: &str, value: &Value) -> Node {
    match value {
        Value::Object(map) => Node::Pair {
            indent,
            key: key.to_string(),
            inline: None,
            children: map
                .iter()
                .map(|(child, child_value)| value_to_nodes(indent + 2, child, child_value))
                .collect(),
        },
        Value::Array(items) => Node::Pair {
            indent,
            key: key.to_string(),
            inline: None,
            children: items
                .iter()
                .map(|item| match item {
                    Value::Object(map) => Node::SequenceItem {
                        indent: indent + 2,
                        inline: None,
                        children: map
                            .iter()
                            .map(|(child, child_value)| {
                                value_to_nodes(indent + 4, child, child_value)
                            })
                            .collect(),
                    },
                    other => Node::SequenceItem {
                        indent: indent + 2,
                        inline: Some(emit_scalar(other)),
                        children: Vec::new(),
                    },
                })
                .collect(),
        },
        other => Node::Pair {
            indent,
            key: key.to_string(),
            inline: Some(keep_or_emit(None, other)),
            children: Vec::new(),
        },
    }
}

fn keep_or_emit(existing: Option<&str>, next: &Value) -> String {
    let Some(existing) = existing else {
        return emit_scalar(next);
    };
    let (value, comment) = split_trailing_comment(existing);
    if json_equal(&scalar(&value), next) {
        return existing.to_string();
    }
    if comment.is_empty() {
        return emit_scalar(next);
    }
    let trimmed = value.trim_end();
    let pad = &value[trimmed.len()..];
    format!("{}{pad}{comment}", emit_scalar(next))
}

fn emit_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => {
            if needs_quotes(text) {
                format!("\"{}\"", text.replace('"', "\\\""))
            } else {
                text.clone()
            }
        }
        Value::Array(_) | Value::Object(_) => String::new(),
    }
}

fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text.contains(':')
        || text.contains('#')
        || text.contains(' ')
        || matches!(text, "true" | "false" | "null" | "~")
}

fn render(nodes: &[Node]) -> String {
    let mut lines = Vec::new();
    render_into(nodes, &mut lines);
    lines.join("\n")
}

fn render_into(nodes: &[Node], lines: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Blank => lines.push(String::new()),
            Node::Comment { indent, text } => {
                lines.push(format!("{}{text}", " ".repeat(*indent)));
            }
            Node::Pair {
                indent,
                key,
                inline,
                children,
            } => {
                let pad = " ".repeat(*indent);
                match inline {
                    Some(value) => lines.push(format!("{pad}{key}: {value}")),
                    None => {
                        lines.push(format!("{pad}{key}:"));
                        render_into(children, lines);
                    }
                }
            }
            Node::SequenceItem {
                indent,
                inline,
                children,
            } => {
                let pad = " ".repeat(*indent);
                match inline {
                    Some(value) => lines.push(format!("{pad}- {value}")),
                    None => {
                        lines.push(format!("{pad}-"));
                        render_into(children, lines);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_comments_and_unregistered_sections() {
        let text = "# personal settings\nui-theme:\n  theme: light\n# owned by a plugin that is not loaded right now\nfuture-plugin:\n  keep: me\n";
        let next = json!({ "theme": "light", "fontSize": 18 });
        let written = patch_namespace(Some(text), "ui-theme", &next);
        assert!(written.contains("# personal settings"));
        assert!(written.contains("# owned by a plugin that is not loaded right now"));
        assert!(written.contains("keep: me"));
        assert!(written.contains("fontSize: 18"));
        assert!(written.contains("theme: light"));
    }

    #[test]
    fn keeps_comments_inside_the_section() {
        let text = "ui-theme:\n  # chosen during onboarding\n  theme: light\n  fontSize: 12\n";
        let written = patch_namespace(
            Some(text),
            "ui-theme",
            &json!({ "theme": "light", "fontSize": 18 }),
        );
        assert!(written.contains("# chosen during onboarding"));
        assert!(written.contains("theme: light"));
        assert!(written.contains("fontSize: 18"));
    }

    #[test]
    fn keeps_a_changed_key_trailing_comment() {
        let text = "ui-theme:\n  theme: light  # chosen during onboarding\n";
        let written = patch_namespace(Some(text), "ui-theme", &json!({ "theme": "dark" }));
        assert!(written.contains("# chosen during onboarding"));
        assert!(written.contains("theme: dark"));
    }

    #[test]
    fn deletes_only_the_removed_key() {
        let text = "ui-theme:\n  # chosen during onboarding\n  theme: light\n  fontSize: 12\n";
        let written = patch_namespace(Some(text), "ui-theme", &json!({ "theme": "light" }));
        assert!(written.contains("# chosen during onboarding"));
        assert!(written.contains("theme: light"));
        assert!(!written.contains("fontSize"));
    }

    #[test]
    fn keeps_unchanged_array_comments_and_replaces_changed_arrays() {
        let text = "workspace:\n  tags:\n    # pinned by hand\n    - alpha\n  label: draft\n";
        let relabel = patch_namespace(
            Some(text),
            "workspace",
            &json!({ "tags": ["alpha"], "label": "final" }),
        );
        assert!(relabel.contains("# pinned by hand"));
        assert!(relabel.contains("label: final"));
        let replaced = patch_namespace(
            Some(&relabel),
            "workspace",
            &json!({ "tags": ["beta"], "label": "final" }),
        );
        assert!(!replaced.contains("# pinned by hand"));
        assert!(replaced.contains("- beta"));
    }

    #[test]
    fn keeps_comment_only_document_preamble() {
        let written = patch_namespace(
            Some("# reserved for future settings\n"),
            "ui-theme",
            &json!({ "theme": "light" }),
        );
        assert!(written.contains("# reserved for future settings"));
        assert!(written.contains("theme: light"));
    }
}
