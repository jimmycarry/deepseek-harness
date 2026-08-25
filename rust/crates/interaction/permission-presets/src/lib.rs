//! User-facing permission presets over sandbox mode and approval policy.
//!
//! A switch records the selected preset, then writes changed knobs through
//! their canonical setters. Headless pins the composition default onto the
//! root session; this crate does not listen for `session/created`, so a
//! child session does not receive a second pin.

use async_trait::async_trait;
use dsh_agent::AgentRegistry;
use dsh_commands::{Command, CommandHandler, CommandInvocation, CommandRegistry, CommandResult};
use dsh_cordis::{Context, Result, Service};
use dsh_sandbox::SandboxMode;
use dsh_sandbox_policy::{effective_sandbox_mode, set_sandbox_mode};
use dsh_session::{Session, SessionEvent, SessionEventData};
use dsh_session_projection::{ProjectionUnit, SessionProjectionRegistry};
use dsh_shell::ShellRuntime;
use dsh_user_approval::{
    effective_approval_policy, set_approval_policy, ApprovalPolicy, ApprovalService,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "dsh-permission-presets"
}

/// Derived not-a-preset state. Never a table key or event payload.
pub const CUSTOM_PRESET: &str = "custom";

/// One preset's sandbox/approval bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetSpec {
    /// `sandbox/mode` value the preset writes.
    pub sandbox: SandboxMode,
    /// `approval/policy` value the preset writes.
    pub approval: ApprovalPolicy,
    /// Display label; the table key when omitted.
    pub name: Option<String>,
    /// One user-facing sentence; omitted when not configured.
    pub description: Option<String>,
}

/// `ctx.permissionPresets`.
pub struct PermissionPresetService {
    presets: Vec<(String, PresetSpec)>,
    default_preset: String,
    shell: Arc<ShellRuntime>,
    approval: Arc<ApprovalService>,
    lookup: Context,
}

impl Service for PermissionPresetService {
    const KEY: &'static str = "permissionPresets";
}

impl PermissionPresetService {
    /// Advertised preset names in table declaration order.
    pub fn names(&self) -> Vec<&str> {
        self.presets.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Composition default for a genuinely fresh session.
    pub fn default_preset(&self) -> &str {
        &self.default_preset
    }

    /// Resolve a table entry.
    ///
    /// # Errors
    /// `name` is not in the table.
    pub fn resolve(&self, name: &str) -> std::result::Result<&PresetSpec, String> {
        self.presets
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, spec)| spec)
            .ok_or_else(|| {
                format!(
                    "permission: unknown preset \"{name}\" (known: {})",
                    self.names().join(", ")
                )
            })
    }

    /// Effective preset for the folded knobs, or [`CUSTOM_PRESET`].
    pub fn current(&self, events: &[SessionEvent]) -> String {
        self.derive(&fold_knobs(events))
    }

    fn derive(&self, state: &KnobState) -> String {
        let standing = self.shell.sandbox_mode();
        let sandbox = state
            .sandbox
            .clone()
            .or(standing)
            .unwrap_or(SandboxMode::WorkspaceWrite);
        let approval = state
            .approval
            .unwrap_or_else(|| self.approval.default_policy());
        let matches = |spec: &PresetSpec| spec.sandbox == sandbox && spec.approval == approval;
        if let Some(selected) = &state.preset {
            if let Ok(spec) = self.resolve(selected) {
                if matches(spec) {
                    return selected.clone();
                }
            }
        }
        for (name, spec) in &self.presets {
            if matches(spec) {
                return name.clone();
            }
        }
        CUSTOM_PRESET.to_string()
    }

    /// Record a changed preset, then update each changed knob through its setter.
    ///
    /// # Errors
    /// Unknown `name`, or a refused session append.
    pub fn set(&self, session: &Session, name: &str) -> std::result::Result<(), String> {
        self.apply(session, name, |policy| set_approval_policy(session, policy))
    }

    /// Apply one preset with the caller-selected live or initialization policy writer.
    ///
    /// # Errors
    /// Unknown `name`, or a refused session append.
    pub fn apply(
        &self,
        session: &Session,
        name: &str,
        set_approval: impl FnOnce(ApprovalPolicy) -> std::result::Result<(), String>,
    ) -> std::result::Result<(), String> {
        let spec = self.resolve(name)?.clone();
        if self.current(&session.events()) != name {
            session
                .append(
                    SessionEventData::PermissionPreset {
                        preset: name.to_string(),
                    },
                    None,
                )
                .map_err(|error| error.to_string())?;
        }
        let events = session.events();
        let standing = self
            .shell
            .sandbox_mode()
            .unwrap_or_else(|| spec.sandbox.clone());
        if spec.sandbox != effective_sandbox_mode(&events).unwrap_or(standing) {
            set_sandbox_mode(session, spec.sandbox.clone())?;
        }
        if spec.approval
            != effective_approval_policy(&events).unwrap_or_else(|| self.approval.default_policy())
        {
            set_approval(spec.approval)?;
        }
        Ok(())
    }

    /// Fill missing permission facts on a freshly created root session.
    ///
    /// A genuinely empty log receives the composition default. Seeded or
    /// partially initialized sessions keep their knobs and only gain missing
    /// durable facts. Child sessions are not pinned from here.
    ///
    /// # Errors
    /// A refused session append.
    pub fn pin_initial(&self, session: &Session) -> std::result::Result<(), String> {
        let events = session.events();
        let selected = effective_permission_preset(&events);
        let sandbox = effective_sandbox_mode(&events);
        let approval = effective_approval_policy(&events);
        let seeded = events
            .iter()
            .any(|event| dsh_session::event_type_name(&event.data) == "session/end-seed");
        if selected.is_none() && sandbox.is_none() && approval.is_none() && !seeded {
            let name = self.default_preset.clone();
            let spec = self.resolve(&name)?.clone();
            session
                .append(
                    SessionEventData::PermissionPreset { preset: name },
                    None,
                )
                .map_err(|error| error.to_string())?;
            set_sandbox_mode(session, spec.sandbox)?;
            set_approval_policy(session, spec.approval)?;
            return Ok(());
        }
        let state = KnobState {
            preset: selected.clone(),
            sandbox: sandbox.clone(),
            approval,
        };
        let effective = self.derive(&state);
        if selected.is_none() && effective != CUSTOM_PRESET {
            session
                .append(
                    SessionEventData::PermissionPreset {
                        preset: effective,
                    },
                    None,
                )
                .map_err(|error| error.to_string())?;
        }
        if sandbox.is_none() {
            if let Some(mode) = self.shell.sandbox_mode() {
                set_sandbox_mode(session, mode)?;
            }
        }
        if approval.is_none() {
            set_approval_policy(session, self.approval.default_policy())?;
        }
        Ok(())
    }
}

/// Last `permission/preset` in log order.
pub fn effective_permission_preset(events: &[SessionEvent]) -> Option<String> {
    for event in events.iter().rev() {
        if let SessionEventData::PermissionPreset { preset } = &event.data {
            return Some(preset.clone());
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
struct KnobState {
    preset: Option<String>,
    sandbox: Option<SandboxMode>,
    approval: Option<ApprovalPolicy>,
}

fn fold_knobs(events: &[SessionEvent]) -> KnobState {
    let mut state = KnobState::default();
    for event in events {
        match &event.data {
            SessionEventData::PermissionPreset { preset } => {
                state.preset = Some(preset.clone());
            }
            SessionEventData::SandboxMode { mode } => {
                state.sandbox = SandboxMode::parse(mode);
            }
            SessionEventData::ApprovalPolicy { policy } => {
                state.approval = ApprovalPolicy::parse(policy);
            }
            _ => {}
        }
    }
    state
}

/// Parse the preset table and composition default.
///
/// # Errors
/// `custom` as a table key, an unknown sandbox/approval value, a missing
/// confining `ctx.shell.sandboxMode`, missing `ctx.approval`, or a default
/// that matches no table entry.
pub fn parse_config(
    ctx: &Context,
    config: Option<&Value>,
) -> std::result::Result<(Vec<(String, PresetSpec)>, String), String> {
    let shell = ctx
        .get::<ShellRuntime>()
        .ok_or_else(|| "permission: ctx.shell is required".to_string())?;
    if shell.sandbox_mode().is_none() {
        return Err(
            "permission: the mounted bash executor does not confine (no sandboxMode) — presets bundle a sandbox mode, so composing this plugin over an unconfined executor is a misconfiguration"
                .into(),
        );
    }
    let approval = ctx
        .get::<ApprovalService>()
        .ok_or_else(|| "permission: ctx.approval is required".to_string())?;
    let presets = match config.and_then(|value| value.get("presets")) {
        None => default_presets(),
        Some(value) => parse_presets(value)?,
    };
    if presets.iter().any(|(name, _)| name == CUSTOM_PRESET) {
        return Err(format!(
            "permission: \"{CUSTOM_PRESET}\" is reserved for the derived not-a-preset state and cannot name a table entry"
        ));
    }
    let inferred = infer_default(&presets, shell.as_ref(), approval.as_ref());
    let default_preset = match config.and_then(|value| value.get("defaultPreset")) {
        None => inferred,
        Some(value) => value
            .as_str()
            .ok_or_else(|| "permission: defaultPreset must be a string".to_string())?
            .to_string(),
    };
    if default_preset == CUSTOM_PRESET {
        return Err(
            "permission: composed sandbox and approval defaults match no preset; configure defaultPreset explicitly"
                .into(),
        );
    }
    if !presets.iter().any(|(name, _)| name == &default_preset) {
        return Err(format!(
            "permission: unknown preset \"{default_preset}\" (known: {})",
            presets
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok((presets, default_preset))
}

fn default_presets() -> Vec<(String, PresetSpec)> {
    vec![
        (
            "workspace-write".into(),
            PresetSpec {
                sandbox: SandboxMode::WorkspaceWrite,
                approval: ApprovalPolicy::Ask,
                name: Some("workspace-write".into()),
                description: Some(
                    "Write inside the workspace and permitted temporary directories; wider retries require approval."
                        .into(),
                ),
            },
        ),
        (
            "danger-full-access".into(),
            PresetSpec {
                sandbox: SandboxMode::DangerFullAccess,
                approval: ApprovalPolicy::Never,
                name: Some("danger-full-access".into()),
                description: Some("Full file access without approval prompts.".into()),
            },
        ),
    ]
}

fn parse_presets(value: &Value) -> std::result::Result<Vec<(String, PresetSpec)>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "permission: presets must be a mapping".to_string())?;
    let mut presets = Vec::with_capacity(object.len());
    for (name, spec) in object {
        let sandbox = spec
            .get("sandbox")
            .and_then(Value::as_str)
            .and_then(SandboxMode::parse)
            .ok_or_else(|| format!("permission: preset \"{name}\" needs a sandbox mode"))?;
        let approval = spec
            .get("approval")
            .and_then(Value::as_str)
            .and_then(ApprovalPolicy::parse)
            .ok_or_else(|| format!("permission: preset \"{name}\" needs an approval policy"))?;
        let label = spec
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let description = spec
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        presets.push((
            name.clone(),
            PresetSpec {
                sandbox,
                approval,
                name: label,
                description,
            },
        ));
    }
    Ok(presets)
}

fn infer_default(
    presets: &[(String, PresetSpec)],
    shell: &ShellRuntime,
    approval: &ApprovalService,
) -> String {
    let sandbox = shell
        .sandbox_mode()
        .unwrap_or(SandboxMode::WorkspaceWrite);
    let policy = approval.default_policy();
    presets
        .iter()
        .find(|(_, spec)| spec.sandbox == sandbox && spec.approval == policy)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| CUSTOM_PRESET.to_string())
}

/// Provide `ctx.permissionPresets`. Registers `/permission` and the
/// `permissions` projection when those services are already mounted.
///
/// # Errors
/// Invalid config, a missing confining shell, missing approval, or a duplicate service.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<Arc<PermissionPresetService>> {
    let (presets, default_preset) =
        parse_config(ctx, config).map_err(dsh_cordis::CordisError::Validation)?;
    let service = Arc::new(PermissionPresetService {
        presets,
        default_preset,
        shell: ctx.service::<ShellRuntime>()?,
        approval: ctx.service::<ApprovalService>()?,
        lookup: ctx.clone(),
    });
    ctx.provide(Arc::clone(&service))?;
    bind_command(ctx)?;
    bind_projection(ctx)?;
    Ok(service)
}

/// Register `/permission` when `ctx.commands` is mounted.
///
/// # Errors
/// Command registration on the owning fiber.
pub fn bind_command(ctx: &Context) -> Result<()> {
    let Some(commands) = ctx.get::<CommandRegistry>() else {
        return Ok(());
    };
    if commands.get("permission").is_some() {
        return Ok(());
    }
    let Some(service) = ctx.get::<PermissionPresetService>() else {
        return Ok(());
    };
    commands.register(
        ctx,
        Command {
            name: "permission".into(),
            description: "Switch the permission preset (sandbox mode + approval policy)".into(),
            model_visible: false,
            record_input: true,
            handler: Arc::new(PermissionCommand { service }),
        },
    )
}

fn bind_projection(ctx: &Context) -> Result<()> {
    let Some(registry) = ctx.get::<SessionProjectionRegistry>() else {
        return Ok(());
    };
    let Some(service) = ctx.get::<PermissionPresetService>() else {
        return Ok(());
    };
    registry
        .register(ProjectionUnit {
            key: "permissions".into(),
            state_version: 1,
            init: Arc::new(|| json!({ "preset": null, "sandbox": null, "approval": null })),
            apply: Arc::new(|state, event| apply_knob_json(state, event)),
            view: Arc::new(move |state| view_select(service.as_ref(), state)),
        })
        .map_err(dsh_cordis::CordisError::Validation)?;
    Ok(())
}

fn apply_knob_json(state: &Value, event: &SessionEvent) -> Value {
    let mut next = state.clone();
    match &event.data {
        SessionEventData::PermissionPreset { preset } => {
            next["preset"] = json!(preset);
        }
        SessionEventData::SandboxMode { mode } => {
            next["sandbox"] = json!(mode);
        }
        SessionEventData::ApprovalPolicy { policy } => {
            next["approval"] = json!(policy);
        }
        _ => return state.clone(),
    }
    next
}

fn view_select(service: &PermissionPresetService, state: &Value) -> Value {
    let knobs = KnobState {
        preset: state
            .get("preset")
            .and_then(Value::as_str)
            .map(str::to_string),
        sandbox: state
            .get("sandbox")
            .and_then(Value::as_str)
            .and_then(SandboxMode::parse),
        approval: state
            .get("approval")
            .and_then(Value::as_str)
            .and_then(ApprovalPolicy::parse),
    };
    let current = service.derive(&knobs);
    let mut options: Vec<Value> = service
        .names()
        .into_iter()
        .map(|name| {
            let spec = service.resolve(name).expect("table name");
            json!({
                "value": name,
                "name": spec.name.as_deref().unwrap_or(name),
                "description": spec.description,
            })
        })
        .collect();
    if current == CUSTOM_PRESET {
        options.push(json!({
            "value": CUSTOM_PRESET,
            "name": "Custom",
            "description": "Current sandbox and approval settings do not match a preset.",
        }));
    }
    json!({ "options": options, "currentValue": current })
}

struct PermissionCommand {
    service: Arc<PermissionPresetService>,
}

#[async_trait]
impl CommandHandler for PermissionCommand {
    async fn handle(&self, _args: &str) -> std::result::Result<String, String> {
        Err("permission command requires a calling session".into())
    }

    async fn handle_invocation(
        &self,
        invocation: CommandInvocation<'_>,
    ) -> std::result::Result<CommandResult, String> {
        let name = invocation.raw_input.trim();
        let available = self.service.names().join(", ");
        if name.is_empty() {
            return Ok(CommandResult::text(format!(
                "current preset {} (available: {available})",
                self.service.current(&invocation.session.events())
            )));
        }
        if !self.service.names().iter().any(|known| *known == name) {
            return Err(format!("unknown preset \"{name}\" (available: {available})"));
        }
        let agents = self.service.lookup.get::<AgentRegistry>();
        let live = agents.and_then(|agents| agents.get(invocation.session.id()));
        if let Some(agent) = live {
            self.service.apply(invocation.session, name, |policy| {
                self.service.approval.set_policy(agent.as_ref(), policy)
            })?;
        } else {
            self.service.set(invocation.session, name)?;
        }
        Ok(CommandResult::text(format!("preset {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dsh_sandbox::SandboxMode;
    use dsh_session::{session_id, SessionStore};
    use dsh_shell::{ShellError, ShellExecutor, ShellSpec};
    use dsh_user_approval::install as install_approval;

    struct NoopShell;

    #[async_trait]
    impl ShellExecutor for NoopShell {
        async fn run(&self, _spec: ShellSpec) -> std::result::Result<String, ShellError> {
            Ok(String::new())
        }
    }

    fn mount(mode: SandboxMode, approval: &str) -> (Context, Arc<PermissionPresetService>) {
        let ctx = Context::new();
        ctx.provide(Arc::new(
            ShellRuntime::new(Arc::new(NoopShell)).with_sandbox_mode(mode),
        ))
        .unwrap();
        install_approval(&ctx, Some(&json!({ "policy": approval }))).unwrap();
        ctx.provide(Arc::new(CommandRegistry::new())).unwrap();
        ctx.provide(Arc::new(SessionStore::new())).unwrap();
        let presets = json!({
            "presets": {
                "read-only": { "sandbox": "read-only", "approval": "ask" },
                "workspace-write": { "sandbox": "workspace-write", "approval": "ask" },
                "danger-full-access": { "sandbox": "danger-full-access", "approval": "never" }
            }
        });
        let service = install(&ctx, Some(&presets)).unwrap();
        (ctx, service)
    }

    #[test]
    fn names_preserve_declaration_order() {
        let (_ctx, service) = mount(SandboxMode::DangerFullAccess, "never");
        assert_eq!(
            service.names(),
            ["read-only", "workspace-write", "danger-full-access"]
        );
        assert_eq!(service.default_preset(), "danger-full-access");
    }

    #[test]
    fn custom_cannot_name_a_table_entry() {
        let ctx = Context::new();
        ctx.provide(Arc::new(
            ShellRuntime::new(Arc::new(NoopShell))
                .with_sandbox_mode(SandboxMode::WorkspaceWrite),
        ))
        .unwrap();
        install_approval(&ctx, None).unwrap();
        let error = parse_config(
            &ctx,
            Some(&json!({ "presets": { "custom": { "sandbox": "read-only", "approval": "ask" } } })),
        )
        .unwrap_err();
        assert!(error.contains("reserved"), "{error}");
    }

    #[test]
    fn unconfined_shell_fails_loud() {
        let ctx = Context::new();
        ctx.provide(Arc::new(ShellRuntime::new(Arc::new(NoopShell))))
            .unwrap();
        install_approval(&ctx, None).unwrap();
        let error = parse_config(&ctx, None).unwrap_err();
        assert!(error.contains("does not confine"), "{error}");
    }

    #[tokio::test]
    async fn permission_command_reports_current_and_rejects_unknown() {
        let (ctx, _service) = mount(SandboxMode::DangerFullAccess, "never");
        let session = ctx.service::<SessionStore>().unwrap().create(session_id("p"));
        ctx.service::<PermissionPresetService>()
            .unwrap()
            .pin_initial(session.as_ref())
            .unwrap();
        let commands = ctx.service::<CommandRegistry>().unwrap();
        let current = commands
            .execute(session.as_ref(), "/permission")
            .await
            .unwrap()
            .unwrap();
        assert!(current.success);
        assert_eq!(
            current.text,
            "current preset danger-full-access (available: read-only, workspace-write, danger-full-access)"
        );
        let unknown = commands
            .execute(session.as_ref(), "/permission nope")
            .await
            .unwrap()
            .unwrap();
        assert!(!unknown.success);
        assert_eq!(
            unknown.text,
            "unknown preset \"nope\" (available: read-only, workspace-write, danger-full-access)"
        );
    }

    #[tokio::test]
    async fn switching_writes_changed_knobs() {
        let (ctx, service) = mount(SandboxMode::DangerFullAccess, "never");
        let session = ctx.service::<SessionStore>().unwrap().create(session_id("s"));
        service.pin_initial(session.as_ref()).unwrap();
        let outcome = ctx
            .service::<CommandRegistry>()
            .unwrap()
            .execute(session.as_ref(), "/permission read-only")
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.text, "preset read-only");
        let events = session.events();
        let types: Vec<_> = events
            .iter()
            .map(|event| dsh_session::event_type_name(&event.data))
            .collect();
        assert!(types.contains(&"permission/preset"));
        assert_eq!(service.current(&session.events()), "read-only");
        assert_eq!(
            effective_sandbox_mode(&session.events()),
            Some(SandboxMode::ReadOnly)
        );
        assert_eq!(
            effective_approval_policy(&session.events()),
            Some(ApprovalPolicy::Ask)
        );
    }
}
