//! Env-over-`.env` credential provider.

use dsh_cordis::{Context, Result};
use dsh_credentials::{Credential, CredentialResolver, CredentialsRuntime};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Process environment, then optional `.env` overlay.
pub struct EnvResolver {
    dotenv: HashMap<String, String>,
}

impl EnvResolver {
    /// Resolve from the process environment only.
    pub fn new() -> Self {
        Self {
            dotenv: HashMap::new(),
        }
    }
}

impl Default for EnvResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialResolver for EnvResolver {
    fn resolve(&self, name: &str) -> Credential {
        if let Some(value) = resolve_env(name) {
            return Credential::Set(value);
        }
        Credential::from_value(self.dotenv.get(name).map(String::as_str))
    }
}

/// Parse a dotenv file into an [`EnvResolver`]. Blank values stay unset.
pub fn from_dotenv(path: impl AsRef<Path>) -> std::io::Result<EnvResolver> {
    let text = std::fs::read_to_string(path)?;
    let mut dotenv = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if key.is_empty() || value.trim().is_empty() {
            continue;
        }
        dotenv.insert(key.to_string(), value);
    }
    Ok(EnvResolver { dotenv })
}

/// Provide [`CredentialsRuntime`] backed by [`EnvResolver`].
pub fn install(ctx: &Context) -> Result<Arc<CredentialsRuntime>> {
    let runtime = Arc::new(CredentialsRuntime::new(Arc::new(EnvResolver::new())));
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

/// Resolve a named process-environment credential. Empty values are unset.
pub fn resolve_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-credentials-local"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn blank_env_is_unset() {
        std::env::set_var("DSH_TEST_BLANK_CRED", "   ");
        assert!(resolve_env("DSH_TEST_BLANK_CRED").is_none());
        assert_eq!(
            EnvResolver::new().resolve("DSH_TEST_BLANK_CRED"),
            Credential::Unset
        );
        std::env::remove_var("DSH_TEST_BLANK_CRED");
    }

    #[test]
    fn from_dotenv_skips_blank_and_comments() {
        let path = std::env::temp_dir().join(format!("dsh-cred-{}.env", std::process::id()));
        std::fs::write(&path, "# c\nFOO=bar\nBLANK=   \nexport BAZ=qux\n").unwrap();
        let resolver = from_dotenv(&path).unwrap();
        assert_eq!(resolver.resolve("FOO"), Credential::Set("bar".into()));
        assert_eq!(resolver.resolve("BLANK"), Credential::Unset);
        assert_eq!(resolver.resolve("BAZ"), Credential::Set("qux".into()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn install_provides_credentials() {
        let ctx = Context::new();
        install(&ctx).unwrap();
        assert!(ctx.has_service("credentials"));
        ctx.dispose();
        assert!(!ctx.has_service("credentials"));
    }
}
