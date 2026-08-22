//! Token measurement (`ctx.tokenMeter`).
//!
//! Density is a construction-time Config field (`chars_per_token`). Structural
//! role and block overheads stay fixed protocol constants.

use dsh_cordis::Service;
use dsh_llm::{ContentBlock, Message};
use dsh_session::Session;

/// Per-block structural overhead for JSON framing and type tags.
const BLOCK_OVERHEAD: usize = 4;

/// Role-field framing overhead added to every priced message.
const ROLE_OVERHEAD: usize = 4;

/// `ctx.tokenMeter`.
pub struct TokenMeter {
    chars_per_token: usize,
}

impl TokenMeter {
    /// Build a meter whose text density is `chars_per_token` characters per token.
    pub fn new(chars_per_token: usize) -> Self {
        if chars_per_token == 0 {
            panic!("TokenMeter: chars_per_token must be a positive integer");
        }
        Self { chars_per_token }
    }

    /// Configured density used by every estimate.
    pub fn chars_per_token(&self) -> usize {
        self.chars_per_token
    }

    fn tokens_for(&self, len: usize) -> usize {
        len.div_ceil(self.chars_per_token)
    }

    /// Price one content list under the configured density.
    pub fn estimate_content(&self, blocks: &[ContentBlock]) -> usize {
        let mut tokens = 0;
        for block in blocks {
            match block {
                ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                    tokens += self.tokens_for(text.len()) + BLOCK_OVERHEAD;
                }
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => {
                    tokens += self.tokens_for(name.len())
                        + self.tokens_for(arguments.len())
                        + BLOCK_OVERHEAD;
                }
                ContentBlock::ToolResult { content, .. } => {
                    tokens += self.estimate_content(content) + BLOCK_OVERHEAD;
                }
            }
        }
        tokens
    }

    /// Price one model-visible message including role framing.
    pub fn estimate_message(&self, message: &Message) -> usize {
        let content = match message {
            Message::User(message) => &message.content,
            Message::Assistant(message) => &message.content,
            Message::Tool(message) => &message.content,
        };
        self.estimate_content(content) + ROLE_OVERHEAD
    }

    /// Price a message list in order.
    pub fn estimate_messages(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|message| self.estimate_message(message))
            .sum()
    }

    /// Price the current session surface through [`Session::derive_messages`].
    pub fn estimate_session(&self, session: &Session) -> usize {
        self.estimate_messages(&session.derive_messages())
    }
}

impl Service for TokenMeter {
    const KEY: &'static str = "tokenMeter";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_llm::UserMessage;
    use dsh_session::{session_id, Session, SessionEventData, SurfaceOp};
    use std::sync::Arc;

    #[test]
    fn provide_and_dispose() {
        let ctx = Context::new();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        assert!(ctx.has_service("tokenMeter"));
        ctx.dispose();
        assert!(!ctx.has_service("tokenMeter"));
    }

    #[test]
    fn estimate_messages_uses_chars_per_token() {
        let meter = TokenMeter::new(4);
        let messages = [Message::User(UserMessage::text("abcd"))];
        assert_eq!(
            meter.estimate_messages(&messages),
            1 + BLOCK_OVERHEAD + ROLE_OVERHEAD
        );
    }

    #[test]
    fn estimate_session_follows_surface() {
        let session = Session::new(session_id("m"));
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("abcd")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        let meter = TokenMeter::new(4);
        assert_eq!(
            meter.estimate_session(&session),
            meter.estimate_messages(&session.derive_messages())
        );
    }
}
