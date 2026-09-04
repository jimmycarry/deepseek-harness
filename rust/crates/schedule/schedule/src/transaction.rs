//! Agent-scoped serialization for Schedule reads and durable mutations.

use dsh_agent::Agent;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

/// Per-agent FIFO tails.
#[derive(Clone, Default)]
pub struct ScheduleTransactions {
    tails: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl ScheduleTransactions {
    /// Run one complete Schedule transaction after its exact Agent's prior transaction.
    pub async fn run<T, F, Fut>(&self, agent: &dyn Agent, operation: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let lock = {
            let mut tails = self.tails.lock().expect("schedule tails");
            tails
                .entry(agent.id().as_str().to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        operation().await
    }
}
