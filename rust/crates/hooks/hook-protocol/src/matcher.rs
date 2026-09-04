//! Matcher shared by both hook dialects.

use crate::types::MatcherMode;
use regex::Regex;

fn is_match_all(matcher: Option<&str>) -> bool {
    matches!(matcher, None | Some("") | Some("*"))
}

fn is_claude_literal(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '|')
}

fn compile_regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

/// Validate one matcher before a bridge accepts its config group.
///
/// # Returns
/// `None` for a valid matcher, otherwise a stable diagnostic.
pub fn matcher_diagnostic(matcher: Option<&str>, mode: MatcherMode) -> Option<String> {
    if is_match_all(matcher) {
        return None;
    }
    let pattern = matcher.expect("match-all already returned");
    if mode == MatcherMode::ClaudeCode && is_claude_literal(pattern) {
        return None;
    }
    if compile_regex(pattern).is_none() {
        let encoded = serde_json::to_string(pattern).unwrap_or_else(|_| format!("\"{pattern}\""));
        Some(format!("invalid {} regex matcher {encoded}", mode.as_str()))
    } else {
        None
    }
}

/// Whether `matcher` selects `query` under the given dialect.
///
/// Invalid regexes return `false` rather than panicking.
pub fn matches_matcher(matcher: Option<&str>, query: &str, mode: MatcherMode) -> bool {
    if is_match_all(matcher) {
        return true;
    }
    let pattern = matcher.expect("match-all already returned");
    if mode == MatcherMode::ClaudeCode && is_claude_literal(pattern) {
        return pattern.split('|').any(|part| part == query);
    }
    compile_regex(pattern)
        .map(|regex| regex.is_match(query))
        .unwrap_or(false)
}
