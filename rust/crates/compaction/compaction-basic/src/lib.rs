//! Basic compaction provider: pressure on `agent/pre-step`, overflow on
//! `agent/request-error`, replace via surface op.

use async_trait::async_trait;
use dsh_agent::{Agent, AgentRegistry};
use dsh_compaction::{
    tool_pairing_balanced_after, tool_pairing_balanced_before, CompactionEngine, CompactionResult,
    CompactionRuntime, CompactionTrigger, ManualCompactionError,
};
use dsh_cordis::Context;
use dsh_llm::{
    BlockAssembler, ContentBlock, FinishReason, LlmCallConfig, LlmRequest, LlmRuntime, Message,
    MessageSource, StreamChunk, UserMessage,
};
use dsh_session::{session_id, derive_event_message, Session, SessionEventData, SurfaceOp};
use dsh_session_persistence::PersistenceRuntime;
use dsh_token_meter::TokenMeter;
use futures::executor::block_on;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
const DEFAULT_RETAIN_RATIO: f64 = 0.16;
const BASIC_COMPACT_CONFIG_KEYS: &[&str] = &[
    "thresholdRatio",
    "retainRatio",
    "retainTokens",
    "summarizationProvider",
    "summarizationModel",
    "maxTokens",
    "compactionRetries",
    "maxOverflowRetries",
    "modelPolicies",
    "auto",
];
const MODEL_POLICY_KEYS: &[&str] = &[
    "provider",
    "model",
    "thresholdRatio",
    "retainRatio",
    "retainTokens",
    "summarizationProvider",
    "summarizationModel",
    "maxTokens",
    "compactionRetries",
    "maxOverflowRetries",
];

/// Exact provider/model override merged over the default compaction policy.
#[derive(Debug, Clone)]
pub struct ModelCompactPolicy {
    /// Registered provider route to match.
    pub provider: String,
    /// Exact routed model id to match within `provider`.
    pub model: String,
    /// Optional pressure-fraction override.
    pub threshold_ratio: Option<f64>,
    /// Optional verbatim-tail fraction override.
    pub retain_ratio: Option<f64>,
    /// Optional absolute tail-budget override.
    pub retain_tokens: Option<u64>,
    /// Optional summarizer provider; a pair with [`Self::summarization_model`].
    pub summarization_provider: Option<String>,
    /// Optional summarizer model; a pair with [`Self::summarization_provider`].
    pub summarization_model: Option<String>,
    /// Optional generation cap override.
    pub max_tokens: Option<u32>,
    /// Optional extra pressure attempts after the first compaction.
    pub compaction_retries: Option<u32>,
    /// Optional overflow-recovery retry budget.
    pub max_overflow_retries: Option<u32>,
}

/// One form of recent-tail retention after defaults and overrides merge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolvedRetention {
    /// Tail priced as a fraction of the routed context window.
    Ratio(f64),
    /// Absolute tail budget in tokens.
    Tokens(u64),
}

/// Merged policy for one routed conversation target, before capacity scaling.
#[derive(Debug, Clone)]
pub struct ResolvedTargetPolicy {
    /// Request-pressure fraction of the routed context window.
    pub threshold_ratio: f64,
    /// Recent-tail retention after override merge.
    pub retention: ResolvedRetention,
    /// Dedicated summarizer provider; empty inherits the last route.
    pub summarization_provider: String,
    /// Dedicated summarizer model; empty inherits the last route.
    pub summarization_model: String,
    /// Output-token cap sent on the summarization request.
    pub max_tokens: u32,
    /// Extra pressure attempts after the first compaction.
    pub compaction_retries: u32,
    /// Overflow-recovery retry budget.
    pub max_overflow_retries: u32,
}

/// TypeScript `BasicCompactionConfig` fields used for pressure and retention.
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Request-pressure fraction of the routed context window.
    pub threshold_ratio: f64,
    /// Default verbatim-tail fraction when no absolute budget is set.
    pub retain_ratio: f64,
    /// Explicit tail budget in tokens, when set at the default scope.
    pub retain_tokens: Option<u64>,
    /// Whether `agent/pre-step` runs automatic pressure compaction.
    pub auto: bool,
    /// Optional dedicated summarizer provider; empty inherits the last route.
    pub summarization_provider: String,
    /// Optional dedicated summarizer model; empty inherits the last route.
    pub summarization_model: String,
    /// Output-token cap sent on the summarization request.
    pub max_tokens: u32,
    /// Extra pressure attempts after the first compaction.
    pub compaction_retries: u32,
    /// Overflow-recovery retry budget.
    pub max_overflow_retries: u32,
    /// Exact provider/model overrides; duplicate targets fail load.
    pub model_policies: Vec<ModelCompactPolicy>,
}

impl CompactionPolicy {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Unknown keys, non-boolean `auto`, ratios outside `(0, 1]`, a merged
    /// `retainRatio` that is not below `thresholdRatio`, or an invalid
    /// `modelPolicies` table.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        if let Some(value) = config {
            validate_keys(value, BASIC_COMPACT_CONFIG_KEYS, "BasicCompactionConfig")?;
        }
        validate_policy(config, "BasicCompactionConfig")?;
        if let Some(auto) = config.and_then(|value| value.get("auto")) {
            if !auto.is_boolean() {
                return Err("BasicCompactionConfig: auto must be a boolean".into());
            }
        }
        let threshold_ratio = optional_ratio(config, "thresholdRatio")?
            .unwrap_or(DEFAULT_THRESHOLD_RATIO);
        let retention = resolve_retention(config, ResolvedRetention::Ratio(DEFAULT_RETAIN_RATIO))?;
        validate_ratio_retention(threshold_ratio, retention, "BasicCompactionConfig")?;
        let model_policies = resolve_model_policies(config.and_then(|value| value.get("modelPolicies")))?;
        for (index, policy) in model_policies.iter().enumerate() {
            validate_ratio_retention(
                policy.threshold_ratio.unwrap_or(threshold_ratio),
                resolve_override_retention(policy, retention),
                &format!("BasicCompactionConfig: modelPolicies[{index}]"),
            )?;
        }
        let (retain_ratio, retain_tokens) = match retention {
            ResolvedRetention::Ratio(ratio) => (ratio, None),
            ResolvedRetention::Tokens(tokens) => (DEFAULT_RETAIN_RATIO, Some(tokens)),
        };
        Ok(Self {
            threshold_ratio,
            retain_ratio,
            retain_tokens,
            auto: config
                .and_then(|value| value.get("auto"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            summarization_provider: string_field(config, "summarizationProvider")?,
            summarization_model: string_field(config, "summarizationModel")?,
            max_tokens: optional_positive_int(config, "maxTokens", "BasicCompactionConfig.maxTokens")?
                .unwrap_or(8192),
            compaction_retries: optional_non_negative_int(
                config,
                "compactionRetries",
                "BasicCompactionConfig.compactionRetries",
            )?
            .unwrap_or(1) as u32,
            max_overflow_retries: optional_non_negative_int(
                config,
                "maxOverflowRetries",
                "BasicCompactionConfig.maxOverflowRetries",
            )?
            .unwrap_or(1) as u32,
            model_policies,
        })
    }

    /// Merge the exact provider/model override over the default policy.
    #[must_use]
    pub fn resolve_target(&self, provider: &str, model: &str) -> ResolvedTargetPolicy {
        let override_policy = self.model_policies.iter().find(|policy| {
            policy.provider == provider && policy.model == model
        });
        let inherited = self.default_retention();
        ResolvedTargetPolicy {
            threshold_ratio: override_policy
                .and_then(|policy| policy.threshold_ratio)
                .unwrap_or(self.threshold_ratio),
            retention: override_policy
                .map(|policy| resolve_override_retention(policy, inherited))
                .unwrap_or(inherited),
            summarization_provider: override_policy
                .and_then(|policy| policy.summarization_provider.clone())
                .unwrap_or_else(|| self.summarization_provider.clone()),
            summarization_model: override_policy
                .and_then(|policy| policy.summarization_model.clone())
                .unwrap_or_else(|| self.summarization_model.clone()),
            max_tokens: override_policy
                .and_then(|policy| policy.max_tokens)
                .unwrap_or(self.max_tokens),
            compaction_retries: override_policy
                .and_then(|policy| policy.compaction_retries)
                .unwrap_or(self.compaction_retries),
            max_overflow_retries: override_policy
                .and_then(|policy| policy.max_overflow_retries)
                .unwrap_or(self.max_overflow_retries),
        }
    }

    fn default_retention(&self) -> ResolvedRetention {
        match self.retain_tokens {
            Some(tokens) => ResolvedRetention::Tokens(tokens),
            None => ResolvedRetention::Ratio(self.retain_ratio),
        }
    }

    fn default_target(&self) -> ResolvedTargetPolicy {
        ResolvedTargetPolicy {
            threshold_ratio: self.threshold_ratio,
            retention: self.default_retention(),
            summarization_provider: self.summarization_provider.clone(),
            summarization_model: self.summarization_model.clone(),
            max_tokens: self.max_tokens,
            compaction_retries: self.compaction_retries,
            max_overflow_retries: self.max_overflow_retries,
        }
    }
}

impl ResolvedTargetPolicy {
}

fn validate_keys(value: &Value, keys: &[&str], name: &str) -> Result<(), String> {
    let Some(map) = value.as_object() else {
        return Ok(());
    };
    for key in map.keys() {
        if !keys.contains(&key.as_str()) {
            return Err(format!("{name}: unknown key \"{key}\""));
        }
    }
    Ok(())
}

fn validate_policy(config: Option<&Value>, name: &str) -> Result<(), String> {
    if let Some(ratio) = optional_ratio(config, "thresholdRatio")? {
        let _ = ratio;
    }
    if let Some(ratio) = optional_ratio(config, "retainRatio")? {
        let _ = ratio;
    }
    if config.and_then(|value| value.get("retainTokens")).is_some() {
        optional_non_negative_int(config, "retainTokens", &format!("{name}.retainTokens"))?;
    }
    if config.and_then(|value| value.get("retainRatio")).is_some()
        && config.and_then(|value| value.get("retainTokens")).is_some()
    {
        return Err(format!("{name}: retainRatio and retainTokens are mutually exclusive"));
    }
    if config.and_then(|value| value.get("maxTokens")).is_some() {
        optional_positive_int(config, "maxTokens", &format!("{name}.maxTokens"))?;
    }
    if config.and_then(|value| value.get("compactionRetries")).is_some() {
        optional_non_negative_int(
            config,
            "compactionRetries",
            &format!("{name}.compactionRetries"),
        )?;
    }
    if config.and_then(|value| value.get("maxOverflowRetries")).is_some() {
        optional_non_negative_int(
            config,
            "maxOverflowRetries",
            &format!("{name}.maxOverflowRetries"),
        )?;
    }
    validate_summarization_pair(config, name)
}

fn validate_summarization_pair(config: Option<&Value>, name: &str) -> Result<(), String> {
    let provider = config.and_then(|value| value.get("summarizationProvider"));
    let model = config.and_then(|value| value.get("summarizationModel"));
    if let Some(value) = provider {
        if !value.is_string() {
            return Err(format!("{name}.summarizationProvider must be a string"));
        }
    }
    if let Some(value) = model {
        if !value.is_string() {
            return Err(format!("{name}.summarizationModel must be a string"));
        }
    }
    if provider.is_none() && model.is_none() {
        return Ok(());
    }
    let provider_empty = provider.and_then(Value::as_str).is_none_or(str::is_empty);
    let model_empty = model.and_then(Value::as_str).is_none_or(str::is_empty);
    if provider.is_none() || model.is_none() || provider_empty != model_empty {
        return Err(format!(
            "{name}: summarizationProvider and summarizationModel must be set together as an empty or non-empty pair"
        ));
    }
    Ok(())
}

fn resolve_retention(
    config: Option<&Value>,
    fallback: ResolvedRetention,
) -> Result<ResolvedRetention, String> {
    if let Some(tokens) = config.and_then(|value| value.get("retainTokens")) {
        let tokens = tokens.as_u64().ok_or_else(|| {
            "BasicCompactionConfig.retainTokens (null) must be a non-negative integer".to_string()
        })?;
        return Ok(ResolvedRetention::Tokens(tokens));
    }
    if let Some(ratio) = optional_ratio(config, "retainRatio")? {
        return Ok(ResolvedRetention::Ratio(ratio));
    }
    Ok(fallback)
}

fn resolve_override_retention(
    policy: &ModelCompactPolicy,
    fallback: ResolvedRetention,
) -> ResolvedRetention {
    if let Some(tokens) = policy.retain_tokens {
        return ResolvedRetention::Tokens(tokens);
    }
    if let Some(ratio) = policy.retain_ratio {
        return ResolvedRetention::Ratio(ratio);
    }
    fallback
}

fn validate_ratio_retention(
    threshold_ratio: f64,
    retention: ResolvedRetention,
    name: &str,
) -> Result<(), String> {
    if let ResolvedRetention::Ratio(retain_ratio) = retention {
        if retain_ratio >= threshold_ratio {
            return Err(format!(
                "{name}: retainRatio ({retain_ratio}) must be less than the resolved thresholdRatio ({threshold_ratio})"
            ));
        }
    }
    Ok(())
}

fn resolve_model_policies(configured: Option<&Value>) -> Result<Vec<ModelCompactPolicy>, String> {
    let Some(configured) = configured else {
        return Ok(Vec::new());
    };
    let Some(items) = configured.as_array() else {
        return Err("BasicCompactionConfig: modelPolicies must be an array".into());
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut policies = Vec::with_capacity(items.len());
    for (index, source) in items.iter().enumerate() {
        let name = format!("BasicCompactionConfig: modelPolicies[{index}]");
        let policy = parse_model_policy(source, &name)?;
        let key = format!("{}\u{0000}{}", policy.provider, policy.model);
        if !seen.insert(key) {
            return Err(format!(
                "BasicCompactionConfig: duplicate model policy for {}/{}",
                policy.provider, policy.model
            ));
        }
        policies.push(policy);
    }
    Ok(policies)
}

fn parse_model_policy(source: &Value, name: &str) -> Result<ModelCompactPolicy, String> {
    if !source.is_object() || source.as_array().is_some() {
        return Err(format!("{name} must be an object"));
    }
    validate_keys(source, MODEL_POLICY_KEYS, name)?;
    let provider = source
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name}.provider must be a non-empty string"))?;
    let model = source
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name}.model must be a non-empty string"))?;
    validate_policy(Some(source), name)?;
    Ok(ModelCompactPolicy {
        provider: provider.to_string(),
        model: model.to_string(),
        threshold_ratio: optional_ratio(Some(source), "thresholdRatio")?,
        retain_ratio: optional_ratio(Some(source), "retainRatio")?,
        retain_tokens: optional_non_negative_int(
            Some(source),
            "retainTokens",
            &format!("{name}.retainTokens"),
        )?,
        summarization_provider: source
            .get("summarizationProvider")
            .and_then(Value::as_str)
            .map(str::to_string),
        summarization_model: source
            .get("summarizationModel")
            .and_then(Value::as_str)
            .map(str::to_string),
        max_tokens: optional_positive_int(Some(source), "maxTokens", &format!("{name}.maxTokens"))?,
        compaction_retries: optional_non_negative_int(
            Some(source),
            "compactionRetries",
            &format!("{name}.compactionRetries"),
        )?
        .map(|value| value as u32),
        max_overflow_retries: optional_non_negative_int(
            Some(source),
            "maxOverflowRetries",
            &format!("{name}.maxOverflowRetries"),
        )?
        .map(|value| value as u32),
    })
}

fn string_field(config: Option<&Value>, key: &str) -> Result<String, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("BasicCompactionConfig.{key} must be a string")),
    }
}

fn optional_ratio(config: Option<&Value>, key: &str) -> Result<Option<f64>, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(None),
        Some(value) => {
            let number = value.as_f64().ok_or_else(|| {
                format!("BasicCompactionConfig.{key} ({value}) must be a number in (0, 1]")
            })?;
            if !number.is_finite() || number <= 0.0 || number > 1.0 {
                return Err(format!(
                    "BasicCompactionConfig.{key} ({number}) must be a number in (0, 1]"
                ));
            }
            Ok(Some(number))
        }
    }
}

fn optional_positive_int(
    config: Option<&Value>,
    key: &str,
    name: &str,
) -> Result<Option<u32>, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(None),
        Some(value) => {
            let number = value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
                format!("{name} ({value}) must be a positive integer")
            })?;
            Ok(Some(number as u32))
        }
    }
}

fn optional_non_negative_int(
    config: Option<&Value>,
    key: &str,
    name: &str,
) -> Result<Option<u64>, String> {
    match config.and_then(|value| value.get(key)) {
        None => Ok(None),
        Some(value) => {
            let number = value.as_u64().ok_or_else(|| {
                format!("{name} ({value}) must be a non-negative integer")
            })?;
            Ok(Some(number))
        }
    }
}

/// Per-session overflow-recovery budget, reset after a later assistant message.
#[derive(Default)]
struct OverflowRetryState {
    retries: u32,
    assistant_seq: u64,
}

/// Token-budget backend. Thresholds are Config, never hidden defaults in `run`.
pub struct BasicCompactionEngine {
    /// Resolved TypeScript pressure / retention policy.
    pub policy: CompactionPolicy,
    /// Meter used to price the shadowed span, when mounted.
    meter: Option<Arc<TokenMeter>>,
    /// Host used to resolve `ctx.llm` at summarize time, not at mount.
    lookup: Context,
    /// Test-only adapter; production reads `ctx.llm` from [`Self::lookup`].
    llm: Option<Arc<LlmRuntime>>,
    /// Overflow attempts since the last assistant message or idle reset.
    overflow_retries: Mutex<HashMap<String, OverflowRetryState>>,
}

impl BasicCompactionEngine {
    /// Build from explicit policy without a token meter.
    pub fn new(policy: CompactionPolicy) -> Self {
        Self {
            policy,
            meter: None,
            lookup: Context::new(),
            llm: None,
            overflow_retries: Mutex::new(HashMap::new()),
        }
    }

    /// Provide `ctx.compaction` and register automatic listeners.
    pub fn install(ctx: &Context, policy: CompactionPolicy) -> dsh_cordis::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            policy,
            meter: ctx.get::<TokenMeter>(),
            lookup: ctx.clone(),
            llm: None,
            overflow_retries: Mutex::new(HashMap::new()),
        });
        engine.register_automatic(ctx)?;
        ctx.provide(Arc::new(CompactionRuntime::new(
            Arc::clone(&engine) as Arc<dyn CompactionEngine>
        )))?;
        Ok(engine)
    }

    /// Register automatic listeners.
    pub fn register_automatic(self: &Arc<Self>, ctx: &Context) -> dsh_cordis::Result<()> {
        if self.policy.auto {
            let engine = Arc::clone(self);
            let lookup = ctx.clone();
            ctx.on_waterfall("agent/pre-step", move |payload, next| {
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
                match engine.recover_overflow(&lookup, &payload) {
                    Some(decision) => decision,
                    None => next.call(payload),
                }
            })?;

            let engine = Arc::clone(self);
            ctx.on("agent/status", move |payload| {
                if payload
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("idle"))
                {
                    engine.overflow_retries.lock().expect("overflow retries").clear();
                }
            })?;
        }

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

    /// Authorize one overflow retry only after a durable surface replacement.
    fn recover_overflow(&self, lookup: &Context, payload: &Value) -> Option<Value> {
        if payload.get("code").and_then(Value::as_str) != Some("CONTEXT_WINDOW_EXCEEDED") {
            return None;
        }
        let id = payload
            .get("sessionId")
            .or_else(|| payload.get("agentId"))
            .and_then(Value::as_str)?;
        let agent = lookup.get::<AgentRegistry>()?.get(&session_id(id))?;
        let session = agent.session();
        let (provider, model) = last_request_route(&session)?;
        let policy = self.policy.resolve_target(&provider, &model);
        if !self.authorize_overflow_retry(&session, policy.max_overflow_retries) {
            return None;
        }
        let generation = session.surface().replace_generation;
        let _ = block_on(self.compact_if_needed(agent.as_ref(), CompactionTrigger::ContextOverflow));
        if session.surface().replace_generation <= generation {
            return None;
        }
        self.record_overflow_retry(&session);
        Some(serde_json::json!({ "kind": "retry" }))
    }

    fn authorize_overflow_retry(&self, session: &Session, max_retries: u32) -> bool {
        let assistant_seq = last_assistant_seq(session);
        let mut map = self.overflow_retries.lock().expect("overflow retries");
        let state = map.entry(session.id().as_str().to_string()).or_default();
        if assistant_seq > state.assistant_seq {
            state.retries = 0;
            state.assistant_seq = assistant_seq;
        }
        state.retries < max_retries
    }

    fn record_overflow_retry(&self, session: &Session) {
        let assistant_seq = last_assistant_seq(session);
        let mut map = self.overflow_retries.lock().expect("overflow retries");
        let state = map.entry(session.id().as_str().to_string()).or_default();
        state.retries += 1;
        state.assistant_seq = assistant_seq;
    }

    async fn resolve_pressure_window(
        &self,
        session: &Session,
    ) -> Result<Option<(String, String, u32)>, ManualCompactionError> {
        let Some((provider, model)) = last_request_route(session) else {
            return Ok(None);
        };
        let target = format!("{provider}/{model}");
        let Some(llm) = self
            .llm
            .clone()
            .or_else(|| self.lookup.get::<LlmRuntime>())
        else {
            return Err(missing_capacity(&target));
        };
        let info = llm
            .resolve_model_info(&provider, &model)
            .await
            .map_err(|error| ManualCompactionError::PressureConfig {
                target: target.clone(),
                message: error.to_string(),
            })?;
        let Some(context) = info.context else {
            return Err(missing_capacity(&target));
        };
        Ok(Some((provider, model, context.context_window)))
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
        let meter = self.meter.as_deref();
        let target = last_request_route(&session);
        let effective = target
            .as_ref()
            .map(|(provider, model)| self.policy.resolve_target(provider, model))
            .unwrap_or_else(|| self.policy.default_target());
        let retain_tokens = if force {
            0
        } else {
            let Some((provider, model, window)) = self.resolve_pressure_window(&session).await?
            else {
                return Ok(None);
            };
            let routed = self.policy.resolve_target(&provider, &model);
            let (threshold, retain_tokens) = compact_spec(&routed, &provider, &model, window)?;
            let total_tokens = meter
                .map(|meter| meter.estimate_session(&session) as u64)
                .unwrap_or(0);
            if total_tokens < threshold {
                return Ok(None);
            }
            retain_tokens
        };
        let Some((start, end, shadowed)) =
            select_compactable_span(&session, meter, retain_tokens)
        else {
            return Ok(None);
        };
        if !tool_pairing_balanced_before(&session, start).unwrap_or(false)
            || !tool_pairing_balanced_after(&session, end).unwrap_or(false)
        {
            return Ok(None);
        }
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
            .map_err(|_| ManualCompactionError::Commit)?;
        let snapshot = session.surface().nodes;
        let shadowed_token_count = self
            .meter
            .as_ref()
            .map(|meter| {
                shadowed
                    .iter()
                    .map(|seq| node_tokens(&session, meter, *seq))
                    .sum()
            })
            .unwrap_or(0);
        let llm = self
            .llm
            .clone()
            .or_else(|| self.lookup.get::<LlmRuntime>());
        let summarized = match summarize_with_llm(
            llm.as_deref(),
            &effective,
            &session,
            &shadowed,
        )
        .await
        {
            Ok(summarized) => summarized,
            Err(error) => {
                close_failed(
                    &session,
                    compaction_id,
                    source_command_id,
                    error,
                )?;
                return Err(ManualCompactionError::Summary);
            }
        };
        let framed = frame_summary(&summarized.summary);
        if let Some(meter) = self.meter.as_deref() {
            let framed_tokens = meter.estimate_message(&Message::User(UserMessage::from_parts(
                framed.clone(),
                MessageSource::plugin("dsh-compaction-basic"),
            ))) as u64;
            if framed_tokens >= shadowed_token_count {
                let error = format!(
                    "summary is not smaller than the shadowed content ({framed_tokens} estimated framed tokens >= {shadowed_token_count})"
                );
                close_failed(
                    &session,
                    compaction_id,
                    source_command_id,
                    error,
                )?;
                return Err(ManualCompactionError::Summary);
            }
        }
        if agent.is_cancelled() {
            close_failed(
                &session,
                compaction_id,
                source_command_id,
                "manual compaction was cancelled".into(),
            )?;
            return Err(ManualCompactionError::Cancelled);
        }
        if session.surface().nodes != snapshot {
            close_failed(
                &session,
                compaction_id,
                source_command_id,
                "the compacted history changed during manual compaction".into(),
            )?;
            return Err(ManualCompactionError::Changed);
        }
        let summary_event = session
            .append(
                SessionEventData::CompactionSummary {
                    compaction_id: compaction_id.clone(),
                    source_command_id: source_command_id.clone(),
                    summary: summarized.summary.clone(),
                    raw_output: Some(summarized.raw_output.clone()),
                    llm_stream_call: Some(true),
                    shadowed_range: serde_json::json!({ "start": start, "end": end }),
                    shadowed_seqs: shadowed.clone(),
                    shadowed_token_count,
                    provider: summarized.provider.clone(),
                    model: summarized.model.clone(),
                    max_tokens: Some(summarized.max_tokens),
                    usage: summarized.usage.clone(),
                },
                None,
            )
            .map_err(|_| ManualCompactionError::Commit)?;
        let summary = framed;
        let mut cited: Vec<u64> = Vec::new();
        cited.push(start_event.seq);
        cited.push(summary_event.seq);
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
            .map_err(|_| ManualCompactionError::Commit)?;
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
            .map_err(|_| ManualCompactionError::Commit)?;
        if let Some(persistence) = self.lookup.get::<PersistenceRuntime>() {
            if persistence.save(session.as_ref()).await.is_err() {
                return Err(ManualCompactionError::Persistence);
            }
        }
        Ok(Some(CompactionResult {
            shadowed_seqs: shadowed,
            shadowed_token_count,
            summary,
            summary_seq: summary_event.seq,
        }))
    }
}

fn close_failed(
    session: &Session,
    compaction_id: String,
    source_command_id: Option<String>,
    error: String,
) -> Result<(), ManualCompactionError> {
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
        .map(|_| ())
        .map_err(|_| ManualCompactionError::Commit)
}

fn missing_capacity(target: &str) -> ManualCompactionError {
    ManualCompactionError::PressureConfig {
        target: target.to_string(),
        message: format!(
            "compaction-basic: no context capacity for {target}; configure contextWindow on that adapter model"
        ),
    }
}

fn compact_spec(
    policy: &ResolvedTargetPolicy,
    provider: &str,
    model: &str,
    window: u32,
) -> Result<(u64, u64), ManualCompactionError> {
    let target = format!("{provider}/{model}");
    if window == 0 {
        return Err(ManualCompactionError::PressureConfig {
            target,
            message: format!(
                "BasicCompactionConfig: contextWindow ({window}) must be a positive integer"
            ),
        });
    }
    let threshold_tokens = ((window as f64) * policy.threshold_ratio).floor() as u64;
    let retain_tokens = match policy.retention {
        ResolvedRetention::Tokens(tokens) => tokens,
        ResolvedRetention::Ratio(ratio) => ((window as f64) * ratio).floor() as u64,
    };
    if retain_tokens >= threshold_tokens {
        return Err(ManualCompactionError::PressureConfig {
            target,
            message: format!(
                "BasicCompactionConfig: {provider}/{model} retainTokens ({retain_tokens}) must be less than threshold tokens {threshold_tokens}"
            ),
        });
    }
    Ok((threshold_tokens, retain_tokens))
}

fn select_compactable_span(
    session: &Session,
    meter: Option<&TokenMeter>,
    retain_tokens: u64,
) -> Option<(u64, u64, Vec<u64>)> {
    let nodes = session.surface().nodes;
    if nodes.is_empty() {
        return None;
    }
    let mut accumulated = 0u64;
    let mut keep_from = nodes.len();
    for index in (0..nodes.len()).rev() {
        accumulated += meter
            .map(|meter| node_tokens(session, meter, nodes[index]))
            .unwrap_or(0);
        keep_from = index;
        if accumulated >= retain_tokens {
            break;
        }
    }
    if keep_from == 0 {
        return None;
    }
    while keep_from > 0 {
        match tool_pairing_balanced_before(session, nodes[keep_from]) {
            Ok(true) => break,
            Ok(false) => keep_from -= 1,
            Err(_) => return None,
        }
    }
    if keep_from == 0 {
        return None;
    }
    let start = nodes[0];
    let end = nodes[keep_from - 1];
    Some((start, end, nodes[..=keep_from - 1].to_vec()))
}

fn node_tokens(session: &Session, meter: &TokenMeter, seq: u64) -> u64 {
    session
        .events()
        .iter()
        .find(|event| event.seq == seq)
        .and_then(|event| derive_event_message(&event.data))
        .map(|message| meter.estimate_message(&message) as u64)
        .unwrap_or(0)
}

/// Model-facing framing that precedes the `<compacted-summary>` block.
pub const CHECKPOINT_PREAMBLE: &str =
    "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// Compaction directive appended after the replayed region, matching TypeScript.
pub const COMPACTION_INSTRUCTION: &str = "You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.\n\nOutput EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write \"(none)\" for an empty section — never drop a section.\n\n## Primary Request and Intent\n- [the user's original and evolving goals; quote verbatim where the exact wording matters]\n\n## Key Technical Concepts\n- [technologies, frameworks, patterns, and conventions in play]\n\n## Files and Code\n- [exact path: why it matters, key changes or snippets]\n\n## Errors and Fixes\n- [error: how it was resolved, plus any related user feedback]\n\n## Pending Jobs\n- [explicitly requested work not yet completed]\n\n## Current Work\n- [precisely what was in progress at this checkpoint]\n\n## Next Step\n- [the single next action, directly in line with the most recent request, or \"(none)\"]\n\n## Critical Context\n- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]\n\nRules:\n- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.\n- Capture user feedback and explicit instructions faithfully, especially corrections.\n- Do NOT mention this summarization request or that the context was compacted.\n- Output only the checkpoint text: do not call any tool or take any other action.\n- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure.";

struct SummarizedText {
    summary: Vec<ContentBlock>,
    raw_output: Vec<ContentBlock>,
    provider: String,
    model: String,
    max_tokens: u32,
    usage: Option<dsh_llm::TokenUsage>,
}

fn frame_summary(summary: &[ContentBlock]) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::text(format!(
        "{CHECKPOINT_PREAMBLE}\n\n<compacted-summary>"
    ))];
    blocks.extend(summary.iter().cloned());
    blocks.push(ContentBlock::text("</compacted-summary>"));
    blocks
}

fn last_assistant_seq(session: &Session) -> u64 {
    session
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match event.data {
            SessionEventData::AssistantMessage { .. } => Some(event.seq),
            _ => None,
        })
        .unwrap_or(0)
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
    policy: &ResolvedTargetPolicy,
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
            provider: provider.clone(),
            model: model.clone(),
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
    let usage = assembler.take_usage();
    let raw_output = assembler.finish();
    let summary: Vec<ContentBlock> = raw_output
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(ContentBlock::text(text.clone())),
            _ => None,
        })
        .collect();
    if !summary.iter().any(|block| match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        _ => false,
    }) {
        return Err("summarization produced no text summary content".into());
    }
    Ok(SummarizedText {
        summary,
        raw_output,
        provider,
        model,
        max_tokens: policy.max_tokens,
        usage,
    })
}

#[async_trait]
impl CompactionEngine for BasicCompactionEngine {
    async fn compact_if_needed(
        &self,
        agent: &dyn Agent,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionResult>, ManualCompactionError> {
        if trigger == CompactionTrigger::ContextOverflow {
            if let Some(pruner) = self.lookup.get::<dsh_tool_result_pruner::ToolResultPruner>() {
                let _ = pruner.prune_session(&agent.session(), self.meter.as_deref());
            }
            return self.compact_session(agent, true, None).await;
        }
        let session = agent.session();
        let Some((provider, model, window)) = self.resolve_pressure_window(&session).await? else {
            return Ok(None);
        };
        let effective = self.policy.resolve_target(&provider, &model);
        let (threshold, _) = compact_spec(&effective, &provider, &model, window)?;
        let mut total_tokens = self
            .meter
            .as_ref()
            .map(|meter| meter.estimate_session(&session) as u64)
            .unwrap_or(0);
        if total_tokens < threshold {
            return Ok(None);
        }
        let mut last = None;
        for _attempt in 0..=effective.compaction_retries {
            match self.compact_session(agent, false, None).await? {
                None => return Ok(last),
                Some(result) => {
                    last = Some(result);
                    total_tokens = self
                        .meter
                        .as_ref()
                        .map(|meter| meter.estimate_session(&session) as u64)
                        .unwrap_or(0);
                    if total_tokens < threshold {
                        return Ok(last);
                    }
                }
            }
        }
        Err(ManualCompactionError::StillAbove {
            attempts: effective.compaction_retries + 1,
            tokens: total_tokens,
            threshold,
        })
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
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentStatus, Inbox, InboxTarget,
    };
    use dsh_llm::{
        call_id, AssistantMessage, LlmAdapter, LlmError, LlmFailure, ToolResultMessage,
    };
    use dsh_session::{session_id, Session, SessionStore};
    use futures::stream;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct ScriptedSummarizer {
        text: String,
        last: Mutex<Option<LlmRequest>>,
        fail: bool,
        context_window: Option<u32>,
        during: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
            if let Some(hook) = self.during.lock().expect("during").take() {
                hook();
            }
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

        async fn resolve_model(
            &self,
            provider: &str,
            model: &str,
        ) -> std::result::Result<dsh_llm::LlmResolvedModelInfo, LlmError> {
            Ok(dsh_llm::LlmResolvedModelInfo {
                context: self.context_window.map(|context_window| {
                    dsh_llm::LlmModelContext { context_window }
                }),
                ..dsh_llm::LlmResolvedModelInfo::identity(provider, model)
            })
        }
    }

    fn scripted_engine_with_window(
        text: &str,
        context_window: Option<u32>,
    ) -> (BasicCompactionEngine, Arc<ScriptedSummarizer>) {
        let adapter = Arc::new(ScriptedSummarizer {
            text: text.into(),
            last: Mutex::new(None),
            fail: false,
            context_window,
            during: Mutex::new(None),
        });
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(4))),
            lookup: Context::new(),
            llm: Some(Arc::new(LlmRuntime::new(Arc::clone(&adapter) as Arc<dyn LlmAdapter>))),
            overflow_retries: Mutex::new(HashMap::new()),
        };
        (engine, adapter)
    }

    fn scripted_engine(text: &str) -> (BasicCompactionEngine, Arc<ScriptedSummarizer>) {
        scripted_engine_with_window(text, None)
    }

    struct StubAgent {
        session: Arc<Session>,
        inbox: Arc<Inbox>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    impl StubAgent {
        fn new(session: Arc<Session>) -> Self {
            Self {
                session,
                inbox: Arc::new(Inbox::default()),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
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
        fn cancel(&self, _: AgentCancelCause) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
        }
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

    fn bulky(label: &str) -> String {
        format!("{label} {}", "context ".repeat(40))
    }

    fn compactable_session(id: &str) -> Arc<Session> {
        let session = Arc::new(Session::new(session_id(id)));
        for label in ["alpha", "bravo", "charlie", "delta"] {
            append_user(&session, &bulky(label));
        }
        session
    }

    #[tokio::test]
    async fn replace_removes_shadowed_nodes_from_derive() {
        let session = compactable_session("c");
        let agent = StubAgent::new(Arc::clone(&session));
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
                    .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("alpha "))),
                _ => false,
            }));
        let summary = session
            .events()
            .into_iter()
            .find_map(|event| match event.data {
                SessionEventData::CompactionSummary {
                    provider,
                    model,
                    llm_stream_call,
                    raw_output,
                    summary,
                    max_tokens,
                    ..
                } => Some((provider, model, llm_stream_call, raw_output, summary, max_tokens)),
                _ => None,
            })
            .expect("compaction/summary");
        assert_eq!(summary.0, "replay");
        assert_eq!(summary.1, "script");
        assert_eq!(summary.2, Some(true));
        assert_eq!(summary.3, Some(vec![ContentBlock::text("condensed checkpoint")]));
        assert_eq!(summary.4, vec![ContentBlock::text("condensed checkpoint")]);
        assert_eq!(summary.5, Some(8192));
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
        let agent = StubAgent::new(Arc::clone(&session));
        let engine = BasicCompactionEngine::new(CompactionPolicy::resolve(None).unwrap());
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Busy));
    }

    struct StubFactory;

    impl AgentFactory for StubFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(StubAgent::new(session))
        }
    }

    fn overflow_host(policy: CompactionPolicy) -> (Context, Arc<Session>, dsh_agent::AgentHandle) {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        ctx.provide(Arc::new(agents)).unwrap();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        let adapter = Arc::new(ScriptedSummarizer {
            text: "overflow checkpoint".into(),
            last: Mutex::new(None),
            fail: false,
            context_window: None,
            during: Mutex::new(None),
        });
        ctx.provide(Arc::new(LlmRuntime::new(
            Arc::clone(&adapter) as Arc<dyn LlmAdapter>,
        )))
        .unwrap();
        BasicCompactionEngine::install(&ctx, policy).unwrap();
        let session = Arc::new(Session::new(session_id("overflow")));
        session
            .append(
                SessionEventData::RequestHeader {
                    header: serde_json::json!({
                        "config": { "provider": "replay", "model": "script" }
                    }),
                    reason: "initial".into(),
                },
                None,
            )
            .unwrap();
        for label in ["alpha", "bravo", "charlie", "delta"] {
            append_user(&session, &bulky(label));
        }
        let handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        (ctx, session, handle)
    }

    fn overflow_payload(session: &Session) -> Value {
        serde_json::json!({
            "code": "CONTEXT_WINDOW_EXCEEDED",
            "agentId": session.id().as_str(),
        })
    }

    fn recover_overflow(ctx: &Context, session: &Session) -> bool {
        ctx.waterfall(
            "agent/request-error",
            overflow_payload(session),
            |payload| payload,
        )
        .unwrap()
        .get("kind")
        .and_then(Value::as_str)
        == Some("retry")
    }

    #[test]
    fn install_provides_compaction() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        BasicCompactionEngine::install(&ctx, CompactionPolicy::resolve(None).unwrap()).unwrap();
        assert!(ctx.has_service("compaction"));
    }

    #[test]
    fn overflow_without_agent_or_route_delegates() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(AgentRegistry::new())).unwrap();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        BasicCompactionEngine::install(&ctx, CompactionPolicy::resolve(None).unwrap()).unwrap();
        let missing_agent = ctx
            .waterfall(
                "agent/request-error",
                serde_json::json!({ "code": "CONTEXT_WINDOW_EXCEEDED" }),
                |payload| payload,
            )
            .unwrap();
        assert_ne!(missing_agent.get("kind").and_then(Value::as_str), Some("retry"));

        let session = Arc::new(Session::new(session_id("headerless")));
        append_user(&session, &bulky("only"));
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        let ctx = Context::new();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        ctx.provide(Arc::new(agents)).unwrap();
        ctx.provide(Arc::new(TokenMeter::new(4))).unwrap();
        BasicCompactionEngine::install(&ctx, CompactionPolicy::resolve(None).unwrap()).unwrap();
        let _handle = ctx
            .service::<AgentRegistry>()
            .unwrap()
            .create(Arc::clone(&session))
            .unwrap();
        assert!(!recover_overflow(&ctx, &session));
    }

    #[test]
    fn overflow_retries_only_after_replacement_and_honors_cap() {
        let (ctx, session, _handle) = overflow_host(CompactionPolicy::resolve(None).unwrap());
        let generation = session.surface().replace_generation;
        assert!(recover_overflow(&ctx, &session));
        assert!(session.surface().replace_generation > generation);
        assert!(session
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::CompactionSummary { .. })));
        assert!(!recover_overflow(&ctx, &session));
    }

    #[test]
    fn overflow_zero_retries_disables_recovery() {
        let (ctx, session, _handle) = overflow_host(
            CompactionPolicy::resolve(Some(&serde_json::json!({ "maxOverflowRetries": 0 })))
                .unwrap(),
        );
        assert!(!recover_overflow(&ctx, &session));
        assert!(!session
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::CompactionStart { .. })));
    }

    #[test]
    fn overflow_model_policy_overrides_retry_cap() {
        let (ctx, session, _handle) = overflow_host(
            CompactionPolicy::resolve(Some(&serde_json::json!({
                "maxOverflowRetries": 2,
                "modelPolicies": [{
                    "provider": "replay",
                    "model": "script",
                    "maxOverflowRetries": 0
                }]
            })))
            .unwrap(),
        );
        assert!(!recover_overflow(&ctx, &session));
    }

    #[test]
    fn overflow_resets_after_a_later_assistant_message() {
        let (ctx, session, _handle) = overflow_host(CompactionPolicy::resolve(None).unwrap());
        assert!(recover_overflow(&ctx, &session));
        session
            .append(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::text("recovered")],
                        "replay",
                        "script",
                    ),
                    usage: None,
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        for label in ["echo", "foxtrot"] {
            append_user(&session, &bulky(label));
        }
        assert!(recover_overflow(&ctx, &session));
    }

    #[test]
    fn overflow_resets_on_idle_status() {
        let (ctx, session, _handle) = overflow_host(CompactionPolicy::resolve(None).unwrap());
        assert!(recover_overflow(&ctx, &session));
        ctx.emit("agent/status", serde_json::json!({ "status": "Idle" }));
        for label in ["echo", "foxtrot"] {
            append_user(&session, &bulky(label));
        }
        assert!(recover_overflow(&ctx, &session));
    }

    #[test]
    fn auto_false_skips_overflow_recovery() {
        let (ctx, session, _handle) = overflow_host(
            CompactionPolicy::resolve(Some(&serde_json::json!({ "auto": false }))).unwrap(),
        );
        assert!(!recover_overflow(&ctx, &session));
    }

    #[test]
    fn resolve_defaults_and_rejects_stale_or_inverted_ratios() {
        let policy = CompactionPolicy::resolve(None).unwrap();
        assert_eq!(policy.threshold_ratio, 0.8);
        assert_eq!(policy.retain_ratio, 0.16);
        assert_eq!(policy.max_tokens, 8192);
        assert_eq!(policy.compaction_retries, 1);
        assert_eq!(policy.max_overflow_retries, 1);
        assert!(policy.auto);
        assert!(policy.summarization_provider.is_empty());
        assert!(policy.model_policies.is_empty());
        let tokens_only = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.1,
            "retainTokens": 70
        })))
        .unwrap();
        assert_eq!(tokens_only.retain_tokens, Some(70));
        let merged = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.8,
            "retainRatio": 0.1,
            "modelPolicies": [{
                "provider": "small-provider",
                "model": "shared-id",
                "thresholdRatio": 0.5,
                "retainTokens": 120,
                "summarizationProvider": "policy-summary",
                "summarizationModel": "policy-summary",
                "maxTokens": 222
            }]
        })))
        .unwrap();
        let small = merged.resolve_target("small-provider", "shared-id");
        assert_eq!(small.threshold_ratio, 0.5);
        assert_eq!(small.retention, ResolvedRetention::Tokens(120));
        assert_eq!(small.summarization_provider, "policy-summary");
        assert_eq!(small.max_tokens, 222);
        let other = merged.resolve_target("large-provider", "shared-id");
        assert_eq!(other.threshold_ratio, 0.8);
        assert_eq!(other.retention, ResolvedRetention::Ratio(0.1));
        let unknown = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdMessages": 40
        })))
        .unwrap_err();
        assert!(unknown.contains("unknown key \"thresholdMessages\""), "{unknown}");
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
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "modelPolicies": [{
                "provider": "replay",
                "model": "script",
                "thresholdRatio": 0.1
            }]
        })))
        .is_err());
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "modelPolicies": [
                { "provider": "replay", "model": "script" },
                { "provider": "replay", "model": "script" }
            ]
        })))
        .is_err());
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "modelPolicies": { "replay": { "retainTokens": 10 } }
        })))
        .is_err());
        assert!(CompactionPolicy::resolve(Some(&serde_json::json!({
            "summarizationProvider": "replay"
        })))
        .is_err());
    }

    #[tokio::test]
    async fn pressure_skips_without_a_context_window() {
        let session = Arc::new(Session::new(session_id("nowindow")));
        for text in ["aaaa", "bbbb", "cccc", "dddd"] {
            append_user(&session, text);
        }
        let agent = StubAgent::new(Arc::clone(&session));
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "thresholdRatio": 0.1,
                "retainRatio": 0.01,
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: Some(Arc::new(TokenMeter::new(1))),
            lookup: Context::new(),
            llm: None,
            overflow_retries: Mutex::new(HashMap::new()),
        };
        assert!(engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn pressure_fails_without_adapter_capacity() {
        let session = Arc::new(Session::new(session_id("nocap")));
        session
            .append(
                SessionEventData::RequestHeader {
                    header: serde_json::json!({
                        "config": { "provider": "replay", "model": "script" }
                    }),
                    reason: "initial".into(),
                },
                None,
            )
            .unwrap();
        for text in ["aaaa", "bbbb", "cccc", "dddd"] {
            append_user(&session, text);
        }
        let agent = StubAgent::new(Arc::clone(&session));
        let (engine, _) = scripted_engine("ok");
        let err = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap_err();
        match err {
            ManualCompactionError::PressureConfig { target, message } => {
                assert_eq!(target, "replay/script");
                assert!(
                    message.contains("no context capacity for replay/script"),
                    "{message}"
                );
            }
            other => panic!("expected PressureConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pressure_compacts_when_tokens_cross_the_window_ratio() {
        let session = Arc::new(Session::new(session_id("window")));
        session
            .append(
                SessionEventData::RequestContext {
                    provider: "deepseek-official".into(),
                    model: "deepseek-v4-flash".into(),
                    context_window: None,
                },
                None,
            )
            .unwrap();
        for label in ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"] {
            append_user(&session, &bulky(label));
        }
        let agent = StubAgent::new(Arc::clone(&session));
        let (mut engine, _) = scripted_engine_with_window("ok", Some(500));
        engine.policy = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.5,
            "retainRatio": 0.1,
            "summarizationProvider": "replay",
            "summarizationModel": "script"
        })))
        .unwrap();
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
        let agent = StubAgent::new(Arc::clone(&session));
        let adapter = Arc::new(ScriptedSummarizer {
            text: String::new(),
            last: Mutex::new(None),
            fail: true,
            context_window: None,
            during: Mutex::new(None),
        });
        let engine = BasicCompactionEngine {
            policy: CompactionPolicy::resolve(Some(&serde_json::json!({
                "summarizationProvider": "replay",
                "summarizationModel": "script"
            })))
            .unwrap(),
            meter: None,
            lookup: Context::new(),
            llm: Some(Arc::new(LlmRuntime::new(
                Arc::clone(&adapter) as Arc<dyn LlmAdapter>
            ))),
            overflow_retries: Mutex::new(HashMap::new()),
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

    #[tokio::test]
    async fn framed_summary_must_be_smaller_than_shadowed_history() {
        let session = Arc::new(Session::new(session_id("noshrink")));
        append_user(&session, "a");
        append_user(&session, "b");
        append_user(&session, "c");
        let agent = StubAgent::new(Arc::clone(&session));
        let verbose = (0..80)
            .map(|index| format!("verbose {index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let (engine, _) = scripted_engine(&verbose);
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Summary));
        let end = session.events().into_iter().rev().find_map(|event| match event.data {
            SessionEventData::CompactionEnd { error, .. } => error,
            _ => None,
        });
        let error = end.expect("compaction/end.error");
        assert!(
            error.contains("summary is not smaller than the shadowed content"),
            "{error}"
        );
        assert!(!session.events().iter().any(|event| {
            matches!(event.data, SessionEventData::CompactionSummary { .. })
        }));
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

    #[tokio::test]
    async fn model_policy_routes_summarization_and_lowers_pressure() {
        let session = Arc::new(Session::new(session_id("policy")));
        session
            .append(
                SessionEventData::RequestContext {
                    provider: "deepseek-official".into(),
                    model: "deepseek-v4-flash".into(),
                    context_window: None,
                },
                None,
            )
            .unwrap();
        for label in ["alpha", "bravo", "charlie", "delta"] {
            append_user(&session, &bulky(label));
        }
        let agent = StubAgent::new(Arc::clone(&session));
        let (mut engine, adapter) = scripted_engine_with_window("policy summary", Some(2000));
        engine.policy = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.95,
            "retainRatio": 0.1,
            "maxTokens": 111,
            "summarizationProvider": "replay",
            "summarizationModel": "script",
            "modelPolicies": [{
                "provider": "deepseek-official",
                "model": "deepseek-v4-flash",
                "thresholdRatio": 0.15,
                "retainRatio": 0.05,
                "summarizationProvider": "policy-summary",
                "summarizationModel": "policy-summary",
                "maxTokens": 222
            }]
        })))
        .unwrap();
        let default_threshold = ((2000.0_f64) * 0.95).floor() as u64;
        let total = engine
            .meter
            .as_ref()
            .expect("meter")
            .estimate_session(session.as_ref()) as u64;
        assert!(
            total < default_threshold,
            "fixture must sit below the default threshold ({total} < {default_threshold})"
        );
        let result = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap()
            .expect("model policy pressure");
        assert!(!result.shadowed_seqs.is_empty());
        let request = adapter.last.lock().expect("last").clone().expect("request");
        assert_eq!(request.config.provider, "policy-summary");
        assert_eq!(request.config.model, "policy-summary");
        assert_eq!(request.config.max_tokens, Some(222));
    }

    fn append_closed_tool_step(session: &Session, call: &str) {
        session
            .append(
                SessionEventData::AssistantMessage {
                    turn: 1,
                    step: 1,
                    message: AssistantMessage::model(
                        vec![ContentBlock::ToolCall {
                            id: call_id(call),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        }],
                        "mock",
                        "mock",
                    ),
                    usage: None,
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
        session
            .append(
                SessionEventData::ToolResult {
                    turn: 1,
                    step: 1,
                    message: ToolResultMessage::new(
                        call_id(call),
                        vec![ContentBlock::text("done")],
                        false,
                    ),
                },
                Some(SurfaceOp::append()),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn retain_cut_snaps_headward_to_keep_tool_pairs() {
        let session = Arc::new(Session::new(session_id("pair")));
        append_user(&session, &bulky("alpha"));
        append_user(&session, &bulky("bravo"));
        append_closed_tool_step(&session, "c1");
        let agent = StubAgent::new(Arc::clone(&session));
        let (engine, _) = scripted_engine("ok");
        let result = engine.compact_now(&agent, None).await.unwrap().unwrap();
        let messages = session.derive_messages();
        let mut calls = std::collections::BTreeSet::new();
        for message in &messages {
            match message {
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let ContentBlock::ToolCall { id, .. } = block {
                            calls.insert(id.as_str().to_string());
                        }
                    }
                }
                Message::Tool(tool) => {
                    let id = tool.tool_call_id().expect("call id");
                    assert!(calls.contains(id), "orphaned tool result {id}");
                }
                _ => {}
            }
        }
        assert!(calls.contains("c1"));
        assert!(!result.shadowed_seqs.is_empty());
        let call_seq = session
            .events()
            .into_iter()
            .find_map(|event| match event.data {
                SessionEventData::AssistantMessage { .. } => Some(event.seq),
                _ => None,
            })
            .expect("assistant");
        assert!(
            !result.shadowed_seqs.contains(&call_seq),
            "tool-call must stay in the retained tail {result:?}"
        );
    }

    #[tokio::test]
    async fn pressure_fails_when_retries_leave_session_above_threshold() {
        let session = Arc::new(Session::new(session_id("still-above")));
        session
            .append(
                SessionEventData::RequestContext {
                    provider: "replay".into(),
                    model: "script".into(),
                    context_window: None,
                },
                None,
            )
            .unwrap();
        for label in ["alpha", "bravo", "charlie", "delta"] {
            append_user(&session, &bulky(label));
        }
        let agent = StubAgent::new(Arc::clone(&session));
        let (mut engine, _) = scripted_engine_with_window("ok", Some(100));
        engine.policy = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.3,
            "retainRatio": 0.05,
            "compactionRetries": 0,
            "summarizationProvider": "replay",
            "summarizationModel": "script"
        })))
        .unwrap();
        let err = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap_err();
        match err {
            ManualCompactionError::StillAbove { attempts, .. } => assert_eq!(attempts, 1),
            other => panic!("expected StillAbove, got {other:?}"),
        }
        assert!(session
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::CompactionSummary { .. })));
    }

    #[tokio::test]
    async fn pressure_rejects_retain_tokens_at_or_above_threshold() {
        let session = Arc::new(Session::new(session_id("retain-too-big")));
        session
            .append(
                SessionEventData::RequestContext {
                    provider: "replay".into(),
                    model: "script".into(),
                    context_window: None,
                },
                None,
            )
            .unwrap();
        append_user(&session, &bulky("alpha"));
        let agent = StubAgent::new(Arc::clone(&session));
        let (mut engine, _) = scripted_engine_with_window("ok", Some(1000));
        engine.policy = CompactionPolicy::resolve(Some(&serde_json::json!({
            "thresholdRatio": 0.5,
            "retainTokens": 500,
            "summarizationProvider": "replay",
            "summarizationModel": "script"
        })))
        .unwrap();
        let err = engine
            .compact_if_needed(&agent, CompactionTrigger::Pressure)
            .await
            .unwrap_err();
        match err {
            ManualCompactionError::PressureConfig { message, .. } => {
                assert!(
                    message.contains("retainTokens (500) must be less than threshold tokens 500"),
                    "{message}"
                );
            }
            other => panic!("expected PressureConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abort_when_the_agent_is_cancelled_during_summary() {
        let session = compactable_session("cancel");
        let agent = StubAgent::new(Arc::clone(&session));
        let (engine, adapter) = scripted_engine("ok");
        *adapter.during.lock().expect("during") = Some(Arc::new({
            let cancelled = Arc::clone(&agent.cancelled);
            move || cancelled.store(true, std::sync::atomic::Ordering::SeqCst)
        }));
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Cancelled));
        assert!(session
            .derive_messages()
            .iter()
            .any(|message| match message {
                Message::User(user) => user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("alpha "))),
                _ => false,
            }));
    }

    #[tokio::test]
    async fn abort_when_the_surface_changes_during_summary() {
        let session = compactable_session("changed");
        let agent = StubAgent::new(Arc::clone(&session));
        let (engine, adapter) = scripted_engine("ok");
        *adapter.during.lock().expect("during") = Some(Arc::new({
            let session = Arc::clone(&session);
            move || append_user(&session, "late arrival")
        }));
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Changed));
        assert!(session
            .derive_messages()
            .iter()
            .any(|message| match message {
                Message::User(user) => user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("alpha "))),
                _ => false,
            }));
    }

    #[tokio::test]
    async fn persistence_failure_after_a_successful_replacement() {
        let session = compactable_session("persist");
        let agent = StubAgent::new(Arc::clone(&session));
        let (engine, _) = scripted_engine("ok");
        struct FailSave;
        #[async_trait]
        impl dsh_session_persistence::SessionStoreBackend for FailSave {
            async fn save(
                &self,
                _: &Session,
            ) -> std::result::Result<(), dsh_session_persistence::PersistenceError> {
                Err(dsh_session_persistence::PersistenceError::Format(
                    "disk full".into(),
                ))
            }
            async fn load(
                &self,
                _: &dsh_session::SessionId,
            ) -> std::result::Result<Session, dsh_session_persistence::PersistenceError> {
                Err(dsh_session_persistence::PersistenceError::Format("nope".into()))
            }
            async fn list_ids(
                &self,
            ) -> std::result::Result<Vec<dsh_session::SessionId>, dsh_session_persistence::PersistenceError>
            {
                Ok(Vec::new())
            }
        }
        engine
            .lookup
            .provide(Arc::new(dsh_session_persistence::PersistenceRuntime::new(
                Arc::new(FailSave),
            )))
            .unwrap();
        let err = engine.compact_now(&agent, None).await.unwrap_err();
        assert!(matches!(err, ManualCompactionError::Persistence));
        assert!(session
            .events()
            .iter()
            .any(|event| matches!(event.data, SessionEventData::CompactionSummary { .. })));
    }
}
