//! Env-over-`.env` credential provider.

use dsh_credentials::Runtime;

pub fn name() -> &'static str {
    "dsh-credentials-local"
}

/// Resolve a named credential. Empty env values are unset.
pub fn resolve_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_env_is_unset() {
        std::env::set_var("DSH_TEST_BLANK_CRED", "   ");
        assert!(resolve_env("DSH_TEST_BLANK_CRED").is_none());
        std::env::remove_var("DSH_TEST_BLANK_CRED");
        let _ = Runtime::new();
    }
}
