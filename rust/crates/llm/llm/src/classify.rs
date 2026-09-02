//! Provider-neutral failure-code classifiers shared by adapters.

/// Canonical code for a request rejected because its context window was exceeded.
pub const CONTEXT_WINDOW_EXCEEDED_CODE: &str = "CONTEXT_WINDOW_EXCEEDED";

/// Canonical code for an exhausted account quota or balance.
pub const QUOTA_EXCEEDED_CODE: &str = "QUOTA";

/// Canonical code for a completed response that carried no content blocks.
pub const EMPTY_RESPONSE_CODE: &str = "EMPTY_RESPONSE";

/// Canonical code for a supplied credential that cannot be used.
pub const INVALID_CREDENTIAL_CODE: &str = "INVALID_CREDENTIAL";

/// Recognize OpenAI-compatible context-overflow wording in provider detail.
#[must_use]
pub fn is_context_window_exceeded_error(detail: &str) -> bool {
    structured_context_overflow(detail)
        || contains_ci_words(
            detail,
            &[
                &["maximum", "context", "length"],
                &["maximum", "context", "window"],
                &["max", "context", "length"],
                &["max", "context", "window"],
                &["maximum", "allowed", "context", "length"],
                &["maximum", "allowed", "context", "window"],
                &["maximum", "supported", "context", "length"],
                &["maximum", "supported", "context", "window"],
                &["max", "allowed", "context", "length"],
                &["max", "allowed", "context", "window"],
                &["max", "supported", "context", "length"],
                &["max", "supported", "context", "window"],
            ],
        )
        || too_large_for_context(detail)
        || too_long_for_this_model(detail)
        || exceeds_model_context(detail)
}

/// Recognize terminal quota / balance / credit wording rather than rate limiting.
#[must_use]
pub fn is_quota_exceeded_error(detail: &str) -> bool {
    let normalized = normalize_separators(detail);
    contains_normalized(&normalized, "insufficient_quota")
        || contains_normalized(&normalized, "insufficient_balance")
        || contains_normalized(&normalized, "insufficient_credit")
        || contains_normalized(&normalized, "insufficient_credits")
        || contains_normalized(&normalized, "quota_exceeded")
        || contains_normalized(&normalized, "quota_exhausted")
        || contains_normalized(&normalized, "quota_reached")
        || contains_normalized(&normalized, "usage_limit_exceeded")
        || contains_normalized(&normalized, "usage_limit_exhausted")
        || contains_normalized(&normalized, "usage_limit_reached")
        || exceeded_quota(&normalized)
        || contains_normalized(&normalized, "balance_exhausted")
        || contains_normalized(&normalized, "balance_depleted")
        || contains_normalized(&normalized, "credit_exhausted")
        || contains_normalized(&normalized, "credits_exhausted")
        || contains_normalized(&normalized, "credit_depleted")
        || contains_normalized(&normalized, "credits_depleted")
        || contains_normalized(&normalized, "out_of_credit")
        || contains_normalized(&normalized, "out_of_credits")
        || contains_normalized(&normalized, "out_of_budget")
}

fn structured_context_overflow(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(rel) = find_ascii(&lower[index..], "context") {
            let start = index + rel;
            if start > 0 && is_word_char(bytes[start - 1]) {
                index = start + 1;
                continue;
            }
            let after_context = start + "context".len();
            let Some(sep1) = skip_separators(&lower, after_context) else {
                index = start + 1;
                continue;
            };
            let (kind, after_kind) = if lower[sep1..].starts_with("length") {
                ("length", sep1 + "length".len())
            } else if lower[sep1..].starts_with("window") {
                ("window", sep1 + "window".len())
            } else {
                index = start + 1;
                continue;
            };
            let _ = kind;
            let Some(sep2) = skip_separators(&lower, after_kind) else {
                index = start + 1;
                continue;
            };
            let rest = &lower[sep2..];
            let matched = if rest.starts_with("exceeded") {
                Some("exceeded".len())
            } else if rest.starts_with("exceeds") {
                Some("exceeds".len())
            } else if rest.starts_with("exceed") {
                Some("exceed".len())
            } else if rest.starts_with("overflowed") {
                Some("overflowed".len())
            } else if rest.starts_with("overflow") {
                Some("overflow".len())
            } else if rest.starts_with("limit") {
                let after_limit = sep2 + "limit".len();
                skip_separators(&lower, after_limit).and_then(|next| {
                    lower[next..]
                        .starts_with("exceeded")
                        .then_some(next + "exceeded".len() - sep2)
                })
            } else {
                None
            };
            if let Some(len) = matched {
                let end = sep2 + len;
                if end == bytes.len() || !is_word_char(bytes[end]) {
                    return true;
                }
            }
            index = start + 1;
        } else {
            break;
        }
    }
    false
}

fn too_large_for_context(detail: &str) -> bool {
    // request|prompt|input|messages? [is|are] too (large|long) for [this|the] [model's] context [window]
    let tokens = tokenize(detail);
    let subjects = ["request", "prompt", "input", "message", "messages"];
    let mut i = 0;
    while i < tokens.len() {
        if !subjects.contains(&tokens[i].as_str()) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < tokens.len() && (tokens[j] == "is" || tokens[j] == "are") {
            j += 1;
        }
        if j + 3 > tokens.len() || tokens[j] != "too" {
            i += 1;
            continue;
        }
        if tokens[j + 1] != "large" && tokens[j + 1] != "long" {
            i += 1;
            continue;
        }
        if tokens[j + 2] != "for" {
            i += 1;
            continue;
        }
        let mut k = j + 3;
        if k < tokens.len() && (tokens[k] == "this" || tokens[k] == "the") {
            k += 1;
        }
        if k < tokens.len() && (tokens[k] == "model" || tokens[k] == "models") {
            k += 1;
        }
        if k < tokens.len() && tokens[k] == "context" {
            return true;
        }
        i += 1;
    }
    false
}

fn too_long_for_this_model(detail: &str) -> bool {
    let tokens = tokenize(detail);
    let subjects = ["input", "prompt", "request"];
    let mut i = 0;
    while i < tokens.len() {
        if !subjects.contains(&tokens[i].as_str()) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < tokens.len() && tokens[j] == "is" {
            j += 1;
        }
        if j + 4 > tokens.len()
            || tokens[j] != "too"
            || (tokens[j + 1] != "long" && tokens[j + 1] != "large")
            || tokens[j + 2] != "for"
            || (tokens[j + 3] != "this" && tokens[j + 3] != "the")
            || tokens.get(j + 4).map(String::as_str) != Some("model")
        {
            i += 1;
            continue;
        }
        return true;
    }
    false
}

fn exceeds_model_context(detail: &str) -> bool {
    let tokens = tokenize(detail);
    let subjects = ["input", "prompt", "request", "message", "messages"];
    let verbs = [
        "exceed",
        "exceeds",
        "exceeded",
        "overflow",
        "overflows",
        "larger",
    ];
    for (i, token) in tokens.iter().enumerate() {
        if !subjects.contains(&token.as_str()) {
            continue;
        }
        let window_end = (i + 1 + 8).min(tokens.len());
        for (j, verb) in tokens[i + 1..window_end].iter().enumerate() {
            let verb_ok = if verb.as_str() == "larger" {
                tokens.get(i + 1 + j + 1).map(String::as_str) == Some("than")
            } else {
                verbs.contains(&verb.as_str())
            };
            if !verb_ok {
                continue;
            }
            let after_verb = if verb.as_str() == "larger" {
                i + 1 + j + 2
            } else {
                i + 1 + j + 1
            };
            let context_end = (after_verb + 8).min(tokens.len());
            if tokens[after_verb..context_end]
                .iter()
                .any(|word| word.as_str() == "context")
            {
                return true;
            }
        }
    }
    false
}

fn contains_ci_words(detail: &str, patterns: &[&[&str]]) -> bool {
    let tokens = tokenize(detail);
    for pattern in patterns {
        if tokens.windows(pattern.len()).any(|window| {
            window
                .iter()
                .zip(pattern.iter())
                .all(|(token, expected)| token == expected)
        }) {
            return true;
        }
    }
    false
}

fn tokenize(detail: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in detail.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn normalize_separators(detail: &str) -> String {
    detail
        .chars()
        .map(|ch| {
            if ch == '-' || ch.is_ascii_whitespace() {
                '_'
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect()
}

fn contains_normalized(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

fn exceeded_quota(normalized: &str) -> bool {
    // exceed(ed|s)?[_-\s](?:(?:your|the)[_-\s])?(?:current[_-\s])?quota
    let bytes = normalized.as_bytes();
    let mut index = 0;
    while let Some(rel) = find_ascii(&normalized[index..], "exceed") {
        let start = index + rel;
        let mut pos = start + "exceed".len();
        if normalized[pos..].starts_with("ed") {
            pos += 2;
        } else if normalized[pos..].starts_with('s') {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b'_' {
            pos += 1;
        } else if pos < bytes.len() {
            index = start + 1;
            continue;
        }
        if normalized[pos..].starts_with("your_") {
            pos += "your_".len();
        } else if normalized[pos..].starts_with("the_") {
            pos += "the_".len();
        }
        if normalized[pos..].starts_with("current_") {
            pos += "current_".len();
        }
        if normalized[pos..].starts_with("quota") {
            let end = pos + "quota".len();
            if end == bytes.len() || !is_word_char(bytes[end]) {
                return true;
            }
        }
        index = start + 1;
    }
    false
}

fn skip_separators(lower: &str, start: usize) -> Option<usize> {
    let bytes = lower.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let ch = bytes[start];
    if ch == b' ' || ch == b'\t' || ch == b'_' || ch == b'-' {
        Some(start + 1)
    } else {
        None
    }
}

fn find_ascii(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle)
}

fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_overflow_matches_structured_and_capacity_wording() {
        assert!(is_context_window_exceeded_error("context_length_exceeded"));
        assert!(is_context_window_exceeded_error(
            "This model maximum context length is 128000 tokens; your input exceeds that limit."
        ));
        assert!(is_context_window_exceeded_error(
            "request too large for model context"
        ));
        assert!(!is_context_window_exceeded_error(
            "invalid input: temperature exceeds maximum allowed value"
        ));
    }

    #[test]
    fn quota_matches_terminal_wording_only() {
        assert!(is_quota_exceeded_error(
            "insufficient_quota account credits exhausted"
        ));
        assert!(is_quota_exceeded_error("out of credits"));
        assert!(!is_quota_exceeded_error("slow down"));
    }
}
