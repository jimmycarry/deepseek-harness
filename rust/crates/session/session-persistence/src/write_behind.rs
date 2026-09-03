//! Per-session write-behind batching.
//!
//! Ports `packages/session/session-persistence/src/write-behind.ts`. The first
//! pending event starts a fixed `maxDelayMs` window; later enqueue calls do
//! not reset it. Concurrent `flush` callers share one persist barrier. A
//! failed background write prepends the batch, pauses automatic flush, and
//! reports through the supplied callback.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dsh_session::SessionEvent;
use dsh_timeout::MAX_TIMER_DELAY_MS;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::PersistenceError;

/// Default `writeBatchMaxDelayMs` when the plugin Config omits the field.
pub const DEFAULT_WRITE_BATCH_MAX_DELAY_MS: u64 = 200;

/// Inclusive upper bound for `writeBatchMaxDelayMs` (`dsh-timeout` max).
pub const MAX_WRITE_BATCH_DELAY_MS: u64 = MAX_TIMER_DELAY_MS;

/// Persist one stable ordered prefix; resolves only after backend durability.
pub type WriteBatchFn = Arc<
    dyn Fn(Vec<SessionEvent>) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send>>
        + Send
        + Sync,
>;

/// Observe a detached background write failure without rejecting the producer.
pub type ReportBackgroundFailureFn = Arc<dyn Fn(PersistenceError) + Send + Sync>;

/// Validates `writeBatchMaxDelayMs` the same way TypeScript `SessionPersistence` does.
pub fn parse_write_batch_max_delay_ms(raw: Option<&serde_json::Value>) -> Result<u64, String> {
    match raw {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_WRITE_BATCH_MAX_DELAY_MS),
        Some(value) => {
            let Some(n) = value.as_u64() else {
                return Err(write_batch_delay_error());
            };
            if n < 1 || n > MAX_WRITE_BATCH_DELAY_MS {
                return Err(write_batch_delay_error());
            }
            Ok(n)
        }
    }
}

fn write_batch_delay_error() -> String {
    format!("writeBatchMaxDelayMs must be an integer between 1 and {MAX_WRITE_BATCH_DELAY_MS}")
}

struct Barrier {
    done: Notify,
    result: std::sync::Mutex<Option<Result<(), PersistenceError>>>,
}

impl Barrier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: Notify::new(),
            result: std::sync::Mutex::new(None),
        })
    }

    fn settle(&self, result: Result<(), PersistenceError>) {
        *self.result.lock().expect("write-behind barrier") = Some(result);
        self.done.notify_waiters();
    }

    async fn wait(&self) -> Result<(), PersistenceError> {
        loop {
            let notified = self.done.notified();
            if let Some(result) = self.result.lock().expect("write-behind barrier").clone() {
                return result;
            }
            notified.await;
        }
    }
}

struct ControllerState {
    pending: Vec<SessionEvent>,
    timer: Option<JoinHandle<()>>,
    active: Option<Arc<Notify>>,
    active_error: Option<PersistenceError>,
    barrier: Option<Arc<Barrier>>,
    deadline_expired: bool,
    automatic_paused: bool,
}

/// Owns one live session's pending events, fixed batching deadline, and flush barrier.
pub struct SessionWriteBehind {
    max_delay: Duration,
    write: WriteBatchFn,
    report_background_failure: ReportBackgroundFailureFn,
    state: std::sync::Mutex<ControllerState>,
    disposed: AtomicBool,
}

impl SessionWriteBehind {
    /// Build one controller around a durable batch sink.
    pub fn new(
        max_delay_ms: u64,
        write: WriteBatchFn,
        report_background_failure: ReportBackgroundFailureFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_delay: Duration::from_millis(max_delay_ms),
            write,
            report_background_failure,
            state: std::sync::Mutex::new(ControllerState {
                pending: Vec::new(),
                timer: None,
                active: None,
                active_error: None,
                barrier: None,
                deadline_expired: false,
                automatic_paused: false,
            }),
            disposed: AtomicBool::new(false),
        })
    }

    /// Whether this controller owns queued events or an active durable write.
    pub fn has_work(&self) -> bool {
        let state = self.state.lock().expect("write-behind");
        !state.pending.is_empty() || state.active.is_some()
    }

    /// Copy one event into the persistence-owned queue and start a fixed deadline
    /// when the automatic path is idle.
    pub fn enqueue(self: &Arc<Self>, event: SessionEvent) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock().expect("write-behind");
        let was_empty = state.pending.is_empty();
        state.pending.push(event);
        if state.barrier.is_some() {
            return;
        }
        if state.automatic_paused {
            state.automatic_paused = false;
            state.deadline_expired = false;
            drop(state);
            self.arm_timer();
        } else if was_empty {
            drop(state);
            self.arm_timer();
        }
    }

    /// Cancel the batching wait and durably drain through a quiescent point.
    pub async fn flush(self: &Arc<Self>) -> Result<(), PersistenceError> {
        if self.disposed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let existing = {
            let mut state = self.state.lock().expect("write-behind");
            if let Some(barrier) = state.barrier.clone() {
                Some(barrier)
            } else {
                self.cancel_timer_locked(&mut state);
                state.deadline_expired = false;
                state.automatic_paused = false;
                let barrier = Barrier::new();
                state.barrier = Some(Arc::clone(&barrier));
                None
            }
        };
        if let Some(barrier) = existing {
            return barrier.wait().await;
        }
        let this = Arc::clone(self);
        this.drain_barrier().await
    }

    /// Cancel the current automatic deadline without draining retained work.
    pub fn cancel_automatic_wait(&self) {
        let mut state = self.state.lock().expect("write-behind");
        self.cancel_timer_locked(&mut state);
        state.deadline_expired = false;
    }

    fn cancel_timer_locked(&self, state: &mut ControllerState) {
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
    }

    fn arm_timer(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let delay = self.max_delay;
        let handle = tokio::spawn(async move {
            sleep(delay).await;
            this.on_deadline();
        });
        let mut state = self.state.lock().expect("write-behind");
        if let Some(previous) = state.timer.replace(handle) {
            previous.abort();
        }
    }

    fn on_deadline(self: &Arc<Self>) {
        let start = {
            let mut state = self.state.lock().expect("write-behind");
            state.timer = None;
            if state.active.is_some() {
                state.deadline_expired = true;
                false
            } else {
                true
            }
        };
        if start {
            self.start_background();
        }
    }

    fn start_background(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match this.start_write(true).await {
                Ok(()) => this.continue_automatic(),
                Err(_) => {}
            }
        });
    }

    fn continue_automatic(self: &Arc<Self>) {
        let start = {
            let mut state = self.state.lock().expect("write-behind");
            if state.barrier.is_some() || state.pending.is_empty() {
                false
            } else if state.deadline_expired {
                state.deadline_expired = false;
                true
            } else {
                false
            }
        };
        if start {
            self.start_background();
        }
    }

    async fn drain_barrier(self: &Arc<Self>) -> Result<(), PersistenceError> {
        let overlapping = { self.state.lock().expect("write-behind").active.clone() };
        if let Some(active) = overlapping {
            active.notified().await;
            let mut state = self.state.lock().expect("write-behind");
            state.automatic_paused = false;
            if let Some(error) = state.active_error.take() {
                if let Some(barrier) = state.barrier.take() {
                    drop(state);
                    barrier.settle(Err(error.clone()));
                }
                return Err(error);
            }
        }
        loop {
            let pending_empty = self.state.lock().expect("write-behind").pending.is_empty();
            if pending_empty {
                break;
            }
            if let Err(error) = self.start_write(false).await {
                let mut state = self.state.lock().expect("write-behind");
                if let Some(barrier) = state.barrier.take() {
                    drop(state);
                    barrier.settle(Err(error.clone()));
                }
                return Err(error);
            }
        }
        let mut state = self.state.lock().expect("write-behind");
        if let Some(barrier) = state.barrier.take() {
            drop(state);
            barrier.settle(Ok(()));
        }
        Ok(())
    }

    async fn start_write(self: &Arc<Self>, background: bool) -> Result<(), PersistenceError> {
        let (batch, done) = {
            let mut state = self.state.lock().expect("write-behind");
            let batch = std::mem::take(&mut state.pending);
            self.cancel_timer_locked(&mut state);
            state.deadline_expired = false;
            let done = Arc::new(Notify::new());
            state.active = Some(Arc::clone(&done));
            state.active_error = None;
            (batch, done)
        };
        if batch.is_empty() {
            done.notify_waiters();
            self.state.lock().expect("write-behind").active = None;
            return Ok(());
        }
        let result = (self.write)(batch.clone()).await;
        {
            let mut state = self.state.lock().expect("write-behind");
            match &result {
                Ok(()) => {}
                Err(error) => {
                    let mut retained = batch;
                    retained.append(&mut state.pending);
                    state.pending = retained;
                    self.cancel_timer_locked(&mut state);
                    state.deadline_expired = false;
                    state.automatic_paused = true;
                    state.active_error = Some(error.clone());
                    if background {
                        (self.report_background_failure)(error.clone());
                    }
                }
            }
            state.active = None;
        }
        done.notify_waiters();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{SessionEvent, SessionEventData};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    fn ev(seq: u64) -> SessionEvent {
        SessionEvent {
            seq,
            time: seq,
            data: SessionEventData::TurnStart {
                turn: seq as u32 + 1,
            },
            source_event_seqs: None,
            surface_op: None,
            ignorable: false,
        }
    }

    fn recording(
        batches: Arc<Mutex<Vec<Vec<u64>>>>,
        delay_ms: u64,
        fail_times: Arc<AtomicUsize>,
    ) -> WriteBatchFn {
        Arc::new(move |events: Vec<SessionEvent>| {
            let batches = Arc::clone(&batches);
            let fail_times = Arc::clone(&fail_times);
            Box::pin(async move {
                if fail_times.load(Ordering::SeqCst) > 0 {
                    fail_times.fetch_sub(1, Ordering::SeqCst);
                    return Err(PersistenceError::Format("persist failed".into()));
                }
                if delay_ms > 0 {
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                batches
                    .lock()
                    .expect("batches")
                    .push(events.into_iter().map(|event| event.seq).collect());
                Ok(())
            })
        })
    }

    fn silent_report() -> ReportBackgroundFailureFn {
        Arc::new(|_| {})
    }

    #[tokio::test]
    async fn batches_writes_until_max_delay() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let wb = SessionWriteBehind::new(
            30,
            recording(Arc::clone(&batches), 0, Arc::new(AtomicUsize::new(0))),
            silent_report(),
        );
        wb.enqueue(ev(0));
        wb.enqueue(ev(1));
        wb.enqueue(ev(2));
        assert!(batches.lock().expect("batches").is_empty());
        sleep(Duration::from_millis(50)).await;
        assert_eq!(*batches.lock().expect("batches"), vec![vec![0, 1, 2]]);
        assert!(!wb.has_work());
    }

    #[tokio::test]
    async fn flush_writes_immediately() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let wb = SessionWriteBehind::new(
            10_000,
            recording(Arc::clone(&batches), 0, Arc::new(AtomicUsize::new(0))),
            silent_report(),
        );
        wb.enqueue(ev(0));
        wb.flush().await.expect("flush");
        assert_eq!(*batches.lock().expect("batches"), vec![vec![0]]);
    }

    #[tokio::test]
    async fn concurrent_flush_shares_barrier() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let wb = SessionWriteBehind::new(
            10_000,
            recording(Arc::clone(&batches), 20, Arc::new(AtomicUsize::new(0))),
            silent_report(),
        );
        wb.enqueue(ev(0));
        wb.enqueue(ev(1));
        let a = {
            let wb = Arc::clone(&wb);
            tokio::spawn(async move { wb.flush().await })
        };
        let b = {
            let wb = Arc::clone(&wb);
            tokio::spawn(async move { wb.flush().await })
        };
        a.await.unwrap().expect("a");
        b.await.unwrap().expect("b");
        assert_eq!(*batches.lock().expect("batches"), vec![vec![0, 1]]);
    }

    #[tokio::test]
    async fn failed_write_retains_events_and_pauses() {
        let batches = Arc::new(Mutex::new(Vec::new()));
        let wb = SessionWriteBehind::new(
            10,
            recording(Arc::clone(&batches), 0, Arc::new(AtomicUsize::new(1))),
            silent_report(),
        );
        wb.enqueue(ev(0));
        sleep(Duration::from_millis(30)).await;
        assert!(batches.lock().expect("batches").is_empty());
        wb.enqueue(ev(1));
        sleep(Duration::from_millis(30)).await;
        assert_eq!(*batches.lock().expect("batches"), vec![vec![0, 1]]);
    }

    #[test]
    fn write_batch_delay_defaults_and_rejects() {
        assert_eq!(parse_write_batch_max_delay_ms(None).unwrap(), 200);
        assert!(parse_write_batch_max_delay_ms(Some(&serde_json::json!(0))).is_err());
        assert!(parse_write_batch_max_delay_ms(Some(&serde_json::json!(
            MAX_WRITE_BATCH_DELAY_MS + 1
        )))
        .is_err());
        assert_eq!(
            parse_write_batch_max_delay_ms(Some(&serde_json::json!(50))).unwrap(),
            50
        );
    }
}
