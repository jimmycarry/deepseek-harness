//! Sandbox policy home (`ctx.sandboxPolicy`).
//!
//! Owns the deployment default mode, the fallback workspace root, and
//! per-call resolve: an explicit mode outranks the last `sandbox/mode` event,
//! which outranks the configured default. Enforcing bash and filesystem
//! backends read this resolved policy; the runtime-context snapshot narrates
//! it without listing capabilities.

use dsh_agent::AgentRegistry;
use dsh_cordis::{Context, Result, Service};
use dsh_sandbox::{SandboxExecutionPolicy, SandboxMode};
use dsh_session::{session_id, Session, SessionEvent, SessionEventData};
use dsh_system_prompt::{PromptContext, PromptContextText, SystemPrompt};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Plugin role name matching the TypeScript `export const name`.
pub fn name() -> &'static str {
    "dsh-sandbox-policy"
}

/// `ctx.sandboxPolicy`.
pub struct SandboxPolicyService {
    default_mode: SandboxMode,
    workspace_root: String,
}

impl Service for SandboxPolicyService {
    const KEY: &'static str = "sandboxPolicy";
}

/// Inputs that select the sandbox policy for one capability call.
#[derive(Clone, Default)]
pub struct SandboxPolicyRequest<'a> {
    /// Calling session; its cwd becomes the workspace-write boundary.
    pub session: Option<&'a Session>,
    /// Explicit approved mode, which outranks the session fold.
    pub mode: Option<SandboxMode>,
}

impl SandboxPolicyService {
    /// Deployment default mode beneath a session override.
    pub fn default_mode(&self) -> SandboxMode {
        self.default_mode.clone()
    }

    /// Absolute fallback workspace root for calls without a session cwd.
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    /// Resolve the complete policy for one capability call.
    pub fn resolve(&self, request: SandboxPolicyRequest<'_>) -> SandboxExecutionPolicy {
        let session = request.session;
        let mode = request
            .mode
            .or_else(|| session.and_then(|session| self.override_of(session)))
            .unwrap_or_else(|| self.default_mode.clone());
        let root = session
            .and_then(|session| session.header().cwd.clone())
            .unwrap_or_else(|| self.workspace_root.clone());
        SandboxExecutionPolicy {
            mode,
            workspace_root: resolve_workspace_root(&root),
        }
    }

    /// Last `sandbox/mode` in the session log, without the deployment default.
    pub fn override_of(&self, session: &Session) -> Option<SandboxMode> {
        effective_sandbox_mode(&session.events())
    }
}

/// Last `sandbox/mode` event in log order, or `None` when the session never switched.
pub fn effective_sandbox_mode(events: &[SessionEvent]) -> Option<SandboxMode> {
    for event in events.iter().rev() {
        if let SessionEventData::SandboxMode { mode } = &event.data {
            return SandboxMode::parse(mode);
        }
    }
    None
}

/// Append one `sandbox/mode` override. The switch is the event.
///
/// # Errors
/// A refused session append.
pub fn set_sandbox_mode(session: &Session, mode: SandboxMode) -> std::result::Result<(), String> {
    session
        .append(
            SessionEventData::SandboxMode {
                mode: mode.as_str().to_string(),
            },
            None,
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Model-facing policy sentence. Matches TypeScript `renderPolicyContext` exactly.
pub fn render_policy_context(policy: &SandboxExecutionPolicy) -> String {
    match policy.mode {
        SandboxMode::ReadOnly => {
            "Current DSH file policy: read-only. Any available operation enforced by the DSH file sandbox cannot modify files in the standing mode. Do not refuse a required modification from this policy alone: try an available tool normally and follow any denial and escalation guidance it returns."
                .into()
        }
        SandboxMode::WorkspaceWrite => {
            let root = serde_json::to_string(&policy.workspace_root)
                .unwrap_or_else(|_| format!("\"{}\"", policy.workspace_root));
            format!(
                "Current DSH file policy: workspace-write. Any available operation enforced by the DSH file sandbox may modify files under the session workspace: {root}. Some platform temporary areas may also be writable."
            )
        }
        SandboxMode::DangerFullAccess => {
            "Current DSH file policy: danger-full-access. The DSH file sandbox does not restrict file modifications by available operations."
                .into()
        }
    }
}

/// Resolve through `ctx.sandboxPolicy` for the live agent identified by `agent_id`.
///
/// Returns [`None`] when the policy service is not mounted. A missing agent
/// still resolves the deployment default (agentless call).
pub fn resolve_from_context(
    ctx: &Context,
    agent_id: Option<&str>,
) -> Option<SandboxExecutionPolicy> {
    let policy = ctx.get::<SandboxPolicyService>()?;
    let session = agent_id.and_then(|id| {
        ctx.get::<AgentRegistry>()
            .and_then(|agents| agents.get(&session_id(id)))
            .map(|agent| agent.session())
    });
    Some(policy.resolve(SandboxPolicyRequest {
        session: session.as_deref(),
        mode: None,
    }))
}

/// Plugin config: deployment default. Omitted `mode` is `read-only`.
#[derive(Debug, Clone)]
pub struct Config {
    /// File-sandbox mode a session starts from.
    pub mode: SandboxMode,
    /// Absolute fallback workspace root.
    pub workspace_root: String,
}

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Unknown `mode`, or a non-string `workspaceRoot`.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let mode = match config.and_then(|value| value.get("mode")) {
            None => SandboxMode::ReadOnly,
            Some(value) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| "sandbox-policy: mode must be a string".to_string())?;
                SandboxMode::parse(text).ok_or_else(|| {
                    format!("sandbox-policy: unknown sandbox mode `{text}`")
                })?
            }
        };
        let workspace_root = match config.and_then(|value| value.get("workspaceRoot")) {
            None => std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into()),
            Some(value) => value
                .as_str()
                .ok_or_else(|| "sandbox-policy: workspaceRoot must be a string".to_string())?
                .to_string(),
        };
        if workspace_root.is_empty() {
            return Err("sandbox-policy: workspaceRoot must be a non-empty path".into());
        }
        Ok(Self {
            mode,
            workspace_root: resolve_workspace_root(&workspace_root),
        })
    }
}

fn resolve_workspace_root(path: &str) -> String {
    let candidate = Path::new(path);
    let absolute = if candidate.is_absolute() {
        PathBuf::from(candidate)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(candidate)
    };
    match absolute.canonicalize() {
        Ok(canonical) => canonical.to_string_lossy().into_owned(),
        Err(_) => absolute.to_string_lossy().into_owned(),
    }
}

/// Provide `ctx.sandboxPolicy` and, when `ctx.systemPrompt` is already mounted,
/// register the dynamic `sandbox:policy` runtime-context contribution.
///
/// # Errors
/// Invalid config, or a duplicate service registration.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<Arc<SandboxPolicyService>> {
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let service = Arc::new(SandboxPolicyService {
        default_mode: resolved.mode,
        workspace_root: resolved.workspace_root,
    });
    ctx.provide(Arc::clone(&service))?;
    bind_prompt(ctx)?;
    Ok(service)
}

/// Register `sandbox:policy` (order 110) when both services are present.
/// Same-name registration replaces, so a later `system-prompt` mount can call this.
///
/// # Errors
/// Prompt registration does not fail; this returns `Ok` when either service is absent.
pub fn bind_prompt(ctx: &Context) -> Result<()> {
    let Some(prompt) = ctx.get::<SystemPrompt>() else {
        return Ok(());
    };
    let Some(service) = ctx.get::<SandboxPolicyService>() else {
        return Ok(());
    };
    prompt.register_context(PromptContext {
        name: "sandbox:policy".into(),
        order: 110,
        text: PromptContextText::Dynamic(Arc::new(move |session| match session {
            None => String::new(),
            Some(session) => render_policy_context(&service.resolve(SandboxPolicyRequest {
                session: Some(session),
                mode: None,
            })),
        })),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_session::{session_id, SessionStore};

    #[test]
    fn omitted_mode_is_read_only() {
        let config = Config::resolve(None).unwrap();
        assert_eq!(config.mode, SandboxMode::ReadOnly);
    }

    #[test]
    fn unknown_mode_fails_loud() {
        let error = Config::resolve(Some(&serde_json::json!({ "mode": "nope" }))).unwrap_err();
        assert!(error.contains("unknown sandbox mode"), "{error}");
    }

    #[test]
    fn session_override_outranks_default() {
        let ctx = Context::new();
        install(
            &ctx,
            Some(&serde_json::json!({
                "mode": "workspace-write",
                "workspaceRoot": "/tmp"
            })),
        )
        .unwrap();
        let store = SessionStore::new();
        let session = store.create(session_id("s"));
        let service = ctx.service::<SandboxPolicyService>().unwrap();
        assert_eq!(
            service
                .resolve(SandboxPolicyRequest {
                    session: Some(session.as_ref()),
                    mode: None,
                })
                .mode,
            SandboxMode::WorkspaceWrite
        );
        set_sandbox_mode(session.as_ref(), SandboxMode::ReadOnly).unwrap();
        assert_eq!(
            service
                .resolve(SandboxPolicyRequest {
                    session: Some(session.as_ref()),
                    mode: None,
                })
                .mode,
            SandboxMode::ReadOnly
        );
    }

    #[test]
    fn agentless_context_is_empty() {
        let ctx = Context::new();
        ctx.provide(Arc::new(SystemPrompt::new())).unwrap();
        install(
            &ctx,
            Some(&serde_json::json!({
                "mode": "danger-full-access",
                "workspaceRoot": "/tmp"
            })),
        )
        .unwrap();
        let prompt = ctx.service::<SystemPrompt>().unwrap();
        assert!(prompt.context_sections(None).is_empty());
        let session = SessionStore::new().create(session_id("ctx"));
        let sections = prompt.context_sections(Some(session.as_ref()));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "sandbox:policy");
        assert!(sections[0].text.contains("danger-full-access"));
    }
}
