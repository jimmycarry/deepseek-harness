//! Codex command-hook bridge on harness interception points.

mod config;

pub use config::{
    parse_codex_config, CodexHookConfig, ParsedCodexConfig, SkippedHook, CODEX_EVENTS,
};

use dsh_agent::{Agent, AgentRegistry};
use dsh_cordis::{Context, Result};
use dsh_hook_protocol::{
    append_hook_invoked, append_hook_result, create_detached_runs, matches_matcher,
    merge_hook_outputs, run_hook, DetachedRuns, HookDialect, HookInvocation, HookOutput,
    HookResultRecord, HookShellRequest, HookShellResult, MatcherGroup, MatcherMode, MergedDecision,
    MergedHookOutcome, RunHookOptions, DEFAULT_HOOK_TIMEOUT_MS, DEFAULT_STDERR_SUMMARY_MAX_CHARS,
};
use dsh_llm::{ContentBlock, MessageSource, UserMessage};
use dsh_session::{session_id, SessionEventData};
use dsh_session_persistence::{PersistenceRuntime, SessionLocation};
use dsh_shell::{resolve, ShellRequest, ShellRuntime};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Plugin role name matching TypeScript `export const name`.
pub fn name() -> &'static str {
    "hooks-codex"
}

/// Plugin config: where the Codex hooks.json lives plus the model name for payloads.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to a Codex `hooks.json`.
    pub config_path: String,
    /// Model name stamped on every payload.
    pub model: String,
    /// Default per-hook timeout in ms when a hook sets none.
    pub default_timeout_ms: u64,
    /// Character cap for the `hook/result` event's persisted stderr summary.
    pub stderr_summary_max_chars: usize,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Missing `configPath`, or a non-positive `stderrSummaryMaxChars`.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let object = config
            .and_then(Value::as_object)
            .ok_or_else(|| "hooks-codex: configPath is required".to_string())?;
        let config_path = object
            .get("configPath")
            .and_then(Value::as_str)
            .ok_or_else(|| "hooks-codex: configPath is required".to_string())?
            .to_string();
        let default_timeout_ms = match object.get("defaultTimeoutMs") {
            None => DEFAULT_HOOK_TIMEOUT_MS,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| "hooks-codex: defaultTimeoutMs must be a number".to_string())?,
        };
        let stderr_summary_max_chars = match object.get("stderrSummaryMaxChars") {
            None => DEFAULT_STDERR_SUMMARY_MAX_CHARS,
            Some(value) => {
                let Some(number) = value.as_u64() else {
                    return Err(
                        "hooks-codex: stderrSummaryMaxChars must be a positive integer".into(),
                    );
                };
                if number < 1 {
                    return Err(
                        "hooks-codex: stderrSummaryMaxChars must be a positive integer".into(),
                    );
                }
                number as usize
            }
        };
        Ok(Self {
            config_path,
            model: object
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            default_timeout_ms,
            stderr_summary_max_chars,
        })
    }
}

fn block_on_async<T>(fut: impl std::future::Future<Output = T>) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => futures::executor::block_on(fut),
    }
}

async fn run_shell(
    shell: Arc<ShellRuntime>,
    request: HookShellRequest,
) -> std::result::Result<HookShellResult, String> {
    let spec = resolve(ShellRequest {
        command: request.command,
        cwd: request.cwd,
        timeout_ms: Some(request.timeout_ms),
        stdin: Some(request.stdin),
        extra_env: request.env,
        ..Default::default()
    });
    let result = shell.run(spec).await.map_err(|error| error.to_string())?;
    Ok(HookShellResult {
        exit_code: result.exit_code,
        stdout: result.stdout.text,
        stderr: result.stderr.text,
    })
}

fn last_turn(agent: Option<&dyn Agent>) -> u32 {
    let Some(agent) = agent else {
        return 0;
    };
    agent
        .session()
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match event.data {
            SessionEventData::TurnStart { turn } => Some(turn),
            _ => None,
        })
        .unwrap_or(0)
}

fn blocks_to_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn transcript_path(ctx: &Context, agent: Option<&dyn Agent>) -> Value {
    let Some(agent) = agent else {
        return Value::Null;
    };
    ctx.get::<PersistenceRuntime>()
        .and_then(|persistence| persistence.locate(agent.session().id()))
        .map(|location| match location {
            SessionLocation::Jsonl { path } => json!(path.display().to_string()),
        })
        .unwrap_or(Value::Null)
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn base(ctx: &Context, agent: Option<&dyn Agent>, event: &str, model: &str) -> Value {
    json!({
        "session_id": agent.map(|item| item.session().id().as_str().to_string()).unwrap_or_default(),
        "transcript_path": transcript_path(ctx, agent),
        "cwd": agent
            .and_then(|item| item.session().header().cwd.clone())
            .unwrap_or_else(process_cwd),
        "hook_event_name": event,
        "model": model,
        "permission_mode": "default",
    })
}

fn turn_base(ctx: &Context, agent: Option<&dyn Agent>, event: &str, model: &str) -> Value {
    let mut value = base(ctx, agent, event, model);
    value["turn_id"] = json!(last_turn(agent).to_string());
    value
}

fn command_of(args: &Value) -> String {
    args.get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn context_from(merged: &MergedHookOutcome) -> Option<UserMessage> {
    if merged.additional_context.is_empty() {
        return None;
    }
    Some(UserMessage::from_parts(
        merged
            .additional_context
            .iter()
            .map(ContentBlock::text)
            .collect(),
        MessageSource::plugin("hooks-codex"),
    ))
}

fn prepend_context(ours: UserMessage, payload: &mut Value) {
    let mut contexts = payload
        .get("additionalContexts")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<UserMessage>>(value).ok())
        .unwrap_or_default();
    contexts.insert(0, ours);
    payload["additionalContexts"] = serde_json::to_value(contexts).unwrap_or(json!([]));
}

fn messages_text(payload: &Value) -> String {
    payload
        .get("messages")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<UserMessage>>(value).ok())
        .map(|messages| {
            messages
                .iter()
                .map(|message| blocks_to_text(&message.content))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Install the Codex hook bridge.
///
/// # Errors
/// Required `ctx.shell` is missing, or config validation fails before the file is read.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let _ = ctx.service::<ShellRuntime>()?;
    apply(ctx, resolved)
}

fn apply(ctx: &Context, config: Config) -> Result<()> {
    let parsed = match std::fs::read_to_string(&config.config_path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()))
        .and_then(|raw| parse_codex_config(&raw))
    {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(
                "hooks-codex: could not load hook config \"{}\": {error} — no hooks registered",
                config.config_path
            );
            return Ok(());
        }
    };
    for skipped in &parsed.skipped {
        tracing::warn!(
            "hooks-codex: skipping {} on {} (only sync command hooks run)",
            skipped.reason,
            skipped.event
        );
    }

    let groups = Arc::new(parsed.config);
    let detached = create_detached_runs();
    let handler_counter = Arc::new(AtomicU64::new(0));
    let last_agent = Arc::new(Mutex::new(None::<String>));
    let drain = detached.clone();
    ctx.effect("hooks-codex: drain detached hook runs", move || {
        move || {
            block_on_async(drain.drain());
        }
    })?;

    let lookup = ctx.clone();
    let groups_start = Arc::clone(&groups);
    let detached_start = detached.clone();
    let counter_start = Arc::clone(&handler_counter);
    let config_start = config.clone();
    ctx.on("agent/session-start", move |payload| {
        let Some(id) = payload.get("agentId").and_then(Value::as_str) else {
            return;
        };
        let Some(agents) = lookup.get::<AgentRegistry>() else {
            return;
        };
        let Some(agent) = agents.get(&session_id(id)) else {
            return;
        };
        let source = payload
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut body = base(
            &lookup,
            Some(agent.as_ref()),
            "SessionStart",
            &config_start.model,
        );
        body["source"] = json!(source);
        let groups = Arc::clone(&groups_start);
        let detached = detached_start.clone();
        let counter = Arc::clone(&counter_start);
        let shell = lookup.get::<ShellRuntime>();
        let config = config_start.clone();
        let lookup = lookup.clone();
        detached_start.track(async move {
            let Some(shell) = shell else {
                return;
            };
            match run_point(
                &lookup,
                shell,
                &groups,
                &config,
                &counter,
                &detached,
                "SessionStart",
                &source,
                body,
                Some(agent.as_ref()),
                None,
                true,
            )
            .await
            {
                Ok(merged) => {
                    if let Some(context) = context_from(&merged) {
                        agent.inject(context);
                    }
                }
                Err(error) => tracing::warn!("hooks-codex: SessionStart hook failed: {error}"),
            }
        });
    })?;

    let lookup = ctx.clone();
    let groups_pre = Arc::clone(&groups);
    let detached_pre = detached.clone();
    let counter_pre = Arc::clone(&handler_counter);
    let config_pre = config.clone();
    let last_pre = Arc::clone(&last_agent);
    ctx.on_waterfall("agent/pre-step", move |payload, next| {
        if payload
            .get("messages")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            return next.call(payload);
        }
        if let Some(id) = payload.get("agentId").and_then(Value::as_str) {
            *last_pre
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.to_string());
        }
        let Some(agents) = lookup.get::<AgentRegistry>() else {
            return next.call(payload);
        };
        let Some(id) = payload.get("agentId").and_then(Value::as_str) else {
            return next.call(payload);
        };
        let Some(agent) = agents.get(&session_id(id)) else {
            return next.call(payload);
        };
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return next.call(payload);
        };
        let turn = payload.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mut body = turn_base(
            &lookup,
            Some(agent.as_ref()),
            "UserPromptSubmit",
            &config_pre.model,
        );
        body["prompt"] = json!(messages_text(&payload));
        let merged = block_on_async(run_point(
            &lookup,
            shell,
            &groups_pre,
            &config_pre,
            &counter_pre,
            &detached_pre,
            "UserPromptSubmit",
            "",
            body,
            Some(agent.as_ref()),
            Some(turn),
            true,
        ));
        let Ok(merged) = merged else {
            return next.call(payload);
        };
        if merged.decision == MergedDecision::Deny {
            return json!({ "reject": true });
        }
        let mut downstream = next.call(payload);
        if downstream.get("reject").and_then(Value::as_bool) == Some(true) {
            return downstream;
        }
        if let Some(ours) = context_from(&merged) {
            let mut messages = downstream
                .get("messages")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<UserMessage>>(value).ok())
                .unwrap_or_default();
            messages.push(ours);
            downstream["messages"] = serde_json::to_value(messages).unwrap_or(json!([]));
        }
        downstream
    })?;

    let lookup = ctx.clone();
    let groups_tool = Arc::clone(&groups);
    let detached_tool = detached.clone();
    let counter_tool = Arc::clone(&handler_counter);
    let config_tool = config.clone();
    ctx.on_waterfall("tools/pre-execute", move |payload, next| {
        let Some(tool_name) = payload
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return next.call(payload);
        };
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return next.call(payload);
        };
        let agent = payload
            .get("agentId")
            .and_then(Value::as_str)
            .and_then(|id| {
                lookup
                    .get::<AgentRegistry>()
                    .and_then(|agents| agents.get(&session_id(id)))
            });
        let turn = last_turn(agent.as_deref());
        let mut body = turn_base(&lookup, agent.as_deref(), "PreToolUse", &config_tool.model);
        body["tool_name"] = json!(tool_name);
        body["tool_input"] =
            json!({ "command": command_of(payload.get("args").unwrap_or(&json!({}))) });
        body["tool_use_id"] = payload.get("callId").cloned().unwrap_or(Value::Null);
        let merged = block_on_async(run_point(
            &lookup,
            shell,
            &groups_tool,
            &config_tool,
            &counter_tool,
            &detached_tool,
            "PreToolUse",
            &tool_name,
            body,
            agent.as_deref(),
            Some(turn),
            false,
        ));
        let Ok(merged) = merged else {
            return next.call(payload);
        };
        if merged.decision == MergedDecision::Deny {
            let mut denied = payload;
            denied["deny"] = json!(true);
            denied["reason"] = json!(merged
                .reason
                .unwrap_or_else(|| "blocked by PreToolUse hook".into()));
            return denied;
        }
        next.call(payload)
    })?;

    let lookup = ctx.clone();
    let groups_post = Arc::clone(&groups);
    let detached_post = detached.clone();
    let counter_post = Arc::clone(&handler_counter);
    let config_post = config.clone();
    ctx.on_waterfall("tools/post-execute", move |payload, next| {
        let Some(tool_name) = payload
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return next.call(payload);
        };
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return next.call(payload);
        };
        let agent = payload
            .get("agentId")
            .and_then(Value::as_str)
            .and_then(|id| {
                lookup
                    .get::<AgentRegistry>()
                    .and_then(|agents| agents.get(&session_id(id)))
            });
        let turn = last_turn(agent.as_deref());
        let content = payload
            .get("content")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<ContentBlock>>(value).ok())
            .unwrap_or_default();
        let mut body = turn_base(&lookup, agent.as_deref(), "PostToolUse", &config_post.model);
        body["tool_name"] = json!(tool_name);
        body["tool_input"] =
            json!({ "command": command_of(payload.get("args").unwrap_or(&json!({}))) });
        body["tool_use_id"] = payload.get("callId").cloned().unwrap_or(Value::Null);
        body["tool_response"] = json!(blocks_to_text(&content));
        let merged = block_on_async(run_point(
            &lookup,
            shell,
            &groups_post,
            &config_post,
            &counter_post,
            &detached_post,
            "PostToolUse",
            &tool_name,
            body,
            agent.as_deref(),
            Some(turn),
            false,
        ));
        let Ok(merged) = merged else {
            return next.call(payload);
        };
        let ours = context_from(&merged);
        if merged.decision == MergedDecision::Deny {
            let mut blocked = payload;
            blocked["isError"] = json!(true);
            blocked["content"] = json!([{
                "type": "text",
                "text": merged.reason.unwrap_or_else(|| "blocked by PostToolUse hook".into()),
            }]);
            if let Some(context) = ours {
                prepend_context(context, &mut blocked);
            }
            return blocked;
        }
        let mut downstream = next.call(payload);
        if let Some(context) = ours {
            prepend_context(context, &mut downstream);
        }
        downstream
    })?;

    let lookup = ctx.clone();
    let groups_stop = Arc::clone(&groups);
    let detached_stop = detached.clone();
    let counter_stop = Arc::clone(&handler_counter);
    let config_stop = config.clone();
    let last_stop = Arc::clone(&last_agent);
    ctx.on_serial("agent/turn-stopping", move |payload| {
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return None;
        };
        let turn = payload.get("turn").and_then(Value::as_u64).unwrap_or(0) as u32;
        let Some(agent_id) = last_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return None;
        };
        let Some(agents) = lookup.get::<AgentRegistry>() else {
            return None;
        };
        let Some(agent) = agents.get(&session_id(&agent_id)) else {
            return None;
        };
        let mut body = turn_base(&lookup, Some(agent.as_ref()), "Stop", &config_stop.model);
        body["stop_hook_active"] = json!(false);
        body["last_assistant_message"] = Value::Null;
        let merged = block_on_async(run_point(
            &lookup,
            shell,
            &groups_stop,
            &config_stop,
            &counter_stop,
            &detached_stop,
            "Stop",
            "",
            body,
            Some(agent.as_ref()),
            Some(turn),
            false,
        ));
        if let Ok(merged) = merged {
            if merged.decision == MergedDecision::Deny {
                let text = merged
                    .reason
                    .unwrap_or_else(|| "continue: blocked by Stop hook".into());
                agent.steer(UserMessage::from_parts(
                    vec![ContentBlock::text(text)],
                    MessageSource::plugin("hooks-codex"),
                ));
            }
        }
        None
    })?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_point(
    _ctx: &Context,
    shell: Arc<ShellRuntime>,
    parsed: &BTreeMap<String, Vec<MatcherGroup>>,
    config: &Config,
    handler_counter: &AtomicU64,
    detached: &DetachedRuns,
    point: &str,
    match_query: &str,
    payload: Value,
    agent: Option<&dyn Agent>,
    turn: Option<u32>,
    plain_stdout_as_context: bool,
) -> std::result::Result<MergedHookOutcome, String> {
    let groups = parsed.get(point).cloned().unwrap_or_default();
    let mut outputs: Vec<HookOutput> = Vec::new();
    let workdir = agent.and_then(|item| item.session().header().cwd.clone());
    for group in groups {
        if !matches_matcher(group.matcher.as_deref(), match_query, MatcherMode::Codex) {
            continue;
        }
        for hook in &group.hooks {
            let handler_id = format!(
                "codex:{point}:{}",
                handler_counter.fetch_add(1, Ordering::SeqCst) + 1
            );
            if let (Some(agent), Some(turn)) = (agent, turn) {
                append_hook_invoked(
                    agent.session().as_ref(),
                    HookInvocation {
                        turn,
                        point: point.to_string(),
                        dialect: HookDialect::Codex,
                        handler_id: handler_id.clone(),
                        matcher: group.matcher.clone(),
                    },
                );
            }
            let mut result = run_hook(
                |request| run_shell(Arc::clone(&shell), request),
                hook,
                RunHookOptions {
                    payload: payload.clone(),
                    env: None,
                    cwd: workdir.clone(),
                    aborted: detached.is_aborted(),
                    trailing_newline: false,
                    default_timeout_ms: config.default_timeout_ms,
                    expected_event_name: Some(point.to_string()),
                },
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_millis())
                        .unwrap_or(0)
                },
            )
            .await;
            if plain_stdout_as_context
                && result.output.exit_code == Some(0)
                && result.output.additional_context.is_none()
                && !result.output.stdout.is_empty()
                && !result.output.stdout.starts_with('{')
            {
                result.output.additional_context = Some(result.output.stdout.clone());
            }
            if result.output.system_message.is_some() {
                tracing::warn!(
                    "hooks-codex: {point} hook emitted a systemMessage, which is not yet surfaced (ignored)"
                );
            }
            if let (Some(agent), Some(turn)) = (agent, turn) {
                append_hook_result(
                    agent.session().as_ref(),
                    HookResultRecord {
                        turn,
                        point: point.to_string(),
                        handler_id,
                        output: result.output.clone(),
                        stderr_summary_max_chars: config.stderr_summary_max_chars,
                        duration_ms: result.duration_ms,
                    },
                );
            }
            outputs.push(result.output);
        }
    }
    Ok(merge_hook_outputs(&outputs))
}
