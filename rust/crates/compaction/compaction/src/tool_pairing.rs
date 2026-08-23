//! Tool-pairing balance over a session surface.
//!
//! Safe compaction cuts are derived from tool-call / tool-result content in
//! current surface order rather than step markers. A safe edge has no
//! unanswered assistant tool call crossing it.

use dsh_llm::ContentBlock;
use dsh_session::{Session, SessionEventData};

/// Whether the cut immediately before a current surface sequence is balanced.
///
/// # Errors
/// The seq is absent from the current surface, a surface sequence has no
/// matching log event, or a tool result has no preceding open call.
pub fn tool_pairing_balanced_before(session: &Session, seq: u64) -> Result<bool, String> {
    cut_balance(session, seq, 0)
}

/// Whether the cut immediately after a current surface sequence is balanced.
///
/// # Errors
/// The seq is absent from the current surface, a surface sequence has no
/// matching log event, or a tool result has no preceding open call.
pub fn tool_pairing_balanced_after(session: &Session, seq: u64) -> Result<bool, String> {
    cut_balance(session, seq, 1)
}

fn cut_balance(session: &Session, seq: u64, offset: usize) -> Result<bool, String> {
    let (cuts, index_by_seq) = fold_surface(session)?;
    let Some(&index) = index_by_seq.get(&seq) else {
        return Err(format!("tool-pairing balance: surface seq {seq} not found"));
    };
    cuts.get(index + offset).copied().ok_or_else(|| {
        format!("tool-pairing balance: surface seq {seq} not found")
    })
}

fn fold_surface(session: &Session) -> Result<(Vec<bool>, std::collections::BTreeMap<u64, usize>), String> {
    let events = session.events();
    let seqs = session.surface().nodes;
    let mut cuts = vec![true];
    let mut index_by_seq = std::collections::BTreeMap::new();
    let mut in_progress = 0i32;
    for (index, seq) in seqs.iter().copied().enumerate() {
        in_progress += event_delta(&events, seq)?;
        if in_progress < 0 {
            return Err(format!(
                "tool-pairing balance: tool/result at surface seq {seq} has no matching tool-call (corrupt surface)"
            ));
        }
        cuts.push(in_progress == 0);
        index_by_seq.insert(seq, index);
    }
    Ok((cuts, index_by_seq))
}

fn event_delta(events: &[dsh_session::SessionEvent], seq: u64) -> Result<i32, String> {
    let event = events.get(seq as usize).ok_or_else(|| {
        format!(
            "tool-pairing balance: surface seq {seq} has no matching session event (corrupt surface)"
        )
    })?;
    if event.seq != seq {
        return Err(format!(
            "tool-pairing balance: surface seq {seq} has no matching session event (corrupt surface)"
        ));
    }
    Ok(match &event.data {
        SessionEventData::AssistantMessage { message, .. } => message
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .count() as i32,
        SessionEventData::ToolResult { .. } => -1,
        _ => 0,
    })
}
