//! Shared sandbox-escalation vocabulary used by `dsh-tool-bash` and `dsh-tool-fs`.
//!
//! The strictly-wider ladder, argument pairing, denial/hint markers, and
//! [`approve_escalation`] live here so the two families cannot drift. This
//! crate does not depend on approval or agent: the tool closes over
//! `ctx.approval.request` and passes a lazy future that is polled only after
//! the widening and channel checks pass.

use super::SandboxMode;

/// The strictly-wider table: what a call whose effective mode is the key may
/// escalate TO. Checked at execution, never baked into a tool schema — the
/// schema enum is [`ESCALATION_TARGETS`], because schemas are registry-global
/// while the effective mode is per-call.
///
/// `None` for `danger-full-access` matches TypeScript `WIDER_MODES` omitting
/// that key (`undefined ?? []` at the check).
pub fn wider_modes(mode: &SandboxMode) -> Option<&'static [SandboxMode]> {
    match mode {
        SandboxMode::ReadOnly => Some(&[
            SandboxMode::WorkspaceWrite,
            SandboxMode::DangerFullAccess,
        ]),
        SandboxMode::WorkspaceWrite => Some(&[SandboxMode::DangerFullAccess]),
        SandboxMode::DangerFullAccess => None,
    }
}

/// Every mode a call could ever escalate TO (`read-only` is the floor).
/// Advertised whenever the mounted capability confines.
pub const ESCALATION_TARGETS: &[SandboxMode] = &[
    SandboxMode::WorkspaceWrite,
    SandboxMode::DangerFullAccess,
];

/// Validate the escalation argument pairing a tool schema cannot express:
/// `sandbox_permissions` and `justification` travel together, and the
/// justification must be a non-empty sentence.
///
/// # Errors
/// One field without the other, or a blank justification.
pub fn validate_escalation_args(
    sandbox_permissions: Option<&str>,
    justification: Option<&str>,
) -> Result<(), String> {
    if sandbox_permissions.is_some() && justification.is_none() {
        return Err("invalid escalation: sandbox_permissions requires a justification".into());
    }
    if justification.is_some() && sandbox_permissions.is_none() {
        return Err(
            "invalid escalation: justification is only valid together with sandbox_permissions"
                .into(),
        );
    }
    if let Some(justification) = justification {
        if justification.trim().is_empty() {
            return Err("invalid justification: expected a non-empty sentence".into());
        }
    }
    Ok(())
}

/// Model-facing denial marker shared by bash and filesystem mutations.
pub fn sandbox_denial_marker(mode: SandboxMode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.as_str())
}

/// Same-turn escalation hint that rides a denial when the composition
/// advertises the escalation fields.
///
/// `subject` is `command` for bash and `operation` for a filesystem mutation.
pub fn escalation_hint_marker(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with sandbox_permissions (the narrowest wider mode that suffices) + justification; the approval prompt asks the user]"
    )
}

/// One escalation request, as [`approve_escalation`] judges it.
pub struct EscalationRequest {
    /// Requested target mode (schema-pinned to [`ESCALATION_TARGETS`] when advertised).
    pub requested_mode: String,
    /// The model's one-sentence reason, shown verbatim inside the audit reason.
    pub justification: String,
    /// The call's effective mode the request must strictly widen.
    pub effective_mode: SandboxMode,
    /// Family noun in user-facing texts (`command` for bash, `operation` for fs).
    pub subject: String,
}

/// Approval-channel facts the tool holds. `ask` is polled only after these
/// checks pass, so a non-widening request never prompts a human.
pub struct EscalationIngredients {
    /// Whether `ctx.approval` is composed.
    pub has_approver: bool,
    /// Whether the call has a live agent to route the ask through.
    pub has_agent: bool,
}

/// Resolve a sandbox-escalation request BEFORE anything executes: strict
/// widening, then the approval channel, then outcome mapping. `ask` yields the
/// approval wire string (`allowed-once` / `rejected` / `cancelled` /
/// `unavailable`) and is polled only after the fail-closed checks pass.
///
/// # Errors
/// A non-widening request, a missing approval service, an agent-less call, a
/// rejection, a cancellation, an unanswerable ask, an unknown outcome, or the
/// `ask` future's own error (for example `approval.request()` outside a turn).
pub async fn approve_escalation(
    request: EscalationRequest,
    ingredients: EscalationIngredients,
    ask: impl std::future::Future<Output = Result<String, String>>,
) -> Result<SandboxMode, String> {
    let EscalationRequest {
        requested_mode: mode,
        effective_mode,
        justification: _justification,
        subject,
    } = request;
    if !wider_modes(&effective_mode)
        .unwrap_or(&[])
        .iter()
        .any(|wider| wider.as_str() == mode)
    {
        return Err(format!(
            "sandbox escalation to \"{mode}\" is not strictly wider than this call's current \"{}\" mode",
            effective_mode.as_str()
        ));
    }
    if !ingredients.has_approver {
        return Err(format!(
            "sandbox escalation to \"{mode}\" requires approval, but no approval service is composed"
        ));
    }
    if !ingredients.has_agent {
        return Err(format!(
            "sandbox escalation to \"{mode}\" requires approval, but the call has no agent to route it through"
        ));
    }
    let outcome = ask.await?;
    match outcome.as_str() {
        "allowed-once" => SandboxMode::parse(&mode).ok_or_else(|| {
            format!(
                "sandbox escalation to \"{mode}\" is not strictly wider than this call's current \"{}\" mode",
                effective_mode.as_str()
            )
        }),
        "rejected" => Err(format!(
            "the user rejected escalating this {subject} to \"{mode}\""
        )),
        "cancelled" => Err(format!("approval for escalating to \"{mode}\" was cancelled")),
        "unavailable" => Err(format!(
            "sandbox escalation to \"{mode}\" requires approval, but no approval channel is available"
        )),
        other => Err(format!(
            "unreachable variant in EscalationOutcome: \"{other}\""
        )),
    }
}

/// Audit reason stored on `approval/asked`.
pub fn escalation_audit_reason(mode: &str, justification: &str) -> String {
    format!("escalate sandbox to {mode}: {justification}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(over: impl FnOnce(&mut EscalationRequest)) -> EscalationRequest {
        let mut request = EscalationRequest {
            requested_mode: "workspace-write".into(),
            justification: "the user asked to write in the workspace".into(),
            effective_mode: SandboxMode::ReadOnly,
            subject: "command".into(),
        };
        over(&mut request);
        request
    }

    fn ingredients(has_approver: bool, has_agent: bool) -> EscalationIngredients {
        EscalationIngredients {
            has_approver,
            has_agent,
        }
    }

    #[test]
    fn the_strictly_wider_ladder() {
        assert_eq!(
            wider_modes(&SandboxMode::ReadOnly),
            Some(
                &[
                    SandboxMode::WorkspaceWrite,
                    SandboxMode::DangerFullAccess
                ][..]
            )
        );
        assert_eq!(
            wider_modes(&SandboxMode::WorkspaceWrite),
            Some(&[SandboxMode::DangerFullAccess][..])
        );
        assert_eq!(wider_modes(&SandboxMode::DangerFullAccess), None);
        assert_eq!(
            ESCALATION_TARGETS,
            &[
                SandboxMode::WorkspaceWrite,
                SandboxMode::DangerFullAccess
            ]
        );
    }

    #[test]
    fn validate_escalation_args_pairing() {
        assert!(validate_escalation_args(None, None).is_ok());
        assert!(validate_escalation_args(
            Some("workspace-write"),
            Some("because the workspace needs it")
        )
        .is_ok());
        assert!(validate_escalation_args(Some("workspace-write"), None)
            .unwrap_err()
            .contains("requires a justification"));
        assert!(validate_escalation_args(None, Some("orphan reason"))
            .unwrap_err()
            .contains("only valid together with sandbox_permissions"));
        assert!(validate_escalation_args(Some("workspace-write"), Some("   "))
            .unwrap_err()
            .contains("non-empty sentence"));
    }

    #[test]
    fn model_facing_markers() {
        assert_eq!(
            sandbox_denial_marker(SandboxMode::ReadOnly),
            "[sandbox: file access denied under read-only mode]"
        );
        assert_eq!(
            sandbox_denial_marker(SandboxMode::WorkspaceWrite),
            "[sandbox: file access denied under workspace-write mode]"
        );
        assert!(escalation_hint_marker("command")
            .contains("retry this exact command once with sandbox_permissions"));
        assert!(escalation_hint_marker("operation")
            .contains("retry this exact operation once with sandbox_permissions"));
    }

    #[tokio::test]
    async fn grants_and_asks_with_the_audit_reason() {
        let request = req(|_| {});
        let reason = escalation_audit_reason(&request.requested_mode, &request.justification);
        assert_eq!(
            reason,
            "escalate sandbox to workspace-write: the user asked to write in the workspace"
        );
        let granted = approve_escalation(
            request,
            ingredients(true, true),
            async { Ok("allowed-once".into()) },
        )
        .await
        .unwrap();
        assert_eq!(granted, SandboxMode::WorkspaceWrite);
    }

    #[tokio::test]
    async fn non_widening_never_asks() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&asked);
        let err = approve_escalation(
            req(|request| request.requested_mode = "read-only".into()),
            ingredients(true, true),
            async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok("allowed-once".into())
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("not strictly wider than this call's current \"read-only\" mode"));
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));

        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&asked);
        let err = approve_escalation(
            req(|request| {
                request.requested_mode = "workspace-write".into();
                request.effective_mode = SandboxMode::DangerFullAccess;
            }),
            ingredients(true, true),
            async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok("allowed-once".into())
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("not strictly wider"));
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn missing_approver_and_agent_fail_closed() {
        let err = approve_escalation(
            req(|_| {}),
            ingredients(false, true),
            async { Ok("allowed-once".into()) },
        )
        .await
        .unwrap_err();
        assert!(err.contains("no approval service is composed"));
        let err = approve_escalation(
            req(|_| {}),
            ingredients(true, false),
            async { Ok("allowed-once".into()) },
        )
        .await
        .unwrap_err();
        assert!(err.contains("no agent to route it through"));
    }

    #[tokio::test]
    async fn maps_each_non_grant_outcome() {
        let err = approve_escalation(
            req(|request| request.subject = "operation".into()),
            ingredients(true, true),
            async { Ok("rejected".into()) },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "the user rejected escalating this operation to \"workspace-write\""
        );
        let err = approve_escalation(
            req(|_| {}),
            ingredients(true, true),
            async { Ok("cancelled".into()) },
        )
        .await
        .unwrap_err();
        assert_eq!(err, "approval for escalating to \"workspace-write\" was cancelled");
        let err = approve_escalation(
            req(|_| {}),
            ingredients(true, true),
            async { Ok("unavailable".into()) },
        )
        .await
        .unwrap_err();
        assert!(err.contains("no approval channel is available"));
    }

    #[tokio::test]
    async fn unknown_outcome_trips_exhaustiveness() {
        let err = approve_escalation(
            req(|_| {}),
            ingredients(true, true),
            async { Ok("bogus".into()) },
        )
        .await
        .unwrap_err();
        assert!(err.contains("unreachable variant in EscalationOutcome"));
        assert!(err.contains("bogus"));
    }
}
