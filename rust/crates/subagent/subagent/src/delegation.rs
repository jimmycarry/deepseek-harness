//! Child-session policy captured at a delegation boundary.
//!
//! Both in-process start paths share these helpers so a child's effective
//! sandbox override and approval pin are reconstructable from its own log.
//! The services are optional `ctx.get` consumers: an uncomposed capability
//! contributes nothing.

use crate::SubagentRuntime;
use dsh_cordis::{Context, Result};
use dsh_sandbox::SandboxMode;
use dsh_sandbox_policy::SandboxPolicyService;
use dsh_session::{Session, SessionError, SessionEventData};
use dsh_system_prompt::{PromptContext, PromptContextText, SystemPrompt};
use dsh_user_approval::{ApprovalPolicy, ApprovalService};
use std::sync::Arc;

/// Model-facing delegation-scope statement. Matches TypeScript
/// `SUBAGENT_DELEGATION_CONTEXT` exactly.
pub const SUBAGENT_DELEGATION_CONTEXT: &str = "You are a delegated subagent: your permission scope was fixed when you were started and cannot be widened from inside this session — operations that require approval are rejected automatically. When the task needs access beyond that scope, do not retry the denied operation; state the limitation in your reply so the delegating agent can handle it.";

/// Policy seeded onto a child session's log at the delegation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedPolicyOverrides {
    /// The parent session's explicit sandbox-mode override, or `None` without one.
    pub sandbox_mode: Option<SandboxMode>,
    /// [`ApprovalPolicy::Never`] whenever the approval capability is composed.
    pub approval_policy: Option<ApprovalPolicy>,
}

/// Capture the policy to seed into one delegation.
///
/// Call synchronously before the child start's first await: a later parent
/// switch belongs to the parent's future, not to this child. Only the parent
/// session's explicit sandbox override is captured — never deployment defaults
/// or one-shot grants — and the approval policy is pinned to `never` whenever
/// `ctx.approval` is composed, regardless of the parent's own policy.
pub fn capture_delegated_policy_overrides(
    ctx: &Context,
    parent_session: Option<&Session>,
) -> DelegatedPolicyOverrides {
    let sandbox_mode = parent_session.and_then(|session| {
        ctx.get::<SandboxPolicyService>()
            .and_then(|policy| policy.override_of(session))
    });
    let approval_policy = ctx.get::<ApprovalService>().map(|_| ApprovalPolicy::Never);
    DelegatedPolicyOverrides {
        sandbox_mode,
        approval_policy,
    }
}

/// Append the captured delegation policy onto the child's own log as
/// `source: "delegation"` events so the child's effective policy is
/// reconstructable from its log alone.
///
/// Call during unpublished setup, after any fork seed, so fresh policy wins
/// stale seed state. Later child-side switches still win over these events.
///
/// # Errors
/// A refused session append.
pub fn append_delegated_policy_overrides(
    child_session: &Session,
    overrides: &DelegatedPolicyOverrides,
) -> std::result::Result<(), SessionError> {
    if let Some(mode) = overrides.sandbox_mode {
        child_session.append(
            SessionEventData::SandboxMode {
                mode: mode.as_str().to_string(),
                source: Some("delegation".into()),
            },
            None,
        )?;
    }
    if let Some(policy) = overrides.approval_policy {
        child_session.append(
            SessionEventData::ApprovalPolicy {
                policy: policy.as_str().to_string(),
                source: Some("delegation".into()),
            },
            None,
        )?;
    }
    Ok(())
}

/// Register `subagent:delegation` (order 120) when both the prompt assembler
/// and the subagent runtime are present. Empty text for a top-level session;
/// same-name registration replaces.
///
/// # Errors
/// Prompt registration does not fail; this returns `Ok` when either service is absent.
pub fn bind_prompt(ctx: &Context) -> Result<()> {
    let Some(prompt) = ctx.get::<SystemPrompt>() else {
        return Ok(());
    };
    if ctx.get::<SubagentRuntime>().is_none() {
        return Ok(());
    }
    prompt.register_context(PromptContext {
        name: "subagent:delegation".into(),
        order: 120,
        text: PromptContextText::Dynamic(Arc::new(|session| match session {
            Some(session) if is_delegated_session(session) => {
                SUBAGENT_DELEGATION_CONTEXT.to_string()
            }
            _ => String::new(),
        })),
    });
    Ok(())
}

fn is_delegated_session(session: &Session) -> bool {
    session.header().origin.as_deref() == Some("subagent") || session.header().delegation_depth > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use dsh_sandbox_policy::set_sandbox_mode;
    use dsh_session::{session_id, SessionHeader, SessionStore};
    use dsh_user_approval::set_approval_policy;

    fn policy_root() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    fn install_sandbox(ctx: &Context) {
        dsh_sandbox_policy::install(
            ctx,
            Some(&serde_json::json!({
                "mode": "workspace-write",
                "workspaceRoot": policy_root()
            })),
        )
        .unwrap();
    }

    #[test]
    fn capture_without_services_or_parent_is_empty() {
        let ctx = Context::new();
        let captured = capture_delegated_policy_overrides(&ctx, None);
        assert_eq!(
            captured,
            DelegatedPolicyOverrides {
                sandbox_mode: None,
                approval_policy: None,
            }
        );
    }

    #[test]
    fn capture_pins_never_when_approval_is_composed_without_a_parent() {
        let ctx = Context::new();
        dsh_user_approval::install(&ctx, None).unwrap();
        let captured = capture_delegated_policy_overrides(&ctx, None);
        assert_eq!(captured.sandbox_mode, None);
        assert_eq!(captured.approval_policy, Some(ApprovalPolicy::Never));
    }

    #[test]
    fn capture_ignores_deployment_default_without_a_session_override() {
        let ctx = Context::new();
        install_sandbox(&ctx);
        dsh_user_approval::install(&ctx, None).unwrap();
        let parent = SessionStore::new().create(session_id("parent"));
        let captured = capture_delegated_policy_overrides(&ctx, Some(parent.as_ref()));
        assert_eq!(captured.sandbox_mode, None);
        assert_eq!(captured.approval_policy, Some(ApprovalPolicy::Never));
    }

    #[test]
    fn capture_reads_explicit_sandbox_override_and_ignores_parent_approval() {
        let ctx = Context::new();
        install_sandbox(&ctx);
        dsh_user_approval::install(&ctx, Some(&serde_json::json!({ "policy": "ask" }))).unwrap();
        let parent = SessionStore::new().create(session_id("parent"));
        set_sandbox_mode(parent.as_ref(), SandboxMode::DangerFullAccess).unwrap();
        set_approval_policy(parent.as_ref(), ApprovalPolicy::Ask).unwrap();
        let captured = capture_delegated_policy_overrides(&ctx, Some(parent.as_ref()));
        assert_eq!(captured.sandbox_mode, Some(SandboxMode::DangerFullAccess));
        assert_eq!(captured.approval_policy, Some(ApprovalPolicy::Never));
    }

    #[test]
    fn capture_without_sandbox_policy_service_skips_parent_mode_events() {
        let ctx = Context::new();
        let parent = SessionStore::new().create(session_id("parent"));
        parent
            .append(
                SessionEventData::SandboxMode {
                    mode: "read-only".into(),
                    source: None,
                },
                None,
            )
            .unwrap();
        let captured = capture_delegated_policy_overrides(&ctx, Some(parent.as_ref()));
        assert_eq!(captured.sandbox_mode, None);
        assert_eq!(captured.approval_policy, None);
    }

    #[test]
    fn append_writes_nothing_for_empty_overrides() {
        let child = SessionStore::new().create(session_id("child"));
        append_delegated_policy_overrides(
            child.as_ref(),
            &DelegatedPolicyOverrides {
                sandbox_mode: None,
                approval_policy: None,
            },
        )
        .unwrap();
        assert!(child.events().is_empty());
    }

    #[test]
    fn append_writes_sandbox_only() {
        let child = SessionStore::new().create(session_id("child"));
        append_delegated_policy_overrides(
            child.as_ref(),
            &DelegatedPolicyOverrides {
                sandbox_mode: Some(SandboxMode::ReadOnly),
                approval_policy: None,
            },
        )
        .unwrap();
        let events = child.events();
        assert_eq!(events.len(), 1);
        match &events[0].data {
            SessionEventData::SandboxMode { mode, source } => {
                assert_eq!(mode, "read-only");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn append_writes_approval_only() {
        let child = SessionStore::new().create(session_id("child"));
        append_delegated_policy_overrides(
            child.as_ref(),
            &DelegatedPolicyOverrides {
                sandbox_mode: None,
                approval_policy: Some(ApprovalPolicy::Never),
            },
        )
        .unwrap();
        match &child.events()[0].data {
            SessionEventData::ApprovalPolicy { policy, source } => {
                assert_eq!(policy, "never");
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn append_after_fork_seed_wins_stale_sandbox_state() {
        let child = SessionStore::new().create(session_id("child"));
        child
            .append(
                SessionEventData::SandboxMode {
                    mode: "workspace-write".into(),
                    source: None,
                },
                None,
            )
            .unwrap();
        append_delegated_policy_overrides(
            child.as_ref(),
            &DelegatedPolicyOverrides {
                sandbox_mode: Some(SandboxMode::ReadOnly),
                approval_policy: Some(ApprovalPolicy::Never),
            },
        )
        .unwrap();
        let events = child.events();
        assert_eq!(events.len(), 3);
        assert_eq!(
            dsh_sandbox_policy::effective_sandbox_mode(&events),
            Some(SandboxMode::ReadOnly)
        );
        assert_eq!(
            dsh_user_approval::effective_approval_policy(&events),
            Some(ApprovalPolicy::Never)
        );
        match &events[1].data {
            SessionEventData::SandboxMode { source, .. } => {
                assert_eq!(source.as_deref(), Some("delegation"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn later_child_switch_wins_over_delegation_snapshot() {
        let child = SessionStore::new().create(session_id("child"));
        append_delegated_policy_overrides(
            child.as_ref(),
            &DelegatedPolicyOverrides {
                sandbox_mode: Some(SandboxMode::DangerFullAccess),
                approval_policy: None,
            },
        )
        .unwrap();
        set_sandbox_mode(child.as_ref(), SandboxMode::ReadOnly).unwrap();
        assert_eq!(
            dsh_sandbox_policy::effective_sandbox_mode(&child.events()),
            Some(SandboxMode::ReadOnly)
        );
        match &child.events()[1].data {
            SessionEventData::SandboxMode { source, .. } => {
                assert!(source.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bind_prompt_is_quiet_without_system_prompt() {
        let ctx = Context::new();
        SubagentRuntime::install(&ctx).unwrap();
        bind_prompt(&ctx).unwrap();
    }

    #[test]
    fn bind_prompt_is_quiet_without_subagent_runtime() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        bind_prompt(&ctx).unwrap();
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        let child = Session::with_header(SessionHeader::for_subagent_child(
            None,
            session_id("parent"),
        ));
        assert!(prompt.context_sections(Some(&child)).is_empty());
    }

    #[test]
    fn delegation_context_is_empty_for_a_top_level_session() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        assert!(prompt.context_sections(None).is_empty());
        let parent = SessionStore::new().create(session_id("parent"));
        assert!(prompt.context_sections(Some(parent.as_ref())).is_empty());
    }

    #[test]
    fn delegation_context_renders_for_origin_or_depth() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        SubagentRuntime::install(&ctx).unwrap();
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        let child = Session::with_header(SessionHeader::for_subagent_child(
            None,
            session_id("parent"),
        ));
        let sections = prompt.context_sections(Some(&child));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "subagent:delegation");
        assert_eq!(sections[0].text, SUBAGENT_DELEGATION_CONTEXT);

        let mut header = SessionHeader::new(session_id("depth-only"), None);
        header.delegation_depth = 1;
        let depth_only = Session::with_header(header);
        let sections = prompt.context_sections(Some(&depth_only));
        assert_eq!(sections[0].text, SUBAGENT_DELEGATION_CONTEXT);
    }
}
