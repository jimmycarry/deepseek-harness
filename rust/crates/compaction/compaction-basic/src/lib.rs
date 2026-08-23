//! Basic compaction provider: pressure on `agent/pre-step`, overflow on
//! `agent/request-error`, replace via surface op.

use async_trait::async_trait;
use dsh_agent::{Agent, AgentRegistry};
use dsh_compaction::{
    CompactionEngine, CompactionResult, CompactionRuntime, CompactionTrigger, ManualCompactionError,
};
use dsh_cordis::Context;
use dsh_llm::ContentBlock;
use dsh_session::{session_id, Session, SessionEventData, SurfaceOp};
use dsh_token_meter::TokenMeter;
use futures::executor::block_on;
use serde_json::Value;
use std::sync::Arc;

const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
const DEFAULT_RETAIN_RATIO: f64 = 0.16;

/// TypeScript `BasicCompactionConfig` fields used for pressure and retention.
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Request-pressure fraction of the routed context window.
    pub threshold_ratio: f64,
    /// Verbatim-tail fraction of the routed context window.
    pub retain_ratio: f64,
    /// Explicit tail budget in tokens, when set.
    pub retain_tokens: Option<u64>,
    /// Whether `agent/pre-step` runs automatic pressure compaction.
    pub auto: bool,
}

impl CompactionPolicy {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Unknown keys, non-boolean `auto`, ratios outside `(0, 1]`, or
    /// `retainRatio >= thresholdRatio`.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        if let Some(Value::Object(map)) = config {
            for key in map.keys() {
                if !matches!(
                    key.as_str(),
                    "thresholdRatio"
                        | "retainRatio"
                        | "retainTokens"
                        | "summarizationProvider"
                        | "summarizationModel"
                        | "maxTokens"
                        | "compactionRetries"
                        | "maxOverflowRetries"
                        | "modelPolicies"
                        | "auto"
                ) {
                    return Err(format!(
                        "compaction-basic: unknown config key {key} (use thresholdRatio / retainRatio)"
                    ));
                }
            }
        }
        let threshold_ratio = ratio_field(config, "thresholdRatio", DEFAULT_THRESHOLD_RATIO)?;
        let retain_ratio = ratio_field(config, "retainRatio", DEFAULT_RETAIN_RATIO)?;
        if retain_ratio >= threshold_ratio {
            return Err(format!(
                "compaction-basic: retainRatio ({retain_ratio}) must be less than the resolved thresholdRatio ({threshold_ratio})"
            ));
        }
        if config.and_then(|value| value.get("retainRatio")).is_some()
            && config.and_then(|value| value.get("retainTokens")).is_some()
        {
            return Err(
                "compaction-basic: retainRatio and retainTokens are mutually exclusive".into(),
            );
        }
        let retain_tokens = match config.and_then(|value| value.get("retainTokens")) {
            None => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                "compaction-basic: retainTokens must be an integer".to_string()
            })?),
        };
        let auto = match config.and_then(|value| value.get("auto")) {
            None => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("compaction-basic: auto must be a boolean".into()),
        };
        Ok(Self {
            threshold_ratio,
            retain_ratio,
            retain_tokens,
            auto,
        })
    }
}

fn ratio_field(config: Option<&Value>, key: &str, default: f64) -> Result<f64, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(default),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("compaction-basic: {key} must be a number"))?;
            if number <= 0.0 || number > 1.0 {
                return Err(format!("compaction-basic: {key} must be a number in (0, 1]"));
            }
            Ok(number)
        }
    }
}

/// Token-budget backend. Thresholds are Config, never hidden defaults in `run`.
pub struct BasicCompactionEngine {
    /// Resolved TypeScript pressure / retention policy.
    pub policy: CompactionPolicy,
    /// Meter used to price the shadowed span, when mounted.
    meter: Option<Arc<TokenMeter>>,
}

impl BasicCompactionEngine {
    /// Build from explicit policy without a token meter.
    pub fn new(policy: CompactionPolicy) -> Self {
        Self {
            policy,
            meter: None,
        }
    }

    /// Provide `ctx.compaction` and register automatic listeners.
    pub fn install(ctx: &Context, policy: CompactionPolicy) -> dsh_cordis::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            policy,
            meter: ctx.get::<TokenMeter>(),
        });
        engine.register_automatic(ctx)?;
        ctx.provide(Arc::new(CompactionRuntime::new(
            Arc::clone(&engine) as Arc<dyn CompactionEngine>
        )))?;
        Ok(engine)
    }

    /// Register automatic listeners.
    pub fn register_automatic(self: &Arc<Self>, ctx: &Context) -> dsh_cordis::Result<()> {
        let engine = Arc::clone(self);
        let lookup = ctx.clone();
        ctx.on_waterfall("agent/pre-step", move |payload, next| {
            if !engine.policy.auto {
                return next.call(payload);
            }
            if let Some(id) = payload
                .get("sessionId")
                .or_else(|| payload.get("agentId"))
                .and_then(|value| value.as_str())
            {
                if let Some(agents) = lookup.get::<AgentRegistry>() {
                    if let Some(agent) = agents.get(&session_id(id)) {
                        let meter = lookup.get::<TokenMeter>();
                        if let Some(pruner) =
                            lookup.get::<dsh_tool_result_pruner::ToolResultPruner>()
                        {
                            let _ = pruner.prune_session(&agent.session(), meter.as_deref());
                        }
                        if let Some(meter) = &meter {
                            let _pressure = meter.estimate_session(&agent.session());
                        }
                        let _ = block_on(
                            engine.compact_if_needed(agent.as_ref(), CompactionTrigger::Pressure),
                        );
                    }
                }
            }
            next.call(payload)
        })?;

        let engine = Arc::clone(self);
        let lookup = ctx.clone();
        ctx.on_waterfall("agent/request-error", move |payload, next| {
            if payload.get("code").and_then(|value| value.as_str())
                == Some("CONTEXT_WINDOW_EXCEEDED")
            {
                if let Some(id) = payload
                    .get("sessionId")
                    .or_else(|| payload.get("agentId"))
                    .and_then(|value| value.as_str())
                {
                    if let Some(agents) = lookup.get::<AgentRegistry>() {
                        if let Some(agent) = agents.get(&session_id(id)) {
                            if let Some(pruner) =
                                lookup.get::<dsh_tool_result_pruner::ToolResultPruner>()
                            {
                                let _ = pruner.prune_session(
                                    &agent.session(),
                                    lookup.get::<TokenMeter>().as_deref(),
                                );
                            }
                            let _ = block_on(engine.compact_if_needed(
                                agent.as_ref(),
                                CompactionTrigger::ContextOverflow,
                            ));
                        }
                    }
                }
                return serde_json::json!({ "kind": "retry" });
            }
            next.call(payload)
        })?;

        let engine = Arc::clone(self);
        let lookup = ctx.clone();
        ctx.on_waterfall("agent/maintenance", move |payload, next| {
            if let Some(id) = payload
                .get("sessionId")
                .or_else(|| payload.get("agentId"))
                .and_then(|value| value.as_str())
            {
                if let Some(agents) = lookup.get::<AgentRegistry>() {
                    if let Some(agent) = agents.get(&session_id(id)) {
                        let _ = block_on(engine.compact_now(agent.as_ref(), None));
                    }
                }
            }
            next.call(payload)
        })?;
        Ok(())
    }

    fn compact_session(
        &self,
        agent: &dyn Agent,
        force: bool,
        source_command_id: Option<&str>,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        let session = agent.session();
        let events = session.events();
        if events.iter().any(|event| {
            matches!(event.data, SessionEventData::CompactionStart { .. })
                && !events.iter().any(|other| {
                    other.seq > event.seq
                        && matches!(other.data, SessionEventData::CompactionEnd { .. })
                })
        }) {
            return Err(ManualCompactionError::Busy);
        }
        let surface = session.surface();
        let meter = self.meter.as_deref();
        let total_tokens = meter
            .map(|meter| meter.estimate_session(&session) as u64)
            .unwrap_or(0);
        let window = last_context_window(&session);
        if !force {
            let Some(window) = window else {
                return Ok(None);
            };
            let threshold = ((window as f64) * self.policy.threshold_ratio).floor() as u64;
            if total_tokens < threshold {
                return Ok(None);
            }
        }
        let retain_tokens = if force {
            0
        } else {
            self.policy.retain_tokens.unwrap_or_else(|| {
                window
                    .map(|window| ((window as f64) * self.policy.retain_ratio).floor() as u64)
                    .unwrap_or(0)
            })
        };
        let retain_tail = retain_tail_nodes(&session, meter, retain_tokens);
        if surface.nodes.len() <= retain_tail + 1 {
            return Ok(None);
        }
        let end_idx = surface.nodes.len() - 1 - retain_tail;
        let start = surface.nodes[0];
        let end = surface.nodes[end_idx];
        let shadowed = surface.nodes[..=end_idx].to_vec();
        let compaction_id = uuid::Uuid::new_v4().to_string();
        let source_command_id = source_command_id.map(str::to_string);
        let start_event = session
            .append(
                SessionEventData::CompactionStart {
                    compaction_id: compaction_id.clone(),
                    source_command_id: source_command_id.clone(),
                    turn: None,
                },
                None,
            )
            .ok();
        let summary_text = "earlier conversation condensed".to_string();
        let shadowed_token_count = self
            .meter
            .as_ref()
            .map(|meter| {
                events
                    .iter()
                    .filter(|event| shadowed.contains(&event.seq))
                    .map(|event| match &event.data {
                        SessionEventData::UserMessage(message) => {
                            meter.estimate_content(&message.content) as u64
                        }
                        SessionEventData::AssistantMessage { message, .. } => {
                            meter.estimate_content(&message.content) as u64
                        }
                        SessionEventData::ToolResult { message, .. } => {
                            meter.estimate_content(message.result_blocks()) as u64
                        }
                        _ => 0,
                    })
                    .sum()
            })
            .unwrap_or(0);
        let summary_event = session
            .append(
                SessionEventData::CompactionSummary {
                    compaction_id: compaction_id.clone(),
                    source_command_id: source_command_id.clone(),
                    summary: summary_text.clone(),
                    shadowed_range: serde_json::json!({ "start": start, "end": end }),
                    shadowed_seqs: shadowed.clone(),
                    shadowed_token_count,
                },
                None,
            )
            .ok();
        let summary = vec![ContentBlock::text(format!(
            "{CHECKPOINT_PREAMBLE}\n\n<compacted-summary>\n{summary_text}\n</compacted-summary>"
        ))];
        let mut cited: Vec<u64> = Vec::new();
        if let Some(event) = &start_event {
            cited.push(event.seq);
        }
        if let Some(event) = &summary_event {
            cited.push(event.seq);
        }
        cited.extend(shadowed.iter().copied());
        session
            .append_cited(
                SessionEventData::UserMessage(dsh_llm::UserMessage::from_parts(
                    summary.clone(),
                    dsh_llm::MessageSource::Plugin {
                        plugin: "compact".into(),
                        form: None,
                        summary: None,
                        sections: Vec::new(),
                        compaction_id: Some(compaction_id.clone()),
                        source_command_id: source_command_id.clone(),
                    },
                )),
                SurfaceOp::Replace { start, end },
                cited,
            )
            .ok();
        session
            .append(
                SessionEventData::CompactionEnd {
                    compaction_id,
                    source_command_id,
                    turn: None,
                    error: None,
                },
                None,
            )
            .ok();
        Ok(Some(CompactionResult {
            shadowed_seqs: shadowed,
            shadowed_token_count,
            summary,
        }))
    }
}

fn last_context_window(session: &Session) -> Option<u32> {
    session.events().into_iter().rev().find_map(|event| match event.data {
        SessionEventData::RequestContext {
            context_window, ..
        } => context_window,
        _ => None,
    })
}

fn retain_tail_nodes(session: &Session, meter: Option<&TokenMeter>, retain_tokens: u64) -> usize {
    let nodes = session.surface().nodes;
    if nodes.is_empty() {
        return 0;
    }
    if retain_tokens == 0 {
        return 1;
    }
    let Some(meter) = meter else {
        return 1;
    };
    let events = session.events();
    let mut kept = 0u64;
    let mut count = 0usize;
    for seq in nodes.iter().rev() {
        let cost = events
            .iter()
            .find(|event| event.seq == *seq)
            .map(|event| match &event.data {
                SessionEventData::UserMessage(message) => {
                    meter.estimate_content(&message.content) as u64
                }
                SessionEventData::AssistantMessage { message, .. } => {
                    meter.estimate_content(&message.content) as u64
                }
                SessionEventData::ToolResult { message, .. } => {
                    meter.estimate_content(message.result_blocks()) as u64
                }
                _ => 0,
            })
            .unwrap_or(0);
        if count > 0 && kept + cost > retain_tokens {
            break;
        }
        kept += cost;
        count += 1;
    }
    count.max(1)
}

/// Model-facing framing that precedes the `<compacted-summary>` block.
pub const CHECKPOINT_PREAMBLE: &str =
    "This is an automatically generated checkpoint condensing an earlier span of this conversation.";

#[async_trait]
impl CompactionEngine for BasicCompactionEngine {
    async fn compact_if_needed(
        &self,
        agent: &dyn Agent,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, trigger == CompactionTrigger::ContextOverflow, None)
    }

    async fn compact_now(
        &self,
        agent: &dyn Agent,
        source_command_id: Option<&str>,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, true, source_command_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{Agent, AgentCancelCause, AgentError, AgentStatus, Inbox, InboxTarget};
    use dsh_session::{session_id, Session, SessionStore};
    use std::sync::Arc;

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn id(&self) -> &dsh_session::SessionId {
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
        fn send(&self, _: dsh_llm::UserMessage, _: InboxTarget, _: bool) {}
        fn cancel(&self, _: AgentCancelCause) {}
        async fn when_idle(&self) {}
        async fn run(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn append_user(session: &Session, text: &str) {
        session
            .append(
                SessionEventData::UserMessage(dsh_llm::UserMessage::text(text)),
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn replace_removes_shadowed_nodes_from_derive() {
        let session = Arc::new(Session::new(session_id("c")));
        for text in ["a", "b", "c", "d"] {
            append_user(&session, text);
        }
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let engine = BasicCompactionEngine::new(CompactionPolicy::resolve(None).unwrap());
        let result = engine.compact_now(&agent, None).await.unwrap().unwrap();
        assert!(!result.shadowed_seqs.is_empty());
        let messages = session.derive_messages();
        assert!(messages.iter().any(|message| match message {
            dsh_llm::Message::User(user) => user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("compacted-summary"))),
            _ => false,
        }));
        assert!(!session
            .derive_messages()
            .iter()
            .any(|message| match message {
                dsh_llm::Message::User(user) => user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "a")),
                _ => false,
            }));
    }

    #[tokio::test]
    async fn leftover_lock_is_busy() {
        let session = Arc::new(Session::new(session_id("busy")));
        append_user(&session, "a");
        append_user(&session, "b");
        session
            .append(
                SessionEventData::CompactionStart {
                    compaction_id: "stale".into(),
                    source_command_id: None,
                    turn: None,
                },
                None,
            )
            .unwrap();
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let engine = BasicCompactionEngine::new(CompactionPolicy::resolve(None).unwrap());
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Busy));
    }

    #[test]
    fn install_provides_compaction_and_retries_overflow() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        BasicCompactionEngine::install(&ctx, CompactionPolicy::resolve(None).unwrap()).unwrap();
        assert!(ctx.has_service("compaction"));

        let recovered = ctx
            .waterfall(
                "agent/request-error",
                serde_json::json!({ "code": "CONTEXT_WINDOW_EXCEEDED" }),
                |payload| payload,
            )
            .unwrap();
        assert_eq!(recovered["kind"], "retry");
    }

    #[test]
    fn resolve_defaults_and_rejects_stale_or_inverted_ratios() {
        let policy = CompactionPolicy::resolve(None).unwrap();
        assert_eq!(policy.threshold_ratio, 0.8);
        assert_eq!(policy.retain_ratio, 0.16);
        assert!(policy.auto);
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdMessages": 40
        })))
        .is_err());
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.1,
            "retainRatio": 0.2
        })))
        .is_err());
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "retainRatio": 0.1,
            "retainTokens": 100
        })))
        .is_err());
    }

    #[tokio::test]
    async fn pressure_skips_without_a_context_window() {
        let session = Arc::new(Session::new(session_id("nowindow")));
        for text in ["aaaa", "bbbb", "cccc", "dddd"] {
            append_user(&session, text);
        }
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "thresholdRatio": 0.1,
                "retainRatio": 0.01
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(1))),
        };
        assert!(engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn pressure_compacts_when_tokens_cross_the_window_ratio() {
        let session = Arc::new(Session::new(session_id("window")));
        session
            .append(
                SessionEventData::RequestContext {
                    provider: "deepseek-official".into(),
                    model: "deepseek-v4-flash".into(),
                    context_window: Some(20),
                },
                None,
            )
            .unwrap();
        for text in ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"] {
            append_user(&session, text);
        }
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "thresholdRatio": 0.5,
                "retainRatio": 0.1
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(1))),
        };
        let result = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap()
            .expect("pressure compaction");
        assert!(!result.shadowed_seqs.is_empty());
    }
}
