//! Quiescence tracking for emit-shaped hook runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

/// In-flight registry for one bridge's detached hook runs.
#[derive(Clone)]
pub struct DetachedRuns {
    inflight: Arc<Mutex<Vec<JoinHandle<()>>>>,
    aborted: Arc<AtomicBool>,
}

impl DetachedRuns {
    /// Whether [`Self::drain`] has fired the shared abort.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Register one detached run until it settles.
    pub fn track<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = spawn_detached(fut);
        self.inflight.lock().expect("detached").push(handle);
        self.prune_finished();
    }

    /// Abort the shared signal, then wait until every tracked chain has settled.
    pub async fn drain(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        loop {
            let pending: Vec<JoinHandle<()>> = {
                let mut inflight = self.inflight.lock().expect("detached");
                inflight.drain(..).collect()
            };
            if pending.is_empty() {
                return;
            }
            futures::future::join_all(pending).await;
        }
    }

    fn prune_finished(&self) {
        let mut inflight = self.inflight.lock().expect("detached");
        inflight.retain(|handle| !handle.is_finished());
    }
}

/// Create a [`DetachedRuns`] tracker (one per bridge `apply()`).
pub fn create_detached_runs() -> DetachedRuns {
    DetachedRuns {
        inflight: Arc::new(Mutex::new(Vec::new())),
        aborted: Arc::new(AtomicBool::new(false)),
    }
}

fn spawn_detached<F>(fut: F) -> JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut)
    } else {
        tokio::spawn(fut)
    }
}
