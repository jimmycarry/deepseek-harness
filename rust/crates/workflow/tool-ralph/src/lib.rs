//! Model-facing `ralph` tool: a deployment-owned fresh-agent loop.
//!
//! The TypeScript plugin runs a fixed JavaScript workflow through
//! `ctx.workflowEngine`. This crate implements that same loop in Rust over
//! `ctx.subagents` so the parent-facing tool, prompts, validation, and
//! renderer stay 1:1 without a JavaScript worker.

use async_trait::async_trait;
use dsh_cordis::{Context, Result};
use dsh_session::session_id;
use dsh_subagent::{SubagentRuntime, SubagentStartRequest};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "tool-ralph"
}

/// Prompt order shared with `tool-subagent`.
const RALPH_SECTION_ORDER: i32 = 116;

const DESCRIPTION: &str = "Run a foreground fresh-agent Ralph loop toward one immutable objective. \
Use only when the direct human explicitly asks for Ralph or fresh-agent iteration. Each round \
opens a new child with no parent conversation or prior child session; the shared workspace is \
long-term memory, and only a bounded structured report crosses rounds. The call returns when \
a worker reports completion or a concrete blocker, or at the round limit. Ordinary long-running same-session work \
belongs to goal tools.";

const SECTION_TEXT: &str = "Use the ralph tool ONLY when the direct human explicitly asks for a Ralph loop or fresh-agent iterative execution. Each Ralph round starts a fresh child with no conversation seed and uses the shared workspace as durable memory. Completion and blockers are worker reports, not independent evaluation. Use same-session goal tools for ordinary long-running objectives, and plain subagents or workflows for bounded delegation and fan-out.";

const TRUNCATION_NOTICE: &str = "\n… [truncated]";

/// Deployment policy for the fixed Ralph loop.
#[derive(Debug, Clone)]
pub struct Config {
    /// Fresh structured-output provider used for every round.
    pub subagent_provider: String,
    /// Default and deployment ceiling for one call's round count.
    pub max_rounds: u64,
    /// Maximum serialized characters in one structured handoff.
    pub max_handoff_chars: usize,
    /// Maximum characters in a successful parent-facing terminal text.
    pub max_result_chars: usize,
}

impl Config {
    /// Validate raw cordis.yml config. Omitted fields take TypeScript defaults.
    ///
    /// # Errors
    /// Empty or untrimmed `subagentProvider`, or a non-positive integer for
    /// any numeric field.
    pub fn resolve(value: Option<&Value>) -> std::result::Result<Self, String> {
        let subagent_provider = value
            .and_then(|value| value.get("subagentProvider"))
            .and_then(Value::as_str)
            .unwrap_or("spawn")
            .to_string();
        if subagent_provider.is_empty() || subagent_provider != subagent_provider.trim() {
            return Err("subagentProvider must be a non-empty normalized string".into());
        }
        Ok(Self {
            subagent_provider,
            max_rounds: positive_int(value, "maxRounds", 256)?,
            max_handoff_chars: positive_int(value, "maxHandoffChars", 16_384)? as usize,
            max_result_chars: positive_int(value, "maxResultChars", 16_384)? as usize,
        })
    }
}

fn positive_int(
    value: Option<&Value>,
    key: &str,
    default: u64,
) -> std::result::Result<u64, String> {
    match value.and_then(|value| value.get(key)) {
        None => Ok(default),
        Some(item) => parse_positive_safe_int(item)
            .ok_or_else(|| format!("{key} must be a positive safe integer")),
    }
}

fn parse_positive_safe_int(value: &Value) -> Option<u64> {
    let number = value.as_u64()?;
    if number < 1 {
        return None;
    }
    Some(number)
}

/// Resolve one model-selected cap against the deployment ceiling.
fn resolve_max_rounds(requested: Option<&Value>, ceiling: u64) -> std::result::Result<u64, String> {
    let value = match requested {
        None => ceiling,
        Some(item) => parse_positive_safe_int(item)
            .ok_or_else(|| "Ralph maxRounds must be a positive safe integer".to_string())?,
    };
    if value > ceiling {
        return Err(format!(
            "Ralph maxRounds {value} exceeds the deployment ceiling {ceiling}"
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RalphRoundReport {
    status: String,
    summary: String,
    evidence: Vec<String>,
    #[serde(rename = "nextSteps")]
    next_steps: Vec<String>,
    blocker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RalphRunResult {
    status: String,
    #[serde(rename = "roundsStarted")]
    rounds_started: u64,
    report: RalphRoundReport,
}

/// Register the Ralph tool and its explicit-ask usage policy.
pub fn install(ctx: &Context, config: Config) -> Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    let subagents = ctx.service::<SubagentRuntime>()?;
    if let Some(prompt) = ctx.get::<SystemPrompt>() {
        prompt.register_section(PromptSection {
            id: "tool:ralph".into(),
            text: SECTION_TEXT.into(),
            order: RALPH_SECTION_ORDER,
        });
    }
    tools.insert(Arc::new(RalphTool { subagents, config }));
    Ok(())
}

struct RalphTool {
    subagents: Arc<SubagentRuntime>,
    config: Config,
}

#[async_trait]
impl Tool for RalphTool {
    fn name(&self) -> &str {
        "ralph"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The immutable completion objective for every fresh Ralph round."
                },
                "maxRounds": {
                    "type": "number",
                    "description": "Optional positive safe-integer round cap, bounded by the deployment ceiling."
                }
            },
            "required": ["objective"]
        })
    }

    async fn execute(&self, args: Value) -> std::result::Result<ToolOutcome, ToolError> {
        self.execute_call(&ToolCall {
            name: self.name().to_string(),
            args,
            agent_id: None,
            call_id: None,
        })
        .await
    }

    async fn execute_call(&self, call: &ToolCall) -> std::result::Result<ToolOutcome, ToolError> {
        match self.run(call).await {
            Ok(text) => Ok(ToolOutcome::text(text)),
            Err(message) => Ok(ToolOutcome::error(message)),
        }
    }
}

impl RalphTool {
    async fn run(&self, call: &ToolCall) -> std::result::Result<String, String> {
        let parent = call.agent_id.as_deref().ok_or_else(|| {
            "Ralph tool requires a calling agent (exec.agent was undefined)".to_string()
        })?;
        let objective = call
            .args
            .get("objective")
            .and_then(Value::as_str)
            .ok_or_else(|| "Ralph objective must be a non-empty string".to_string())?
            .trim();
        if objective.is_empty() {
            return Err("Ralph objective must be a non-empty string".into());
        }
        let max_rounds = resolve_max_rounds(call.args.get("maxRounds"), self.config.max_rounds)?;
        require_fresh_provider(&self.subagents, &self.config.subagent_provider)?;

        let mut previous: Option<RalphRoundReport> = None;
        for round in 1..=max_rounds {
            let prompt = build_round_prompt(objective, round, max_rounds, previous.as_ref());
            let raw = match self
                .subagents
                .start(
                    &self.config.subagent_provider,
                    SubagentStartRequest {
                        label: format!("Ralph round {round}"),
                        prompt,
                        parent_id: session_id(parent),
                        seed: None,
                    },
                )
                .await
            {
                Ok(result) => result.output,
                Err(error) => {
                    return Err(format!("Ralph workflow failed: {error}"));
                }
            };
            let Some(value) = parse_structured_report(&raw) else {
                return Err(render_round_failure(
                    round,
                    previous.as_ref(),
                    self.config.max_result_chars,
                ));
            };
            let report = match validate_report(&value, self.config.max_handoff_chars) {
                Ok(report) => report,
                Err(error) => return Err(format!("Ralph workflow failed: {error}")),
            };
            match report.status.as_str() {
                "complete" | "blocked" => {
                    let result = RalphRunResult {
                        status: report.status.clone(),
                        rounds_started: round,
                        report,
                    };
                    return Ok(render_result(&result, self.config.max_result_chars));
                }
                _ => previous = Some(report),
            }
        }
        let report = previous.ok_or_else(|| {
            "Ralph workflow failed: Ralph workflow returned a malformed terminal result".to_string()
        })?;
        let result = RalphRunResult {
            status: "budget-limited".into(),
            rounds_started: max_rounds,
            report,
        };
        Ok(render_result(&result, self.config.max_result_chars))
    }
}

/// Require the configured route to mean a genuinely fresh structured child.
fn require_fresh_provider(
    runtime: &SubagentRuntime,
    name: &str,
) -> std::result::Result<(), String> {
    let Some(provider) = runtime.get_provider(name) else {
        return Err(format!(
            "Ralph subagent provider \"{name}\" is not registered"
        ));
    };
    if !provider.supports_output_schema() {
        return Err(format!(
            "Ralph subagent provider \"{name}\" does not support structured output"
        ));
    }
    if provider.inherits_parent_context() {
        return Err(format!(
            "Ralph subagent provider \"{name}\" inherits parent context; Ralph requires a fresh provider"
        ));
    }
    Ok(())
}

fn build_round_prompt(
    objective: &str,
    round: u64,
    max_rounds: u64,
    previous: Option<&RalphRoundReport>,
) -> String {
    let prior = match previous {
        None => "(none — this is the first round)".to_string(),
        Some(report) => serde_json::to_string(report).expect("report is JSON"),
    };
    [
        "You are one fresh worker in a foreground Ralph loop. You receive no parent conversation and no prior child session. Do not call the ralph tool: this round already is its worker.",
        &format!("Immutable objective:\n{objective}"),
        &format!("Ralph round: {round} of {max_rounds}."),
        "The shared workspace and its current working tree are the long-term memory and source of truth. Inspect them before acting, preserve existing work, perform concrete in-scope work, and verify what you change. Treat the previous report only as a bounded handoff; confirm it against the workspace.",
        &format!("Previous structured handoff:\n{prior}"),
        "Return one report with exact normalized strings. Use status continue with at least one nextSteps entry while useful work remains; complete only with concrete evidence and no nextSteps; blocked only when no meaningful progress is possible without human input or an external-state change. blocker must be empty unless blocked.",
    ]
    .join("\n\n")
}

/// Structured-output capture: a JSON object with the Ralph report field set.
fn parse_structured_report(text: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(text.trim()).ok()?;
    let object = value.as_object()?;
    let mut keys: Vec<_> = object.keys().cloned().collect();
    keys.sort();
    if keys.join(",") != "blocker,evidence,nextSteps,status,summary" {
        return None;
    }
    Some(value)
}

fn normalized_text(value: &Value) -> Option<&str> {
    let text = value.as_str()?;
    if text.is_empty() || text != text.trim() {
        return None;
    }
    Some(text)
}

fn normalized_list(value: &Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(normalized_text(item)?.to_string());
    }
    Some(out)
}

/// Script-side report validation from the TypeScript `RALPH_SCRIPT`.
fn validate_report(
    value: &Value,
    max_handoff_chars: usize,
) -> std::result::Result<RalphRoundReport, String> {
    if !value.is_object() || value.is_null() {
        return Err("Ralph child returned no structured round report".into());
    }
    let summary = normalized_text(value.get("summary").unwrap_or(&Value::Null))
        .ok_or_else(|| "Ralph round report summary must be non-empty and normalized".to_string())?;
    let evidence = normalized_list(value.get("evidence").unwrap_or(&Value::Null)).ok_or_else(|| {
        "Ralph round report evidence and nextSteps must contain only non-empty normalized strings"
            .to_string()
    })?;
    let next_steps = normalized_list(value.get("nextSteps").unwrap_or(&Value::Null)).ok_or_else(
        || {
            "Ralph round report evidence and nextSteps must contain only non-empty normalized strings"
                .to_string()
        },
    )?;
    let blocker = value
        .get("blocker")
        .and_then(Value::as_str)
        .filter(|text| *text == text.trim())
        .ok_or_else(|| "Ralph round report blocker must be a normalized string".to_string())?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match status {
        "continue" => {
            if next_steps.is_empty() || !blocker.is_empty() {
                return Err(
                    "a continuing Ralph report needs nextSteps and an empty blocker".into(),
                );
            }
        }
        "complete" => {
            if evidence.is_empty() || !next_steps.is_empty() || !blocker.is_empty() {
                return Err(
                    "a complete Ralph report needs evidence, no nextSteps, and an empty blocker"
                        .into(),
                );
            }
        }
        "blocked" => {
            if normalized_text(&Value::String(blocker.to_string())).is_none() {
                return Err("a blocked Ralph report needs a concrete blocker".into());
            }
        }
        _ => return Err("Ralph round report status is invalid".into()),
    }
    let report = RalphRoundReport {
        status: status.to_string(),
        summary: summary.to_string(),
        evidence,
        next_steps,
        blocker: blocker.to_string(),
    };
    let serialized = serde_json::to_string(&report).expect("report is JSON");
    if serialized.len() > max_handoff_chars {
        return Err(format!(
            "Ralph round report exceeds maxHandoffChars ({} > {max_handoff_chars})",
            serialized.len()
        ));
    }
    Ok(report)
}

/// Bound complete parent-facing text, including its envelope and truncation marker.
pub fn bound_result(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    if max_chars <= TRUNCATION_NOTICE.len() {
        return TRUNCATION_NOTICE.chars().take(max_chars).collect();
    }
    let keep = max_chars - TRUNCATION_NOTICE.len();
    let mut cut = text.chars().take(keep).collect::<String>();
    while cut.len() > keep {
        cut.pop();
    }
    cut.push_str(TRUNCATION_NOTICE);
    cut
}

fn pretty_report(report: &RalphRoundReport) -> String {
    serde_json::to_string_pretty(report).expect("report is JSON")
}

fn render_result(result: &RalphRunResult, max_chars: usize) -> String {
    let rounds = if result.rounds_started == 1 {
        "1 round".to_string()
    } else {
        format!("{} rounds", result.rounds_started)
    };
    let pretty = pretty_report(&result.report);
    let text = match result.status.as_str() {
        "complete" => format!(
            "Ralph worker reported completion after {rounds}.\nFinal report:\n{pretty}"
        ),
        "blocked" => {
            format!("Ralph worker reported a blocker after {rounds}.\nFinal report:\n{pretty}")
        }
        _ => format!(
            "Ralph reached its {rounds} limit; the worker reported work remaining.\nFinal report:\n{pretty}"
        ),
    };
    bound_result(&text, max_chars)
}

fn render_round_failure(
    rounds_started: u64,
    last_report: Option<&RalphRoundReport>,
    max_chars: usize,
) -> String {
    let header =
        format!("Ralph round {rounds_started} child failed before producing a structured report.");
    let text = match last_report {
        None => format!("{header}\nNo previous handoff was available."),
        Some(report) => format!(
            "{header}\nLast successful handoff:\n{}",
            pretty_report(report)
        ),
    };
    bound_result(&text, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_llm::ContentBlock;
    use dsh_subagent::{SubagentError, SubagentProvider, SubagentResult, SubagentRun};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const CONTINUE: &str = r#"{"status":"continue","summary":"Implemented the first slice.","evidence":["Focused tests pass."],"nextSteps":["Implement the second slice."],"blocker":""}"#;
    const COMPLETE: &str = r#"{"status":"complete","summary":"The objective is complete.","evidence":["All required gates pass."],"nextSteps":[],"blocker":""}"#;
    const BLOCKED: &str = r#"{"status":"blocked","summary":"No local work can progress.","evidence":["The required remote service is unavailable."],"nextSteps":["Retry after service recovery."],"blocker":"The required remote service is unavailable."}"#;

    struct ScriptedProvider {
        name: String,
        output_schema: bool,
        inherits: bool,
        replies: Mutex<VecDeque<std::result::Result<String, String>>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn fresh(replies: Vec<std::result::Result<String, String>>) -> Self {
            Self {
                name: "fresh".into(),
                output_schema: true,
                inherits: false,
                replies: Mutex::new(replies.into()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SubagentProvider for ScriptedProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn inherits_parent_context(&self) -> bool {
            self.inherits
        }
        fn supports_output_schema(&self) -> bool {
            self.output_schema
        }
        async fn start(
            &self,
            request: SubagentStartRequest,
        ) -> std::result::Result<SubagentRun, SubagentError> {
            self.prompts.lock().expect("prompts").push(request.prompt);
            let reply = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .unwrap_or_else(|| Err("no scripted Ralph reply".into()));
            match reply {
                Ok(output) => Ok(SubagentRun::ready(SubagentResult {
                    output,
                    id: session_id("child"),
                    stop_reason: "completed".into(),
                })),
                Err(error) => Err(SubagentError::NoProvider(error)),
            }
        }
    }

    fn setup(
        config: Config,
        provider: Option<ScriptedProvider>,
    ) -> (Context, Arc<ScriptedProvider>) {
        let ctx = Context::new();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        let provider = Arc::new(provider.unwrap_or_else(|| ScriptedProvider::fresh(vec![])));
        ctx.service::<SubagentRuntime>()
            .unwrap()
            .register_provider(Arc::clone(&provider) as Arc<dyn SubagentProvider>)
            .unwrap();
        install(&ctx, config).unwrap();
        (ctx, provider)
    }

    fn default_config() -> Config {
        Config {
            subagent_provider: "fresh".into(),
            max_rounds: 9,
            max_handoff_chars: 9000,
            max_result_chars: 16_384,
        }
    }

    async fn execute(ctx: &Context, args: Value, agent: Option<&str>) -> ToolOutcome {
        ctx.service::<ToolRuntime>()
            .unwrap()
            .execute_for(ctx, "ralph", args, agent)
            .await
            .unwrap()
            .outcome
    }

    fn text_of(outcome: &ToolOutcome) -> &str {
        match &outcome.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("text"),
        }
    }

    #[test]
    fn resolve_rejects_invalid_direct_apply_config() {
        assert!(Config::resolve(Some(&json!({ "subagentProvider": " " })))
            .unwrap_err()
            .contains("non-empty normalized"));
        assert!(Config::resolve(Some(&json!({ "maxRounds": 0 })))
            .unwrap_err()
            .contains("positive safe integer"));
        assert!(Config::resolve(Some(&json!({ "maxHandoffChars": 1.5 })))
            .unwrap_err()
            .contains("positive safe integer"));
        assert!(Config::resolve(Some(&json!({ "maxResultChars": 0 })))
            .unwrap_err()
            .contains("positive safe integer"));
    }

    #[test]
    fn resolve_defaults_match_typescript() {
        let config = Config::resolve(None).unwrap();
        assert_eq!(config.subagent_provider, "spawn");
        assert_eq!(config.max_rounds, 256);
        assert_eq!(config.max_handoff_chars, 16_384);
        assert_eq!(config.max_result_chars, 16_384);
    }

    #[tokio::test]
    async fn renders_completion_and_uses_trimmed_objective() {
        let (ctx, provider) = setup(
            default_config(),
            Some(ScriptedProvider::fresh(vec![Ok(COMPLETE.into())])),
        );
        let outcome = execute(
            &ctx,
            json!({ "objective": "  Finish the migration.  ", "maxRounds": 4 }),
            Some("caller"),
        )
        .await;
        assert!(!outcome.is_error);
        let text = text_of(&outcome);
        assert!(text.contains("Ralph worker reported completion after 1 round."));
        assert!(text.contains("All required gates pass."));
        let prompt = &provider.prompts.lock().unwrap()[0];
        assert!(prompt.contains("Immutable objective:\nFinish the migration."));
        assert!(prompt.contains("Ralph round: 1 of 4."));
        assert!(prompt.contains("(none — this is the first round)"));
    }

    #[tokio::test]
    async fn renders_blocked_and_budget_limited() {
        let (ctx, _) = setup(
            Config {
                max_rounds: 2,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![Ok(BLOCKED.into())])),
        );
        let blocked = execute(&ctx, json!({ "objective": "Ship it." }), Some("caller")).await;
        assert!(text_of(&blocked).contains("Ralph worker reported a blocker after 1 round."));

        let (ctx, _) = setup(
            Config {
                max_rounds: 2,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![
                Ok(CONTINUE.into()),
                Ok(CONTINUE.into()),
            ])),
        );
        let limited = execute(&ctx, json!({ "objective": "Ship it." }), Some("caller")).await;
        assert!(text_of(&limited)
            .contains("Ralph reached its 2 rounds limit; the worker reported work remaining."));
        assert!(text_of(&limited).contains("Implemented the first slice."));
    }

    #[tokio::test]
    async fn bounds_parent_result_and_short_marker() {
        let (ctx, _) = setup(
            Config {
                max_result_chars: 160,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![Ok(
                r#"{"status":"complete","summary":"The objective is complete.","evidence":["xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"],"nextSteps":[],"blocker":""}"#.into(),
            )])),
        );
        let outcome = execute(&ctx, json!({ "objective": "Ship it." }), Some("caller")).await;
        let text = text_of(&outcome);
        assert_eq!(text.len(), 160);
        assert!(text.contains("Ralph worker reported completion after 1 round."));
        assert!(text.ends_with("… [truncated]"));

        let (ctx, _) = setup(
            Config {
                max_result_chars: 5,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![Ok(COMPLETE.into())])),
        );
        let short = execute(&ctx, json!({ "objective": "Ship it." }), Some("caller")).await;
        assert_eq!(text_of(&short), "\n… [t");
    }

    #[tokio::test]
    async fn reports_round_failure_with_and_without_handoff() {
        let (ctx, _) = setup(
            Config {
                max_rounds: 2,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![Ok("not-json".into())])),
        );
        let first = execute(
            &ctx,
            json!({ "objective": "Ship it.", "maxRounds": 2 }),
            Some("caller"),
        )
        .await;
        assert!(first.is_error);
        assert!(text_of(&first).contains("Ralph round 1 child failed"));
        assert!(text_of(&first).contains("No previous handoff was available."));

        let (ctx, _) = setup(
            Config {
                max_rounds: 2,
                ..default_config()
            },
            Some(ScriptedProvider::fresh(vec![
                Ok(CONTINUE.into()),
                Ok("not-json".into()),
            ])),
        );
        let later = execute(
            &ctx,
            json!({ "objective": "Ship it.", "maxRounds": 2 }),
            Some("caller"),
        )
        .await;
        assert!(later.is_error);
        assert!(text_of(&later).contains("Ralph round 2 child failed"));
        assert!(text_of(&later).contains("Implemented the first slice."));
    }

    #[tokio::test]
    async fn maps_start_failure_to_workflow_error() {
        let (ctx, _) = setup(
            default_config(),
            Some(ScriptedProvider::fresh(vec![Err(
                "engine refused fixed script".into(),
            )])),
        );
        let outcome = execute(&ctx, json!({ "objective": "Work." }), Some("caller")).await;
        assert!(outcome.is_error);
        assert!(text_of(&outcome).contains("Ralph workflow failed:"));
        assert!(text_of(&outcome).contains("engine refused fixed script"));
    }

    #[tokio::test]
    async fn rejects_absent_authority_empty_objective_and_bad_caps() {
        let (ctx, provider) = setup(default_config(), Some(ScriptedProvider::fresh(vec![])));
        assert!(
            execute(&ctx, json!({ "objective": "Work." }), None)
                .await
                .is_error
        );
        assert!(
            execute(&ctx, json!({ "objective": "   " }), Some("caller"))
                .await
                .is_error
        );
        for max_rounds in [json!(0), json!(1.5), json!(10)] {
            let outcome = execute(
                &ctx,
                json!({ "objective": "Work.", "maxRounds": max_rounds }),
                Some("caller"),
            )
            .await;
            assert!(outcome.is_error, "{max_rounds}");
        }
        assert!(provider.prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_missing_unstructured_and_inheriting_providers() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        install(&ctx, default_config()).unwrap();
        let missing = execute(&ctx, json!({ "objective": "Work." }), Some("caller")).await;
        assert!(text_of(&missing).contains("is not registered"));

        let unstructured = ScriptedProvider {
            output_schema: false,
            ..ScriptedProvider::fresh(vec![])
        };
        let (ctx, _) = setup(default_config(), Some(unstructured));
        assert!(
            text_of(&execute(&ctx, json!({ "objective": "Work." }), Some("caller")).await)
                .contains("does not support structured output")
        );

        let inherited = ScriptedProvider {
            inherits: true,
            ..ScriptedProvider::fresh(vec![])
        };
        let (ctx, _) = setup(default_config(), Some(inherited));
        assert!(
            text_of(&execute(&ctx, json!({ "objective": "Work." }), Some("caller")).await)
                .contains("inherits parent context")
        );
    }

    #[tokio::test]
    async fn invalid_complete_report_is_a_workflow_failure() {
        let (ctx, _) = setup(
            default_config(),
            Some(ScriptedProvider::fresh(vec![Ok(
                r#"{"status":"complete","summary":"done","evidence":[],"nextSteps":[],"blocker":""}"#.into(),
            )])),
        );
        let outcome = execute(&ctx, json!({ "objective": "Work." }), Some("caller")).await;
        assert!(outcome.is_error);
        assert!(text_of(&outcome).contains("Ralph workflow failed:"));
        assert!(text_of(&outcome).contains("a complete Ralph report needs evidence"));
    }

    #[tokio::test]
    async fn registers_scoped_guidance() {
        let (ctx, _) = setup(default_config(), Some(ScriptedProvider::fresh(vec![])));
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        let assembly = prompt.assemble(Vec::new());
        assert!(assembly
            .system
            .contains("ONLY when the direct human explicitly asks"));
        assert!(assembly
            .system
            .contains("worker reports, not independent evaluation"));
        let tool = ctx.service::<ToolRuntime>().unwrap().get("ralph").unwrap();
        assert!(tool.description().contains("worker reports completion"));
    }

    #[test]
    fn bound_result_matches_typescript_short_marker() {
        assert_eq!(bound_result("abcdefghij", 5), "\n… [t");
    }
}
