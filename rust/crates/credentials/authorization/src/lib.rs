//! Authorization seam (`ctx.authorization`).

use dsh_cordis::Service;
use dsh_credentials::{Credential, CredentialsRuntime};
use std::sync::Arc;
use thiserror::Error;

/// Failures from [`AuthorizationRuntime::require`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthorizationError {
    /// The named credential is missing or blank.
    #[error("credential `{0}` is unset")]
    Unset(String),
}

/// `ctx.authorization`.
pub struct AuthorizationRuntime {
    credentials: Arc<CredentialsRuntime>,
}

impl AuthorizationRuntime {
    /// Bind to the credential seam that `require` consults.
    pub fn new(credentials: Arc<CredentialsRuntime>) -> Self {
        Self { credentials }
    }

    /// Return the secret for `name`, or fail when it is unset.
    pub fn require(&self, name: &str) -> Result<String, AuthorizationError> {
        match self.credentials.resolve(name) {
            Credential::Set(value) => Ok(value),
            Credential::Unset => Err(AuthorizationError::Unset(name.into())),
        }
    }
}

impl Service for AuthorizationRuntime {
    const KEY: &'static str = "authorization";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_credentials::CredentialResolver;

    struct Fixed(Credential);

    impl CredentialResolver for Fixed {
        fn resolve(&self, _name: &str) -> Credential {
            self.0.clone()
        }
    }

    #[test]
    fn require_fails_when_unset() {
        let credentials = Arc::new(CredentialsRuntime::new(Arc::new(Fixed(Credential::Unset))));
        let auth = AuthorizationRuntime::new(credentials);
        assert_eq!(
            auth.require("DEEPSEEK_API_KEY"),
            Err(AuthorizationError::Unset("DEEPSEEK_API_KEY".into()))
        );
    }

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        let credentials = Arc::new(CredentialsRuntime::new(Arc::new(Fixed(Credential::Set(
            "k".into(),
        )))));
        ctx.provide(Arc::new(AuthorizationRuntime::new(credentials)))
            .unwrap();
        assert!(ctx.has_service("authorization"));
        ctx.dispose();
        assert!(!ctx.has_service("authorization"));
    }
}
