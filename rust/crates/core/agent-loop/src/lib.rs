//! Default driver implementing the public `Agent` contract (`ctx.agentLoop`).

use async_trait::async_trait;
use dsh_agent::{
    Agent, AgentCancelCause, AgentError, AgentFactory, AgentRegistry, AgentStatus, Inbox,
    InboxEntry, InboxTarget,
};
use dsh_cordis::{Context, Service};
use dsh_llm::{
    call_id, AssistantMessage, BlockAssembler, LlmRequest, LlmRuntime, ToolResultMessage,
    UserMessage,
};
use dsh_session::{
    Session, SessionEventData, SessionId, SurfaceOp, TurnEndReason,
};
use dsh_system_prompt::{render_prompt, SystemPrompt};
use dsh_tools::ToolRuntime;
use futures::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Concrete loop driver service.
pub struct AgentLoop {
    ctx: Context,
}

impl AgentLoop {
    /// Install the factory on `ctx.agents`.
    pub fn install(ctx: &Context) -> dsh_cordis::Result<Arc<Self>> {
        let driver = Arc::new(Self { ctx: ctx.clone() });
        let agents = ctx.service::<AgentRegistry>()?;
        agents.set_factory(Arc::clone(&driver) as Arc<dyn AgentFactory>);
        ctx.provide(Arc::clone(&driver))?;
        Ok(driver)
    }
}

impl Service for AgentLoop {
    const KEY: &'static str = "agentLoop";
}

#[async_trait]
impl AgentFactory for AgentLoop {
    fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
        Arc::new(LoopAgent::new(self.ctx.clone(), session))
    }
}

struct LoopAgent {
    ctx: Context,
    session: Arc<Session>,
    inbox: Arc<Inbox>,
    status: Mutex<AgentStatus>,
    cancelled: AtomicBool,
    cancel_reason: Mutex<Option<String>>,
    idle: Notify,
    max_tokens: AtomicBool,
}

impl LoopAgent {
    fn new(ctx: Context, session: Arc<Session>) -> Self {
        Self {
            ctx,
            session,
            inbox: Arc::new(Inbox::default()),
            status: Mutex::new(AgentStatus::Idle),
            cancelled: AtomicBool::new(false),
            cancel_reason: Mutex::new(None),
            idle: Notify::new(),
            max_tokens: AtomicBool::new(false),
        }
    }

    fn set_status(&self, status: AgentStatus) {
        *self.status.lock().expect("status") = status;
        self.ctx.emit(
            "agent/status",
            serde_json::json!({ "status": format!("{status:?}") }),
        );
        if status == AgentStatus::Idle {
            self.idle.notify_waiters();
        }
    }

    async fn turn(&self, turn: u32) -> Result<bool, AgentError> {
        self.session
            .append(SessionEventData::TurnStart { turn }, None)
            .ok();
        let mut turn_end: Option<TurnEndReason> = None;
        let mut target = InboxTarget::NextTurn;
        let mut step = 0u32;
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                turn_end = Some(TurnEndReason::Aborted {
                    reason: self
                        .cancel_reason
                        .lock()
                        .expect("cancel")
                        .clone()
                        .unwrap_or_else(|| "cancelled".into()),
                });
                break;
            }
            let claimed = self.inbox.claim(target);
            let decision = self.ctx.waterfall(
                "agent/pre-step",
                serde_json::json!({
                    "messages": claimed.iter().map(|message| serde_json::to_value(message).unwrap_or_default()).collect::<Vec<_>>(),
                    "turn": turn,
                }),
                |payload| payload,
            );
            let messages = match decision {
                Ok(value) if value.get("reject").and_then(|v| v.as_bool()) == Some(true) => {
                    turn_end = Some(TurnEndReason::Blocked);
                    break;
                }
                Ok(value) => value
                    .get("messages")
                    .and_then(|v| serde_json::from_value::<Vec<UserMessage>>(v.clone()).ok())
                    .unwrap_or(claimed),
                Err(_) => claimed,
            };
            if step == 0 && messages.is_empty() {
                turn_end = Some(TurnEndReason::Completed);
                break;
            }
            if turn_end.is_some() && messages.is_empty() {
                break;
            }
            step += 1;
            self.session
                .append(SessionEventData::StepStart { turn, step }, None)
                .ok();
            for message in &messages {
                self.session
                    .append(SessionEventData::UserMessage(message.clone()), Some(SurfaceOp::append()))
                    .ok();
            }
            let step_end = self.step(turn, step).await;
            if self.max_tokens.load(Ordering::SeqCst) {
                turn_end = Some(TurnEndReason::MaxTokens);
            } else if turn_end.is_none() {
                turn_end = step_end;
            }
            self.session
                .append(SessionEventData::StepEnd { turn, step }, None)
                .ok();
            if turn_end.is_some() && !self.inbox.next_step_pending() {
                let _ = self.ctx.serial(
                    "agent/turn-stopping",
                    serde_json::json!({ "turn": turn }),
                );
            }
            if turn_end.is_some() && !self.inbox.next_step_pending() {
                break;
            }
            target = InboxTarget::NextStep;
        }
        self.session
            .append(
                SessionEventData::TurnEnd {
                    turn,
                    reason: turn_end.unwrap_or(TurnEndReason::Completed),
                },
                None,
            )
            .ok();
        Ok(self.inbox.has_pending())
    }

    async fn step(&self, turn: u32, step: u32) -> Option<TurnEndReason> {
      loop {
        let tools = self.ctx.get::<ToolRuntime>().map(|runtime| runtime.schemas()).unwrap_or_default();
        let assembly = self
            .ctx
            .get::<SystemPrompt>()
            .map(|prompt| prompt.assemble(tools.clone()))
            .unwrap_or_default();
        let system = render_prompt(&assembly);
        let request = LlmRequest {
            config: dsh_llm::LlmCallConfig::default(),
            system: if system.is_empty() { None } else { Some(system) },
            messages: self.session.derive_messages(),
            tools: assembly.tools,
            purpose: None,
        };
        let prepared = self.ctx.waterfall(
            "agent/request",
            serde_json::to_value(&request).unwrap_or_default(),
            |payload| payload,
        );
        let request: LlmRequest = prepared
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or(request);

        let Some(llm) = self.ctx.get::<LlmRuntime>() else {
            return Some(TurnEndReason::Error {
                message: "missing llm".into(),
                code: "UNKNOWN".into(),
            });
        };
        let stream = match llm.stream(request).await {
            Ok(stream) => stream,
            Err(error) => {
                let recovery = self.ctx.waterfall(
                    "agent/request-error",
                    serde_json::json!({
                        "code": match &error {
                            dsh_llm::LlmError::Failure(failure) => failure.code.clone(),
                        },
                        "message": error.to_string(),
                    }),
                    |payload| payload,
                );
                if recovery.ok().and_then(|value| value.get("kind").and_then(|v| v.as_str()).map(str::to_string))
                    == Some("retry".into())
                {
                    continue;
                }
                return Some(TurnEndReason::Error {
                    message: error.to_string(),
                    code: "UNKNOWN".into(),
                });
            }
        };
        let mut assembler = BlockAssembler::default();
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            self.session
                .append(
                    SessionEventData::AssistantChunk {
                        turn,
                        step,
                        chunk: chunk.clone(),
                    },
                    None,
                )
                .ok();
            assembler.push(&chunk);
        }
        let message = assembler.finish();
        if message.content.is_empty() {
            self.session
                .append(
                    SessionEventData::AssistantMessage {
                        turn,
                        step,
                        message: AssistantMessage::default(),
                        usage: None,
                    },
                    Some(SurfaceOp::append()),
                )
                .ok();
        } else {
            self.session
                .append(
                    SessionEventData::AssistantMessage {
                        turn,
                        step,
                        message: message.clone(),
                        usage: None,
                    },
                    Some(SurfaceOp::append()),
                )
                .ok();
        }
        let calls = message.tool_calls();
        if calls.is_empty() {
            return Some(TurnEndReason::Completed);
        }
        if let Some(tools) = self.ctx.get::<ToolRuntime>() {
            for call in calls {
                self.session
                    .append(
                        SessionEventData::ToolCall {
                            turn,
                            step,
                            call_id: call.id.as_str().to_string(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                        None,
                    )
                    .ok();
                let args = serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
                let outcome = tools
                    .execute(&self.ctx, &call.name, args)
                    .await
                    .unwrap_or_else(|error| dsh_tools::ToolOutcome::error(error.to_string()));
                let tool_message = ToolResultMessage {
                    tool_call_id: call_id(call.id.as_str()),
                    content: outcome.content,
                    is_error: Some(outcome.is_error).filter(|flag| *flag),
                };
                self.session
                    .append(
                        SessionEventData::ToolResult {
                            turn,
                            step,
                            message: tool_message,
                        },
                        Some(SurfaceOp::append()),
                    )
                    .ok();
            }
        }
        return None;
      }
    }
}

#[async_trait]
impl Agent for LoopAgent {
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
        *self.status.lock().expect("status")
    }

    fn send(&self, message: UserMessage, target: InboxTarget, wakeup: bool) {
        self.inbox.push(InboxEntry {
            message,
            target,
            wakeup,
        });
        if wakeup {
            self.idle.notify_waiters();
        }
    }

    fn cancel(&self, cause: AgentCancelCause) {
        if self
            .cancel_reason
            .lock()
            .expect("cancel")
            .replace(cause.kind)
            .is_none()
        {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    async fn when_idle(&self) {
        loop {
            if self.status() == AgentStatus::Idle && !self.inbox.has_pending() {
                return;
            }
            self.idle.notified().await;
        }
    }

    async fn run(&self) -> Result<(), AgentError> {
        self.set_status(AgentStatus::Running);
        let mut turn = 0u32;
        while self.inbox.has_pending() {
            turn += 1;
            let more = self.turn(turn).await?;
            if !more {
                break;
            }
        }
        self.set_status(AgentStatus::Idle);
        Ok(())
    }
}

/// Run one followup to idle. Used by headless and tests.
pub async fn run_followup(agent: &dyn Agent, message: UserMessage) -> Result<(), AgentError> {
    agent.followup(message);
    agent.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{ContentBlock, LlmAdapter, StreamChunk};
    use dsh_session::SessionStore;
    use futures::stream;

    struct HelloAdapter;

    #[async_trait]
    impl LlmAdapter for HelloAdapter {
        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, dsh_llm::LlmError> {
            Ok(Box::pin(stream::iter([StreamChunk::Text {
                text: "hello".into(),
            }])))
        }
    }

    #[tokio::test]
    async fn rejected_first_claim_closes_turn_without_a_step() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HelloAdapter))))
            .unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        ctx.on_waterfall("agent/pre-step", |_payload, _next| {
            serde_json::json!({ "reject": true })
        })
        .unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
        run_followup(
            handle.agent.as_ref(),
            UserMessage {
                content: vec![ContentBlock::text("hi")],
                source: None,
            },
        )
        .await
        .unwrap();
        let types: Vec<_> = handle
            .agent
            .session()
            .events()
            .into_iter()
            .map(|event| format!("{:?}", event.data).chars().take(20).collect::<String>())
            .collect();
        assert!(handle
            .agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::TurnStart { turn: 1 })));
        assert!(handle
            .agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(
                event.data,
                SessionEventData::TurnEnd {
                    reason: TurnEndReason::Blocked,
                    ..
                }
            )));
        assert!(!handle
            .agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::StepStart { .. })));
        let _ = types;
    }

    #[tokio::test]
    async fn text_turn_writes_surface_messages() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HelloAdapter))))
            .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx.service::<AgentRegistry>().unwrap().create(session).unwrap();
        run_followup(
            handle.agent.as_ref(),
            UserMessage {
                content: vec![ContentBlock::text("hi")],
                source: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            handle.agent.session().last_assistant_text().as_deref(),
            Some("hello")
        );
        assert_eq!(handle.agent.session().derive_messages().len(), 2);
    }
}
