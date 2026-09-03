#!/usr/bin/env python3
"""Generate remaining seam crates with real SD/Provider/Consumer skeletons."""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def crate(path: str, name: str, desc: str, deps: list[str], lib: str):
    dest = ROOT / path
    dest.mkdir(parents=True, exist_ok=True)
    dep_lines = "\n".join(f"{d}.workspace = true" for d in deps)
    (dest / "Cargo.toml").write_text(
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
        encoding="utf-8",
    )
    src = dest / "src"
    src.mkdir(exist_ok=True)
    (src / "lib.rs").write_text(lib, encoding="utf-8")
    print(path)


# Shared tiny service crate factory
def service_crate(path, name, desc, key, extra_deps=None, extra_lib=""):
    deps = ["dsh-cordis"] + (extra_deps or [])
    crate(
        path,
        name,
        desc,
        deps,
        f'''//! {desc}
use dsh_cordis::Service;

pub struct Runtime;

impl Runtime {{
    pub fn new() -> Self {{ Self }}
}}

impl Default for Runtime {{
    fn default() -> Self {{ Self::new() }}
}}

impl Service for Runtime {{
    const KEY: &'static str = "{key}";
}}

{extra_lib}

#[cfg(test)]
mod tests {{
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    #[test]
    fn registers_on_context() {{
        let ctx = Context::new();
        ctx.provide(Arc::new(Runtime::new())).unwrap();
        assert!(ctx.has_service("{key}"));
        ctx.dispose();
        assert!(!ctx.has_service("{key}"));
    }}
}}
''',
    )


# execution
exec(open(str(Path(__file__).with_name("gen_remaining.py"))).read()) if False else None

from textwrap import dedent

# I'll just call crate() for everything remaining below.

# Already planning to write subprocess etc. here fully.

SEAMS = [
    ("crates/shell/shell", "dsh-shell", "Shell executor seam (ctx.shell).", "shell"),
    ("crates/fs/fs", "dsh-fs", "Filesystem seam (ctx.fs).", "fs"),
    ("crates/terminal/terminal", "dsh-terminal", "Persistent PTY seam (ctx.terminals).", "terminals"),
    ("crates/lsp/lsp", "dsh-lsp", "Language-server seam (ctx.lsp).", "lsp"),
    ("crates/credentials/credentials", "dsh-credentials", "Credential-reference seam (ctx.credentials).", "credentials"),
    ("crates/credentials/authorization", "dsh-authorization", "Human credential-grant seam (ctx.authorization).", "authorization"),
    ("crates/skill/skill", "dsh-skill", "Skill provider registry (ctx.skills).", "skills"),
    ("crates/web/web", "dsh-web", "Web search/fetch seam (ctx.web).", "web"),
    ("crates/jobs/jobs", "dsh-jobs", "Background-job runtime (ctx.jobs).", "jobs"),
    ("crates/plan/plan-mode", "dsh-plan-mode", "Plan collaboration state (ctx.planMode).", "planMode"),
    ("crates/goal/goal", "dsh-goal", "Same-session goal domain (ctx.goals).", "goals"),
    ("crates/subagent/subagent", "dsh-subagent", "Subagent provider registry (ctx.subagents).", "subagents"),
    ("crates/workflow/workflow", "dsh-workflow", "Workflow seam (ctx.workflowEngine).", "workflowEngine"),
    ("crates/session/session-persistence", "dsh-session-persistence", "Durable session persistence seam (ctx.sessionPersistence).", "sessionPersistence"),
    ("crates/session/session-projection", "dsh-session-projection", "Session projection units (ctx.sessionProjections).", "sessionProjections"),
    ("crates/interaction/commands", "dsh-commands", "Human command registry (ctx.commands).", "commands"),
    ("crates/host/apiproxy", "dsh-apiproxy", "Host API proxy (ctx.apiProxy).", "apiProxy"),
    ("crates/llm/token-meter", "dsh-token-meter", "Replay token measurement (ctx.tokenMeter).", "tokenMeter"),
    ("crates/compaction/compaction", "dsh-compaction", "Compaction engine seam (ctx.compaction).", "compaction"),
    ("crates/compaction/tool-result-pruner", "dsh-tool-result-pruner", "Model-free tool-result pruning (ctx.toolResultPruner).", "toolResultPruner"),
]

for path, name, desc, key in SEAMS:
    service_crate(path, name, desc, key)

print("base seams done")
