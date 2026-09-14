//! Protocol and transport failures for the SDK client.

use thiserror::Error;

/// The runtime answered outside its documented protocol (for example a
/// `session/prompt` response without `messageId`, or a malformed
/// `session.event` envelope).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SdkProtocolError {
    /// Protocol violation description.
    pub message: String,
}

impl SdkProtocolError {
    /// Build a protocol error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Failures from handshake, prompt, collection, or a closed harness.
#[derive(Debug, Error)]
pub enum SdkError {
    /// A response or notification violated the documented protocol.
    #[error("{0}")]
    Protocol(#[from] SdkProtocolError),
    /// Stdio or child-process I/O failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The harness or runtime is closed and will not retry.
    #[error("{0}")]
    Closed(String),
}

impl SdkError {
    /// Closed-harness failure with the TypeScript client sentence.
    pub fn closed() -> Self {
        Self::Closed("DeepSeek Harness runtime client is closed".into())
    }
}
