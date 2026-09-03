//! Evaluate the `!!js` expressions that shipped patch files actually write.
//!
//! Allowed forms: `process.env.*`, `process.platform`, `process.cwd()`,
//! `dshHomePath(...)`, `ctx.<service>.<field>`, `||` / `??`, `===` / `!==`,
//! and one ternary. Anything else fails loud.

use serde_json::Value;
use std::collections::HashMap;
use std::env;

use crate::yaml::as_js_expr;
use crate::LoaderError;

/// Host values a `!!js` expression may read.
#[derive(Debug, Clone)]
pub struct EvalHost {
    /// Node `process.platform` (`linux` / `darwin` / `win32`).
    pub platform: String,
    /// Node `process.cwd()`.
    pub cwd: String,
    /// Harness home used by `dshHomePath`.
    pub dsh_home: String,
    /// `ctx.<service>.<field>` string lookups.
    pub lookup: HashMap<String, String>,
}

impl EvalHost {
    /// Build from this process. `DSH_HOME` then `$HOME/.dsh`.
    pub fn from_process() -> Self {
        let platform = if cfg!(windows) {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let cwd = env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let dsh_home = env::var("DSH_HOME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| env::var("HOME").ok().map(|home| format!("{home}/.dsh")))
            .unwrap_or_else(|| ".dsh".into());
        Self {
            platform: platform.into(),
            cwd,
            dsh_home,
            lookup: HashMap::new(),
        }
    }

    fn env_value(&self, key: &str) -> Option<String> {
        env::var(key).ok()
    }
}

/// Walk a JSON value and replace `{ "__jsExpr" }` nodes.
pub fn eval_value(value: &Value, host: &EvalHost) -> Result<Value, LoaderError> {
    if let Some(expr) = as_js_expr(value) {
        return eval_js(expr, host);
    }
    match value {
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval_value(item, host)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                out.insert(key.clone(), eval_value(child, host)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// Whether a `disabled` field evaluates to true.
pub fn eval_disabled(disabled: Option<&Value>, host: &EvalHost) -> Result<bool, LoaderError> {
    let Some(value) = disabled else {
        return Ok(false);
    };
    if let Some(flag) = value.as_bool() {
        return Ok(flag);
    }
    match eval_value(value, host)? {
        Value::Bool(flag) => Ok(flag),
        Value::Null => Ok(false),
        other => Err(LoaderError::Parse(format!(
            "disabled expression must be boolean, got {other}"
        ))),
    }
}

/// Evaluate one expression string.
pub fn eval_js(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    eval_ternary(expr.trim(), host)
}

fn eval_ternary(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    if let Some((cond, rest)) = split_top(expr, '?') {
        if rest.starts_with('?') {
            return eval_or(expr, host);
        }
        let (then_part, else_part) = split_top(rest, ':').ok_or_else(|| {
            LoaderError::Parse(format!("ternary missing ':': {expr}"))
        })?;
        let cond = eval_or(cond.trim(), host)?;
        if is_truthy(&cond) {
            eval_or(then_part.trim(), host)
        } else {
            eval_or(else_part.trim(), host)
        }
    } else {
        eval_or(expr, host)
    }
}

fn eval_or(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    if let Some((left, right)) = split_top(expr, '|') {
        if right.starts_with('|') {
            let right = right[1..].trim();
            let left = eval_coalesce(left.trim(), host)?;
            if is_truthy(&left) {
                return Ok(left);
            }
            return eval_or(right, host);
        }
    }
    eval_coalesce(expr, host)
}

fn eval_coalesce(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    if let Some((left, right)) = split_top(expr, '?') {
        if right.starts_with('?') {
            let right = right[1..].trim();
            let left = eval_compare(left.trim(), host)?;
            if left.is_null() {
                return eval_coalesce(right, host);
            }
            return Ok(left);
        }
    }
    eval_compare(expr, host)
}

fn eval_compare(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    if let Some((left, right)) = split_top(expr, '!') {
        if let Some(right) = right.strip_prefix("==") {
            let left = eval_primary(left.trim(), host)?;
            let right = eval_primary(right.trim(), host)?;
            return Ok(Value::Bool(left != right));
        }
    }
    if let Some((left, right)) = split_top(expr, '=') {
        if let Some(right) = right.strip_prefix("==") {
            let left = eval_primary(left.trim(), host)?;
            let right = eval_primary(right.trim(), host)?;
            return Ok(Value::Bool(left == right));
        }
    }
    eval_primary(expr, host)
}

fn eval_primary(expr: &str, host: &EvalHost) -> Result<Value, LoaderError> {
    let expr = expr.trim();
    if let Some(inner) = expr.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')) {
        return eval_ternary(inner, host);
    }
    if let Some(text) = parse_quoted(expr) {
        return Ok(Value::String(text));
    }
    if expr == "process.platform" {
        return Ok(Value::String(host.platform.clone()));
    }
    if expr == "process.cwd()" {
        return Ok(Value::String(host.cwd.clone()));
    }
    if let Some(key) = expr.strip_prefix("process.env.") {
        return Ok(match host.env_value(key) {
            Some(value) => Value::String(value),
            None => Value::Null,
        });
    }
    if let Some(rest) = expr.strip_prefix("dshHomePath(") {
        let inner = rest
            .strip_suffix(')')
            .ok_or_else(|| LoaderError::Parse(format!("unclosed dshHomePath: {expr}")))?;
        let part = parse_quoted(inner.trim()).ok_or_else(|| {
            LoaderError::Parse(format!("dshHomePath expects a string: {expr}"))
        })?;
        let home = host.dsh_home.trim_end_matches('/');
        return Ok(Value::String(format!("{home}/{part}")));
    }
    if let Some(path) = expr.strip_prefix("ctx.") {
        return Ok(match host.lookup.get(path) {
            Some(value) => Value::String(value.clone()),
            None => Value::Null,
        });
    }
    Err(LoaderError::Parse(format!(
        "unsupported !!js expression: {expr}"
    )))
}

fn parse_quoted(expr: &str) -> Option<String> {
    let expr = expr.trim();
    if expr.len() >= 2 {
        let bytes = expr.as_bytes();
        if (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
        {
            return Some(expr[1..expr.len() - 1].to_string());
        }
    }
    None
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn split_top(expr: &str, needle: char) -> Option<(&str, &str)> {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for (i, ch) in expr.char_indices() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(q), ch) if ch == q => quote = None,
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ch) if ch == needle && depth == 0 => {
                return Some((&expr[..i], &expr[i + ch.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> EvalHost {
        let mut host = EvalHost::from_process();
        host.platform = "linux".into();
        host.cwd = "/tmp".into();
        host.dsh_home = "/home/u/.dsh".into();
        host.lookup
            .insert("headlessStartup.task".into(), "ping".into());
        host
    }

    #[test]
    fn platform_compare_and_home_path() {
        let host = host();
        assert_eq!(
            eval_js("process.platform === 'win32'", &host).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_js("process.platform !== 'win32'", &host).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_js("dshHomePath('sessions')", &host).unwrap(),
            Value::String("/home/u/.dsh/sessions".into())
        );
        assert_eq!(
            eval_js("ctx.headlessStartup.task", &host).unwrap(),
            Value::String("ping".into())
        );
        assert_eq!(eval_js("process.cwd()", &host).unwrap(), Value::String("/tmp".into()));
    }

    #[test]
    fn env_fallback_and_ternary() {
        let host = host();
        std::env::remove_var("DSH_EVAL_MISSING");
        assert_eq!(
            eval_js("process.env.DSH_EVAL_MISSING || 'DISABLED'", &host).unwrap(),
            Value::String("DISABLED".into())
        );
        assert_eq!(
            eval_js(
                "(process.env.DSH_EVAL_MISSING ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'",
                &host
            )
            .unwrap(),
            Value::String("ask".into())
        );
    }
}
