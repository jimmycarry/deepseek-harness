//! Credential seam (`ctx.credentials`).

use dsh_cordis::Service;
use std::sync::Arc;

/// A resolved credential value, or an explicit absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A non-empty secret value.
    Set(String),
    /// Missing or blank; never treated as a configured secret.
    Unset,
}

impl Credential {
    /// Treat blank and missing values as [`Credential::Unset`].
    pub fn from_value(value: Option<impl AsRef<str>>) -> Self {
        match value {
            Some(value) if !value.as_ref().trim().is_empty() => Self::Set(value.as_ref().to_string()),
            _ => Self::Unset,
        }
    }
}

/// Looks up one named credential.
pub trait CredentialResolver: Send + Sync {
    /// Resolve `name` once; callers must not cache across operations.
    fn resolve(&self, name: &str) -> Credential;
}

/// `ctx.credentials`.
pub struct CredentialsRuntime {
    resolver: Arc<dyn CredentialResolver>,
}

impl CredentialsRuntime {
    /// Wrap a resolver as the seam.
    pub fn new(resolver: Arc<dyn CredentialResolver>) -> Self {
        Self { resolver }
    }

    /// Resolve one named credential through the registered resolver.
    pub fn resolve(&self, name: &str) -> Credential {
        self.resolver.resolve(name)
    }
}

impl Service for CredentialsRuntime {
    const KEY: &'static str = "credentials";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    struct Fixed(Credential);

    impl CredentialResolver for Fixed {
        fn resolve(&self, _name: &str) -> Credential {
            self.0.clone()
        }
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(CredentialsRuntime::new(Arc::new(Fixed(Credential::Unset)))))
            .unwrap();
        assert!(ctx.has_service("credentials"));
        ctx.dispose();
        assert!(!ctx.has_service("credentials"));
    }

    #[test]
    fn blank_value_is_unset() {
        assert_eq!(Credential::from_value(Some("   ")), Credential::Unset);
        assert_eq!(Credential::from_value(Some("k")), Credential::Set("k".into()));
    }
}
