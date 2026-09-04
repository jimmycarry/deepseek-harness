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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{create_after_schedule_record, ScheduleChange, ScheduleRecord};
    use async_trait::async_trait;
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentStatus, Inbox, InboxTarget,
    };
    use dsh_llm::{ContentBlock, UserMessage};
    use dsh_session::{
        session_id, Session, SessionEvent, SessionEventData, SessionHeader, SessionId, SessionStore,
    };
    use dsh_session_persistence::{
        PersistenceError, PersistenceRuntime, SessionInspection, SessionStoreBackend,
    };
    use dsh_tools::ToolRuntime;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct MemoryBackend {
        sessions: Mutex<HashMap<String, (SessionHeader, Vec<SessionEvent>)>>,
    }

    impl MemoryBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sessions: Mutex::new(HashMap::new()),
            })
        }
    }

    #[async_trait]
    impl SessionStoreBackend for MemoryBackend {
        async fn save(&self, session: &Session) -> std::result::Result<(), PersistenceError> {
            self.sessions.lock().expect("memory").insert(
                session.id().as_str().to_string(),
                (session.header().clone(), session.events()),
            );
            Ok(())
        }

        async fn load(&self, id: &SessionId) -> std::result::Result<Session, PersistenceError> {
            self.inspect(id).await?.into_session()
        }

        async fn inspect(
            &self,
            id: &SessionId,
        ) -> std::result::Result<SessionInspection, PersistenceError> {
            let guard = self.sessions.lock().expect("memory");
            let (header, events) = guard
                .get(id.as_str())
                .cloned()
                .ok_or_else(|| PersistenceError::NotFound(id.as_str().to_string()))?;
            Ok(SessionInspection {
                meta: header,
                events,
            })
        }

        async fn list_ids(&self) -> std::result::Result<Vec<SessionId>, PersistenceError> {
            Ok(self
                .sessions
                .lock()
                .expect("memory")
                .keys()
                .cloned()
                .map(session_id)
                .collect())
        }
    }

    struct RecordingAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
        followed: Arc<Mutex<Vec<UserMessage>>>,
    }

    #[async_trait]
    impl Agent for RecordingAgent {
        fn id(&self) -> &SessionId {
            self.session.id()
        }
        fn session(&self) -> Arc<Session> {
            Arc::clone(&self.session)
        }
        fn inbox(&self) -> Arc<Inbox> {
            Arc::clone(&self.inbox)
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn send(&self, message: UserMessage, target: InboxTarget, wakeup: bool) {
            if target == InboxTarget::NextTurn && wakeup {
                self.followed.lock().expect("followed").push(message);
            }
        }
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> std::result::Result<(), AgentError> {
            Ok(())
        }
    }

    struct RecordingFactory {
        ctx: Context,
        followed: Arc<Mutex<Vec<UserMessage>>>,
    }

    impl AgentFactory for RecordingFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(RecordingAgent {
                inbox: Arc::new(Inbox::for_session(Arc::clone(&session))),
                session,
                followed: Arc::clone(&self.followed),
            })
        }

        fn announce_start(&self, agent: &dyn Agent, source: &str) {
            self.ctx.emit(
                "agent/session-start",
                json!({
                    "agentId": agent.id().as_str(),
                    "sessionId": agent.session().id().as_str(),
                    "source": source,
                }),
            );
        }
    }

    fn outcome_json(outcome: &dsh_tools::ToolOutcome) -> Value {
        let text = outcome
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        serde_json::from_str(text).unwrap()
    }

    fn host() -> (
        Context,
        Arc<SessionStore>,
        Arc<ToolRuntime>,
        Arc<Mutex<Vec<UserMessage>>>,
    ) {
        let ctx = Context::new();
        let store = Arc::new(SessionStore::new());
        ctx.provide(Arc::clone(&store)).unwrap();
        let agents = AgentRegistry::new();
        let followed = Arc::new(Mutex::new(Vec::new()));
        agents.set_factory(Arc::new(RecordingFactory {
            ctx: ctx.clone(),
            followed: Arc::clone(&followed),
        }));
        ctx.provide(Arc::new(agents)).unwrap();
        let tools = Arc::new(ToolRuntime::new());
        ctx.provide(Arc::clone(&tools)).unwrap();
        ctx.provide(Arc::new(PersistenceRuntime::new(MemoryBackend::new())))
            .unwrap();
        install(&ctx, None).unwrap();
        (ctx, store, tools, followed)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_create_list_and_delete_for_a_live_root() {
        let (ctx, store, tools, _followed) = host();
        let session = store.create(session_id("root"));
        let _handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        assert!(tools
            .get("schedule_create")
            .unwrap()
            .enabled_for(Some("root")));
        assert!(!tools.get("schedule_create").unwrap().enabled_for(None));

        let created = tools
            .execute_for(
                &ctx,
                "schedule_create",
                json!({ "prompt": "check logs", "after_seconds": 30 }),
                Some("root"),
            )
            .await
            .unwrap();
        let created = outcome_json(&created.outcome);
        assert_eq!(created["id"], "schedule-1");
        assert_eq!(created["state"], "scheduled");
        assert_eq!(created["kind"], "after");

        let listed = tools
            .execute_for(&ctx, "schedule_list", json!({}), Some("root"))
            .await
            .unwrap();
        let listed = outcome_json(&listed.outcome);
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["id"], "schedule-1");

        let deleted = tools
            .execute_for(
                &ctx,
                "schedule_delete",
                json!({ "id": "schedule-1" }),
                Some("root"),
            )
            .await
            .unwrap();
        let deleted = outcome_json(&deleted.outcome);
        assert_eq!(deleted["deleted"], true);
        assert_eq!(deleted["id"], "schedule-1");

        let listed = tools
            .execute_for(&ctx, "schedule_list", json!({}), Some("root"))
            .await
            .unwrap();
        assert_eq!(outcome_json(&listed.outcome), json!([]));
        ctx.dispose();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overdue_one_shot_followup_then_dispatch() {
        let (ctx, store, _tools, followed) = host();
        let session = store.create(session_id("root"));
        let now = chrono::Utc::now().timestamp_millis() as f64;
        let record =
            create_after_schedule_record("schedule-1", "check logs", 1.0, now - 10_000.0).unwrap();
        session
            .append(
                SessionEventData::Extension {
                    type_name: "schedule/change".into(),
                    data: ScheduleChange::Create {
                        schedule: ScheduleRecord::After(record),
                    }
                    .to_json(),
                },
                None,
            )
            .unwrap();
        let _handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !followed.lock().expect("followed").is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("due reminder followup");
        let text = followed.lock().expect("followed")[0]
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(text.contains("[SCHEDULE REMINDER]"));
        assert!(text.contains("check logs"));
        assert!(session.events().iter().any(|event| {
            matches!(
                &event.data,
                SessionEventData::Extension { type_name, data }
                    if type_name == "schedule/change"
                        && data.get("operation").and_then(Value::as_str) == Some("dispatch")
            )
        }));
        ctx.dispose();
    }
}
