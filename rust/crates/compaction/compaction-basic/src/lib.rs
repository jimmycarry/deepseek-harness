//! Basic compaction provider: pressure on `agent/pre-step`, overflow on
//! `agent/request-error`, replace via surface op.

use async_trait::async_trait;
use dsh_agent::{Agent, AgentRegistry};
use dsh_compaction::{
    CompactionEngine, CompactionResult, CompactionRuntime, CompactionTrigger, ManualCompactionError,
};
use dsh_cordis::Context;
use dsh_llm::{
    BlockAssembler, ContentBlock, FinishReason, LlmCallConfig, LlmRequest, LlmRuntime, Message,
    MessageSource, StreamChunk, UserMessage,
};
use dsh_session::{session_id, derive_event_message, Session, SessionEventData, SurfaceOp};
use dsh_token_meter::TokenMeter;
use futures::executor::block_on;
use futures::StreamExt;
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
    /// Optional dedicated summarizer provider; empty inherits the last route.
    pub summarization_provider: String,
    /// Optional dedicated summarizer model; empty inherits the last route.
    pub summarization_model: String,
    /// Output-token cap sent on the summarization request.
    pub max_tokens: u32,
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
        let summarization_provider = config
            .and_then(|value| value.get("summarizationProvider"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let summarization_model = config
            .and_then(|value| value.get("summarizationModel"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let max_tokens = match config.and_then(|value| value.get("maxTokens")) {
            None => 8192,
            Some(value) => value
                .as_u64()
                .filter(|value| *value > 0)
                .map(|value| value as u32)
                .ok_or_else(|| "compaction-basic: maxTokens must be a positive integer".to_string())?,
        };
        Ok(Self {
            threshold_ratio,
            retain_ratio,
            retain_tokens,
            auto,
            summarization_provider,
            summarization_model,
            max_tokens,
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
    /// `ctx.llm` used for the one-shot `purpose: "compaction"` call.
    llm: Option<Arc<LlmRuntime>>,
}

impl BasicCompactionEngine {
    /// Build from explicit policy without a token meter.
    pub fn new(policy: CompactionPolicy) -> Self {
        Self {
            policy,
            meter: None,
            llm: None,
        }
    }

    /// Provide `ctx.compaction` and register automatic listeners.
    pub fn install(ctx: &Context, policy: CompactionPolicy) -> dsh_cordis::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            policy,
            meter: ctx.get::<TokenMeter>(),
            llm: ctx.get::<LlmRuntime>(),
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

    async fn compact_session(
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
        let summarized = match summarize_with_llm(
            self.llm.as_deref(),
            &self.policy,
            &session,
            &shadowed,
        )
        .await
        {
            Ok(summarized) => summarized,
            Err(error) => {
                session
                    .append(
                        SessionEventData::CompactionEnd {
                            compaction_id,
                            source_command_id,
                            turn: None,
                            error: Some(error),
                        },
                        None,
                    )
                    .ok();
                return Err(ManualCompactionError::Summary);
            }
        };
        let summary_event = session
            .append(
                SessionEventData::CompactionSummary {
                    compaction_id: compaction_id.clone(),
                    source_command_id: source_command_id.clone(),
                    summary: summarized.summary_text.clone(),
                    shadowed_range: serde_json::json!({ "start": start, "end": end }),
                    shadowed_seqs: shadowed.clone(),
                    shadowed_token_count,
                },
                None,
            )
            .ok();
        let summary = frame_summary(&summarized.summary_text);
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
                SessionEventData::UserMessage(UserMessage::from_parts(
                    summary.clone(),
                    MessageSource::Plugin {
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
            summary_seq: summary_event.map(|event| event.seq).unwrap_or(0),
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
    "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// Compaction directive appended after the replayed region, matching TypeScript.
pub const COMPACTION_INSTRUCTION: &str = "You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.\n\nOutput EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write \"(none)\" for an empty section — never drop a section.\n\n## Primary Request and Intent\n- [the user's original and evolving goals; quote verbatim where the exact wording matters]\n\n## Key Technical Concepts\n- [technologies, frameworks, patterns, and conventions in play]\n\n## Files and Code\n- [exact path: why it matters, key changes or snippets]\n\n## Errors and Fixes\n- [error: how it was resolved, plus any related user feedback]\n\n## Pending Jobs\n- [explicitly requested work not yet completed]\n\n## Current Work\n- [precisely what was in progress at this checkpoint]\n\n## Next Step\n- [the single next action, directly in line with the most recent request, or \"(none)\"]\n\n## Critical Context\n- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]\n\nRules:\n- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.\n- Capture user feedback and explicit instructions faithfully, especially corrections.\n- Do NOT mention this summarization request or that the context was compacted.\n- Output only the checkpoint text: do not call any tool or take any other action.\n- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure.";

struct SummarizedText {
    summary_text: String,
}

fn frame_summary(summary_text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(format!(
        "{CHECKPOINT_PREAMBLE}\n\n<compacted-summary>{summary_text}</compacted-summary>"
    ))]
}

fn last_request_route(session: &Session) -> Option<(String, String)> {
    session.events().into_iter().rev().find_map(|event| match event.data {
        SessionEventData::RequestContext {
            provider, model, ..
        } => Some((provider, model)),
        SessionEventData::RequestHeader { header, .. } => {
            let config = header.get("config")?;
            Some((
                config.get("provider")?.as_str()?.to_string(),
                config.get("model")?.as_str()?.to_string(),
            ))
        }
        _ => None,
    })
}

fn last_request_prefix(session: &Session) -> (Option<String>, Vec<dsh_llm::ToolSchema>) {
    for event in session.events().into_iter().rev() {
        if let SessionEventData::RequestHeader { header, .. } = event.data {
            let system = header
                .get("system")
                .and_then(Value::as_str)
                .map(str::to_string);
            let tools = header
                .get("tools")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
            return (system, tools);
        }
    }
    (None, Vec::new())
}

fn shadowed_messages(session: &Session, shadowed: &[u64]) -> Vec<Message> {
    let events = session.events();
    shadowed
        .iter()
        .filter_map(|seq| {
            events
                .iter()
                .find(|event| event.seq == *seq)
                .and_then(|event| derive_event_message(&event.data))
        })
        .collect()
}

async fn summarize_with_llm(
    llm: Option<&LlmRuntime>,
    policy: &CompactionPolicy,
    session: &Session,
    shadowed: &[u64],
) -> Result<SummarizedText, String> {
    let Some(llm) = llm else {
        return Err(
            "no provider/model available for summarization: set both BasicCompactionConfig summarization fields, route one request, or set both AgentOptions fields"
                .into(),
        );
    };
    let (provider, model) = if !policy.summarization_provider.is_empty() {
        (
            policy.summarization_provider.clone(),
            policy.summarization_model.clone(),
        )
    } else {
        last_request_route(session).ok_or_else(|| {
            "no provider/model available for summarization: set both BasicCompactionConfig summarization fields, route one request, or set both AgentOptions fields"
                .to_string()
        })?
    };
    let (system, tools) = last_request_prefix(session);
    let mut messages = shadowed_messages(session, shadowed);
    messages.push(Message::User(UserMessage::from_parts(
        vec![ContentBlock::text(COMPACTION_INSTRUCTION)],
        MessageSource::plugin("dsh-compaction-basic"),
    )));
    let request = LlmRequest {
        adapter_defaults: None,
        config: LlmCallConfig {
            provider,
            model,
            reasoning_effort: None,
            max_tokens: Some(policy.max_tokens),
        },
        system,
        messages,
        tools,
        purpose: Some("compaction".into()),
    };
    let stream = llm
        .stream(request)
        .await
        .map_err(|error| error.to_string())?;
    let mut assembler = BlockAssembler::default();
    let mut finish: Option<FinishReason> = None;
    futures::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        if let StreamChunk::Finish { reason, .. } = &chunk {
            finish = Some(reason.clone());
        }
        assembler.push(&chunk);
    }
    if let Some(reason) = finish.as_ref() {
        match reason {
            FinishReason::Error { failure } | FinishReason::Aborted { failure } => {
                return Err(failure.message.clone());
            }
            FinishReason::MaxTokens => {
                return Err(
                    "summarization truncated at the token cap (incomplete checkpoint)".into(),
                );
            }
            _ => {}
        }
    }
    let summary_text = assembler
        .finish()
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if summary_text.trim().is_empty() {
        return Err("summarization produced no text summary content".into());
    }
    Ok(SummarizedText { summary_text })
}

#[async_trait]
impl CompactionEngine for BasicCompactionEngine {
    async fn compact_if_needed(
        &self,
        agent: &dyn Agent,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, trigger == CompactionTrigger::ContextOverflow, None)
            .await
    }

    async fn compact_now(
        &self,
        agent: &dyn Agent,
        source_command_id: Option<&str>,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        self.compact_session(agent, true, source_command_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{Agent, AgentCancelCause, AgentError, AgentStatus, Inbox, InboxTarget};
    use dsh_llm::{LlmAdapter, LlmError, LlmFailure};
    use dsh_session::{session_id, Session, SessionStore};
    use futures::stream;
    use std::sync::{Arc, Mutex};

    struct ScriptedSummarizer {
        text: String,
        last: Mutex<Option<LlmRequest>>,
        fail: bool,
    }

    #[async_trait]
    impl LlmAdapter for ScriptedSummarizer {
        async fn stream(
            &self,
            request: LlmRequest,
        ) -> std::result::Result<
            futures::stream::BoxStream<'static, StreamChunk>,
            LlmError,
        > {
            *self.last.lock().expect("last") = Some(request);
            if self.fail {
                return Err(LlmError::Failure(LlmFailure {
                    message: "summarizer boom".into(),
                    code: "SUMMARIZER".into(),
                    status: None,
                }));
            }
            Ok(Box::pin(stream::iter(StreamChunk::text_stream(
                self.text.clone(),
            ))))
        }
    }

    fn scripted_engine(text: &str) -> (BasicCompactionEngine, Arc<ScriptedSummarizer>) {
        let adapter = Arc::new(ScriptedSummarizer {
            text: text.into(),
            last: Mutex::new(None),
            fail: false,
        });
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(4))),
            llm: Some(Arc::new(LlmRuntime::new(Arc::clone(&adapter) as Arc<dyn LlmAdapter>))),
        };
        (engine, adapter)
    }

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
        let (engine, adapter) = scripted_engine("condensed checkpoint");
        let result = engine.compact_now(&agent, None).await.unwrap().unwrap();
        let request = adapter.last.lock().expect("last").clone().expect("request");
        assert_eq!(request.purpose.as_deref(), Some("compaction"));
        let instruction = match request.messages.last() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
            _ => "",
        };
        assert!(
            instruction.contains("You are now acting as a compaction engine"),
            "{instruction}"
        );
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
        assert_eq!(policy.max_tokens, 8192);
        assert!(policy.auto);
        assert!(policy.summarization_provider.is_empty());
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
                "retainRatio": 0.01,
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(1))),
            llm: None,
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
        let (mut engine, _) = scripted_engine("ratio checkpoint");
        engine.policy = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.5,
            "retainRatio": 0.1,
            "summarizationProvider": "replay",
            "summarizationModel": "script"
        })))
        .unwrap();
        engine.meter = Some(Arc::new(TokenMeter::new(1)));
        let result = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap()
            .expect("pressure compaction");
        assert!(!result.shadowed_seqs.is_empty());
    }

    #[tokio::test]
    async fn summarize_failure_writes_end_error_and_keeps_history() {
        let session = Arc::new(Session::new(session_id("fail")));
        for text in ["a", "b", "c", "d"] {
            append_user(&session, text);
        }
        let agent = StubAgent {
            session: Arc::clone(&session),
            inbox: Arc::new(Inbox::default()),
        };
        let adapter = Arc::new(ScriptedSummarizer {
            text: String::new(),
            last: Mutex::new(None),
            fail: true,
        });
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: None,
            llm: Some(Arc::new(LlmRuntime::new(
                Arc::clone(&adapter) as Arc<dyn LlmAdapter>
            ))),
        };
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Summary));
        let types: Vec<&str> = session
            .events()
            .iter()
            .map(|event| match event.data {
                SessionEventData::CompactionStart { .. } => "compaction/start",
                SessionEventData::CompactionSummary { .. } => "compaction/summary",
                SessionEventData::CompactionEnd { .. } => "compaction/end",
                _ => "other",
            })
            .collect();
        assert!(types.contains(&"compaction/start"));
        assert!(types.contains(&"compaction/end"));
        assert!(!types.contains(&"compaction/summary"));
        assert!(session
            .derive_messages()
            .iter()
            .any(|message| match message {
                Message::User(user) => user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "a")),
                _ => false,
            }));
    }
}
