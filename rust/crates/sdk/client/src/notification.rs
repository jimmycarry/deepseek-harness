//! Wire notifications and client-side session-tree membership.
//!
//! The runtime notifies for every session in its context. Scoping is
//! client-side: [`record_session_relationship`] extends the parent map from
//! `subagent.started`, and [`in_session_tree`] filters one root plus those
//! descendants. Empty or self-loop edges never enter the map.

use crate::error::SdkProtocolError;
use dsh_sdk_protocol::methods;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// One server-to-client notification as received off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessNotification {
    /// JSON-RPC notification method name.
    pub method: String,
    /// Raw params object. Missing or non-object wire `params` become `{}`.
    pub params: Value,
}

impl HarnessNotification {
    /// Build a notification from a method name and params object.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params: if params.is_object() {
                params
            } else {
                json!({})
            },
        }
    }

    /// Parse a JSON-RPC notification frame (`method` required; `id` absent).
    ///
    /// Response frames (an `id` member) and frames without `method` return
    /// `None`.
    pub fn from_frame(frame: &Value) -> Option<Self> {
        if frame.get("id").is_some() {
            return None;
        }
        let method = frame.get("method")?.as_str()?.to_string();
        let params = match frame.get("params") {
            Some(value) if value.is_object() => value.clone(),
            _ => json!({}),
        };
        Some(Self { method, params })
    }
}

/// Child → parent edges discovered from `subagent.started`.
#[derive(Debug, Default, Clone)]
pub struct SessionParents {
    parents: HashMap<String, String>,
}

impl SessionParents {
    /// Empty parent map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a valid `subagent.started` edge, then report whether `notification`
    /// belongs to `root_session_id`'s tree.
    ///
    /// Recording runs first so the started edge that introduces a child is
    /// itself in-tree when the parent is already a descendant of the root.
    pub fn observe(&mut self, notification: &HarnessNotification, root_session_id: &str) -> bool {
        record_session_relationship(&mut self.parents, notification);
        in_session_tree(&self.parents, notification, root_session_id)
    }

    /// Walk `session_id` toward parents until `root_session_id` or a missing edge.
    pub fn is_descendant_of(&self, session_id: &str, root_session_id: &str) -> bool {
        is_descendant_of(&self.parents, session_id, root_session_id)
    }
}

/// Record `childSessionId → parentSessionId` from a `subagent.started` frame.
///
/// Both ids must be non-empty strings and must differ. Other methods are ignored.
pub fn record_session_relationship(
    parents: &mut HashMap<String, String>,
    notification: &HarnessNotification,
) {
    if notification.method != methods::SUBAGENT_STARTED {
        return;
    }
    let parent_id = string_field(&notification.params, "parentSessionId");
    let child_id = string_field(&notification.params, "childSessionId");
    if let (Some(parent_id), Some(child_id)) = (parent_id, child_id) {
        if !parent_id.is_empty() && !child_id.is_empty() && parent_id != child_id {
            parents.insert(child_id.to_string(), parent_id.to_string());
        }
    }
}

/// Whether `session_id` is `root_session_id` or a recorded descendant of it.
///
/// A cycle in the parent map stops the walk and returns `false`.
pub fn is_descendant_of(
    parents: &HashMap<String, String>,
    session_id: &str,
    root_session_id: &str,
) -> bool {
    let mut visited = HashSet::new();
    let mut current = session_id;
    while !visited.contains(current) {
        if current == root_session_id {
            return true;
        }
        visited.insert(current.to_string());
        match parents.get(current) {
            Some(parent) => current = parent.as_str(),
            None => return false,
        }
    }
    false
}

/// TypeScript `subscribeSessionTree` membership predicate.
///
/// `subagent.started` / `subagent.finished` match when their parent is in the
/// tree, or when `childSessionId` equals the root. Every other method matches
/// when `params.sessionId` is a descendant of the root.
pub fn in_session_tree(
    parents: &HashMap<String, String>,
    notification: &HarnessNotification,
    root_session_id: &str,
) -> bool {
    if notification.method == methods::SUBAGENT_STARTED
        || notification.method == methods::SUBAGENT_FINISHED
    {
        if let Some(parent_id) = string_field(&notification.params, "parentSessionId") {
            if is_descendant_of(parents, parent_id, root_session_id) {
                return true;
            }
        }
        return string_field(&notification.params, "childSessionId") == Some(root_session_id);
    }
    match string_field(&notification.params, "sessionId") {
        Some(related_id) => is_descendant_of(parents, related_id, root_session_id),
        None => false,
    }
}

/// Whether `value` is a JSON object (the wire-boundary probe).
pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// Require a JSON object with a string `type` (and kind-tagged content when
/// `type` is `assistant/message`).
pub fn validated_session_event(value: &Value) -> Result<Value, SdkProtocolError> {
    if !is_record(value) || value.get("type").and_then(Value::as_str).is_none() {
        return Err(SdkProtocolError::new(format!(
            "session.event carried no event envelope: {value}"
        )));
    }
    if value["type"] == "assistant/message" {
        let content = value
            .get("data")
            .and_then(|data| if is_record(data) { Some(data) } else { None })
            .and_then(|data| data.get("message"))
            .and_then(|message| {
                if is_record(message) {
                    Some(message)
                } else {
                    None
                }
            })
            .and_then(|message| message.get("content"));
        let valid = content
            .and_then(Value::as_array)
            .is_some_and(|blocks| blocks.iter().all(is_kind_tagged_block));
        if !valid {
            return Err(SdkProtocolError::new(format!(
                "assistant/message event carried malformed content: {value}"
            )));
        }
    }
    Ok(value.clone())
}

/// Whether a raw session event is the durable enqueue receipt for `message_id`.
pub fn is_inbox_receipt(value: &Value, message_id: &str) -> bool {
    if !is_record(value) || value.get("type") != Some(&json!("agent/inbox/spliced")) {
        return false;
    }
    let Some(data) = value.get("data").filter(|data| is_record(data)) else {
        return false;
    };
    let Some(inserted) = data.get("inserted").and_then(Value::as_array) else {
        return false;
    };
    inserted.iter().any(|message| {
        is_record(message) && message.get("id").and_then(Value::as_str) == Some(message_id)
    })
}

fn is_kind_tagged_block(block: &Value) -> bool {
    is_record(block) && block.get("type").and_then(Value::as_str).is_some()
}

fn string_field<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(method: &str, params: Value) -> HarnessNotification {
        HarnessNotification::new(method, params)
    }

    #[test]
    fn from_frame_rejects_responses_and_coerces_params() {
        assert_eq!(
            HarnessNotification::from_frame(&json!({"id": 1, "method": "initialize"})),
            None
        );
        assert_eq!(
            HarnessNotification::from_frame(&json!({"jsonrpc": "2.0"})),
            None
        );
        let parsed = HarnessNotification::from_frame(&json!({
            "method": "session.status",
            "params": ["not-an-object"],
        }))
        .expect("notification");
        assert_eq!(parsed.params, json!({}));
    }

    #[test]
    fn record_ignores_empty_and_self_loop_edges() {
        let mut parents = HashMap::new();
        record_session_relationship(
            &mut parents,
            &note(
                "subagent.started",
                json!({"parentSessionId": "loop", "childSessionId": "loop"}),
            ),
        );
        record_session_relationship(
            &mut parents,
            &note(
                "subagent.started",
                json!({"parentSessionId": "", "childSessionId": "x"}),
            ),
        );
        record_session_relationship(
            &mut parents,
            &note(
                "subagent.finished",
                json!({"parentSessionId": "root", "childSessionId": "child"}),
            ),
        );
        assert!(parents.is_empty());
    }

    #[test]
    fn descendant_walk_and_cycle() {
        let mut parents = HashMap::new();
        parents.insert("child".into(), "root".into());
        parents.insert("grandchild".into(), "child".into());
        assert!(is_descendant_of(&parents, "root", "root"));
        assert!(is_descendant_of(&parents, "grandchild", "root"));
        assert!(!is_descendant_of(&parents, "stranger", "root"));
        parents.insert("a".into(), "b".into());
        parents.insert("b".into(), "a".into());
        assert!(!is_descendant_of(&parents, "a", "root"));
    }

    #[test]
    fn session_tree_includes_multi_hop_and_root_child_finished() {
        let mut tree = SessionParents::new();
        assert!(tree.observe(
            &note(
                "subagent.started",
                json!({"parentSessionId": "root", "childSessionId": "child"}),
            ),
            "root",
        ));
        assert!(tree.observe(
            &note(
                "subagent.started",
                json!({"parentSessionId": "child", "childSessionId": "grandchild"}),
            ),
            "root",
        ));
        assert!(tree.observe(
            &note(
                "session.event",
                json!({"sessionId": "grandchild", "event": {"type": "noop"}}),
            ),
            "root",
        ));
        assert!(!tree.observe(
            &note(
                "session.event",
                json!({"sessionId": "stranger", "event": {"type": "noop"}}),
            ),
            "root",
        ));
        assert!(!tree.observe(
            &note(
                "subagent.started",
                json!({"parentSessionId": "other-root", "childSessionId": "other-child"}),
            ),
            "root",
        ));
        assert!(tree.observe(
            &note(
                "subagent.finished",
                json!({"parentSessionId": "child", "childSessionId": "grandchild"}),
            ),
            "root",
        ));
        assert!(tree.observe(
            &note("subagent.finished", json!({"childSessionId": "root"})),
            "root",
        ));
    }
}
