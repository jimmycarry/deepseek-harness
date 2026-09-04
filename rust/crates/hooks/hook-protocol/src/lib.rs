//! Shared hook protocol: matcher, codec, merge, log-only events, detached
//! runs, and command execution. Bridges own payloads and decision mapping.

mod codec;
mod detached;
mod events;
mod matcher;
mod merge;
mod runner;
mod types;

pub use codec::parse_hook_output;
pub use detached::{create_detached_runs, DetachedRuns};
pub use events::{
    append_hook_invoked, append_hook_result, summarize_stderr, HookInvocation, HookResultRecord,
    DEFAULT_STDERR_SUMMARY_MAX_CHARS,
};
pub use matcher::{matcher_diagnostic, matches_matcher};
pub use merge::{merge_hook_outputs, MergedDecision, MergedHookOutcome};
pub use runner::{
    run_hook, HookShellRequest, HookShellResult, RunHookOptions, RunHookResult,
    DEFAULT_HOOK_TIMEOUT_MS,
};
pub use types::{CommandHook, HookDecision, HookDialect, HookOutput, MatcherGroup, MatcherMode};

/// Plugin role name matching TypeScript `export const name`.
pub fn name() -> &'static str {
    "hook-protocol"
}

#[cfg(test)]
mod tests;
