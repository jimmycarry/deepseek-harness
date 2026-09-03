//! Shared timeout arithmetic, signal fusion, and classification.
//!
//! The library only notifies through abort tokens; each capability still owns
//! the mechanism that stops its work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::time::sleep;

/// Largest delay this crate schedules without clamping.
pub const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// Internal abort reason carrying a capability-owned code and elapsed deadline.
#[derive(Debug, Error, Clone)]
#[error("{code} after {timeout_ms}ms")]
pub struct TimeoutReason {
    /// Capability-owned timeout code (e.g. `BASH_TIMEOUT`).
    pub code: String,
    /// The deadline that elapsed, in milliseconds.
    pub timeout_ms: u64,
}

impl TimeoutReason {
    /// Build a timeout reason the capability later translates.
    pub fn new(code: impl Into<String>, timeout_ms: u64) -> Self {
        Self {
            code: code.into(),
            timeout_ms,
        }
    }
}

/// Validate a caller's optional timeout hint, use the backend default, then cap it.
pub fn clamp_timeout(requested: Option<u64>, default: u64, max: u64, name: &str) -> Result<u64, String> {
    if let Some(requested) = requested {
        if requested == 0 {
            return Err(format!("{name} must be a positive finite number"));
        }
    }
    Ok(requested.unwrap_or(default).min(max).min(MAX_TIMER_DELAY_MS))
}

/// Deadline signal plus the cleanup that clears its timer.
#[derive(Clone)]
pub struct Deadline {
    cancelled: Arc<AtomicBool>,
    reason: Arc<std::sync::Mutex<Option<TimeoutReason>>>,
}

impl Deadline {
    /// Arm a deadline that fires after `timeout_ms` unless dropped or cleared.
    pub fn arm(code: impl Into<String>, timeout_ms: u64) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let reason = Arc::new(std::sync::Mutex::new(None));
        let code = code.into();
        let flag = Arc::clone(&cancelled);
        let slot = Arc::clone(&reason);
        tokio::spawn(async move {
            sleep(Duration::from_millis(timeout_ms)).await;
            if !flag.swap(true, Ordering::SeqCst) {
                *slot.lock().expect("deadline reason") = Some(TimeoutReason::new(code, timeout_ms));
            }
        });
        Self { cancelled, reason }
    }

    /// Whether the deadline or an upstream cancel has fired.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// The timeout reason when this deadline fired on its own timer.
    pub fn timeout_reason(&self) -> Option<TimeoutReason> {
        self.reason.lock().expect("deadline reason").clone()
    }

    /// Cancel without recording a timeout reason (upstream abort).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_rejects_zero() {
        assert!(clamp_timeout(Some(0), 1_000, 5_000, "timeoutMs").is_err());
    }

    #[test]
    fn clamp_uses_default_then_cap() {
        assert_eq!(clamp_timeout(None, 2_000, 1_500, "timeoutMs").unwrap(), 1_500);
        assert_eq!(clamp_timeout(Some(800), 2_000, 1_500, "timeoutMs").unwrap(), 800);
    }

    #[tokio::test]
    async fn deadline_fires_with_capability_code() {
        let deadline = Deadline::arm("BASH_TIMEOUT", 10);
        sleep(Duration::from_millis(30)).await;
        assert!(deadline.is_cancelled());
        let reason = deadline.timeout_reason().expect("timeout");
        assert_eq!(reason.code, "BASH_TIMEOUT");
        assert_eq!(reason.timeout_ms, 10);
    }
}
