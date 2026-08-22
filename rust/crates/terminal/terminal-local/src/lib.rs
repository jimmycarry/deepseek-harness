//! Local in-memory terminal provider.

use dsh_cordis::Context;
use dsh_terminal::TerminalRuntime;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-terminal-local"
}

/// Provide `ctx.terminal` as an in-memory [`TerminalRuntime`].
pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<TerminalRuntime>> {
    let runtime = Arc::new(TerminalRuntime::new());
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn names_the_role() {
        assert_eq!(name(), "dsh-terminal-local");
    }

    #[test]
    fn install_provides_terminal() {
        let ctx = Context::new();
        let runtime = install(&ctx).unwrap();
        assert!(ctx.has_service("terminal"));
        let id = runtime.open();
        runtime.write(&id, "hi").unwrap();
        assert_eq!(runtime.history(&id).unwrap(), ["hi"]);
        ctx.dispose();
        assert!(!ctx.has_service("terminal"));
    }
}
