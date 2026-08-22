//! Language-server seam (`ctx.lsp`).

use dsh_cordis::Service;
use serde_json::Value;
use std::sync::Mutex;

/// Last successful initialize recorded by [`LspRuntime`].
#[derive(Debug, Clone)]
pub struct LspInitialize {
    /// Server or provider name.
    pub name: String,
    /// Initialize arguments as received.
    pub args: Value,
    /// Capabilities advertised back to the caller.
    pub capabilities: Value,
}

/// `ctx.lsp`.
pub struct LspRuntime {
    initialized: Mutex<Option<LspInitialize>>,
}

impl Default for LspRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LspRuntime {
    /// Create an uninitialized runtime.
    pub fn new() -> Self {
        Self {
            initialized: Mutex::new(None),
        }
    }

    /// Record `name` plus `args` and return the initialize result.
    pub fn initialize(&self, name: String, args: Value) -> Value {
        let capabilities = serde_json::json!({
            "textDocumentSync": 1,
        });
        let result = serde_json::json!({
            "name": name,
            "capabilities": capabilities,
            "args": args,
        });
        *self.initialized.lock().expect("lsp") = Some(LspInitialize {
            name,
            args,
            capabilities,
        });
        result
    }

    /// Last initialize, if any.
    pub fn initialized(&self) -> Option<LspInitialize> {
        self.initialized.lock().expect("lsp").clone()
    }
}

impl Service for LspRuntime {
    const KEY: &'static str = "lsp";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(LspRuntime::new())).unwrap();
        assert!(ctx.has_service("lsp"));
        ctx.dispose();
        assert!(!ctx.has_service("lsp"));
    }

    #[test]
    fn initialize_records_name_and_args() {
        let runtime = LspRuntime::new();
        let result = runtime.initialize(
            "rust-analyzer".into(),
            serde_json::json!({ "root": "/tmp" }),
        );
        assert_eq!(result["name"], "rust-analyzer");
        assert_eq!(runtime.initialized().unwrap().name, "rust-analyzer");
    }
}
