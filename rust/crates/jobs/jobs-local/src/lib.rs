//! Process-local provider for `ctx.jobs`.

use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, CordisError, Result};
use dsh_jobs::JobRegistry;
use serde_json::Value;
use std::sync::Arc;

/// Default maximum `running` plus `stopping` jobs per exact owner.
pub const DEFAULT_MAX_CONCURRENT_JOBS_PER_OWNER: usize = 10;

/// Deployment-varying admission cap.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum active jobs per exact owner or the shared unowned bucket.
    pub max_concurrent_jobs_per_owner: usize,
}

impl Config {
    /// Validate raw cordis.yml config. Omission defaults to 10.
    ///
    /// # Errors
    /// Non-positive `maxConcurrentJobsPerOwner`.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let max = match config.and_then(|value| value.get("maxConcurrentJobsPerOwner")) {
            None => DEFAULT_MAX_CONCURRENT_JOBS_PER_OWNER,
            Some(value) => value
                .as_u64()
                .filter(|value| *value > 0)
                .map(|value| value as usize)
                .ok_or_else(|| {
                    "jobs-local: maxConcurrentJobsPerOwner must be a positive integer".to_string()
                })?,
        };
        Ok(Self {
            max_concurrent_jobs_per_owner: max,
        })
    }
}

/// Provide an in-process [`JobRegistry`].
///
/// # Errors
/// Invalid Config, or the `jobs` service is already provided.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<Arc<JobRegistry>> {
    let resolved = Config::resolve(config).map_err(CordisError::Validation)?;
    let runtime = Arc::new(JobRegistry::new(resolved.max_concurrent_jobs_per_owner));
    if let Some(agents) = ctx.get::<AgentRegistry>() {
        runtime.bind_agents(agents);
    }
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-jobs-local"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn install_provides_jobs() {
        let ctx = Context::new();
        install(&ctx, None).unwrap();
        assert!(ctx.has_service("jobs"));
        ctx.dispose();
        assert!(!ctx.has_service("jobs"));
    }

    #[test]
    fn resolve_rejects_zero_cap() {
        let error = Config::resolve(Some(&serde_json::json!({
            "maxConcurrentJobsPerOwner": 0
        })))
        .unwrap_err();
        assert!(error.contains("positive integer"));
    }
}
