//! Plan mode as logged state (`ctx.planMode`).
//!
//! Activation is committed as a log-only `plan/mode` event; the configured
//! policy `section` renders as prompt section `plan:policy` (order 50) only
//! while plan mode is in force. `exit_plan_mode` presents the plan through
//! `ctx.userQuestions`; without a registered provider (automation
//! assemblies) it fails with the provider's exact sentence and the session
//! stays in plan mode.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, CordisError, Service};
use dsh_session::{session_id, Session, SessionEventData};
use dsh_system_prompt::{PromptSection, SystemPrompt};
use dsh_tools::{Tool, ToolCall, ToolError, ToolOutcome, ToolRuntime};
use dsh_user_questions::{UserQuestion, UserQuestionsService};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Required deployment policy text; this plugin adds no defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Plan-mode policy section rendered while plan mode is active.
    pub section: String,
}

impl Config {
    /// Validate raw cordis.yml config: exactly one non-empty `section`.
    ///
    /// # Errors
    /// A missing/empty `section` or unknown keys.
    pub fn resolve(config: Option<&Value>) -> Result<Self, String> {
        let map = config
            .and_then(Value::as_object)
            .ok_or_else(|| "plan-mode: config must supply a non-empty section".to_string())?;
        let unknown: Vec<&str> = map
            .keys()
            .filter(|key| key.as_str() != "section")
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "PlanModeConfig has unknown key(s) {} — config is {{ section }}",
                unknown.join(", ")
            ));
        }
        let section = map
            .get("section")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        if section.trim().is_empty() {
            return Err("plan-mode: config must supply a non-empty section".into());
        }
        Ok(Self { section })
    }
}

/// `ctx.planMode`.
pub struct PlanRuntime {
    active: AtomicBool,
    section: String,
    prompt: Arc<SystemPrompt>,
}

impl PlanRuntime {
    /// Whether plan mode is currently in force.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Commit one transition: append `plan/mode` and re-render `plan:policy`.
    ///
    /// # Errors
    /// A refused session append.
    pub fn set_active(&self, session: &Session, active: bool) -> Result<(), String> {
        session
            .append(SessionEventData::PlanMode { active }, None)
            .map_err(|error| error.to_string())?;
        self.active.store(active, Ordering::SeqCst);
        self.render(active);
        Ok(())
    }

    fn render(&self, active: bool) {
        self.prompt.register_section(PromptSection {
            id: "plan:policy".into(),
            text: if active {
                self.section.clone()
            } else {
                String::new()
            },
            order: 50,
        });
    }
}

impl Service for PlanRuntime {
    const KEY: &'static str = "planMode";
}

/// `exit_plan_mode`: present the plan for review; approval leaves plan mode.
struct ExitPlanModeTool {
    plan: Arc<PlanRuntime>,
    agents: Arc<AgentRegistry>,
    lookup: Context,
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "exit_plan_mode"
    }

    fn description(&self) -> &str {
        "Use only in plan mode. Present your plan for the user's review; approval exits plan mode so implementation can begin."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The complete plan, as markdown, starting with a # heading that names it."
                }
            },
            "required": ["plan"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        Err(ToolError::Body(
            "Error: exit_plan_mode requires a calling agent (no session to switch)".into(),
        ))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let agent = call
            .agent_id
            .as_deref()
            .and_then(|id| self.agents.get(&session_id(id)))
            .ok_or_else(|| {
                ToolError::Body(
                    "Error: exit_plan_mode requires a calling agent (no session to switch)".into(),
                )
            })?;
        if !self.plan.is_active() {
            return Err(ToolError::Body(
                "Error: exit_plan_mode is only available in plan mode".into(),
            ));
        }
        let plan_text = call
            .args
            .get("plan")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !starts_with_heading(&plan_text) {
            return Err(ToolError::Body(
                "Error: exit_plan_mode requires a non-empty markdown plan starting with a # heading"
                    .into(),
            ));
        }
        let Some(questions) = self.lookup.get::<UserQuestionsService>() else {
            return Err(ToolError::Body(
                "Error: no user-questions channel is available to review the plan; ask the user to switch the session mode instead".into(),
            ));
        };
        let reply = questions
            .ask(UserQuestion {
                id: "plan-review".into(),
                header: "Plan review".into(),
                question: "Approve this plan and leave plan mode?".into(),
                options: vec!["Approve".into(), "Keep planning".into()],
            })
            .await
            .map_err(|error| ToolError::Body(format!("Error: {error}")))?;
        if reply.choice != "Approve" {
            return Err(ToolError::Body(match reply.feedback {
                Some(feedback) if !feedback.trim().is_empty() => {
                    format!("Error: The user chose to keep planning; their feedback: {feedback}")
                }
                _ => {
                    "Error: The user chose to keep planning; revise the plan and present it again."
                        .into()
                }
            }));
        }
        self.plan
            .set_active(agent.session().as_ref(), false)
            .map_err(ToolError::Body)?;
        Ok(ToolOutcome::text(
            "Plan approved — plan mode exited; carry out the plan starting with your next step.",
        ))
    }
}

/// Whether a trimmed plan starts with `# <name>` (the TypeScript `/^#\s+\S/`).
fn starts_with_heading(plan: &str) -> bool {
    let Some(rest) = plan.strip_prefix('#') else {
        return false;
    };
    let after_gap = rest.trim_start();
    after_gap.len() < rest.len() && !after_gap.is_empty()
}

/// Install `ctx.planMode`, `exit_plan_mode`, and the `/plan` command.
///
/// # Errors
/// Missing `ctx.systemPrompt`, `ctx.tools`, or `ctx.agents`.
pub fn install(ctx: &Context, config: Config) -> dsh_cordis::Result<Arc<PlanRuntime>> {
    let prompt = ctx.service::<SystemPrompt>()?;
    let tools = ctx.service::<ToolRuntime>()?;
    let agents = ctx.service::<AgentRegistry>()?;
    let plan = Arc::new(PlanRuntime {
        active: AtomicBool::new(false),
        section: config.section,
        prompt,
    });
    plan.render(false);
    ctx.provide(Arc::clone(&plan))?;
    tools.insert(Arc::new(ExitPlanModeTool {
        plan: Arc::clone(&plan),
        agents,
        lookup: ctx.clone(),
    }));
    if let Some(commands) = ctx.get::<dsh_commands::CommandRegistry>() {
        commands.register(
            ctx,
            dsh_commands::Command {
                name: "plan".into(),
                description: "Toggle plan mode".into(),
                model_visible: false,
                record_input: true,
                handler: Arc::new(PlanCommand {
                    plan: Arc::clone(&plan),
                    lookup: ctx.clone(),
                }),
            },
        )?;
    }
    Ok(plan)
}

struct PlanCommand {
    plan: Arc<PlanRuntime>,
    lookup: Context,
}

#[async_trait]
impl dsh_commands::CommandHandler for PlanCommand {
    async fn handle(&self, args: &str) -> Result<String, String> {
        let want_off = args.trim() == "off";
        if !want_off && !args.trim().is_empty() {
            return Err("Usage: /plan [off]".into());
        }
        let session = self
            .lookup
            .serial("command/plan", serde_json::json!({}))
            .and_then(|value| {
                value
                    .get("sessionId")
                    .and_then(|id| id.as_str().map(str::to_string))
            })
            .and_then(|id| {
                self.lookup
                    .get::<AgentRegistry>()
                    .and_then(|agents| agents.get(&session_id(id)))
            })
            .ok_or_else(|| "sessionId required".to_string())?
            .session();
        if want_off {
            if !self.plan.is_active() {
                return Ok("Plan mode is already inactive.".into());
            }
            self.plan.set_active(session.as_ref(), false)?;
            return Ok("Plan mode off.".into());
        }
        if self.plan.is_active() {
            return Ok("Plan mode on. Use /plan off to leave.".into());
        }
        self.plan.set_active(session.as_ref(), true)?;
        Ok("Plan mode on. Use /plan off to leave.".into())
    }
}

/// Resolve config and install; the loader entry point.
///
/// # Errors
/// Invalid config or missing dependencies.
pub fn apply(ctx: &Context, config: Option<&Value>) -> dsh_cordis::Result<Arc<PlanRuntime>> {
    let resolved = Config::resolve(config).map_err(CordisError::Validation)?;
    install(ctx, resolved)
}

/// Plugin name used by loader diagnostics.
pub fn name() -> &'static str {
    "dsh-plan-mode"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_agent::{
        Agent, AgentCancelCause, AgentError, AgentFactory, AgentStatus, Inbox, InboxTarget,
    };
    use dsh_user_questions::{UserQuestionProvider, UserQuestionReply};

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

    struct StubFactory;

    impl AgentFactory for StubFactory {
        fn create(&self, session: Arc<Session>) -> Arc<dyn Agent> {
            Arc::new(StubAgent {
                session,
                inbox: Arc::new(Inbox::default()),
            })
        }
    }

    fn base_ctx(agent: &str) -> (Context, Arc<PlanRuntime>) {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        ctx.provide(Arc::new(ToolRuntime::new())).unwrap();
        let agents = AgentRegistry::new();
        agents.set_factory(Arc::new(StubFactory));
        agents
            .create(Arc::new(Session::new(session_id(agent))))
            .unwrap();
        ctx.provide(Arc::new(agents)).unwrap();
        let plan = install(
            &ctx,
            Config {
                section: "You are in plan mode.".into(),
            },
        )
        .unwrap();
        (ctx, plan)
    }

    fn call(args: Value, agent_id: Option<&str>) -> ToolCall {
        ToolCall {
            name: "exit_plan_mode".into(),
            args,
            agent_id: agent_id.map(str::to_string),
            call_id: None,
        }
    }

    #[test]
    fn config_rejects_unknown_keys_and_empty_section() {
        assert!(Config::resolve(None).is_err());
        assert!(Config::resolve(Some(&serde_json::json!({ "section": "  " }))).is_err());
        let err =
            Config::resolve(Some(&serde_json::json!({ "section": "x", "extra": 1 }))).unwrap_err();
        assert!(err.contains("unknown key(s) extra"), "{err}");
    }

    #[test]
    fn active_section_renders_and_clears() {
        let (ctx, plan) = base_ctx("plan-a");
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        assert!(!prompt.assemble(vec![]).system.contains("plan mode"));
        let agents = ctx.service::<AgentRegistry>().unwrap();
        let session = agents.get(&session_id("plan-a")).unwrap().session();
        plan.set_active(session.as_ref(), true).unwrap();
        assert!(prompt
            .assemble(vec![])
            .system
            .contains("You are in plan mode."));
        assert!(matches!(
            session.events()[0].data,
            SessionEventData::PlanMode { active: true }
        ));
        plan.set_active(session.as_ref(), false).unwrap();
        assert!(!prompt.assemble(vec![]).system.contains("plan mode"));
    }

    #[tokio::test]
    async fn exit_plan_mode_guards_and_headless_failure() {
        let (ctx, plan) = base_ctx("plan-b");
        let tools = ctx.service::<ToolRuntime>().unwrap();
        let tool = tools.get("exit_plan_mode").unwrap();
        let inactive = tool
            .execute_call(&call(serde_json::json!({ "plan": "# P" }), Some("plan-b")))
            .await
            .unwrap_err();
        assert!(matches!(
            inactive,
            ToolError::Body(message) if message == "Error: exit_plan_mode is only available in plan mode"
        ));
        let agents = ctx.service::<AgentRegistry>().unwrap();
        let session = agents.get(&session_id("plan-b")).unwrap().session();
        plan.set_active(session.as_ref(), true).unwrap();
        let no_heading = tool
            .execute_call(&call(
                serde_json::json!({ "plan": "no heading" }),
                Some("plan-b"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            no_heading,
            ToolError::Body(message)
                if message
                    == "Error: exit_plan_mode requires a non-empty markdown plan starting with a # heading"
        ));
        let no_channel = tool
            .execute_call(&call(
                serde_json::json!({ "plan": "# Plan" }),
                Some("plan-b"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            no_channel,
            ToolError::Body(message)
                if message
                    == "Error: no user-questions channel is available to review the plan; ask the user to switch the session mode instead"
        ));
        UserQuestionsService::install(&ctx).unwrap();
        let no_provider = tool
            .execute_call(&call(
                serde_json::json!({ "plan": "# Plan" }),
                Some("plan-b"),
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            no_provider,
            ToolError::Body(message)
                if message == "Error: no user-questions provider is registered"
        ));
        assert!(plan.is_active(), "failure keeps plan mode");
    }

    #[tokio::test]
    async fn approval_exits_plan_mode() {
        struct Approver;
        #[async_trait]
        impl UserQuestionProvider for Approver {
            async fn ask(&self, _: UserQuestion) -> Result<UserQuestionReply, String> {
                Ok(UserQuestionReply {
                    choice: "Approve".into(),
                    feedback: None,
                })
            }
        }
        let (ctx, plan) = base_ctx("plan-c");
        let questions = UserQuestionsService::install(&ctx).unwrap();
        let _disposer = questions.register(Arc::new(Approver));
        let agents = ctx.service::<AgentRegistry>().unwrap();
        let session = agents.get(&session_id("plan-c")).unwrap().session();
        plan.set_active(session.as_ref(), true).unwrap();
        let tools = ctx.service::<ToolRuntime>().unwrap();
        let outcome = tools
            .get("exit_plan_mode")
            .unwrap()
            .execute_call(&call(
                serde_json::json!({ "plan": "# Plan" }),
                Some("plan-c"),
            ))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert!(!plan.is_active());
        assert!(matches!(
            session.events().last().unwrap().data,
            SessionEventData::PlanMode { active: false }
        ));
    }
}
