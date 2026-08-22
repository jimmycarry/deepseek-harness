//! Cordis plugin runtime.
//!
//! Plugins contribute services, typed events, and reversible effects to a
//! shared [`Context`]. Registrations unwind when their owning fiber unloads.

mod context;
mod error;
mod events;
mod fiber;
mod plugin;
mod service;

pub use context::Context;
pub use error::CordisError;
pub use events::{DispatchMode, EventPayload, Next, WaterfallDecision};
pub use fiber::{Fiber, FiberHandle, FiberState};
pub use plugin::{FnPlugin, Plugin};
pub use service::{Service, ServiceExt};

/// Result alias for Cordis operations.
pub type Result<T, E = CordisError> = std::result::Result<T, E>;
