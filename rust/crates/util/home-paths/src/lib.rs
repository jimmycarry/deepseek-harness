//! Shared filesystem path helpers for DeepSeek Harness user data.

use std::env;
use std::path::{Path, PathBuf};

/// Directory name for the default DeepSeek Harness home under the OS home.
pub const DSH_HOME_DIR_NAME: &str = ".dsh";

/// Stable user-facing display form for the default DeepSeek Harness home.
pub const DEFAULT_DSH_HOME_DISPLAY: &str = "~/.dsh";

/// Environment variable that overrides the default DeepSeek Harness home.
pub const DSH_HOME_ENV: &str = "DSH_HOME";

/// Resolve the default DeepSeek Harness home using the platform home directory.
pub fn default_dsh_home() -> PathBuf {
    home_dir().join(DSH_HOME_DIR_NAME)
}

/// Expand supported tilde prefixes against the operating-system home.
pub fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// Resolve the single-root DeepSeek Harness home.
///
/// Precedence, highest first: an explicit configured path, `$DSH_HOME`, then
/// `~/.dsh`. An empty or whitespace-only `$DSH_HOME` is treated as unset.
pub fn resolve_dsh_home(configured: Option<&str>) -> PathBuf {
    resolve_dsh_home_with_env(configured, |key| env::var(key).ok())
}

/// Resolve the home against an injected environment reader.
pub fn resolve_dsh_home_with_env<F>(configured: Option<&str>, mut env_get: F) -> PathBuf
where
    F: FnMut(&str) -> Option<String>,
{
    let selected = if let Some(configured) = configured {
        configured.to_string()
    } else if let Some(from_env) = env_get(DSH_HOME_ENV) {
        if from_env.trim().is_empty() {
            default_dsh_home().to_string_lossy().into_owned()
        } else {
            from_env
        }
    } else {
        default_dsh_home().to_string_lossy().into_owned()
    };
    let expanded = expand_home_path(&selected);
    if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

/// Join path segments onto the resolved DeepSeek Harness home.
pub fn dsh_home_path<I, S>(segments: I) -> PathBuf
where
    I: IntoIterator<Item = S>,
    S: AsRef<Path>,
{
    let mut path = resolve_dsh_home(None);
    for segment in segments {
        path.push(segment);
    }
    path
}

/// Describe a resolved harness home symbolically for user-facing display.
pub fn dsh_home_display(resolved_home: &Path) -> String {
    if resolved_home == default_dsh_home() {
        DEFAULT_DSH_HOME_DISPLAY.to_string()
    } else {
        format!("${DSH_HOME_ENV}")
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_env_falls_back_to_default() {
        let resolved = resolve_dsh_home_with_env(None, |_| Some("   ".into()));
        assert_eq!(resolved, default_dsh_home());
    }

    #[test]
    fn configured_wins_over_env() {
        let resolved = resolve_dsh_home_with_env(Some("/explicit"), |_| Some("/from-env".into()));
        assert_eq!(resolved, PathBuf::from("/explicit"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let expanded = expand_home_path("~/.dsh");
        assert_eq!(expanded, home_dir().join(".dsh"));
    }

    #[test]
    fn display_never_leaks_an_absolute_default() {
        assert_eq!(dsh_home_display(&default_dsh_home()), DEFAULT_DSH_HOME_DISPLAY);
        assert_eq!(dsh_home_display(Path::new("/tmp/other")), "$DSH_HOME");
    }
}
