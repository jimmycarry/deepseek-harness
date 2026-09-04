//! Dialect-neutral hook vocabulary.

/// Bridge that ran a hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDialect {
    /// Claude Code bridge stamps `claude-code`.
    ClaudeCode,
    /// Codex bridge stamps `codex`.
    Codex,
}

impl HookDialect {
    /// Wire token written on `hook/invoked`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// How a matcher pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherMode {
    /// Word-and-pipe patterns are literal alternation; otherwise unanchored regex.
    ClaudeCode,
    /// Every non-empty pattern is an unanchored regex.
    Codex,
}

impl MatcherMode {
    /// Wire token used in matcher diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// One configured command hook.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandHook {
    /// Shell command line.
    pub command: String,
    /// Per-hook timeout in seconds.
    pub timeout_sec: Option<f64>,
}

/// One matcher group plus the command hooks that run when it matches.
#[derive(Debug, Clone, PartialEq)]
pub struct MatcherGroup {
    /// Absent / empty / `*` are match-all.
    pub matcher: Option<String>,
    /// Command hooks in config order.
    pub hooks: Vec<CommandHook>,
}

/// Neutral permission decision folded from top-level `decision` and
/// `hookSpecificOutput.permissionDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDecision {
    /// Legacy top-level permit.
    Approve,
    /// `permissionDecision` permit.
    Allow,
    /// Legacy top-level forbid.
    Block,
    /// `permissionDecision` forbid.
    Deny,
    /// `permissionDecision` confirmation request.
    Ask,
}

impl HookDecision {
    /// Wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Allow => "allow",
            Self::Block => "block",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// Dialect-neutral outcome decoded from exit code, stdout, and stderr.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookOutput {
    /// Process exit, or `None` when the hook could not run.
    pub exit_code: Option<i32>,
    /// Trimmed stderr.
    pub stderr: String,
    /// Trimmed stdout.
    pub stdout: String,
    /// `false` asks to halt.
    pub continue_run: Option<bool>,
    /// Reason shown when [`Self::continue_run`] is `false`.
    pub stop_reason: Option<String>,
    /// Parsed permission decision, if any.
    pub decision: Option<HookDecision>,
    /// Explanation accompanying [`Self::decision`].
    pub reason: Option<String>,
    /// Discriminator claimed by `hookSpecificOutput`.
    pub hook_event_name: Option<String>,
    /// Extra context to inject.
    pub additional_context: Option<String>,
    /// Warning the bridge may surface.
    pub system_message: Option<String>,
    /// Parsed but not honored tool-input rewrite.
    pub updated_input: Option<serde_json::Map<String, serde_json::Value>>,
}
