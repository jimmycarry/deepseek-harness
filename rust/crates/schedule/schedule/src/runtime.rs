//! Disposable live timer projection for one exact root agent.

use crate::domain::{
    fold_schedule_events, render_every_reminder_batch_framing, render_reminder_framing,
    resolve_every_occurrence, EveryScheduleRecord, FoldedSchedules, ScheduleChange,
    ScheduleLogError, ScheduleRecord,
};
use crate::persistence::flush_schedule_persistence;
use crate::transaction::ScheduleTransactions;
use dsh_agent::Agent;
use dsh_cordis::Context;
use dsh_llm::{ContentBlock, MessageSource, UserMessage};
use dsh_session::{SessionEventData, SessionHeader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Largest delay that Node timers represent without clamping.
pub const MAX_TIMER_DELAY_MS: i64 = 2_147_483_647;

/// One live root plus its projection.
#[derive(Clone)]
pub struct Owner {
    /// Exact live root agent.
    pub agent: Arc<dyn Agent>,
    /// Process-local timer projection.
    pub runtime: Arc<ScheduleRuntime>,
}

enum DueDecision {
    OneShot(ScheduleRecord),
    Every {
        reminders: Vec<(EveryScheduleRecord, String)>,
        accepted_at: String,
    },
    Wait(Option<i64>),
}

fn due_decision(folded: &FoldedSchedules, now: i64) -> Result<DueDecision, ScheduleLogError> {
    let indexed: Vec<(usize, &ScheduleRecord)> = folded.active.iter().enumerate().collect();
    let mut one_shots: Vec<(usize, &ScheduleRecord)> = indexed
        .iter()
        .copied()
        .filter(|(_, record)| {
            !matches!(record, ScheduleRecord::Every(_)) && parse_ms(record.scheduled_at()) <= now
        })
        .collect();
    one_shots.sort_by(|left, right| {
        parse_ms(left.1.scheduled_at())
            .cmp(&parse_ms(right.1.scheduled_at()))
            .then(left.0.cmp(&right.0))
    });
    if let Some((_, record)) = one_shots.first() {
        return Ok(DueDecision::OneShot((*record).clone()));
    }

    let mut every: Vec<(usize, &EveryScheduleRecord)> = indexed
        .iter()
        .filter_map(|(index, record)| match record {
            ScheduleRecord::Every(every) if parse_ms(&every.scheduled_at) <= now => {
                Some((*index, every))
            }
            _ => None,
        })
        .collect();
    every.sort_by(|left, right| {
        parse_ms(&left.1.scheduled_at)
            .cmp(&parse_ms(&right.1.scheduled_at))
            .then(left.0.cmp(&right.0))
    });
    if !every.is_empty() {
        let accepted_at = format_ms(now);
        let reminders = every
            .into_iter()
            .map(|(_, record)| {
                let occurrence = resolve_every_occurrence(record, now)?;
                Ok((record.clone(), occurrence.occurrence_at))
            })
            .collect::<Result<Vec<_>, ScheduleLogError>>()?;
        return Ok(DueDecision::Every {
            reminders,
            accepted_at,
        });
    }

    let target = folded.active.iter().fold(None, |selected, record| {
        let candidate = parse_ms(record.scheduled_at());
        if candidate > now && selected.map(|value| candidate < value).unwrap_or(true) {
            Some(candidate)
        } else {
            selected
        }
    });
    Ok(DueDecision::Wait(target))
}

fn parse_ms(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(i64::MAX)
}

fn format_ms(epoch: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

fn render_thrown(error: &dyn std::fmt::Display) -> String {
    error.to_string()
}

fn seed_length(header: &SessionHeader) -> i64 {
    header.seed_length.unwrap_or(0) as i64
}

/// One process-local, disposable projection of an exact agent's durable schedules.
pub struct ScheduleRuntime {
    ctx: Context,
    agent: Arc<dyn Agent>,
    transactions: ScheduleTransactions,
    stop: Notify,
    timer: Mutex<Option<JoinHandle<()>>>,
    requested: AtomicBool,
    stopping: AtomicBool,
    faulted: AtomicBool,
    running: Mutex<Option<JoinHandle<()>>>,
}

impl ScheduleRuntime {
    /// Construct an inactive runtime; [`Self::start`] begins the first preflight.
    pub fn new(ctx: Context, agent: Arc<dyn Agent>) -> Self {
        Self {
            ctx,
            agent,
            transactions: ScheduleTransactions::default(),
            stop: Notify::new(),
            timer: Mutex::new(None),
            requested: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            running: Mutex::new(None),
        }
    }

    /// Shared transaction queue used by the management tools.
    pub fn transactions(&self) -> ScheduleTransactions {
        self.transactions.clone()
    }

    /// Begin the initial durability preflight and timer derivation.
    pub fn start(self: &Arc<Self>) {
        self.request_drive();
    }

    /// Recompute the live projection after a committed mutation or idle transition.
    pub fn request_drive(self: &Arc<Self>) {
        if self.stopping.load(Ordering::SeqCst) || self.faulted.load(Ordering::SeqCst) {
            return;
        }
        self.clear_timer();
        self.requested.store(true, Ordering::SeqCst);
        let mut running = self.running.lock().expect("runtime");
        if running.is_some() {
            return;
        }
        let this = Arc::clone(self);
        *running = Some(tokio::spawn(async move {
            this.run_requested().await;
        }));
    }

    /// Stop future work, cancel timers, and await every outstanding runtime promise.
    pub async fn dispose(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.requested.store(false, Ordering::SeqCst);
        self.clear_timer();
        self.stop.notify_waiters();
        let run = self.running.lock().expect("runtime").take();
        if let Some(run) = run {
            let _ = run.await;
        }
    }

    async fn run_requested(self: Arc<Self>) {
        while self.requested.swap(false, Ordering::SeqCst)
            && !self.stopping.load(Ordering::SeqCst)
            && !self.faulted.load(Ordering::SeqCst)
        {
            let this = Arc::clone(&self);
            let agent = Arc::clone(&this.agent);
            this.transactions
                .run(agent.as_ref(), {
                    let this = Arc::clone(&this);
                    move || this.drive_once()
                })
                .await;
        }
        *self.running.lock().expect("runtime") = None;
        if self.requested.load(Ordering::SeqCst)
            && !self.stopping.load(Ordering::SeqCst)
            && !self.faulted.load(Ordering::SeqCst)
        {
            self.request_drive();
        }
    }

    fn is_live(&self) -> bool {
        self.ctx
            .get::<dsh_agent::AgentRegistry>()
            .and_then(|agents| agents.get(self.agent.id()))
            .is_some_and(|live| {
                live.id().as_str() == self.agent.id().as_str()
                    && agents_contains_root(&self.ctx, self.agent.as_ref())
            })
    }

    fn is_runnable(&self) -> bool {
        !self.stopping.load(Ordering::SeqCst) && self.is_live()
    }

    fn clear_timer(&self) {
        if let Some(timer) = self.timer.lock().expect("timer").take() {
            timer.abort();
        }
    }

    fn arm(self: &Arc<Self>, target: i64, now: i64) {
        let delay = (target - now).min(MAX_TIMER_DELAY_MS).max(0) as u64;
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            this.request_drive();
        });
        *self.timer.lock().expect("timer") = Some(handle);
    }

    fn read_folded(&self) -> Option<FoldedSchedules> {
        match fold_schedule_events(
            &self.agent.session().events(),
            seed_length(self.agent.session().header()),
        ) {
            Ok(folded) => Some(folded),
            Err(error) => {
                self.faulted.store(true, Ordering::SeqCst);
                if self.is_live() {
                    tracing::warn!(
                        "schedule: corrupt schedule log for agent \"{}\": {}",
                        self.agent.id().as_str(),
                        error
                    );
                }
                None
            }
        }
    }

    async fn drive_once(self: Arc<Self>) {
        self.clear_timer();
        if !self.is_runnable() {
            return;
        }
        if let Err(error) =
            flush_schedule_persistence(&self.ctx, self.agent.session().as_ref()).await
        {
            if self.is_live() {
                tracing::warn!(
                    "schedule: preflight failed for agent \"{}\": {}",
                    self.agent.id().as_str(),
                    render_thrown(&error)
                );
            }
            return;
        }
        if !self.is_runnable() {
            return;
        }
        let Some(folded) = self.read_folded() else {
            return;
        };
        let wake_now = chrono::Utc::now().timestamp_millis();
        let wake_decision = match due_decision(&folded, wake_now) {
            Ok(decision) => decision,
            Err(error) => {
                tracing::warn!(
                    "schedule: fixed-rate decision failed for agent \"{}\": {}",
                    self.agent.id().as_str(),
                    error
                );
                return;
            }
        };
        if let DueDecision::Wait(target) = wake_decision {
            if let Some(target) = target {
                self.arm(target, wake_now);
            }
            return;
        }

        if self.agent.status() != dsh_agent::AgentStatus::Idle {
            return;
        }

        let text = match &wake_decision {
            DueDecision::OneShot(record) => render_reminder_framing(record),
            DueDecision::Every { reminders, .. } => render_every_reminder_batch_framing(reminders),
            DueDecision::Wait(_) => return,
        };
        let message = UserMessage::from_parts(
            vec![ContentBlock::text(text)],
            MessageSource::plugin("schedule"),
        );
        self.agent.followup(message);

        let appends: Result<(), String> = (|| {
            match wake_decision {
                DueDecision::OneShot(record) => {
                    self.agent
                        .session()
                        .append(
                            SessionEventData::Extension {
                                type_name: "schedule/change".into(),
                                data: ScheduleChange::Dispatch {
                                    id: record.id().to_string(),
                                    accepted_at: None,
                                }
                                .to_json(),
                            },
                            None,
                        )
                        .map_err(|error| error.to_string())?;
                }
                DueDecision::Every {
                    reminders,
                    accepted_at,
                } => {
                    for (record, _) in reminders {
                        self.agent
                            .session()
                            .append(
                                SessionEventData::Extension {
                                    type_name: "schedule/change".into(),
                                    data: ScheduleChange::Dispatch {
                                        id: record.id,
                                        accepted_at: Some(accepted_at.clone()),
                                    }
                                    .to_json(),
                                },
                                None,
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
                DueDecision::Wait(_) => {}
            }
            Ok(())
        })();
        if let Err(error) = appends {
            self.faulted.store(true, Ordering::SeqCst);
            self.clear_timer();
            tracing::warn!(
                "schedule: dispatch append failed for agent \"{}\": {error}",
                self.agent.id().as_str()
            );
            return;
        }
        if let Err(error) =
            flush_schedule_persistence(&self.ctx, self.agent.session().as_ref()).await
        {
            if self.is_live() {
                tracing::warn!(
                    "schedule: dispatch barrier failed for agent \"{}\": {}",
                    self.agent.id().as_str(),
                    render_thrown(&error)
                );
            }
            return;
        }
        if self.is_runnable() {
            self.request_drive();
        }
    }
}

fn agents_contains_root(ctx: &Context, agent: &dyn Agent) -> bool {
    ctx.get::<dsh_agent::AgentRegistry>()
        .map(|agents| {
            agents
                .roots()
                .iter()
                .any(|root| root.id().as_str() == agent.id().as_str())
        })
        .unwrap_or(false)
}
