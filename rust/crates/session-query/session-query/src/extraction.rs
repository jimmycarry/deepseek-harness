//! First-party semantic text extraction for session-query consumers.

use dsh_llm::ContentBlock;
use dsh_session::{SessionEvent, SessionEventData, TurnEndReason};
use serde_json::Value;

/// Extract searchable semantic text from one first-party session event.
///
/// Structural boundaries, raw stream chunks, request envelopes, and unknown
/// extension events contribute no text.
///
/// @param event - event to inspect.
/// @returns newline-joined semantic text, or an empty string when non-searchable.
pub fn extract_session_event_text(event: &SessionEvent) -> String {
    match &event.data {
        SessionEventData::UserMessage(message) => content_text(&message.content),
        SessionEventData::AssistantMessage { message, .. } => content_text(&message.content),
        SessionEventData::ToolCall {
            name, arguments, ..
        } => join_text(&[name.as_str(), arguments.as_str()]),
        SessionEventData::ToolResult { message, error, .. } => join_text(&[
            content_text(&message.content),
            error
                .as_ref()
                .map(|error| error.name.as_str())
                .unwrap_or("")
                .to_string(),
            error
                .as_ref()
                .map(|error| error.code.as_str())
                .unwrap_or("")
                .to_string(),
        ]),
        SessionEventData::TodoWrite { todos } => todo_text(todos),
        SessionEventData::TurnEnd { reason, .. } => turn_end_text(reason),
        _ => String::new(),
    }
}

fn turn_end_text(reason: &TurnEndReason) -> String {
    match reason {
        TurnEndReason::Error { message, .. } => join_text(&["error", message.as_str()]),
        TurnEndReason::Aborted { .. } => "aborted".into(),
        TurnEndReason::MaxTokens => "max-tokens".into(),
        TurnEndReason::Interrupted => "interrupted".into(),
        TurnEndReason::Completed | TurnEndReason::Blocked => String::new(),
    }
}

fn todo_text(todos: &Value) -> String {
    let Some(items) = todos.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for todo in items {
        if let Some(status) = todo.get("status").and_then(Value::as_str) {
            parts.push(status.to_string());
        }
        if let Some(content) = todo.get("content").and_then(Value::as_str) {
            parts.push(content.to_string());
        }
    }
    join_text(&parts)
}

fn content_text(content: &[ContentBlock]) -> String {
    join_text(&content.iter().flat_map(block_text).collect::<Vec<_>>())
}

fn block_text(block: &ContentBlock) -> Vec<String> {
    match block {
        ContentBlock::Text { text } => vec![text.clone()],
        ContentBlock::Reasoning { .. } | ContentBlock::Image { .. } => Vec::new(),
        ContentBlock::ToolCall {
            name, arguments, ..
        } => vec![name.clone(), arguments.clone()],
        ContentBlock::ToolResult { content, .. } => content.iter().flat_map(block_text).collect(),
    }
}

fn join_text<S: AsRef<str>>(parts: &[S]) -> String {
    parts
        .iter()
        .map(|part| part.as_ref().trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{call_id, AssistantMessage, ContentBlock, ToolResultMessage, UserMessage};
    use dsh_session::{session_id, Session, SessionEventData, SurfaceOp, ToolRecoveryError};

    fn event(data: SessionEventData, surface_op: Option<SurfaceOp>) -> SessionEvent {
        let session = Session::new(session_id("extract"));
        session.append(data, surface_op).unwrap()
    }

    #[test]
    fn extracts_message_tool_todo_and_failure_detail() {
        let call = call_id("call");
        let content = vec![
            ContentBlock::text(" visible "),
            ContentBlock::Reasoning {
                text: "thought".into(),
            },
            ContentBlock::ToolCall {
                id: call.clone(),
                name: "read".into(),
                arguments: r#"{"path":"a"}"#.into(),
            },
            ContentBlock::ToolResult {
                tool_call_id: call.clone(),
                content: vec![ContentBlock::text("nested")],
                is_error: false,
            },
        ];
        let user = event(
            SessionEventData::UserMessage(UserMessage::from_parts(
                content.clone(),
                dsh_llm::MessageSource::User,
            )),
            Some(SurfaceOp::Append),
        );
        assert_eq!(
            extract_session_event_text(&user),
            "visible\nread\n{\"path\":\"a\"}\nnested"
        );
        let assistant = event(
            SessionEventData::AssistantMessage {
                turn: 1,
                step: 1,
                message: AssistantMessage::model(content, "p", "m"),
                usage: None,
            },
            Some(SurfaceOp::Append),
        );
        assert_eq!(
            extract_session_event_text(&assistant),
            "visible\nread\n{\"path\":\"a\"}\nnested"
        );
        let reasoning_only = event(
            SessionEventData::AssistantMessage {
                turn: 1,
                step: 1,
                message: AssistantMessage::model(
                    vec![ContentBlock::Reasoning {
                        text: "private thought".into(),
                    }],
                    "p",
                    "m",
                ),
                usage: None,
            },
            Some(SurfaceOp::Append),
        );
        assert_eq!(extract_session_event_text(&reasoning_only), "");
        let call_event = event(
            SessionEventData::ToolCall {
                turn: 1,
                step: 1,
                call_id: "call".into(),
                name: "bash".into(),
                arguments: r#"{"cmd":"pwd"}"#.into(),
            },
            None,
        );
        assert_eq!(
            extract_session_event_text(&call_event),
            "bash\n{\"cmd\":\"pwd\"}"
        );
        let failed = event(
            SessionEventData::ToolResult {
                turn: 1,
                step: 1,
                message: ToolResultMessage::new(
                    call.clone(),
                    vec![ContentBlock::text("failed")],
                    true,
                ),
                error: Some(ToolRecoveryError {
                    name: "Oops".into(),
                    code: "E_OOPS".into(),
                }),
            },
            Some(SurfaceOp::Append),
        );
        assert_eq!(extract_session_event_text(&failed), "failed\nOops\nE_OOPS");
        let empty_result = event(
            SessionEventData::ToolResult {
                turn: 1,
                step: 1,
                message: ToolResultMessage::new(call, Vec::new(), false),
                error: None,
            },
            Some(SurfaceOp::Append),
        );
        assert_eq!(extract_session_event_text(&empty_result), "");
        let todos = event(
            SessionEventData::TodoWrite {
                todos: serde_json::json!([{ "status": "in_progress", "content": "ship search" }]),
            },
            None,
        );
        assert_eq!(
            extract_session_event_text(&todos),
            "in_progress\nship search"
        );
    }

    #[test]
    fn extracts_turn_outcomes_and_skips_structural_events() {
        let cases = [
            (
                TurnEndReason::Error {
                    message: "boom".into(),
                    code: "UNKNOWN".into(),
                },
                "error\nboom",
            ),
            (
                TurnEndReason::Aborted {
                    reason: "user".into(),
                },
                "aborted",
            ),
            (TurnEndReason::MaxTokens, "max-tokens"),
            (TurnEndReason::Interrupted, "interrupted"),
            (TurnEndReason::Completed, ""),
            (TurnEndReason::Blocked, ""),
        ];
        for (reason, text) in cases {
            let event = event(SessionEventData::TurnEnd { turn: 1, reason }, None);
            assert_eq!(extract_session_event_text(&event), text);
        }
        let structural = event(SessionEventData::TurnStart { turn: 1 }, None);
        assert_eq!(extract_session_event_text(&structural), "");
    }
}
