//! Stdio LSP provider.

use dsh_cordis::Context;
use dsh_lsp::LspRuntime;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-lsp-stdio"
}

/// Provide `ctx.lsp` as an in-process [`LspRuntime`].
pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<LspRuntime>> {
    let runtime = Arc::new(LspRuntime::new());
    ctx.provide(Arc::clone(&runtime))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    #[test]
    fn names_the_role() {
        assert_eq!(name(), "dsh-lsp-stdio");
    }

    #[test]
    fn install_provides_lsp() {
        let ctx = Context::new();
        let runtime = install(&ctx).unwrap();
        assert!(ctx.has_service("lsp"));
        let result = runtime.initialize("stdio".into(), serde_json::json!({}));
        assert_eq!(result["name"], "stdio");
        ctx.dispose();
        assert!(!ctx.has_service("lsp"));
    }
}
