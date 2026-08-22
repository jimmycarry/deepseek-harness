//! Spill storage Service Definition (`ctx.spillStore`).
//!
//! `save_text` persists the full content verbatim and returns an opaque
//! locator, exact byte length, and model-facing retrieval guidance. The
//! backend owns naming and location; this crate owns no retention or
//! replacement policy.

use dsh_cordis::Service;
use std::sync::Arc;
use thiserror::Error;

/// Opaque model-facing handle for one spilled artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillLocator(pub String);

/// Save-time storage namespace for a spilled artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillOwner {
    /// Owning session id. Forked children keep inherited locators.
    pub session_id: String,
}

/// Tool and call that produced one spilled artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillSource {
    /// Tool whose result was spilled.
    pub tool_name: String,
    /// Model-issued call id, when the loop supplied one.
    pub call_id: String,
    /// Short human label (`result` or `dispatch`).
    pub label: String,
}

/// One request to persist text to a spill artifact.
#[derive(Debug, Clone)]
pub struct SaveTextSpill {
    /// Session that owns the artifact.
    pub owner: SpillOwner,
    /// Descriptive producer fields; not used for access control.
    pub source: SpillSource,
    /// Caller-suggested base name. The backend sanitizes it to one segment.
    pub suggested_name: String,
    /// Full UTF-8 text to persist.
    pub content: String,
}

/// Saved spill artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    /// Backend-produced locator. Consumers render it; they do not parse it.
    pub locator: SpillLocator,
    /// Exact UTF-8 byte length of the stored content.
    pub bytes: usize,
    /// Backend-specific retrieval guidance shown to the model.
    pub retrieval_hint: String,
}

/// Failures from a spill backend.
#[derive(Debug, Error)]
pub enum SpillError {
    /// Real storage failure. Callers decide how to degrade.
    #[error("{0}")]
    Storage(String),
}

/// Persist oversized tool text.
pub trait SpillBackend: Send + Sync {
    /// Persist `input.content` and return its locator.
    ///
    /// @param input - owner, source, suggested name, and full text.
    /// @returns the saved artifact; rejects on a storage failure.
    fn save_text(&self, input: SaveTextSpill) -> Result<SpillRef, SpillError>;
}

/// `ctx.spillStore`.
pub struct SpillStore {
    backend: Arc<dyn SpillBackend>,
}

impl SpillStore {
    /// Wrap a backend.
    pub fn new(backend: Arc<dyn SpillBackend>) -> Self {
        Self { backend }
    }

    /// Persist `input.content` to a session-scoped spill artifact.
    ///
    /// @param input - owner, source, suggested name, and full text.
    /// @returns the saved artifact; rejects on a storage failure.
    pub fn save_text(&self, input: SaveTextSpill) -> Result<SpillRef, SpillError> {
        self.backend.save_text(input)
    }
}

impl Service for SpillStore {
    const KEY: &'static str = "spillStore";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;

    struct Stub;

    impl SpillBackend for Stub {
        fn save_text(&self, input: SaveTextSpill) -> Result<SpillRef, SpillError> {
            Ok(SpillRef {
                locator: SpillLocator(format!("/spill/{}", input.suggested_name)),
                bytes: input.content.len(),
                retrieval_hint: "stub".into(),
            })
        }
    }

    #[test]
    fn provide_and_save() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SpillStore::new(Arc::new(Stub))))
            .unwrap();
        assert!(ctx.has_service("spillStore"));
        let store = ctx.service::<SpillStore>().unwrap();
        let saved = store
            .save_text(SaveTextSpill {
                owner: SpillOwner {
                    session_id: "s".into(),
                },
                source: SpillSource {
                    tool_name: "bash".into(),
                    call_id: "c1".into(),
                    label: "result".into(),
                },
                suggested_name: "bash.txt".into(),
                content: "hello".into(),
            })
            .unwrap();
        assert_eq!(saved.bytes, 5);
        assert_eq!(saved.locator.0, "/spill/bash.txt");
        ctx.dispose();
        assert!(!ctx.has_service("spillStore"));
    }
}
