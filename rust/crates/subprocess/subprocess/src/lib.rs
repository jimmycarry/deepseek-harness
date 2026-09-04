//! Subprocess seam (`ctx.subprocess`).

use async_trait::async_trait;
use dsh_cordis::Service;
use std::collections::BTreeMap;
use thiserror::Error;

/// Prefix of every managed `DSH_*` environment key.
pub const DSH_ENV_PREFIX: &str = "DSH_";

/// Credential-shaped ambient names dropped from every harness child environment.
pub fn is_sensitive_env_name(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.contains("KEY")
        || upper.contains("PASSWORD")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
}

/// Ambient parent environment minus credential-shaped names and all `DSH_*` names.
pub fn scrubbed_parent_env() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| {
            !is_sensitive_env_name(key) && !key.to_ascii_uppercase().starts_with(DSH_ENV_PREFIX)
        })
        .collect()
}

/// Resolved spawn request. Defaulting happens in `resolve`, never inside `run`.
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Program path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Trusted managed environment overlay. `None` keeps the inherited environment.
    pub dsh_env: Option<BTreeMap<String, String>>,
}

/// Caller-facing request before resolve.
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// Program path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Optional cwd.
    pub cwd: Option<String>,
    /// Trusted managed environment overlay.
    pub dsh_env: Option<BTreeMap<String, String>>,
}

/// Completed process output.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Exit status.
    pub status: i32,
}

/// Spawn failures.
#[derive(Debug, Error)]
pub enum SubprocessError {
    /// OS spawn or wait failed.
    #[error("{0}")]
    Spawn(String),
}

/// Explicit resolve step.
pub fn resolve(request: SpawnRequest) -> SpawnSpec {
    SpawnSpec {
        program: request.program,
        args: request.args,
        cwd: request.cwd,
        dsh_env: request.dsh_env,
    }
}

/// Provider interface.
#[async_trait]
pub trait SubprocessExecutor: Send + Sync {
    /// Run one resolved spec.
    async fn run(&self, spec: SpawnSpec) -> Result<ProcessOutput, SubprocessError>;
}

/// `ctx.subprocess`.
pub struct SubprocessRuntime {
    backend: std::sync::Arc<dyn SubprocessExecutor>,
}

impl SubprocessRuntime {
    /// Wrap a backend.
    pub fn new(backend: std::sync::Arc<dyn SubprocessExecutor>) -> Self {
        Self { backend }
    }

    /// Run a resolved spec.
    pub async fn run(&self, spec: SpawnSpec) -> Result<ProcessOutput, SubprocessError> {
        self.backend.run(spec).await
    }
}

impl Service for SubprocessRuntime {
    const KEY: &'static str = "subprocess";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbed_parent_env_drops_credential_and_dsh_names() {
        std::env::set_var("DSH_SCRUB_TEST_TOKEN", "secret");
        std::env::set_var("DSH_SCRUB_TEST_HOME", "/tmp");
        let env = scrubbed_parent_env();
        assert!(!env.contains_key("DSH_SCRUB_TEST_TOKEN"));
        assert!(!env
            .keys()
            .any(|key| key.eq_ignore_ascii_case("dsh_scrub_test_home")));
        std::env::remove_var("DSH_SCRUB_TEST_TOKEN");
        std::env::remove_var("DSH_SCRUB_TEST_HOME");
    }

    #[test]
    fn resolve_does_not_invent_a_cwd() {
        let spec = resolve(SpawnRequest {
            program: "echo".into(),
            args: vec![],
            cwd: None,
            dsh_env: None,
        });
        assert!(spec.cwd.is_none());
    }
}
