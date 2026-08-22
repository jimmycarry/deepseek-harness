//! JSON-RPC protocol types shared by the TS and Python SDKs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Error payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
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
    /// Shutdown.
    pub const SHUTDOWN: &str = "shutdown";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = JsonRpcRequest::new(1, methods::SESSION_PROMPT, Some(serde_json::json!({"text":"hi"})));
        let json = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, methods::SESSION_PROMPT);
    }
}
