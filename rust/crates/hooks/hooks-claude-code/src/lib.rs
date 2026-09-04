//! Claude Code command-hook bridge on harness interception points.

mod config;

pub use config::{
    parse_claude_code_config, substitute_command, ClaudeCodeHookConfig, ParsedClaudeConfig,
    SkippedHook, SubstitutionVars,
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
use dsh_user_approval::{ApprovalOutcome, ApprovalRequest, ApprovalService};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SUBAGENT_TYPE: &str = "general-purpose";

/// Plugin role name matching TypeScript `export const name`.
pub fn name() -> &'static str {
    "hooks-claude-code"
}

/// Plugin config: where the CC hook config lives plus substitution roots.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to `hooks.json` or a settings file whose `hooks` key holds the config.
    pub config_path: String,
    /// Replaces `${CLAUDE_PLUGIN_ROOT}` in command strings.
    pub plugin_root: Option<String>,
    /// Replaces `${CLAUDE_PROJECT_DIR}` and is exported as that env var.
    pub project_dir: Option<String>,
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
            .ok_or_else(|| "hooks-claude-code: configPath is required".to_string())?;
        let config_path = object
            .get("configPath")
            .and_then(Value::as_str)
            .ok_or_else(|| "hooks-claude-code: configPath is required".to_string())?
            .to_string();
        let default_timeout_ms = match object.get("defaultTimeoutMs") {
            None => DEFAULT_HOOK_TIMEOUT_MS,
            Some(value) => value.as_u64().ok_or_else(|| {
                "hooks-claude-code: defaultTimeoutMs must be a number".to_string()
            })?,
        };
        let stderr_summary_max_chars = match object.get("stderrSummaryMaxChars") {
            None => DEFAULT_STDERR_SUMMARY_MAX_CHARS,
            Some(value) => {
                let Some(number) = value.as_u64() else {
                    return Err(
                        "hooks-claude-code: stderrSummaryMaxChars must be a positive integer"
                            .into(),
                    );
                };
                if number < 1 {
                    return Err(
                        "hooks-claude-code: stderrSummaryMaxChars must be a positive integer"
                            .into(),
                    );
                }
                number as usize
            }
        };
        Ok(Self {
            config_path,
            plugin_root: object
                .get("pluginRoot")
                .and_then(Value::as_str)
                .map(str::to_string),
            project_dir: object
                .get("projectDir")
                .and_then(Value::as_str)
                .map(str::to_string),
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

fn last_turn(agent: &dyn Agent) -> u32 {
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

fn transcript_path(ctx: &Context, agent: Option<&dyn Agent>) -> String {
    let Some(agent) = agent else {
        return String::new();
    };
    ctx.get::<PersistenceRuntime>()
        .and_then(|persistence| persistence.locate(agent.session().id()))
        .map(|location| match location {
            SessionLocation::Jsonl { path } => path.display().to_string(),
        })
        .unwrap_or_default()
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn base(ctx: &Context, agent: Option<&dyn Agent>, event: &str) -> Value {
    json!({
        "session_id": agent.map(|item| item.session().id().as_str().to_string()).unwrap_or_default(),
        "transcript_path": transcript_path(ctx, agent),
        "cwd": agent
            .and_then(|item| item.session().header().cwd.clone())
            .unwrap_or_else(process_cwd),
        "hook_event_name": event,
    })
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
        MessageSource::plugin("hooks-claude-code"),
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

/// Install the Claude Code hook bridge.
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
        .and_then(|raw| {
            parse_claude_code_config(
                &raw,
                &SubstitutionVars {
                    plugin_root: config.plugin_root.clone(),
                    project_dir: config.project_dir.clone(),
                },
            )
        }) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(
                "hooks-claude-code: could not load hook config \"{}\": {error} — no hooks registered",
                config.config_path
            );
            return Ok(());
        }
    };
    for skipped in &parsed.skipped {
        tracing::warn!(
            "hooks-claude-code: skipping unsupported \"{}\" hook on {} (only command hooks run)",
            skipped.type_name,
            skipped.event
        );
    }

    let groups = Arc::new(parsed.config);
    let detached = create_detached_runs();
    let handler_counter = Arc::new(AtomicU64::new(0));
    let last_agent = Arc::new(Mutex::new(None::<String>));
    let subagent_children: Arc<Mutex<BTreeMap<String, String>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let drain = detached.clone();
    ctx.effect("hooks-claude-code: drain detached hook runs", move || {
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
        let mut body = base(&lookup, Some(agent.as_ref()), "SessionStart");
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
            )
            .await
            {
                Ok(merged) => {
                    if let Some(context) = context_from(&merged) {
                        agent.inject(context);
                    }
                }
                Err(error) => {
                    tracing::warn!("hooks-claude-code: SessionStart hook failed: {error}");
                }
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
        let mut body = base(&lookup, Some(agent.as_ref()), "UserPromptSubmit");
        body["prompt"] = json!(messages_text(&payload));
        let groups = Arc::clone(&groups_pre);
        let detached = detached_pre.clone();
        let counter = Arc::clone(&counter_pre);
        let config = config_pre.clone();
        let lookup_run = lookup.clone();
        let merged = block_on_async(run_point(
            &lookup_run,
            shell,
            &groups,
            &config,
            &counter,
            &detached,
            "UserPromptSubmit",
            "",
            body,
            Some(agent.as_ref()),
            Some(turn),
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
        let turn = agent
            .as_ref()
            .map(|item| last_turn(item.as_ref()))
            .unwrap_or(0);
        let mut body = base(&lookup, agent.as_deref(), "PreToolUse");
        body["tool_name"] = json!(tool_name);
        body["tool_input"] = payload.get("args").cloned().unwrap_or(json!({}));
        body["tool_use_id"] = payload.get("callId").cloned().unwrap_or(Value::Null);
        let groups = Arc::clone(&groups_tool);
        let detached = detached_tool.clone();
        let counter = Arc::clone(&counter_tool);
        let config = config_tool.clone();
        let lookup_run = lookup.clone();
        let merged = block_on_async(run_point(
            &lookup_run,
            shell,
            &groups,
            &config,
            &counter,
            &detached,
            "PreToolUse",
            &tool_name,
            body,
            agent.as_deref(),
            Some(turn),
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
        if merged.decision == MergedDecision::Ask {
            let reason = merged.reason.clone();
            if let (Some(approval), Some(agent)) = (lookup.get::<ApprovalService>(), agent.as_ref())
            {
                match approval.request(
                    &lookup,
                    agent.session().as_ref(),
                    ApprovalRequest {
                        tool_name: tool_name.clone(),
                        call_id: payload
                            .get("callId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        reason: reason.clone(),
                    },
                ) {
                    Ok(ApprovalOutcome::AllowedOnce) => return next.call(payload),
                    _ => {
                        let mut denied = payload;
                        denied["deny"] = json!(true);
                        denied["reason"] =
                            json!(reason.unwrap_or_else(|| "blocked by PreToolUse hook".into()));
                        return denied;
                    }
                }
            }
            let mut denied = payload;
            denied["deny"] = json!(true);
            denied["reason"] = json!(reason.unwrap_or_else(|| "blocked by PreToolUse hook".into()));
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
        let turn = agent
            .as_ref()
            .map(|item| last_turn(item.as_ref()))
            .unwrap_or(0);
        let content = payload
            .get("content")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<ContentBlock>>(value).ok())
            .unwrap_or_default();
        let mut body = base(&lookup, agent.as_deref(), "PostToolUse");
        body["tool_name"] = json!(tool_name);
        body["tool_input"] = payload.get("args").cloned().unwrap_or(json!({}));
        body["tool_use_id"] = payload.get("callId").cloned().unwrap_or(Value::Null);
        body["tool_response"] = json!(blocks_to_text(&content));
        let groups = Arc::clone(&groups_post);
        let detached = detached_post.clone();
        let counter = Arc::clone(&counter_post);
        let config = config_post.clone();
        let lookup_run = lookup.clone();
        let merged = block_on_async(run_point(
            &lookup_run,
            shell,
            &groups,
            &config,
            &counter,
            &detached,
            "PostToolUse",
            &tool_name,
            body,
            agent.as_deref(),
            Some(turn),
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
        let agent_id = last_stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(agent_id) = agent_id else {
            return None;
        };
        let Some(agents) = lookup.get::<AgentRegistry>() else {
            return None;
        };
        let Some(agent) = agents.get(&session_id(&agent_id)) else {
            return None;
        };
        let mut body = base(&lookup, Some(agent.as_ref()), "Stop");
        body["stop_hook_active"] = json!(false);
        let groups = Arc::clone(&groups_stop);
        let detached = detached_stop.clone();
        let counter = Arc::clone(&counter_stop);
        let config = config_stop.clone();
        let lookup_run = lookup.clone();
        let merged = block_on_async(run_point(
            &lookup_run,
            shell,
            &groups,
            &config,
            &counter,
            &detached,
            "Stop",
            "",
            body,
            Some(agent.as_ref()),
            Some(turn),
        ));
        if let Ok(merged) = merged {
            if merged.decision == MergedDecision::Deny {
                let text = merged
                    .reason
                    .unwrap_or_else(|| "continue: blocked by Stop hook".into());
                agent.steer(UserMessage::from_parts(
                    vec![ContentBlock::text(text)],
                    MessageSource::plugin("hooks-claude-code"),
                ));
            }
        }
        None
    })?;

    let lookup = ctx.clone();
    let groups_sub_start = Arc::clone(&groups);
    let detached_sub_start = detached.clone();
    let counter_sub_start = Arc::clone(&handler_counter);
    let config_sub_start = config.clone();
    let children_start = Arc::clone(&subagent_children);
    ctx.on("subagent/start", move |payload| {
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Some(run_id) = payload.get("runId").and_then(Value::as_str) {
            children_start
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(run_id.to_string(), id.to_string());
        }
        let child = lookup
            .get::<AgentRegistry>()
            .and_then(|agents| agents.get(&session_id(id)));
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return;
        };
        let mut body = base(&lookup, child.as_deref(), "SubagentStart");
        body["agent_id"] = json!(id);
        body["agent_type"] = json!(SUBAGENT_TYPE);
        let groups = Arc::clone(&groups_sub_start);
        let detached = detached_sub_start.clone();
        let counter = Arc::clone(&counter_sub_start);
        let config = config_sub_start.clone();
        let lookup_run = lookup.clone();
        let child = child.clone();
        detached_sub_start.track(async move {
            match run_point(
                &lookup_run,
                shell,
                &groups,
                &config,
                &counter,
                &detached,
                "SubagentStart",
                SUBAGENT_TYPE,
                body,
                child.as_deref(),
                None,
            )
            .await
            {
                Ok(merged) => {
                    if let (Some(context), Some(child)) = (context_from(&merged), child) {
                        child.inject(context);
                    }
                }
                Err(error) => {
                    tracing::warn!("hooks-claude-code: SubagentStart hook failed: {error}");
                }
            }
        });
    })?;

    let lookup = ctx.clone();
    let groups_sub_end = Arc::clone(&groups);
    let detached_sub_end = detached.clone();
    let counter_sub_end = Arc::clone(&handler_counter);
    let config_sub_end = config.clone();
    let children_end = Arc::clone(&subagent_children);
    ctx.on("subagent/end", move |payload| {
        let id = payload
            .get("runId")
            .and_then(Value::as_str)
            .and_then(|run_id| {
                children_end
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(run_id)
            })
            .or_else(|| {
                payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(id) = id else {
            return;
        };
        let child = lookup
            .get::<AgentRegistry>()
            .and_then(|agents| agents.get(&session_id(&id)));
        let Some(shell) = lookup.get::<ShellRuntime>() else {
            return;
        };
        let mut body = base(&lookup, child.as_deref(), "SubagentStop");
        body["agent_id"] = json!(id);
        body["agent_type"] = json!(SUBAGENT_TYPE);
        body["stop_hook_active"] = json!(false);
        let groups = Arc::clone(&groups_sub_end);
        let detached = detached_sub_end.clone();
        let counter = Arc::clone(&counter_sub_end);
        let config = config_sub_end.clone();
        let lookup_run = lookup.clone();
        let child_ref = child.clone();
        detached_sub_end.track(async move {
            let _ = run_point(
                &lookup_run,
                shell,
                &groups,
                &config,
                &counter,
                &detached,
                "SubagentStop",
                SUBAGENT_TYPE,
                body,
                child_ref.as_deref(),
                None,
            )
            .await;
        });
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
) -> std::result::Result<MergedHookOutcome, String> {
    let groups = parsed.get(point).cloned().unwrap_or_default();
    let mut outputs: Vec<HookOutput> = Vec::new();
    let workdir = agent.and_then(|item| item.session().header().cwd.clone());
    let project_dir = config.project_dir.clone().or_else(|| workdir.clone());
    let hook_env = project_dir.map(|dir| {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_PROJECT_DIR".into(), dir);
        env
    });
    for group in groups {
        if !matches_matcher(
            group.matcher.as_deref(),
            match_query,
            MatcherMode::ClaudeCode,
        ) {
            continue;
        }
        for hook in &group.hooks {
            let handler_id = format!(
                "claude-code:{point}:{}",
                handler_counter.fetch_add(1, Ordering::SeqCst) + 1
            );
            if let (Some(agent), Some(turn)) = (agent, turn) {
                append_hook_invoked(
                    agent.session().as_ref(),
                    HookInvocation {
                        turn,
                        point: point.to_string(),
                        dialect: HookDialect::ClaudeCode,
                        handler_id: handler_id.clone(),
                        matcher: group.matcher.clone(),
                    },
                );
            }
            let result = run_hook(
                |request| run_shell(Arc::clone(&shell), request),
                hook,
                RunHookOptions {
                    payload: payload.clone(),
                    env: hook_env.clone(),
                    cwd: workdir.clone(),
                    aborted: detached.is_aborted(),
                    trailing_newline: true,
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
            if result.output.updated_input.is_some() {
                tracing::warn!(
                    "hooks-claude-code: {point} hook requested updatedInput, which is not yet honored (ignored)"
                );
            }
            if result.output.system_message.is_some() {
                tracing::warn!(
                    "hooks-claude-code: {point} hook emitted a systemMessage, which is not yet surfaced (ignored)"
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
