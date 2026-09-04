//! Agent-scoped durable one-shot and fixed-rate reminders over the session log.

mod domain;
mod persistence;
mod runtime;
mod tools;
mod transaction;

pub use domain::{
    allocate_schedule_id, canonicalize_time_zone, create_after_schedule_record,
    create_at_schedule_record, create_every_schedule_record, decode_schedule_change,
    fold_schedule_events, render_every_reminder_batch_framing, render_reminder_framing,
    resolve_every_occurrence, schedule_view, AfterScheduleRecord, AtScheduleRecord,
    EveryOccurrence, EveryScheduleRecord, FoldedSchedules, ScheduleChange, ScheduleInputError,
    ScheduleLogError, ScheduleRecord, ScheduleView, MIN_EVERY_INTERVAL_SECONDS,
    SCHEDULE_CHANGE_VERSION,
};
pub use runtime::{ScheduleRuntime, MAX_TIMER_DELAY_MS};
pub use tools::register_schedule_tools;

use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result};
use dsh_session::session_id;
use dsh_session_persistence::PersistenceRuntime;
use dsh_tools::ToolRuntime;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Plugin role name matching TypeScript `export const name`.
pub fn name() -> &'static str {
    "schedule"
}

/// Install Schedule for root agents published after this plugin loads.
///
/// # Errors
/// Required services (`agents`, `sessions`, `tools`, `sessionPersistence`) are missing.
pub fn install(ctx: &Context, _config: Option<&Value>) -> Result<()> {
    let _ = ctx.service::<AgentRegistry>()?;
    let _ = ctx.service::<dsh_session::SessionStore>()?;
    let _ = ctx.service::<ToolRuntime>()?;
    let _ = ctx.service::<PersistenceRuntime>()?;

    let owners: Arc<Mutex<HashMap<String, runtime::Owner>>> = Arc::new(Mutex::new(HashMap::new()));
    let stopping = Arc::new(Mutex::new(false));
    tools::register_shared_tools(ctx, Arc::clone(&owners))?;

    let lookup = ctx.clone();
    let owners_created = Arc::clone(&owners);
    let stopping_created = Arc::clone(&stopping);
    ctx.on("agent/session-start", move |payload| {
        if *stopping_created.lock().expect("stopping") {
            return;
        }
        let Some(id) = payload.get("agentId").and_then(Value::as_str) else {
            return;
        };
        let Some(agents) = lookup.get::<AgentRegistry>() else {
            return;
        };
        let Some(agent) = agents.get(&session_id(id)) else {
            return;
        };
        if !agents
            .roots()
            .iter()
            .any(|root| root.id().as_str() == agent.id().as_str())
        {
            return;
        }
        let mut map = owners_created.lock().expect("owners");
        if map.contains_key(id) {
            return;
        }
        let runtime = Arc::new(ScheduleRuntime::new(lookup.clone(), Arc::clone(&agent)));
        runtime.start();
        map.insert(id.to_string(), runtime::Owner { agent, runtime });
    })?;

    let owners_status = Arc::clone(&owners);
    ctx.on("agent/status", move |payload| {
        if payload.get("status").and_then(Value::as_str) != Some("idle") {
            return;
        }
        let Some(id) = payload.get("agentId").and_then(Value::as_str) else {
            return;
        };
        let Some(owner) = owners_status.lock().expect("owners").get(id).cloned() else {
            return;
        };
        if owner
            .agent
            .session()
            .events()
            .iter()
            .any(|event| dsh_session::event_type_name(&event.data) == "schedule/change")
        {
            owner.runtime.request_drive();
        }
    })?;

    let owners_dispose = Arc::clone(&owners);
    let stopping_dispose = Arc::clone(&stopping);
    ctx.effect("schedule.lifecycle()", move || {
        move || {
            *stopping_dispose.lock().expect("stopping") = true;
            let runtimes: Vec<Arc<ScheduleRuntime>> = owners_dispose
                .lock()
                .expect("owners")
                .drain()
                .map(|(_, owner)| owner.runtime)
                .collect();
            for runtime in runtimes {
                let _ = futures::executor::block_on(runtime.dispose());
            }
        }
    })?;
    Ok(())
}
