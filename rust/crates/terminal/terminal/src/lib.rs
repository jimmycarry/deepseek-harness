//! Persistent terminal seam (`ctx.terminal`).
//!
//! The runtime owns session ids and an in-memory write history. PTY backends
//! replace the storage without changing `open` / `write`.

use dsh_cordis::Service;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use thiserror::Error;

/// Terminal session failures.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// No session exists for this id.
    #[error("unknown terminal `{0}`")]
    Unknown(String),
}

struct SessionState {
    history: Vec<String>,
}

/// `ctx.terminal`.
pub struct TerminalRuntime {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl Default for TerminalRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalRuntime {
    /// Create an empty session table.
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Open a session and return its id.
    pub fn open(&self) -> String {
        let id = format!("term-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.sessions.lock().expect("terminal").insert(
            id.clone(),
            SessionState {
                history: Vec::new(),
            },
        );
        id
    }

    /// Append `data` to the write history of `id`.
    pub fn write(&self, id: &str, data: &str) -> Result<(), TerminalError> {
        let mut sessions = self.sessions.lock().expect("terminal");
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| TerminalError::Unknown(id.to_string()))?;
        session.history.push(data.to_string());
        Ok(())
    }

    /// Snapshot the write history of `id` in append order.
    pub fn history(&self, id: &str) -> Result<Vec<String>, TerminalError> {
        self.sessions
            .lock()
            .expect("terminal")
            .get(id)
            .map(|session| session.history.clone())
            .ok_or_else(|| TerminalError::Unknown(id.to_string()))
    }
}

impl Service for TerminalRuntime {
    const KEY: &'static str = "terminal";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(TerminalRuntime::new())).unwrap();
        assert!(ctx.has_service("terminal"));
        ctx.dispose();
        assert!(!ctx.has_service("terminal"));
    }

    #[test]
    fn open_write_history() {
        let runtime = TerminalRuntime::new();
        let id = runtime.open();
        runtime.write(&id, "hello").unwrap();
        runtime.write(&id, "world").unwrap();
        assert_eq!(runtime.history(&id).unwrap(), ["hello", "world"]);
    }

    #[test]
    fn write_unknown_is_error() {
        let runtime = TerminalRuntime::new();
        let err = runtime.write("missing", "x").unwrap_err();
        assert!(matches!(err, TerminalError::Unknown(id) if id == "missing"));
    }
}
