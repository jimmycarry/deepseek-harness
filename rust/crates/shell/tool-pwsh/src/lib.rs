//! Model-facing PowerShell Consumer of the `ctx.shell` capability seam.
//! Intended for Windows compositions; the tool contract is PowerShell-dialect
//! (`C:\...` paths and `$env:NAME`). Behavior mirrors `dsh-tool-bash`.

use dsh_cordis::Context;
use dsh_jobs::JobRegistry;
use dsh_shell::ShellRuntime;
use dsh_shell_env::ShellEnvRegistry;
use dsh_tool_bash::{require_confining_policy_named, BashTool, ShellToolDialect};
use dsh_tools::ToolRuntime;
use serde_json::Value;
use std::sync::Arc;

/// Plugin role name.
pub fn name() -> &'static str {
    "dsh-tool-pwsh"
}

/// Deployment-varying background switch. Same fields as the bash tool.
pub type Config = dsh_tool_bash::Config;

fn pwsh_description(background_enabled: bool, advertises_escalation: bool) -> String {
    let background = if background_enabled {
        "Set `run_in_background: true` for long-running commands: the call returns a job id immediately; read its output with `job_output` and stop it with `job_kill`."
    } else {
        "Background execution is not available; long-running commands must finish within the timeout."
    };
    let mut base = format!(
        "Execute a PowerShell command (`pwsh -Command`) and return its stdout/stderr. \
Each call runs in a fresh pwsh process: no state (cwd, variables, functions) persists between calls — \
pass `workdir` instead of using `cd`. Paths use native Windows form (`C:\\...`); read environment \
variables with `$env:NAME`. Non-zero exits are reported as `[exit code: N]`. \
Current harness environment facts are exposed through managed `$env:DSH_*` variables; inspect them when needed. \
Commands may run under a file sandbox; a blocked file operation is reported as `[sandbox: file access denied under <mode> mode]` — a policy denial, not a bug in the command; do not retry another way. \
Long output is truncated to its tail; the full output is saved to a file whose path is reported when available. \
On Windows a force-killed command settles as `[exit code: 1]` without a signal marker — treat it as an interruption, not a command failure. \
{background}"
    );
    if advertises_escalation {
        base.push_str(
            " Under the Windows sandbox, read-only pwsh runs in PowerShell ConstrainedLanguage mode, while \
workspace-write stays in FullLanguage unless host policy says otherwise. In read-only, prefer cmdlets and core types (`[string]`, `[datetime]`, `[regex]`, `[guid]`); \
.NET static calls (`[System.IO.*]::`, `[math]::`), `Add-Type`, COM objects, and reflection fail \
with \"only core types\" errors. `-f` formatting, property access, and core cmdlets work. \
In both confined modes, programs cannot open named pipes, so a command that captures another \
program's output through piped stdio (Node.js `child_process.spawn`/`exec` with the default \
`stdio: 'pipe'`) fails with EPERM, while `stdio: 'inherit'` and `stdio: 'ignore'` spawns \
work and PowerShell's own pipelines are unaffected. That EPERM is the documented boundary: \
do not retry the command another way — escalate the exact command once or restructure it to \
avoid capturing output. \
Attempting a command the sandbox may deny is safe and expected: run it and read the \
marker rather than assuming the denial. When a command is denied and a wider mode would let it \
succeed, escalate immediately in the same turn — the one sanctioned exception to a denial: retry \
the exact same command once with `sandbox_permissions` (the narrowest wider mode that suffices) \
plus a one-sentence `justification`. Do not detour through chat to ask permission first — the \
approval prompt raised by that retry is how the user consents. If the session states approval \
prompts are disabled, there is no exception: a denial is final — do not set `sandbox_permissions`. \
Never escalate speculatively: ground the request in a real denial — normally the one this command \
just hit; escalating up front is fine only when this session already denied the same access. \
A rejected escalation is final for that command — stop and explain, never work around \
it — but it does not forbid attempting or escalating other commands later.",
        );
    }
    base
}

/// PowerShell dialect. The missing-policy sentence keeps the TypeScript wording.
pub const PWSH_DIALECT: ShellToolDialect = ShellToolDialect {
    name: "pwsh",
    job_kind: "pwsh",
    missing_policy: "tool-pwsh: the mounted bash executor confines but ctx.sandboxPolicy is missing",
    command_param: "The PowerShell command to execute.",
    description_examples: "Examples: \"ls\" → \"List files in current directory\"; \"git status\" → \"Show working tree status\"; \"Get-Process\" → \"List running processes\".",
    describe: pwsh_description,
};

/// Model-facing `pwsh` tool.
pub type PwshTool = BashTool;

/// Bind a pwsh-dialect tool to `ctx.shell`.
pub fn new_tool(
    shell: Arc<ShellRuntime>,
    jobs: Option<Arc<JobRegistry>>,
    enable_run_in_background: bool,
) -> PwshTool {
    BashTool::with_dialect(shell, jobs, enable_run_in_background, PWSH_DIALECT)
}

/// Fail loud when a confining executor is mounted without `ctx.sandboxPolicy`.
///
/// # Errors
/// The TypeScript load-failure sentence (it still says "bash executor").
pub fn require_confining_policy(ctx: &Context, shell: &ShellRuntime) -> Result<(), String> {
    require_confining_policy_named(ctx, shell, PWSH_DIALECT.missing_policy)
}

/// Provide the `pwsh` tool on `ctx.tools`.
pub fn install(ctx: &Context, config: Option<&Value>) -> dsh_cordis::Result<()> {
    let tools = ctx.service::<ToolRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-pwsh requires ctx.tools".into())
    })?;
    let shell = ctx.service::<ShellRuntime>().map_err(|_| {
        dsh_cordis::CordisError::Validation("tool-pwsh requires ctx.shell".into())
    })?;
    require_confining_policy(ctx, shell.as_ref()).map_err(dsh_cordis::CordisError::Validation)?;
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let jobs = ctx.get::<JobRegistry>();
    let mut tool = new_tool(shell, jobs, resolved.enable_run_in_background).with_context(ctx.clone());
    if let Some(shell_env) = ctx.get::<ShellEnvRegistry>() {
        tool = tool.with_shell_env(shell_env);
    }
    tools.insert(Arc::new(tool));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_sandbox::SandboxMode;
    use dsh_shell::{ShellError, ShellExecutor, ShellRunResult, ShellSpec};
    use dsh_tools::{Tool, ToolCallKind, ToolCallView};
    use serde_json::json;

    struct RecordingPwsh;

    #[async_trait::async_trait]
    impl ShellExecutor for RecordingPwsh {
        async fn run(&self, _spec: ShellSpec) -> Result<ShellRunResult, ShellError> {
            Ok(ShellRunResult::from_stdout(""))
        }
    }

    #[test]
    fn advertises_pwsh_name_and_schema() {
        let tool = new_tool(
            Arc::new(ShellRuntime::new(Arc::new(RecordingPwsh))),
            None,
            true,
        );
        assert_eq!(tool.name(), "pwsh");
        let parameters = tool.parameters();
        assert_eq!(
            parameters["properties"]["command"]["description"],
            "The PowerShell command to execute."
        );
        assert!(tool.description().contains("pwsh -Command"));
        assert!(tool.description().contains("$env:NAME"));
        assert_eq!(
            tool.output_schema().unwrap()["oneOf"][0]["properties"]["kind"]["const"],
            "background"
        );
        match tool
            .present_call(&json!({
                "command": "Get-Process",
                "description": "List running processes"
            }))
            .unwrap()
        {
            ToolCallView::Terminal(view) => assert_eq!(view.title, "Get-Process"),
            other => panic!("expected terminal, got {other:?}"),
        }
        match tool
            .present_call(&json!({
                "command": "sleep 1",
                "description": "wait",
                "run_in_background": true
            }))
            .unwrap()
        {
            ToolCallView::Generic(view) => assert_eq!(view.kind, Some(ToolCallKind::Execute)),
            other => panic!("expected generic, got {other:?}"),
        }
        assert!(tool
            .present_call(&json!({ "command": "Get-Process" }))
            .is_none());
    }

    #[test]
    fn confining_without_policy_fails_loud() {
        let ctx = Context::new();
        let shell = ShellRuntime::new(Arc::new(RecordingPwsh)).with_sandbox_mode(SandboxMode::ReadOnly);
        let err = require_confining_policy(&ctx, &shell).unwrap_err();
        assert!(err.contains(
            "tool-pwsh: the mounted bash executor confines but ctx.sandboxPolicy is missing"
        ));
    }
}
