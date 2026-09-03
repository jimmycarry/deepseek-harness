use thiserror::Error;

/// Failures owned by the Cordis runtime.
#[derive(Debug, Error)]
pub enum CordisError {
    /// Plugin configuration failed validation.
    #[error("invalid config: {0}")]
    Validation(String),
    /// A required service is not provided on this context.
    #[error("missing service `{0}`")]
    MissingService(String),
    /// Effect registration while the owner is unloading.
    #[error("inactive effect: fiber is {0}")]
    InactiveEffect(String),
    /// Plugin apply or listener failed.
    #[error("{0}")]
    Plugin(String),
    /// A waterfall or serial listener failed.
    #[error("event `{event}`: {message}")]
    Event {
        /// Event name.
        event: String,
        /// Failure text.
        message: String,
    },
}

impl CordisError {
    /// Wrap an arbitrary apply failure.
    pub fn plugin(message: impl Into<String>) -> Self {
        Self::Plugin(message.into())
    }
}
