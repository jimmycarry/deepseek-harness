//! Local jobs provider.

use dsh_cordis::{Context, Result};
use dsh_jobs::JobsRuntime;
use std::sync::Arc;

/// Provide an in-process [`JobsRuntime`].
pub fn install(ctx: &Context) -> Result<Arc<JobsRuntime>> {
    let runtime = Arc::new(JobsRuntime::new());
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
        install(&ctx).unwrap();
        assert!(ctx.has_service("jobs"));
        ctx.dispose();
        assert!(!ctx.has_service("jobs"));
    }
}
