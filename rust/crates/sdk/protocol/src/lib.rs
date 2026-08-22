//! JSON-RPC wire types for the DeepSeek Harness SDK runtime protocol: the
//! request/response frames, the three request methods, and the four
//! server-to-client notification payloads exchanged over the newline-delimited
//! stdio transport. `serverInfo.name` stays the wire-stable
//! `deepseek-harness-sdk-runtime`.

use dsh_llm::ContentBlock;
use dsh_session::SessionEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-stable SDK runtime server name returned by `initialize`.
pub const SERVER_NAME: &str = "deepseek-harness-sdk-runtime";
/// SDK runtime server version returned by `initialize`.
pub const SERVER_VERSION: &str = "0.0.1";

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Protocol version.
    pub jsonrpc: String,
    /// Request id.
    pub id: Value,
    /// Method name.
    pub method: String,
    /// Params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Protocol version.
    pub jsonrpc: String,
    /// Matching request id.
    pub id: Value,
    /// Result payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error payload (`{code, message}` plus optional `data`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// JSON-RPC 2.0 notification: a method frame without an id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Protocol version.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Params object; omitted params produce no `params` member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Build a request.
    pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcResponse {
    /// Successful result.
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response carrying the wire `code` and `message`.
    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(serde_json::json!({ "code": code, "message": message.into() })),
        }
    }
}

impl JsonRpcNotification {
    /// Build a notification.
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

/// Methods the Python/TS clients project.
pub mod methods {
    /// Handshake.
    pub const INITIALIZE: &str = "initialize";
    /// Deliver a user prompt.
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// Session event notification.
    pub const SESSION_EVENT: &str = "session.event";
    /// Agent status notification.
    pub const SESSION_STATUS: &str = "session.status";
    /// Child-session creation notification.
    pub const SUBAGENT_STARTED: &str = "subagent.started";
    /// In-process subagent run-end notification.
    pub const SUBAGENT_FINISHED: &str = "subagent.finished";
    /// Shutdown.
    pub const SHUTDOWN: &str = "shutdown";
}

/// Parameters for the process-wide SDK handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Working directory recorded on every SDK-created session's header.
    pub cwd: String,
    /// Provider route every SDK-created agent runs on.
    pub provider: String,
    /// Model name every SDK-created agent runs on.
    pub model: String,
    /// Optional positive output-token cap inherited by SDK-created agents.
    #[serde(rename = "maxTokens", default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

/// Wire-stable server identity returned by initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Wire-stable server identity ([`SERVER_NAME`]) and version.
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// Server name/version pair inside [`InitializeResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

/// One user turn on one SDK session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptParams {
    /// The SDK-side session id; an unknown id lazily creates the agent+session pair.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The prompt content blocks, sent verbatim as the user message.
    #[serde(rename = "contentBlocks")]
    pub content_blocks: Vec<ContentBlock>,
}

/// Durable enqueue receipt for one prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptResult {
    /// Identity of the queued user message.
    #[serde(rename = "messageId")]
    pub message_id: String,
}

/// `session.event` payload: one session-log event, streamed as it is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventNotification {
    /// Session the event belongs to.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The full session-log event envelope.
    pub event: SessionEvent,
}

/// `session.status` payload: whole-agent lifecycle state for one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatusNotification {
    /// Session whose live agent changed status.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The whole-agent state after the transition (`idle` or `running`).
    pub status: String,
}

/// `subagent.started` payload: an in-runtime child session was created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStartedNotification {
    /// The delegating session.
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: String,
    /// The new child session.
    #[serde(rename = "childSessionId")]
    pub child_session_id: String,
}

/// `subagent.finished` payload: an in-process subagent run ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentFinishedNotification {
    /// Subagent provider name that ran the child.
    pub provider: String,
    /// The child agent's id (equals `childSessionId` for local runs).
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// The delegating session.
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: String,
    /// The child session.
    #[serde(rename = "childSessionId")]
    pub child_session_id: String,
    /// Deployment-mapped run outcome (`ok` or `error`).
    pub status: String,
    /// The provider-reported stop reason.
    #[serde(rename = "stopReason")]
    pub stop_reason: String,
    /// The child's selected assistant output; absent when the child produced none.
    #[serde(
        rename = "lastAssistantMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_assistant_message: Option<Vec<ContentBlock>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = JsonRpcRequest::new(
            1,
            methods::SESSION_PROMPT,
            Some(serde_json::json!({"text":"hi"})),
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, methods::SESSION_PROMPT);
    }

    #[test]
    fn notification_omits_absent_params_and_id() {
        let wire =
            serde_json::to_string(&JsonRpcNotification::new("session.status", None)).unwrap();
        assert_eq!(wire, r#"{"jsonrpc":"2.0","method":"session.status"}"#);
        let with_params = JsonRpcNotification::new(
            methods::SESSION_STATUS,
            Some(serde_json::json!({"sessionId":"s","status":"idle"})),
        );
        let wire = serde_json::to_string(&with_params).unwrap();
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","method":"session.status","params":{"sessionId":"s","status":"idle"}}"#
        );
    }

    #[test]
    fn error_response_carries_code_and_message() {
        let response = JsonRpcResponse::error(serde_json::json!(7), -32603, "boom");
        let wire = serde_json::to_value(&response).unwrap();
        assert_eq!(wire["error"]["code"], -32603);
        assert_eq!(wire["error"]["message"], "boom");
        assert!(wire.get("result").is_none());
    }

    #[test]
    fn typed_payloads_use_wire_field_names() {
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "cwd": "/tmp",
            "provider": "deepseek-official",
            "model": "deepseek-v4-flash",
            "maxTokens": 128,
        }))
        .unwrap();
        assert_eq!(params.max_tokens, Some(128));
        let result = InitializeResult {
            server_info: ServerInfo {
                name: SERVER_NAME.into(),
                version: SERVER_VERSION.into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            r#"{"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"0.0.1"}}"#
        );
        let receipt = SessionPromptResult {
            message_id: "m".into(),
        };
        assert_eq!(
            serde_json::to_string(&receipt).unwrap(),
            r#"{"messageId":"m"}"#
        );
    }
}
