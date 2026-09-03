//! Tool render-intent vocabulary: `present_call` / `present_result`.

use dsh_llm::ContentBlock;
use serde_json::Value;

/// Category of a tool call, used by a UI to pick an icon or treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallKind {
    /// File read.
    Read,
    /// File edit.
    Edit,
    /// File delete.
    Delete,
    /// File or path move.
    Move,
    /// Content or path search.
    Search,
    /// Command execution.
    Execute,
    /// Network fetch.
    Fetch,
    /// Default / unspecified.
    Other,
}

/// Provider-neutral pending-call presentation.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallView {
    /// Default titled card.
    Generic(GenericCallView),
    /// Foreground shell command as a terminal card.
    Terminal(TerminalCallView),
}

/// Default card: a titled tool-call row.
#[derive(Debug, Clone, PartialEq)]
pub struct GenericCallView {
    /// Always-visible label for this call.
    pub title: String,
    /// Category for icon/treatment.
    pub kind: Option<ToolCallKind>,
    /// Salient input for a detail view. A string renders as-is.
    pub raw_input: Option<Value>,
    /// UI-facing content blocks shown on the pending call.
    pub content: Option<Vec<ContentBlock>>,
}

/// A call that is a shell command running in a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCallView {
    /// The command, shown as the terminal card's title.
    pub title: String,
    /// One-line summary rendered above the terminal card.
    pub description: Option<String>,
    /// Working directory shown as the terminal header.
    pub cwd: Option<String>,
}

/// How a tool wants the completed call shown.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultView {
    /// Default completed card.
    Generic(GenericResultView),
    /// Completed terminal card with parsed exit status.
    Terminal(TerminalResultView),
}

/// Default completed card.
#[derive(Debug, Clone, PartialEq)]
pub struct GenericResultView {
    /// Replacement title. `None` keeps the pending-state title.
    pub title: Option<String>,
    /// UI-facing result content.
    pub content: Option<Vec<ContentBlock>>,
}

/// Completed state of a [`TerminalCallView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalResultView {
    /// Replacement title. `None` keeps the pending-state title.
    pub title: Option<String>,
    /// Captured command output.
    pub output: Option<String>,
    /// Process exit code when the run ended by exiting.
    pub exit_code: Option<i32>,
    /// Signal name that killed the process.
    pub signal: Option<String>,
}
