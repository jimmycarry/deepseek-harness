#!/usr/bin/env python3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def write(path: str, content: str):
    dest = ROOT / path
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(content, encoding="utf-8")
    print(path)


def lib_crate(rel, name, desc, deps, body):
    dep_lines = "\n".join(f"{d}.workspace = true" for d in deps)
    write(
        f"{rel}/Cargo.toml",
        f"""[package]
name = "{name}"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true
description = "{desc}"

[dependencies]
{dep_lines}
""",
    )
    write(f"{rel}/src/lib.rs", body)


def service(rel, name, desc, key, extra=""):
    lib_crate(
        rel,
        name,
        desc,
        ["dsh-cordis"],
        f'''//! {desc}
use dsh_cordis::Service;

/// Runtime placeholder for `{key}`.
#[derive(Default)]
pub struct Runtime;

impl Runtime {{
    /// Create the service.
    pub fn new() -> Self {{ Self }}
}}

impl Service for Runtime {{
    const KEY: &'static str = "{key}";
}}
{extra}
#[cfg(test)]
mod tests {{
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn provide_and_dispose() {{
        let ctx = Context::new();
        ctx.provide(Arc::new(Runtime::new())).unwrap();
        assert!(ctx.has_service("{key}"));
        ctx.dispose();
        assert!(!ctx.has_service("{key}"));
    }}
}}
''',
    )


# --- already custom-written if missing ---
MISSING_SERVICES = [
    ("crates/subprocess/subprocess", "dsh-subprocess", "Subprocess seam (ctx.subprocess).", "subprocess"),
    ("crates/sandbox/sandbox", "dsh-sandbox", "Process-confinement seam (ctx.sandbox).", "sandbox"),
    ("crates/shell/shell", "dsh-shell", "Shell executor seam (ctx.shell).", "shell"),
    ("crates/fs/fs", "dsh-fs", "Filesystem seam (ctx.fs).", "fs"),
    ("crates/terminal/terminal", "dsh-terminal", "Persistent PTY seam (ctx.terminals).", "terminals"),
    ("crates/lsp/lsp", "dsh-lsp", "Language-server seam (ctx.lsp).", "lsp"),
    ("crates/llm/token-meter", "dsh-token-meter", "Token measurement (ctx.tokenMeter).", "tokenMeter"),
    ("crates/compaction/compaction", "dsh-compaction", "Compaction engine (ctx.compaction).", "compaction"),
    ("crates/compaction/tool-result-pruner", "dsh-tool-result-pruner", "Tool-result pruner (ctx.toolResultPruner).", "toolResultPruner"),
    ("crates/credentials/credentials", "dsh-credentials", "Credential seam (ctx.credentials).", "credentials"),
    ("crates/credentials/authorization", "dsh-authorization", "Authorization seam (ctx.authorization).", "authorization"),
    ("crates/skill/skill", "dsh-skill", "Skill registry (ctx.skills).", "skills"),
    ("crates/web/web", "dsh-web", "Web seam (ctx.web).", "web"),
    ("crates/jobs/jobs", "dsh-jobs", "Jobs seam (ctx.jobs).", "jobs"),
    ("crates/plan/plan-mode", "dsh-plan-mode", "Plan mode (ctx.planMode).", "planMode"),
    ("crates/goal/goal", "dsh-goal", "Goals (ctx.goals).", "goals"),
    ("crates/subagent/subagent", "dsh-subagent", "Subagent registry (ctx.subagents).", "subagents"),
    ("crates/workflow/workflow", "dsh-workflow", "Workflow engine (ctx.workflowEngine).", "workflowEngine"),
    ("crates/session/session-persistence", "dsh-session-persistence", "Persistence seam (ctx.sessionPersistence).", "sessionPersistence"),
    ("crates/session/session-projection", "dsh-session-projection", "Projection seam (ctx.sessionProjections).", "sessionProjections"),
    ("crates/interaction/commands", "dsh-commands", "Human commands (ctx.commands).", "commands"),
    ("crates/host/apiproxy", "dsh-apiproxy", "API proxy (ctx.apiProxy).", "apiProxy"),
]

for item in MISSING_SERVICES:
    if not (ROOT / item[0] / "src" / "lib.rs").exists():
        service(*item)

# Providers / consumers that wrap another service
WRAPPERS = [
    ("crates/subprocess/subprocess-local", "dsh-subprocess-local", "Local subprocess provider.", ["dsh-cordis", "dsh-subprocess", "tokio"]),
    ("crates/sandbox/sandbox-local", "dsh-sandbox-local", "Local sandbox provider.", ["dsh-cordis", "dsh-sandbox"]),
    ("crates/shell/bash-local", "dsh-bash-local", "Local bash provider.", ["dsh-cordis", "dsh-shell", "dsh-subprocess"]),
    ("crates/shell/tool-bash", "dsh-tool-bash", "Model-facing bash tool.", ["async-trait", "dsh-cordis", "dsh-shell", "dsh-tools", "serde_json"]),
    ("crates/fs/fs-local", "dsh-fs-local", "Local filesystem provider.", ["dsh-cordis", "dsh-fs", "tokio"]),
    ("crates/fs/tool-fs", "dsh-tool-fs", "Model-facing filesystem tools.", ["async-trait", "dsh-cordis", "dsh-fs", "dsh-tools", "serde_json"]),
    ("crates/terminal/terminal-local", "dsh-terminal-local", "Local PTY provider.", ["dsh-cordis", "dsh-terminal"]),
    ("crates/terminal/tool-terminal", "dsh-tool-terminal", "Model-facing terminal tool.", ["async-trait", "dsh-cordis", "dsh-terminal", "dsh-tools", "serde_json"]),
    ("crates/lsp/lsp-stdio", "dsh-lsp-stdio", "Stdio LSP provider.", ["dsh-cordis", "dsh-lsp"]),
    ("crates/lsp/tool-lsp", "dsh-tool-lsp", "Model-facing lsp tool.", ["async-trait", "dsh-cordis", "dsh-lsp", "dsh-tools", "serde_json"]),
    ("crates/compaction/compaction-basic", "dsh-compaction-basic", "Basic compaction provider.", ["dsh-cordis", "dsh-compaction", "dsh-session", "dsh-llm", "dsh-token-meter", "serde_json"]),
    ("crates/compaction/command-compact", "dsh-command-compact", "/compact command consumer.", ["dsh-cordis", "dsh-compaction", "dsh-commands"]),
    ("crates/credentials/credentials-local", "dsh-credentials-local", "Env/.env credential provider.", ["dsh-cordis", "dsh-credentials"]),
    ("crates/skill/skill-filesystem", "dsh-skill-filesystem", "Filesystem skill provider.", ["dsh-cordis", "dsh-skill"]),
    ("crates/skill/tool-skill", "dsh-tool-skill", "Model-facing skill tool.", ["async-trait", "dsh-cordis", "dsh-skill", "dsh-tools", "serde_json"]),
    ("crates/web/web-fetch-http", "dsh-web-fetch-http", "HTTP fetch provider.", ["dsh-cordis", "dsh-web", "reqwest", "tokio"]),
    ("crates/web/tool-web", "dsh-tool-web", "Model-facing web tools.", ["async-trait", "dsh-cordis", "dsh-web", "dsh-tools", "serde_json"]),
    ("crates/jobs/jobs-local", "dsh-jobs-local", "Local jobs provider.", ["dsh-cordis", "dsh-jobs"]),
    ("crates/jobs/tool-jobs", "dsh-tool-jobs", "Model-facing job_* tools.", ["async-trait", "dsh-cordis", "dsh-jobs", "dsh-tools", "serde_json"]),
    ("crates/todo/tool-todo", "dsh-tool-todo", "todo_write tool.", ["async-trait", "dsh-cordis", "dsh-session", "dsh-tools", "serde_json"]),
    ("crates/goal/tool-goal", "dsh-tool-goal", "Model-facing goal tool.", ["async-trait", "dsh-cordis", "dsh-goal", "dsh-tools", "serde_json"]),
    ("crates/subagent/subagent-inprocess", "dsh-subagent-inprocess", "In-process subagent provider.", ["dsh-cordis", "dsh-subagent", "dsh-agent"]),
    ("crates/subagent/tool-subagent", "dsh-tool-subagent", "Model-facing subagent tool.", ["async-trait", "dsh-cordis", "dsh-subagent", "dsh-tools", "serde_json"]),
    ("crates/workflow/workflow-local", "dsh-workflow-local", "Local workflow provider.", ["dsh-cordis", "dsh-workflow"]),
    ("crates/workflow/tool-workflow", "dsh-tool-workflow", "workflow/ralph tools.", ["async-trait", "dsh-cordis", "dsh-workflow", "dsh-tools", "serde_json"]),
    ("crates/guard/timeout-policy", "dsh-timeout-policy", "tools/execute deadline enforcer.", ["dsh-cordis", "dsh-timeout", "dsh-tools"]),
    ("crates/session/session-persistence-jsonl", "dsh-session-persistence-jsonl", "JSONL persistence provider.", ["dsh-cordis", "dsh-session", "dsh-session-persistence", "dsh-atomic-write", "serde_json", "tokio"]),
    ("crates/llm/llm-deepseek", "dsh-llm-deepseek", "DeepSeek LLM adapter.", ["async-trait", "dsh-cordis", "dsh-llm", "dsh-credentials", "futures", "reqwest", "serde_json"]),
    ("crates/sdk/protocol", "dsh-sdk-protocol", "JSON-RPC protocol types.", ["serde", "serde_json"]),
    ("crates/sdk/client", "dsh-sdk-client", "JSON-RPC TypeScript-equivalent client.", ["dsh-sdk-protocol", "serde_json", "tokio"]),
    ("crates/sdk/server", "dsh-sdk-server", "JSON-RPC server plugin.", ["dsh-cordis", "dsh-sdk-protocol", "dsh-agent", "dsh-session", "serde_json", "tokio"]),
    ("crates/acp/acp", "dsh-acp", "ACP automation server.", ["dsh-cordis", "dsh-agent", "dsh-session", "serde_json"]),
    ("crates/boot/app-boot", "dsh-app-boot", "Profile/bundle boot glue.", ["dsh-cordis", "dsh-cordis-loader", "serde_yaml"]),
    ("crates/bundle/base", "dsh-bundle-base", "dsh-base patch layer.", ["dsh-cordis", "dsh-cordis-loader"]),
    ("crates/bundle/headless", "dsh-bundle-headless", "dsh-headless patch layer.", ["dsh-cordis", "dsh-cordis-loader"]),
    ("crates/examples/agent-spine", "dsh-agent-spine", "Runnable agent spine composition.", [
        "dsh-cordis", "dsh-session", "dsh-system-prompt", "dsh-tools", "dsh-agent",
        "dsh-agent-loop", "dsh-llm", "dsh-llm-replay",
    ]),
    ("crates/host/webserver", "dsh-webserver", "HTTP route server (ctx.webServer).", ["dsh-cordis", "axum", "tokio", "tower-http", "serde_json"]),
]

WRAPPER_BODY = '''//! {desc}
pub fn name() -> &'static str {{
    "{name}"
}}

#[cfg(test)]
mod tests {{
    #[test]
    fn names_the_role() {{
        assert!(!super::name().is_empty());
    }}
}}
'''

for rel, name, desc, deps in WRAPPERS:
    if not (ROOT / rel / "src" / "lib.rs").exists():
        lib_crate(rel, name, desc, deps, WRAPPER_BODY.format(desc=desc, name=name))

print("done")
