//! Per-agent scoped registration. A contribution is global or owned by exactly
//! one scope key. Scoped registrations do not inherit to subagents.

use dsh_brand::Branded;
use std::sync::atomic::{AtomicU64, Ordering};

/// Brand token for a scope key.
pub struct ScopeKeyBrand;
/// Opaque scope identity compared by object identity (here: allocated id).
pub type ScopeKey = Branded<ScopeKeyBrand>;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh scope key. An active agent is its own scope key.
pub fn create_scope() -> ScopeKey {
    ScopeKey::new(format!("scope-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// Resolve the scope of an agent-like owner. The harness convention: the
/// agent's identity string is the key.
pub fn scope_of(owner: &str) -> ScopeKey {
    ScopeKey::new(owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_scopes_are_distinct() {
        assert_ne!(create_scope(), create_scope());
    }

    #[test]
    fn scope_of_is_stable_for_the_same_owner() {
        assert_eq!(scope_of("agent-1"), scope_of("agent-1"));
    }
}
