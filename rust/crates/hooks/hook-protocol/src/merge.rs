//! Merge matched hooks into one most-restrictive outcome.

use crate::types::{HookDecision, HookOutput};
use std::collections::HashMap;

/// The single decision a hook point resolves to after merging all matched hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergedDecision {
    /// Most-restrictive forbid (`block` / `deny`).
    Deny,
    /// Confirmation requested.
    Ask,
    /// Permit (`approve` / `allow`).
    Allow,
    /// No hook expressed a decision.
    None,
}

impl MergedDecision {
    /// Wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::None => "none",
        }
    }
}

/// Folded outcome of every hook that matched one point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedHookOutcome {
    /// Most-restrictive permission decision.
    pub decision: MergedDecision,
    /// Joined reasons from the winning rank, when any exist.
    pub reason: Option<String>,
    /// `true` when any hook asked to halt (`continue: false`).
    pub stop: bool,
    /// First halting hook's `stopReason`.
    pub stop_reason: Option<String>,
    /// Every hook's `additionalContext`, in hook order.
    pub additional_context: Vec<String>,
    /// Every hook's `systemMessage`, in hook order.
    pub system_messages: Vec<String>,
}

fn rank(decision: Option<HookDecision>) -> u8 {
    match decision {
        Some(HookDecision::Deny | HookDecision::Block) => 3,
        Some(HookDecision::Ask) => 2,
        Some(HookDecision::Approve | HookDecision::Allow) => 1,
        None => 0,
    }
}

fn decision_for_rank(max_rank: u8) -> MergedDecision {
    match max_rank {
        3 => MergedDecision::Deny,
        2 => MergedDecision::Ask,
        1 => MergedDecision::Allow,
        _ => MergedDecision::None,
    }
}

/// Fold `outputs` in hook order into one [`MergedHookOutcome`].
pub fn merge_hook_outputs(outputs: &[HookOutput]) -> MergedHookOutcome {
    let mut max_rank = 0u8;
    let mut reasons_by_rank: HashMap<u8, Vec<String>> = HashMap::new();
    let mut stop = false;
    let mut stop_reason = None;
    let mut additional_context = Vec::new();
    let mut system_messages = Vec::new();

    for out in outputs {
        let r = rank(out.decision);
        if r > max_rank {
            max_rank = r;
        }
        if (r == 3 || r == 2) && out.reason.as_ref().is_some_and(|s| !s.is_empty()) {
            reasons_by_rank
                .entry(r)
                .or_default()
                .push(out.reason.clone().expect("checked"));
        }
        if out.continue_run == Some(false) && !stop {
            stop = true;
            stop_reason = out.stop_reason.clone();
        }
        if let Some(ctx) = &out.additional_context {
            if !ctx.is_empty() {
                additional_context.push(ctx.clone());
            }
        }
        if let Some(msg) = &out.system_message {
            if !msg.is_empty() {
                system_messages.push(msg.clone());
            }
        }
    }

    let reasons = reasons_by_rank.remove(&max_rank).unwrap_or_default();
    MergedHookOutcome {
        decision: decision_for_rank(max_rank),
        reason: if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("\n\n"))
        },
        stop,
        stop_reason,
        additional_context,
        system_messages,
    }
}
