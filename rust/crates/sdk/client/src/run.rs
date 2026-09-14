//! Receipt-to-idle collection for one SDK `Session.run` interval.
//!
//! Notifications that precede the durable `agent/inbox/spliced` receipt for
//! the queued `messageId` are dropped. `events` keeps only the root session's
//! `session.event` payloads. `notifications` keeps every in-tree frame after
//! the receipt, in wire order, including descendant `subagent.started` /
//! `finished` and child-session events.

use crate::error::SdkProtocolError;
use crate::notification::{
    is_inbox_receipt, validated_session_event, HarnessNotification, SessionParents,
};
use dsh_sdk_protocol::methods;
use serde_json::{json, Value};

/// One owned session activity interval, from enqueue receipt through idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// Session the activity ran on.
    pub session_id: String,
    /// Concatenated text of the interval's last root assistant message
    /// (empty when none).
    pub final_response: String,
    /// Every root-session `session.event` payload, in wire order.
    pub events: Vec<Value>,
    /// Every in-tree notification after the receipt, in wire order.
    pub notifications: Vec<HarnessNotification>,
}

/// Incremental collector matching TypeScript `HarnessSession.run`.
#[derive(Debug)]
pub struct RunCollector {
    root_session_id: String,
    message_id: String,
    received: bool,
    events: Vec<Value>,
    notifications: Vec<HarnessNotification>,
    done: bool,
}

impl RunCollector {
    /// Start collecting one interval for `root_session_id` / `message_id`.
    pub fn new(root_session_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            root_session_id: root_session_id.into(),
            message_id: message_id.into(),
            received: false,
            events: Vec::new(),
            notifications: Vec::new(),
            done: false,
        }
    }

    /// Whether a root `session.status` `idle` has already been collected.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Observe one wire notification.
    ///
    /// Returns `true` when the root session became idle. Notifications outside
    /// the session tree, and every frame before the inbox receipt, are dropped.
    /// A malformed root `session.event` after the receipt is a protocol error.
    pub fn push(
        &mut self,
        tree: &mut SessionParents,
        notification: HarnessNotification,
    ) -> Result<bool, SdkProtocolError> {
        if self.done {
            return Ok(true);
        }
        if !tree.observe(&notification, &self.root_session_id) {
            return Ok(false);
        }
        if !self.received {
            if !is_root_inbox_receipt(&notification, &self.root_session_id, &self.message_id) {
                return Ok(false);
            }
            self.received = true;
        }
        let done = is_root_idle(&notification, &self.root_session_id);
        if notification.method == methods::SESSION_EVENT
            && notification.params.get("sessionId").and_then(Value::as_str)
                == Some(self.root_session_id.as_str())
        {
            let event = validated_session_event(&notification.params["event"])?;
            self.notifications.push(notification);
            self.events.push(event);
        } else {
            self.notifications.push(notification);
        }
        self.done = done;
        Ok(self.done)
    }

    /// Number of collected (post-receipt, in-tree) notifications.
    pub fn collected_len(&self) -> usize {
        self.notifications.len()
    }

    /// Last collected notification, when any exist.
    pub fn last_collected(&self) -> Option<&HarnessNotification> {
        self.notifications.last()
    }

    /// Finish the interval. `final_response` is `""` when no root assistant
    /// message was collected.
    pub fn finish(self) -> RunResult {
        let final_response = final_response(&self.events);
        RunResult {
            session_id: self.root_session_id,
            final_response,
            events: self.events,
            notifications: self.notifications,
        }
    }
}

/// Collect a finite notification stream through the next root idle.
///
/// Ends with a protocol error when the iterator is exhausted before idle.
pub fn collect_run(
    root_session_id: impl Into<String>,
    message_id: &str,
    notifications: impl IntoIterator<Item = HarnessNotification>,
) -> Result<RunResult, SdkProtocolError> {
    collect_run_with(root_session_id, message_id, notifications, |_| {})
}

/// [`collect_run`] plus an observer invoked for every collected notification.
pub fn collect_run_with<F>(
    root_session_id: impl Into<String>,
    message_id: &str,
    notifications: impl IntoIterator<Item = HarnessNotification>,
    mut on_notification: F,
) -> Result<RunResult, SdkProtocolError>
where
    F: FnMut(&HarnessNotification),
{
    let mut collector = RunCollector::new(root_session_id, message_id);
    let mut tree = SessionParents::new();
    for notification in notifications {
        let before = collector.collected_len();
        let done = collector.push(&mut tree, notification)?;
        if collector.collected_len() > before {
            on_notification(collector.last_collected().expect("just collected"));
        }
        if done {
            return Ok(collector.finish());
        }
    }
    Err(SdkProtocolError::new(
        "notification stream ended before session idle",
    ))
}

/// Concatenated text of the last `assistant/message` in `events`.
///
/// Returns `""` when no assistant message exists. Non-text blocks are skipped.
pub fn final_response(events: &[Value]) -> String {
    for event in events.iter().rev() {
        if event.get("type").and_then(Value::as_str) != Some("assistant/message") {
            continue;
        }
        let Some(content) = event
            .pointer("/data/message/content")
            .and_then(Value::as_array)
        else {
            return String::new();
        };
        return content
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<String>();
    }
    String::new()
}

/// Last committed assistant text among raw `session.event` frames.
///
/// Session id, receipt, and idle are not applied. Empty text is `None`.
/// Prefer [`final_response`] for a `Session.run` interval.
pub fn final_assistant_text(notifications: &[Value]) -> Option<String> {
    let events: Vec<Value> = notifications
        .iter()
        .filter(|frame| frame.get("method").and_then(Value::as_str) == Some(methods::SESSION_EVENT))
        .map(|frame| frame["params"]["event"].clone())
        .collect();
    let text = final_response(&events);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// A string becomes one text block; blocks pass through.
pub enum RunInput {
    /// Prompt text sent as one `{ type: "text", text }` block.
    Text(String),
    /// Content blocks sent verbatim on `session/prompt`.
    Blocks(Vec<Value>),
}

impl From<&str> for RunInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for RunInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<Value>> for RunInput {
    fn from(value: Vec<Value>) -> Self {
        Self::Blocks(value)
    }
}

/// Normalize run input: a string becomes one text block; blocks pass through.
pub fn normalize_input(input: &RunInput) -> Vec<Value> {
    match input {
        RunInput::Text(text) => vec![json!({ "type": "text", "text": text })],
        RunInput::Blocks(blocks) => blocks.clone(),
    }
}

fn is_root_inbox_receipt(
    notification: &HarnessNotification,
    root_session_id: &str,
    message_id: &str,
) -> bool {
    notification.method == methods::SESSION_EVENT
        && notification.params.get("sessionId").and_then(Value::as_str) == Some(root_session_id)
        && is_inbox_receipt(&notification.params["event"], message_id)
}

fn is_root_idle(notification: &HarnessNotification, root_session_id: &str) -> bool {
    notification.method == methods::SESSION_STATUS
        && notification.params.get("sessionId").and_then(Value::as_str) == Some(root_session_id)
        && notification.params.get("status").and_then(Value::as_str) == Some("idle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::is_inbox_receipt;

    fn note(method: &str, params: Value) -> HarnessNotification {
        HarnessNotification::new(method, params)
    }

    fn receipt(session_id: &str, message_id: &str) -> HarnessNotification {
        note(
            "session.event",
            json!({
                "sessionId": session_id,
                "event": {
                    "type": "agent/inbox/spliced",
                    "seq": 0,
                    "time": 0,
                    "data": {
                        "target": "next-turn",
                        "start": 0,
                        "inserted": [{
                            "id": message_id,
                            "role": "user",
                            "content": [],
                            "source": { "kind": "user" }
                        }],
                    },
                },
            }),
        )
    }

    fn assistant(session_id: &str, text: &str) -> HarnessNotification {
        note(
            "session.event",
            json!({
                "sessionId": session_id,
                "event": {
                    "type": "assistant/message",
                    "data": { "message": { "content": [{ "type": "text", "text": text }] } },
                },
            }),
        )
    }

    fn idle(session_id: &str) -> HarnessNotification {
        note(
            "session.status",
            json!({ "sessionId": session_id, "status": "idle" }),
        )
    }

    #[test]
    fn ignores_notifications_before_the_inbox_receipt() {
        let result = collect_run(
            "owned",
            "accepted-message",
            [
                note(
                    "session.status",
                    json!({ "sessionId": "owned", "status": "running" }),
                ),
                note(
                    "session.event",
                    json!({ "sessionId": "owned", "event": { "type": "turn/start", "data": { "turn": 1 } } }),
                ),
                note(
                    "session.event",
                    json!({
                        "sessionId": "owned",
                        "event": { "type": "agent/inbox/spliced", "data": { "inserted": null } },
                    }),
                ),
                receipt("owned", "accepted-message"),
                idle("owned"),
            ],
        )
        .expect("collect");
        assert_eq!(
            result
                .notifications
                .iter()
                .map(|notification| notification.method.as_str())
                .collect::<Vec<_>>(),
            ["session.event", "session.status"]
        );
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event["type"].as_str())
                .collect::<Vec<_>>(),
            [Some("agent/inbox/spliced")]
        );
        assert_eq!(result.final_response, "");
    }

    #[test]
    fn events_stay_root_scoped_while_notifications_merge_descendants() {
        let mut seen = Vec::new();
        let result = collect_run_with(
            "parent-1",
            "m1",
            [
                receipt("parent-1", "m1"),
                note(
                    "subagent.started",
                    json!({ "parentSessionId": "parent-1", "childSessionId": "parent-1-child" }),
                ),
                assistant("parent-1-child", "child says hi"),
                note(
                    "session.status",
                    json!({ "sessionId": "parent-1-child", "status": "idle" }),
                ),
                note(
                    "subagent.finished",
                    json!({
                        "parentSessionId": "parent-1",
                        "childSessionId": "parent-1-child",
                    }),
                ),
                assistant("parent-1", "root says hi"),
                note(
                    "session.event",
                    json!({ "sessionId": "stranger", "event": { "type": "assistant/message" } }),
                ),
                idle("parent-1"),
            ],
            |notification| seen.push(notification.method.clone()),
        )
        .expect("collect");
        assert!(seen.contains(&"subagent.started".to_string()));
        assert!(seen.contains(&"subagent.finished".to_string()));
        assert_eq!(
            result
                .notifications
                .iter()
                .filter(|notification| {
                    notification.method == "session.event"
                        && notification.params["sessionId"] == "parent-1-child"
                })
                .count(),
            1
        );
        assert!(result.events.iter().all(|event| {
            event["type"] != "assistant/message"
                || event["data"]["message"]["content"][0]["text"] != "child says hi"
        }));
        assert_eq!(result.final_response, "root says hi");
        assert_eq!(result.session_id, "parent-1");
    }

    #[test]
    fn last_root_assistant_text_wins() {
        let result = collect_run(
            "root",
            "m",
            [
                receipt("root", "m"),
                assistant("root", "first"),
                note(
                    "session.event",
                    json!({
                        "sessionId": "root",
                        "event": {
                            "type": "assistant/message",
                            "data": { "message": { "content": [
                                { "type": "text", "text": "a" },
                                { "type": "tool-call" },
                                { "type": "text", "text": "b" },
                            ] } },
                        },
                    }),
                ),
                idle("root"),
            ],
        )
        .expect("collect");
        assert_eq!(result.final_response, "ab");
    }

    #[test]
    fn malformed_root_assistant_message_is_a_protocol_error() {
        let error = collect_run(
            "root",
            "m",
            [
                receipt("root", "m"),
                note(
                    "session.event",
                    json!({
                        "sessionId": "root",
                        "event": { "type": "assistant/message", "data": { "message": {} } },
                    }),
                ),
                idle("root"),
            ],
        )
        .expect_err("malformed");
        assert!(error
            .message
            .contains("assistant/message event carried malformed content"));
    }

    #[test]
    fn missing_event_envelope_is_a_protocol_error() {
        let error = collect_run(
            "root",
            "m",
            [
                receipt("root", "m"),
                note(
                    "session.event",
                    json!({ "sessionId": "root", "event": "nope" }),
                ),
                idle("root"),
            ],
        )
        .expect_err("malformed");
        assert!(error
            .message
            .contains("session.event carried no event envelope"));
    }

    #[test]
    fn assistant_message_without_data_is_a_protocol_error() {
        let error = collect_run(
            "root",
            "m",
            [
                receipt("root", "m"),
                note(
                    "session.event",
                    json!({ "sessionId": "root", "event": { "type": "assistant/message" } }),
                ),
            ],
        )
        .expect_err("malformed");
        assert!(error
            .message
            .contains("assistant/message event carried malformed content"));
    }

    #[test]
    fn parent_map_survives_across_intervals() {
        let mut tree = SessionParents::new();
        let mut first = RunCollector::new("root", "m1");
        for notification in [
            receipt("root", "m1"),
            note(
                "subagent.started",
                json!({ "parentSessionId": "root", "childSessionId": "child" }),
            ),
            idle("root"),
        ] {
            first.push(&mut tree, notification).unwrap();
        }
        let mut second = RunCollector::new("root", "m2");
        assert!(!second.push(&mut tree, receipt("root", "m2")).unwrap());
        assert!(!second
            .push(
                &mut tree,
                note(
                    "session.event",
                    json!({
                        "sessionId": "child",
                        "event": { "type": "turn/start", "data": { "turn": 2 } },
                    }),
                ),
            )
            .unwrap());
        assert_eq!(second.collected_len(), 2);
        assert!(second.push(&mut tree, idle("root")).unwrap());
    }

    #[test]
    fn stream_end_before_idle_is_a_protocol_error() {
        let error = collect_run("root", "m", [receipt("root", "m")]).expect_err("eof");
        assert_eq!(
            error.message,
            "notification stream ended before session idle"
        );
    }

    #[test]
    fn inbox_receipt_requires_inserted_id() {
        assert!(!is_inbox_receipt(
            &json!({ "type": "agent/inbox/spliced", "data": { "inserted": null } }),
            "m",
        ));
        assert!(is_inbox_receipt(
            &json!({
                "type": "agent/inbox/spliced",
                "data": { "inserted": [{ "id": "m" }] },
            }),
            "m",
        ));
    }

    #[test]
    fn normalize_input_wraps_strings_and_passes_blocks() {
        assert_eq!(
            normalize_input(&RunInput::from("x")),
            vec![json!({ "type": "text", "text": "x" })]
        );
        let blocks = vec![json!({ "type": "text", "text": "y" })];
        assert_eq!(normalize_input(&RunInput::from(blocks.clone())), blocks);
    }

    #[test]
    fn final_response_reads_the_last_assistant_message_and_tolerates_absence() {
        assert_eq!(final_response(&[]), "");
        assert_eq!(
            final_response(&[json!({ "type": "turn/start", "data": { "turn": 0 } })]),
            ""
        );
        assert_eq!(
            final_response(&[
                json!({
                    "type": "assistant/message",
                    "data": { "message": { "content": [{ "type": "text", "text": "first" }] } },
                }),
                json!({
                    "type": "assistant/message",
                    "data": { "message": { "content": [
                        { "type": "text", "text": "a" },
                        { "type": "tool-call" },
                        { "type": "text", "text": "b" },
                    ] } },
                }),
            ]),
            "ab"
        );
    }
}
