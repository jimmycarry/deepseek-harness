//! Crash-recovery closers for an interrupted session log.

use crate::{SessionEvent, SessionEventData, SurfaceOp, ToolRecoveryError, TurnEndReason};
use dsh_llm::{call_id, ContentBlock, MessageSource, ToolResultMessage};

/// Recovery code for an assistant tool request that never reached a recorded call start.
pub const TOOL_NOT_STARTED: &str = "TOOL_NOT_STARTED";

/// Recovery code for a recorded tool call whose completed outcome was not durably recorded.
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";

const TOOL_NOT_STARTED_TEXT: &str = "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed.";

const TOOL_OUTCOME_UNKNOWN_TEXT: &str = "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly.";

/// Return deterministic synthetic events that close an open tail turn.
///
/// Unmatched calls receive error results first, then an open `step/end` and an
/// interrupted `turn/end`. Sequences continue the log and timestamps reuse the
/// last real event. A balanced or empty log returns no events.
pub fn interrupted_turn_closers(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let mut open_turn: Option<u32> = None;
    let mut open_step: Option<u32> = None;
    let mut pending: Vec<(String, u32, Option<u64>)> = Vec::new();
    for event in events {
        match &event.data {
            SessionEventData::TurnStart { turn } => {
                open_turn = Some(*turn);
                open_step = None;
                pending.clear();
            }
            SessionEventData::TurnEnd { .. } => {
                open_turn = None;
                open_step = None;
                pending.clear();
            }
            SessionEventData::StepStart { step, .. } => {
                open_step = Some(*step);
            }
            SessionEventData::StepEnd { .. } => {
                pending.clear();
                open_step = None;
            }
            SessionEventData::AssistantMessage { step, message, .. } => {
                for block in &message.content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        pending.push((id.as_str().to_string(), *step, None));
                    }
                }
            }
            SessionEventData::ToolCall { call_id, .. } => {
                if let Some(entry) = pending.iter_mut().find(|(id, _, _)| id == call_id) {
                    entry.2 = Some(event.seq);
                }
            }
            SessionEventData::ToolResult { message, .. } => {
                if let Some(call_id) = message.tool_call_id() {
                    pending.retain(|(id, _, _)| id != call_id);
                }
            }
            _ => {}
        }
    }
    let Some(last) = events.last() else {
        return Vec::new();
    };
    let Some(open_turn) = open_turn else {
        return Vec::new();
    };
    let mut seq = last.seq + 1;
    let time = last.time;
    let mut closers = Vec::new();
    for (call, step, call_seq) in pending {
        let started = call_seq.is_some();
        let text = if started {
            TOOL_OUTCOME_UNKNOWN_TEXT
        } else {
            TOOL_NOT_STARTED_TEXT
        };
        let (name, code) = if started {
            ("ToolOutcomeUnknownError", TOOL_OUTCOME_UNKNOWN)
        } else {
            ("ToolNotStartedError", TOOL_NOT_STARTED)
        };
        let branded = call_id(call.clone());
        let message = ToolResultMessage {
            source: MessageSource::Tool {
                call_id: call.clone(),
            },
            content: vec![ContentBlock::ToolResult {
                tool_call_id: branded,
                content: vec![ContentBlock::text(text)],
                is_error: true,
            }],
            role: "user".into(),
            id: format!("interrupted-tool-result-{call}-{seq}"),
        };
        closers.push(SessionEvent {
            seq,
            time,
            data: SessionEventData::ToolResult {
                turn: open_turn,
                step,
                message,
                error: Some(ToolRecoveryError {
                    name: name.into(),
                    code: code.into(),
                }),
            },
            source_event_seqs: call_seq.map(|cited| vec![cited]),
            surface_op: Some(SurfaceOp::Append),
            ignorable: false,
        });
        seq += 1;
    }
    if let Some(step) = open_step {
        closers.push(SessionEvent {
            seq,
            time,
            data: SessionEventData::StepEnd {
                turn: open_turn,
                step,
            },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        });
        seq += 1;
    }
    closers.push(SessionEvent {
        seq,
        time,
        data: SessionEventData::TurnEnd {
            turn: open_turn,
            reason: TurnEndReason::Interrupted,
        },
        source_event_seqs: None,
        surface_op: None,
        ignorable: false,
    });
    closers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEvent;
    use dsh_llm::{call_id, AssistantMessage, ContentBlock};

    fn event(seq: u64, data: SessionEventData) -> SessionEvent {
        SessionEvent {
            seq,
            time: seq,
            data,
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        }
    }

    #[test]
    fn balanced_and_empty_logs_need_no_closers() {
        assert!(interrupted_turn_closers(&[]).is_empty());
        let balanced = [
            event(0, SessionEventData::TurnStart { turn: 1 }),
            event(
                1,
                SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        assert!(interrupted_turn_closers(&balanced).is_empty());
    }

    #[test]
    fn closes_open_turn_and_open_step() {
        let open_turn = [event(0, SessionEventData::TurnStart { turn: 1 })];
        let closers = interrupted_turn_closers(&open_turn);
        assert_eq!(closers.len(), 1);
        assert!(matches!(
            closers[0].data,
            SessionEventData::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        let open_step = [
            event(0, SessionEventData::TurnStart { turn: 1 }),
            event(1, SessionEventData::StepStart { turn: 1, step: 1 }),
        ];
        let types: Vec<_> = interrupted_turn_closers(&open_step)
            .into_iter()
            .map(|event| match event.data {
                SessionEventData::StepEnd { .. } => "step/end",
                SessionEventData::TurnEnd { .. } => "turn/end",
                _ => "other",
            })
            .collect();
        assert_eq!(types, ["step/end", "turn/end"]);
    }

    #[test]
    fn synthesizes_not_started_tool_result() {
        let message = AssistantMessage::model(
            vec![
                ContentBlock::text("calling a tool"),
                ContentBlock::ToolCall {
                    id: call_id("call-1"),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ],
            "mock",
            "mock",
        );
        let events = [
            event(0, SessionEventData::TurnStart { turn: 2 }),
            event(1, SessionEventData::StepStart { turn: 2, step: 1 }),
            event(
                2,
                SessionEventData::AssistantMessage {
                    turn: 2,
                    step: 1,
                    message,
                    usage: None,
                },
            ),
        ];
        let closers = interrupted_turn_closers(&events);
        assert_eq!(closers.len(), 3);
        match &closers[0].data {
            SessionEventData::ToolResult { error, message, .. } => {
                assert_eq!(
                    error.as_ref().map(|item| item.code.as_str()),
                    Some(TOOL_NOT_STARTED)
                );
                assert!(message.is_error());
                assert_eq!(message.id, "interrupted-tool-result-call-1-3");
            }
            other => panic!("expected tool/result, got {other:?}"),
        }
    }
}
