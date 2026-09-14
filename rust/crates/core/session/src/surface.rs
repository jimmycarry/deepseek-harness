//! Detached surface fold: current nodes plus replacement history.
//!
//! [`fold_surface`] replays one complete contiguous log through the same
//! eligibility, provenance, and tool-result rewrite checks as TypeScript
//! `foldSurface`. Incremental live append still uses [`SessionSurface::apply`].

use crate::{event_type_name, SessionError, SessionEvent, SessionEventData, SurfaceOp};
use serde_json::{json, Value};

/// One replacement observed while folding a session surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceFoldReplacement {
    /// Seq of the event that replaced the prior surface range.
    pub seq: u64,
    /// Declared inclusive start seq of the replaced surface range.
    pub start: u64,
    /// Declared inclusive end seq of the replaced surface range.
    pub end: u64,
    /// Actual surface entries removed by the operation, in surface order.
    pub shadowed_seqs: Vec<u64>,
}

/// Complete result of replaying the surface operations in a session log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SurfaceFoldResult {
    /// Current surface event sequences in model-visible order.
    pub nodes: Vec<u64>,
    /// Replacement operations in event order.
    pub replacements: Vec<SurfaceFoldReplacement>,
}

/// Replay a complete session log through the canonical surface fold.
///
/// @param events - session events in contiguous seq order starting at 0.
/// @returns detached current sequences and replacement history.
pub fn fold_surface(events: &[SessionEvent]) -> Result<SurfaceFoldResult, SessionError> {
    let mut nodes = Vec::new();
    let mut replacements = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let expected = index as u64;
        if event.seq != expected {
            return Err(surface_err(format!(
                "session event seq {} is not contiguous; expected {expected}",
                event.seq
            )));
        }
        if let Some(replacement) = apply_surface_event(&mut nodes, event, events)? {
            replacements.push(replacement);
        }
    }
    Ok(SurfaceFoldResult {
        nodes,
        replacements,
    })
}

fn apply_surface_event(
    nodes: &mut Vec<u64>,
    event: &SessionEvent,
    events: &[SessionEvent],
) -> Result<Option<SurfaceFoldReplacement>, SessionError> {
    let Some(op) = surface_op_of(event)? else {
        return Ok(None);
    };
    match op {
        SurfaceOp::Append => {
            assert_provenance(event, &[])?;
            nodes.push(event.seq);
            Ok(None)
        }
        SurfaceOp::Replace { start, end } => {
            let (start_idx, end_idx, shadowed) = replacement_range(nodes, start, end)?;
            assert_provenance(event, &shadowed)?;
            assert_tool_result_rewrite(event, &shadowed, events)?;
            nodes.splice(start_idx..=end_idx, [event.seq]);
            Ok(Some(SurfaceFoldReplacement {
                seq: event.seq,
                start,
                end,
                shadowed_seqs: shadowed,
            }))
        }
    }
}

fn surface_op_of(event: &SessionEvent) -> Result<Option<SurfaceOp>, SessionError> {
    let type_name = event_type_name(&event.data);
    if !event.data.is_surface() {
        if event.surface_op.is_some() {
            return Err(surface_err(format!(
                "session event \"{type_name}\" is not surface-eligible and cannot carry surfaceOp"
            )));
        }
        if event.source_event_seqs.is_some() {
            return Err(surface_err(format!(
                "session event \"{type_name}\" is not surface-eligible and cannot carry sourceEventSeqs"
            )));
        }
        return Ok(None);
    }
    match &event.surface_op {
        Some(op) => Ok(Some(op.clone())),
        None => Err(surface_err(format!(
            "session event \"{type_name}\" is surface-eligible and requires a surfaceOp marker"
        ))),
    }
}

fn replacement_range(
    nodes: &[u64],
    start: u64,
    end: u64,
) -> Result<(usize, usize, Vec<u64>), SessionError> {
    let start_idx = nodes
        .iter()
        .position(|node| *node == start)
        .ok_or_else(|| {
            surface_err(format!(
                "surface replace: start seq {start} not found in surface"
            ))
        })?;
    let end_idx = nodes.iter().position(|node| *node == end).ok_or_else(|| {
        surface_err(format!(
            "surface replace: end seq {end} not found in surface"
        ))
    })?;
    if start_idx > end_idx {
        return Err(surface_err(format!(
            "surface replace: start seq {start} (index {start_idx}) is after end seq {end} (index {end_idx})"
        )));
    }
    Ok((start_idx, end_idx, nodes[start_idx..=end_idx].to_vec()))
}

fn assert_provenance(event: &SessionEvent, shadowed_seqs: &[u64]) -> Result<(), SessionError> {
    let mut sources = std::collections::HashSet::new();
    if let Some(raw) = &event.source_event_seqs {
        if raw.is_empty() && !matches!(event.data, SessionEventData::AssistantMessage { .. }) {
            return Err(surface_err(
                "sourceEventSeqs must not be empty except on assistant/message",
            ));
        }
        let mut non_earlier = None;
        for source in raw {
            if !sources.insert(*source) {
                return Err(surface_err("sourceEventSeqs must not contain duplicates"));
            }
            if *source >= event.seq {
                non_earlier = Some(*source);
            }
        }
        if let Some(source) = non_earlier {
            return Err(surface_err(format!(
                "sourceEventSeqs must reference earlier events: {source} >= current seq {}",
                event.seq
            )));
        }
    }
    let missing: Vec<u64> = shadowed_seqs
        .iter()
        .copied()
        .filter(|seq| !sources.contains(seq))
        .collect();
    if !missing.is_empty() {
        let joined = missing
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(surface_err(format!(
            "surface replace: sourceEventSeqs must include every shadowed surface node; missing {joined}"
        )));
    }
    Ok(())
}

fn assert_tool_result_rewrite(
    event: &SessionEvent,
    shadowed_seqs: &[u64],
    events: &[SessionEvent],
) -> Result<(), SessionError> {
    if !matches!(event.data, SessionEventData::ToolResult { .. }) {
        return Ok(());
    }
    if shadowed_seqs.len() != 1 {
        return Err(surface_err(
            "tool/result surface replacement must rewrite exactly one current node",
        ));
    }
    for original_seq in shadowed_seqs {
        let original = events.get(*original_seq as usize).ok_or_else(|| {
            surface_err("tool/result surface replacement must target a current tool/result")
        })?;
        if !matches!(original.data, SessionEventData::ToolResult { .. }) {
            return Err(surface_err(
                "tool/result surface replacement must target a current tool/result",
            ));
        }
        if tool_result_except_content(&original.data)? != tool_result_except_content(&event.data)? {
            return Err(surface_err(
                "tool/result surface replacement may change only content",
            ));
        }
    }
    Ok(())
}

fn tool_result_except_content(data: &SessionEventData) -> Result<Value, SessionError> {
    let SessionEventData::ToolResult {
        turn,
        step,
        message,
        error,
    } = data
    else {
        return Err(surface_err(
            "tool/result surface replacement must target a current tool/result",
        ));
    };
    let mut message_value = serde_json::to_value(message).map_err(|error| {
        SessionError::InvalidSurface(format!("tool/result message is not JSON: {error}"))
    })?;
    if let Some(first) = message_value
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|content| content.get_mut(0))
        .and_then(Value::as_object_mut)
    {
        first.insert("content".into(), Value::Null);
    }
    Ok(json!({
        "turn": turn,
        "step": step,
        "message": message_value,
        "error": error,
    }))
}

fn surface_err(message: impl Into<String>) -> SessionError {
    SessionError::InvalidSurface(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session_id, Session, SessionEventData, SurfaceOp};
    use dsh_llm::{AssistantMessage, ContentBlock, UserMessage};

    fn live_log(build: impl FnOnce(&Session)) -> Vec<SessionEvent> {
        let session = Session::new(session_id("fold"));
        build(&session);
        session.events()
    }

    #[test]
    fn empty_log_is_an_empty_surface() {
        let folded = fold_surface(&[]).unwrap();
        assert!(folded.nodes.is_empty());
        assert!(folded.replacements.is_empty());
    }

    #[test]
    fn replace_records_shadowed_seqs_and_current_nodes() {
        let events = live_log(|session| {
            session
                .append(SessionEventData::TurnStart { turn: 1 }, None)
                .unwrap();
            let first = session
                .append(
                    SessionEventData::UserMessage(UserMessage::text("first")),
                    Some(SurfaceOp::Append),
                )
                .unwrap();
            session
                .append(
                    SessionEventData::AssistantChunk {
                        turn: 1,
                        step: 1,
                        chunk: dsh_llm::StreamChunk::TextDelta {
                            index: 0,
                            text: "draft".into(),
                        },
                    },
                    None,
                )
                .unwrap();
            session
                .append_cited(
                    SessionEventData::AssistantMessage {
                        turn: 1,
                        step: 1,
                        message: AssistantMessage::model(
                            vec![ContentBlock::text("replacement")],
                            "p",
                            "m",
                        ),
                        usage: None,
                    },
                    SurfaceOp::Replace {
                        start: first.seq,
                        end: first.seq,
                    },
                    vec![first.seq],
                )
                .unwrap();
        });
        let folded = fold_surface(&events).unwrap();
        assert_eq!(folded.nodes, vec![3]);
        assert_eq!(folded.replacements[0].shadowed_seqs, vec![1]);
        assert_eq!(folded.replacements[0].seq, 3);
    }

    #[test]
    fn rejects_non_surface_source_citations() {
        let mut events = live_log(|session| {
            session
                .append(SessionEventData::TurnStart { turn: 1 }, None)
                .unwrap();
        });
        events[0].source_event_seqs = Some(vec![0]);
        let error = fold_surface(&events).unwrap_err();
        assert!(
            matches!(error, SessionError::InvalidSurface(message) if message.contains("not surface-eligible"))
        );
    }

    #[test]
    fn rejects_replacement_missing_a_shadowed_source() {
        let events = live_log(|session| {
            let first = session
                .append(
                    SessionEventData::UserMessage(UserMessage::text("first")),
                    Some(SurfaceOp::Append),
                )
                .unwrap();
            session
                .append(
                    SessionEventData::UserMessage(UserMessage::text("next")),
                    Some(SurfaceOp::Replace {
                        start: first.seq,
                        end: first.seq,
                    }),
                )
                .unwrap();
        });
        let error = fold_surface(&events).unwrap_err();
        assert!(
            matches!(error, SessionError::InvalidSurface(message) if message.contains("missing 0"))
        );
    }

    #[test]
    fn rejects_duplicate_sources() {
        let mut events = live_log(|session| {
            session
                .append(
                    SessionEventData::UserMessage(UserMessage::text("first")),
                    Some(SurfaceOp::Append),
                )
                .unwrap();
            session
                .append(
                    SessionEventData::UserMessage(UserMessage::text("next")),
                    Some(SurfaceOp::Append),
                )
                .unwrap();
        });
        events[1].source_event_seqs = Some(vec![0, 0]);
        let error = fold_surface(&events).unwrap_err();
        assert!(
            matches!(error, SessionError::InvalidSurface(message) if message.contains("duplicates"))
        );
    }
}
