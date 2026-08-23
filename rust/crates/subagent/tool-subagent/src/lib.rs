//! Model-facing delegation through one configured `ctx.subagents` provider.
//! Foreground calls collect the child's result; continuable background calls
//! return the durable child id through `ctx.subagents.start_continuable()`.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result};
use dsh_jobs::{JobHooks, JobOutcome, JobRegistry, JobStart, JobStatus};
use dsh_llm::ContentBlock;
use dsh_session::session_id;
use dsh_subagent::{SubagentRuntime, SubagentStartRequest};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-subagent"
}

/// Prompt order after bounded delegation policy and before child reporting.
const SUBAGENT_SECTION_ORDER: i32 = 116;

/// Tool construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// `ctx.subagents` provider name.
    pub provider: String,
    /// Model-facing tool name.
    pub tool_name: String,
    /// `one-shot` or `continuable`.
    pub background_mode: String,
    /// Whether the `run_in_background` parameter is exposed.
    pub enable_run_in_background: bool,
}

impl Config {
    /// Resolve plugin config. `provider` is required.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let provider = value
            .and_then(|value| value.get("provider"))
            .and_then(Value::as_str)
            .ok_or_else(|| "tool-subagent: provider is required".to_string())?
            .to_string();
        let tool_name = value
            .and_then(|value| value.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_string();
        let background_mode = value
            .and_then(|value| value.get("backgroundMode"))
            .and_then(Value::as_str)
            .unwrap_or("one-shot")
            .to_string();
        if background_mode != "one-shot" && background_mode != "continuable" {
            return Err("tool-subagent: backgroundMode must be one-shot or continuable".into());
        }
        let enable_run_in_background = value
            .and_then(|value| value.get("enableRunInBackground"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok(Self {
            provider,
            tool_name,
            background_mode,
            enable_run_in_background,
        })
    }
}

/// Model-facing wording from the provider's conversation-history descriptor.
fn provider_wording(inherits: bool) -> (&'static str, &'static str) {
    if inherits {
        (
            "Delegate a task to a subagent that inherits this conversation: a child agent seeded with all \
completed turns so far (it does not see the current in-flight turn). Use this when the subtask \
builds on this conversation's context — a follow-up analysis, \
a review, a continuation — without consuming this conversation's context for the work itself. \
You receive its result, not its intermediate steps.",
            "The task for the subagent. It already sees this conversation's completed turns, so build on them \
freely and state only what is new.",
        )
    } else {
        (
            "Delegate a self-contained task to a subagent (a separate agent that works in its own context) \
to offload focused, independent work — research, a scoped \
implementation, an analysis — so it does not consume this conversation's context. The subagent \
returns its result, not its intermediate steps. Give it a \
complete, standalone prompt: it does not see this conversation.",
            "The complete, self-contained task for the subagent. It does not share this \
conversation's context, so include everything it needs.",
        )
    }
}

/// Register one delegation tool.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    let agents = ctx.get::<AgentRegistry>();
    let jobs = ctx.get::<JobRegistry>();
    let provider = subagents.get_provider(&config.provider);
    let inherits = provider
        .as_ref()
        .map(|provider| provider.inherits_parent_context())
        .unwrap_or(false);
    let continuable = config.background_mode == "continuable";
    if continuable {
        if let Some(provider) = &provider {
            if !provider.supports_continuable() {
                return Err(dsh_cordis::CordisError::Validation(format!(
                    "tool-subagent: provider \"{}\" does not support `backgroundMode: continuable`",
                    config.provider
                )));
            }
        }
    }
    if config.enable_run_in_background && continuable {
        if let Some(prompt) = ctx.get::<SystemPrompt>() {
            prompt.register_section(PromptSection {
                id: format!("tool:{}", config.tool_name),
                text: format!(
                    "Use {} in the background by default. Start independent delegations together in one \
assistant message and continue useful work while they run. Set `run_in_background: false` only when \
your next action depends on that subagent's result. When a background run settles, the runtime sends \
you a notice containing its outcome and any final assistant message.",
                    config.tool_name
                ),
                order: SUBAGENT_SECTION_ORDER,
            });
        }
    }
    let description = description_text(inherits, &config);
    tools.insert(Arc::new(DelegateTool {
        subagents,
        agents,
        jobs,
        config,
        inherits,
        description,
    }));
    Ok(())
}

/// Assemble the model-facing tool description from provider wording and the
/// instance's background policy.
fn description_text(inherits: bool, config: &Config) -> String {
    let (base, _prompt) = provider_wording(inherits);
    let continuable = config.background_mode == "continuable";
    let suffix = if config.enable_run_in_background {
        if continuable {
            " This tool runs in the background by default, immediately returns a durable subagent id, and \
keeps the child conversation available for later turns. When that run settles, the runtime sends the \
parent a notice containing its outcome and any final assistant message; `send_message` starts a later \
turn in the same child conversation. Set `run_in_background: false` only when your next action depends \
on receiving the result."
        } else {
            " This call waits for the result by default. Set `run_in_background: true` to return a job id; \
collect with `job_output` and stop with `job_kill`."
        }
    } else {
        " This call waits for the subagent and returns its result."
    };
    format!("{base}{suffix}")
}

struct DelegateTool {
    subagents: Arc<SubagentRuntime>,
    agents: Option<Arc<AgentRegistry>>,
    jobs: Option<Arc<JobRegistry>>,
    config: Config,
    inherits: bool,
    description: String,
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        &self.config.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let (_base, prompt_description) = provider_wording(self.inherits);
        let continuable = self.config.background_mode == "continuable";
        let mut properties = serde_json::Map::new();
        properties.insert(
            "description".into(),
            json!({
                "type": "string",
                "description": "A short (3-5 word) description of the delegated task, for display.",
            }),
        );
        properties.insert(
            "prompt".into(),
            json!({
                "type": "string",
                "description": prompt_description,
            }),
        );
        if self.config.enable_run_in_background {
            properties.insert(
                "run_in_background".into(),
                json!({
                    "type": "boolean",
                    "description": if continuable {
                        "Whether to run in the background and return a durable subagent id immediately. Defaults to true. Set false to wait for the result when your next action depends on it."
                    } else {
                        "Whether to run as a background job and return its id. Defaults to false; collect with job_output or stop with job_kill."
                    },
                }),
            );
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        let description = call
            .args
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("description required".into()))?;
        let prompt = call
            .args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Body("prompt required".into()))?;
        let continuable = self.config.background_mode == "continuable";
        let requested = call.args.get("run_in_background").and_then(Value::as_bool);
        if !self.config.enable_run_in_background && requested == Some(true) {
            return Ok(ToolOutcome::error(
                "Error: run_in_background is disabled for this tool instance (enableRunInBackground: false)"
                    .to_string(),
            ));
        }
        let background = if self.config.enable_run_in_background {
            requested.unwrap_or(continuable)
        } else {
            false
        };
        if background {
            if !continuable {
                let Some(jobs) = &self.jobs else {
                    return Ok(ToolOutcome::error(
                        "Error: background jobs unavailable: load @deepseek-ai/dsh-jobs and @deepseek-ai/dsh-tool-jobs"
                            .to_string(),
                    ));
                };
                let parent_id = call.agent_id.clone().unwrap_or_else(|| "unknown".into());
                let provider = self.config.provider.clone();
                let label = description.to_string();
                let prompt = prompt.to_string();
                let subagents = Arc::clone(&self.subagents);
                return match jobs.start(JobStart {
                    kind: "subagent".into(),
                    label: label.clone(),
                    output_limit_bytes: None,
                    owner_session: call.agent_id.clone(),
                    run: Box::new(move || {
                        start_one_shot_job(subagents, provider, label, prompt, parent_id)
                    }),
                }) {
                    Ok(id) => Ok(ToolOutcome::text(format!("started background job {id}"))),
                    Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
                };
            }
            let parent = call
                .agent_id
                .as_deref()
                .and_then(|id| self.agents.as_ref()?.get(&session_id(id)))
                .ok_or_else(|| {
                    ToolError::Body(
                        "subagent tool requires a calling agent (exec.agent was undefined)".into(),
                    )
                })?;
            return match self.subagents.start_continuable(
                &self.config.provider,
                description,
                vec![ContentBlock::text(prompt)],
                &parent,
            ) {
                Ok(started) => Ok(ToolOutcome::text(format!(
                    "started subagent {}",
                    started.child_id
                ))),
                Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
            };
        }
        let parent_id = call
            .agent_id
            .as_deref()
            .map(session_id)
            .unwrap_or_else(|| session_id("unknown"));
        match self
            .subagents
            .start(
                &self.config.provider,
                SubagentStartRequest {
                    label: description.to_string(),
                    prompt: prompt.to_string(),
                    parent_id,
                    seed: None,
                },
            )
            .await
        {
            Ok(result) => Ok(ToolOutcome::text(result.output)),
            Err(error) => Ok(ToolOutcome::error(format!("Error: {error}"))),
        }
    }
}

fn start_one_shot_job(
    subagents: Arc<SubagentRuntime>,
    provider: String,
    label: String,
    prompt: String,
    parent_id: String,
) -> std::result::Result<JobHooks, String> {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("dsh-subagent-job".into())
        .spawn(move || {
            let outcome = match futures::executor::block_on(subagents.start(
                &provider,
                SubagentStartRequest {
                    label,
                    prompt,
                    parent_id: session_id(parent_id),
                    seed: None,
                },
            )) {
                Ok(result) => JobOutcome {
                    status: if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        JobStatus::Killed
                    } else {
                        JobStatus::Completed
                    },
                    detail: Some(result.stop_reason),
                    output: Some(result.output),
                },
                Err(error) => JobOutcome {
                    status: JobStatus::Failed,
                    detail: Some(error.to_string()),
                    output: None,
                },
            };
            let _ = tx.send(outcome);
        })
        .map_err(|error| error.to_string())?;
    let rx = std::sync::Mutex::new(rx);
    Ok(JobHooks {
        cancel: Arc::new(move |_| {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }),
        wait_done: Arc::new(move || {
            rx.lock()
                .expect("subagent job")
                .recv()
                .unwrap_or(JobOutcome {
                    status: JobStatus::Failed,
                    detail: Some("background subagent waiter dropped".into()),
                    output: None,
                })
        }),
        read_output: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_requires_provider() {
        assert!(Config::resolve(None).is_err());
        let config = Config::resolve(Some(&json!({
            "provider": "spawn",
            "toolName": "subagent",
            "backgroundMode": "continuable"
        })))
        .unwrap();
        assert_eq!(config.provider, "spawn");
        assert_eq!(config.background_mode, "continuable");
        assert!(config.enable_run_in_background);
    }

    #[test]
    fn resolve_rejects_unknown_background_mode() {
        let error = Config::resolve(Some(&json!({
            "provider": "spawn",
            "backgroundMode": "sometimes"
        })))
        .unwrap_err();
        assert!(error.contains("backgroundMode"));
    }
}
