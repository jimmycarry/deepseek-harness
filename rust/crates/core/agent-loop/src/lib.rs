//! Default driver implementing the public `Agent` contract (`ctx.agentLoop`).

use async_trait::async_trait;
use dsh_agent::{
    Agent, AgentCancelCause, AgentDefaultModel, AgentError, AgentFactory, AgentRegistry,
    AgentStatus, Inbox, InboxEntry, InboxTarget,
};
use dsh_cordis::{Context, Service};
use dsh_llm::{
    call_id, BlockAssembler, LlmCallConfig, LlmRequest, LlmRuntime,
    MessageSource, ToolResultMessage, UserMessage,
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
    titled: AtomicBool,
    last_context: Mutex<Option<String>>,
    last_header: Mutex<Option<serde_json::Value>>,
    last_request_context: Mutex<Option<(String, String)>>,
}

impl LoopAgent {
    fn new(ctx: Context, session: Arc<Session>) -> Self {
        Self {
            ctx,
            inbox: Arc::new(Inbox::for_session(Arc::clone(&session))),
            session,
            status: Mutex::new(AgentStatus::Idle),
            cancelled: AtomicBool::new(false),
            cancel_reason: Mutex::new(None),
            idle: Notify::new(),
            max_tokens: AtomicBool::new(false),
            titled: AtomicBool::new(false),
            last_context: Mutex::new(None),
            last_header: Mutex::new(None),
            last_request_context: Mutex::new(None),
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
        self.max_tokens.store(false, Ordering::SeqCst);
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
                    "agentId": self.session.id().as_str(),
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
            let mut messages = messages;
            if let Some(snapshot) = self.runtime_context_message() {
                messages.push(snapshot);
            }
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
            let mut title_from = None;
            for message in &messages {
                let event = self
                    .session
                    .append(
                        SessionEventData::UserMessage(message.clone()),
                        Some(SurfaceOp::append()),
                    )
                    .ok();
                if title_from.is_none()
                    && matches!(message.source, MessageSource::User)
                    && !self.titled.load(Ordering::SeqCst)
                {
                    title_from = event.map(|event| (event.seq, user_text(message)));
                }
            }
            if let Some((seq, text)) = title_from {
                let title = fallback_title(&text);
                if !title.is_empty() {
                    self.session
                        .append(
                            SessionEventData::SessionTitle {
                                title,
                                message_seqs: vec![seq],
                                source: serde_json::json!({ "kind": "fallback" }),
                            },
                            None,
                        )
                        .ok();
                    self.titled.store(true, Ordering::SeqCst);
                }
            }
            let step_end = self.step(turn, step).await;
            if self.cancelled.load(Ordering::SeqCst) {
                turn_end = Some(TurnEndReason::Aborted {
                    reason: self
                        .cancel_reason
                        .lock()
                        .expect("cancel")
                        .clone()
                        .unwrap_or_else(|| "cancelled".into()),
                });
            } else if self.max_tokens.load(Ordering::SeqCst) {
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
        let config = self
            .ctx
            .get::<AgentDefaultModel>()
            .map(|model| LlmCallConfig {
                provider: model.provider.clone(),
                model: model.model.clone(),
            })
            .unwrap_or_default();
        let request = LlmRequest {
            config,
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
        self.log_request(&request);

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
            if let dsh_llm::StreamChunk::Finish {
                reason: dsh_llm::FinishReason::MaxTokens,
                ..
            } = &chunk
            {
                self.max_tokens.store(true, Ordering::SeqCst);
            }
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
        let usage = assembler.take_usage();
        let message = assembler.finish();
        self.session
            .append(
                SessionEventData::AssistantMessage {
                    turn,
                    step,
                    message: message.clone(),
                    usage,
                },
                Some(SurfaceOp::append()),
            )
            .ok();
        let calls = message.tool_calls();
        if calls.is_empty() {
            if self.max_tokens.load(Ordering::SeqCst) {
                return Some(TurnEndReason::MaxTokens);
            }
            return Some(TurnEndReason::Completed);
        }
        if let Some(tools) = self.ctx.get::<ToolRuntime>() {
            for call in &calls {
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
            }
            let scheduled: Vec<(String, serde_json::Value)> = calls
                .iter()
                .map(|call| {
                    let name = call.name.clone();
                    let args =
                        serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
                    (name, args)
                })
                .collect();
            let outcomes = tools
                .execute_many_for(&self.ctx, scheduled, Some(self.session.id().as_str()))
                .await;
            let mut extra = Vec::new();
            for (call, outcome) in calls.into_iter().zip(outcomes) {
                let (outcome, contexts) = match outcome {
                    Ok(result) => (result.outcome, result.additional_contexts),
                    Err(error) => (dsh_tools::ToolOutcome::error(error.to_string()), Vec::new()),
                };
                extra.extend(contexts);
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
            for message in extra {
                self.inbox.push(InboxEntry {
                    message,
                    target: InboxTarget::NextStep,
                    wakeup: true,
                });
            }
        }
        return None;
      }
    }

    fn runtime_context_message(&self) -> Option<UserMessage> {
        let prompt = self.ctx.get::<SystemPrompt>()?;
        let sections = prompt.context_sections();
        let text = prompt.render_context_snapshot();
        if text.is_empty() {
            return None;
        }
        let mut last = self.last_context.lock().expect("context");
        if last.as_deref() == Some(text.as_str()) {
            return None;
        }
        *last = Some(text.clone());
        Some(UserMessage {
            content: vec![dsh_llm::ContentBlock::text(text)],
            source: MessageSource::snapshot("@deepseek-ai/dsh-system-prompt", sections),
        })
    }

    fn log_request(&self, request: &LlmRequest) {
        let header = serde_json::json!({
            "config": request.config,
            "system": request.system,
            "tools": request.tools,
        });
        let reason = {
            let mut last = self.last_header.lock().expect("header");
            if last.is_none() {
                *last = Some(header.clone());
                "initial"
            } else if last.as_ref() != Some(&header) {
                *last = Some(header.clone());
                "change"
            } else {
                ""
            }
        };
        if !reason.is_empty() {
            self.session
                .append(
                    SessionEventData::RequestHeader {
                        header,
                        reason: reason.into(),
                    },
                    None,
                )
                .ok();
        }
        let key = (request.config.provider.clone(), request.config.model.clone());
        let mut last = self.last_request_context.lock().expect("request-context");
        if last.as_ref() != Some(&key) {
            *last = Some(key.clone());
            self.session
                .append(
                    SessionEventData::RequestContext {
                        provider: key.0,
                        model: key.1,
                        context_window: None,
                    },
                    None,
                )
                .ok();
        }
    }
}

fn user_text(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            dsh_llm::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn fallback_title(text: &str) -> String {
    text.split_whitespace().take(5).collect::<Vec<_>>().join(" ")
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
        let mut slot = self.cancel_reason.lock().expect("cancel");
        if slot.is_none() {
            *slot = Some(cause.kind);
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

    async fn run_maintenance(&self) -> Result<(), AgentError> {
        self.set_status(AgentStatus::Maintenance);
        let _ = self.ctx.serial(
            "agent/maintenance",
            serde_json::json!({ "sessionId": self.session.id().as_str() }),
        );
        self.set_status(AgentStatus::Idle);
        Ok(())
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
    agent.run().await?;
    agent.when_idle().await;
    agent.run_maintenance().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_llm::{ContentBlock, LlmAdapter, MessageSource, StreamChunk};
    use dsh_session::SessionStore;
    use futures::stream;

    struct HelloAdapter;

    #[async_trait]
    impl LlmAdapter for HelloAdapter {
        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, dsh_llm::LlmError> {
            Ok(Box::pin(stream::iter(StreamChunk::text_stream("hello"))))
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
            UserMessage::text("hi"),
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
            UserMessage::text("hi"),
        )
        .await
        .unwrap();
        assert_eq!(
            handle.agent.session().last_assistant_text().as_deref(),
            Some("hello")
        );
        assert_eq!(handle.agent.session().derive_messages().len(), 2);
    }

    struct MaxTokensAdapter;

    #[async_trait]
    impl LlmAdapter for MaxTokensAdapter {
        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, dsh_llm::LlmError> {
            let mut chunks = dsh_llm::text_block(0, "cut");
            chunks.push(StreamChunk::Finish {
                reason: dsh_llm::FinishReason::MaxTokens,
                replay_state: None,
            });
            Ok(Box::pin(stream::iter(chunks)))
        }
    }

    fn followup_text() -> UserMessage {
        UserMessage::text("hi")
    }

    #[tokio::test]
    async fn max_tokens_is_sticky_for_the_turn() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(MaxTokensAdapter))))
            .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        run_followup(handle.agent.as_ref(), followup_text())
            .await
            .unwrap();
        assert!(handle.agent.session().events().iter().any(|event| {
            matches!(
                &event.data,
                SessionEventData::TurnEnd {
                    reason: TurnEndReason::MaxTokens,
                    ..
                }
            )
        }));
    }

    #[tokio::test]
    async fn steer_at_turn_stopping_opens_another_step() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HelloAdapter))))
            .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let id = session.id().clone();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        let steered = std::sync::atomic::AtomicBool::new(false);
        let lookup = ctx.clone();
        ctx.on_serial("agent/turn-stopping", move |_| {
            if steered.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            if let Some(agent) = lookup
                .get::<AgentRegistry>()
                .and_then(|agents| agents.get(&id))
            {
                agent.steer(UserMessage {
                    content: vec![ContentBlock::text("more")],
                    source: MessageSource::plugin("steer"),
                });
            }
            None
        })
        .unwrap();
        run_followup(handle.agent.as_ref(), followup_text())
            .await
            .unwrap();
        let steps = handle
            .agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.data, SessionEventData::StepStart { .. }))
            .count();
        assert_eq!(steps, 2);
    }

    struct HoldAdapter {
        released: Arc<Notify>,
    }

    #[async_trait]
    impl LlmAdapter for HoldAdapter {
        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<futures::stream::BoxStream<'static, StreamChunk>, dsh_llm::LlmError> {
            self.released.notified().await;
            Ok(Box::pin(stream::iter(StreamChunk::text_stream("late"))))
        }
    }

    #[tokio::test]
    async fn first_cancel_reason_wins() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        let released = Arc::new(Notify::new());
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HoldAdapter {
            released: Arc::clone(&released),
        }))))
        .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        let agent = Arc::clone(&handle.agent);
        let task = tokio::spawn(async move { run_followup(agent.as_ref(), followup_text()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle.agent.cancel(AgentCancelCause {
            kind: "user".into(),
        });
        handle.agent.cancel(AgentCancelCause {
            kind: "disposed".into(),
        });
        released.notify_waiters();
        task.await.unwrap().unwrap();
        assert!(handle.agent.session().events().iter().any(|event| {
            matches!(
                &event.data,
                SessionEventData::TurnEnd {
                    reason: TurnEndReason::Aborted { reason },
                    ..
                } if reason.as_str() == "user"
            )
        }));
    }

    #[tokio::test]
    async fn derived_messages_remain_a_prefix_of_the_request() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HelloAdapter))))
            .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&seen);
        ctx.on_waterfall("agent/request", move |payload, next| {
            let original: LlmRequest = serde_json::from_value(payload.clone()).unwrap();
            let mut modified = original.clone();
            modified.messages.push(dsh_llm::Message::User(UserMessage {
                content: vec![ContentBlock::text("plugin")],
                source: MessageSource::plugin("plugin"),
            }));
            *recorded.lock().expect("seen") =
                Some((original.messages.clone(), modified.messages.clone()));
            next.call(serde_json::to_value(modified).unwrap())
        })
        .unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        run_followup(handle.agent.as_ref(), followup_text())
            .await
            .unwrap();
        let (derived, sent) = seen.lock().expect("seen").clone().expect("request");
        assert!(!derived.is_empty());
        assert!(sent.starts_with(derived.as_slice()));
    }

    #[tokio::test]
    async fn maintenance_serial_runs_after_followup() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(LlmRuntime::new(Arc::new(HelloAdapter))))
            .unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        AgentLoop::install(&ctx).unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&hits);
        ctx.on_serial("agent/maintenance", move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        })
        .unwrap();
        let session = ctx.service::<SessionStore>().unwrap().create_fresh();
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(session)
            .unwrap();
        run_followup(handle.agent.as_ref(), followup_text())
            .await
            .unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
