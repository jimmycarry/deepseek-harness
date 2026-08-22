//! Projection seam (`ctx.sessionProjection`).

use dsh_cordis::Service;
use dsh_llm::Message;
use dsh_session::Session;

/// `ctx.sessionProjection`.
#[derive(Default)]
pub struct SessionProjection;

impl SessionProjection {
    /// Create the service.
    pub fn new() -> Self {
        Self
    }

    /// Project model history from the current surface.
    pub fn project(&self, session: &Session) -> Vec<Message> {
        session.derive_messages()
    }
}

impl Service for SessionProjection {
    const KEY: &'static str = "sessionProjection";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_llm::UserMessage;
    use dsh_session::{session_id, SessionEventData, SurfaceOp};
    use std::sync::Arc;

    #[test]
    fn provide_and_project() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionProjection::new())).unwrap();
        assert!(ctx.has_service("sessionProjection"));
        let session = Session::new(session_id("s"));
        session
            .append(
                SessionEventData::UserMessage(UserMessage::text("hi")),
                Some(SurfaceOp::append()),
            )
            .unwrap();
        let messages = SessionProjection::new().project(&session);
        assert_eq!(messages.len(), 1);
        ctx.dispose();
        assert!(!ctx.has_service("sessionProjection"));
    }
}
