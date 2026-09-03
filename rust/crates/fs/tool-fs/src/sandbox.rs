//! Sandbox-escalation API shared by `write` and `edit`.
//!
//! Advertisement, per-call policy resolution, and denial-marker mapping all
//! delegate to `dsh-sandbox`. TypeScript captures `ctx.fs.sandboxMode` at
//! apply time because Cordis `inject` delays the plugin until `ctx.fs` is the
//! confining backend. Rust dump order mounts `tool-fs` before `fs-sandbox`, so
//! this controller reads the live `ctx.fs` on every schema and execute call.

use dsh_agent::AgentRegistry;
use dsh_cordis::Context;
use dsh_fs::{FsError, FsErrorCode, FsRuntime, FsWritePolicy};
use dsh_sandbox::{
    approve_escalation, escalation_audit_reason, escalation_hint_marker, sandbox_denial_marker,
    validate_escalation_args, EscalationIngredients, EscalationRequest, ESCALATION_TARGETS,
    SandboxExecutionPolicy,
};
use dsh_sandbox_policy::SandboxPolicyService;
use dsh_session::session_id;
use dsh_user_approval::{ApprovalRequest, ApprovalService};
use serde_json::{json, Value};
use std::sync::Arc;

/// Shared sandbox-escalation API for the mutating filesystem tools.
pub struct FsSandboxController {
    ctx: Context,
    fallback: Arc<FsRuntime>,
}

impl FsSandboxController {
    /// Fail loud when a confining backend is already mounted without a policy resolver.
    ///
    /// # Errors
    /// `ctx.fs` confines and `ctx.sandboxPolicy` is missing.
    pub fn new(ctx: Context, fallback: Arc<FsRuntime>) -> Result<Self, String> {
        let live = FsRuntime::from_context(&ctx, &fallback);
        if live.sandbox_mode().is_some() && ctx.get::<SandboxPolicyService>().is_none() {
            return Err(
                "tool-fs: the mounted filesystem confines but ctx.sandboxPolicy is missing".into(),
            );
        }
        Ok(Self { ctx, fallback })
    }

    fn live_fs(&self) -> Arc<FsRuntime> {
        FsRuntime::from_context(&self.ctx, &self.fallback)
    }

    /// Whether the live filesystem confines (advertise escalation fields).
    pub fn advertises_escalation(&self) -> bool {
        self.live_fs().sandbox_mode().is_some()
    }

    /// Schema fields for `sandbox_permissions` and `justification`.
    pub fn schema_fields(&self) -> serde_json::Map<String, Value> {
        let enum_values: Vec<Value> = ESCALATION_TARGETS
            .iter()
            .map(|mode| Value::String(mode.as_str().to_string()))
            .collect();
        let mut fields = serde_json::Map::new();
        fields.insert(
            "sandbox_permissions".into(),
            json!({
                "type": "string",
                "enum": enum_values,
                "description": "The wider sandbox mode this file operation needs. Only valid as a one-shot retry of an operation the sandbox just denied; requires justification and user approval."
            }),
        );
        fields.insert(
            "justification".into(),
            json!({
                "type": "string",
                "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact file operation needs the wider access."
            }),
        );
        fields
    }

    /// Policy to stamp onto this mutation, resolved before anything executes.
    ///
    /// # Errors
    /// Pairing failure, unadvertised fields, a non-grant, or an approval-channel error.
    pub async fn resolve_policy(
        &self,
        tool_name: &str,
        args: &Value,
        agent_id: Option<&str>,
    ) -> Result<Option<SandboxExecutionPolicy>, String> {
        let sandbox_permissions = args.get("sandbox_permissions").and_then(Value::as_str);
        let justification = args.get("justification").and_then(Value::as_str);
        validate_escalation_args(sandbox_permissions, justification)?;
        let standing = dsh_sandbox_policy::resolve_from_context(&self.ctx, agent_id);
        if sandbox_permissions.is_none() || justification.is_none() {
            return Ok(standing);
        }
        if !self.advertises_escalation() {
            return Err(
                "sandbox_permissions is not available in this composition (no sandboxing filesystem to escalate)"
                    .into(),
            );
        }
        let policy = standing.ok_or_else(|| {
            "tool-fs: the mounted filesystem confines but ctx.sandboxPolicy is missing".to_string()
        })?;
        let requested = sandbox_permissions.expect("paired").to_string();
        let justification = justification.expect("paired").to_string();
        let reason = escalation_audit_reason(&requested, &justification);
        let approver = self.ctx.get::<ApprovalService>();
        let agent = agent_id.and_then(|id| {
            self.ctx
                .get::<AgentRegistry>()
                .and_then(|registry| registry.get(&session_id(id)))
        });
        let has_approver = approver.is_some();
        let has_agent = agent.is_some();
        let ctx = self.ctx.clone();
        let tool_name = tool_name.to_string();
        let approved = approve_escalation(
            EscalationRequest {
                requested_mode: requested,
                justification,
                effective_mode: policy.mode,
                subject: "operation".into(),
            },
            EscalationIngredients {
                has_approver,
                has_agent,
            },
            async move {
                let Some(approver) = approver else {
                    return Ok("unavailable".into());
                };
                let Some(agent) = agent else {
                    return Ok("unavailable".into());
                };
                approver
                    .request(
                        &ctx,
                        agent.session().as_ref(),
                        ApprovalRequest {
                            tool_name,
                            call_id: None,
                            reason: Some(reason),
                        },
                    )
                    .map(|outcome| outcome.as_str().to_string())
            },
        )
        .await?;
        Ok(Some(SandboxExecutionPolicy {
            mode: approved,
            workspace_root: policy.workspace_root,
        }))
    }

    /// Map a sandbox denial to the shared marker plus the operation hint.
    pub fn map_error(&self, error: FsError, policy: Option<&SandboxExecutionPolicy>) -> FsError {
        if error.code() != Some(FsErrorCode::SandboxDenied) {
            return error.remediate();
        }
        let Some(policy) = policy else {
            return error.remediate();
        };
        let text = format!(
            "{}\n{}",
            sandbox_denial_marker(policy.mode),
            escalation_hint_marker("operation")
        );
        FsError::sandbox_denied(text)
    }
}

/// Convert a resolved execution policy into the filesystem write fence input.
pub fn write_policy_from(policy: &SandboxExecutionPolicy) -> FsWritePolicy {
    FsWritePolicy {
        mode: policy.mode.as_str().to_string(),
        workspace_root: policy.workspace_root.clone(),
    }
}
