# Agent Note: Rust ports MCP, schedule, and hook bridges as opt-in plugins

Status: implemented

English | [中文](2026-09-04-rust-mcp-schedule-hooks.zh.md)

## Problem

The TypeScript tree already owns three opt-in capability seams that never belong in the default headless patch: `@deepseek-ai/dsh-mcp-client` publishes one external MCP server's tools on `ctx.tools`; `@deepseek-ai/dsh-schedule` gives later live root Agents three session-log reminders; `@deepseek-ai/dsh-hook-protocol` plus the Claude Code and Codex bridges run command hooks on harness interception points. A Rust host that omitted those names would load official overlays (`examples/mcp-memory`, `examples/web-schedule`, `examples/acp-agent`) as no-ops and silently drop model-visible tools, reminder follow-ups, and hook deny/ask decisions. Inventing a second MCP SDK, a cron scheduler, or honoring `updatedInput` on the Rust side would fork the TypeScript Known Limitations.

## Decision

`dsh-app-boot` mounts `@deepseek-ai/dsh-schedule`, `@deepseek-ai/dsh-mcp-client`, `@deepseek-ai/dsh-hooks-claude-code`, and `@deepseek-ai/dsh-hooks-codex` through `apply_named` when the composition tree names those plugins. The default headless tree does not insert those rows. TypeScript under `packages/` remains the behavior source; crate directories mirror `packages/<group>/<pkg>/`. [The port Agent Note](../architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule. [The gap ranking](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md) records these P3/P4 rows as closed.

### MCP client

One plugin instance owns one `serverName` (`/^[A-Za-z0-9_-]{1,32}$/`). A second live instance with the same name fails at load with the TypeScript sentence. The wire client identifies as `{ name: 'dsh-mcp-client', version: '0.0.1' }` on protocol `2025-03-26`. `callTool` always sends the raw MCP name. The public registry name is `mcp__<server>__<raw>`; characters outside `[A-Za-z0-9_-]` become `_`, and a lossy rewrite appends the first 12 hex characters of `sha256(server + '\0' + raw)`. stdio inherits `scrubbed_parent_env()` plus explicit `env`. Streamable HTTP uses `curl`. Reconnect defaults are `enabled: true`, `initialDelayMs: 500`, `maxDelayMs: 30000`, `maxAttempts: 10`. `failOnStartupError: true` rejects activation with `mcp-client(<server>): initial connection or tool synchronization failed`. Image blocks become durable attachments only after `ctx.attachments` and the exact default-model route declare image input; refused images, audio, embedded resources, and unknown blocks stay as the TypeScript diagnostic sentences. A supported advertised `outputSchema` becomes `structuredContent` on `Tool::output_schema`; unsupported vocabulary falls back to unconstrained JSON. MCP Resources and Prompts stay unbridged.

### Schedule

Version-1 `schedule/change` create, delete, and dispatch records fold from the session log. Tools are `schedule_create`, `schedule_list`, and `schedule_delete`. Rust has no per-agent `ctx.tools` and no `agent/created` event: the plugin registers the three tools once and enables them with `Tool::enabled_for` for live root owners published after load, and it listens to `agent/session-start`. Every management preflight and due decision awaits `ctx.sessionPersistence` flush; a missing or rejected barrier returns `persistence_uncertain`. Due one-shots have priority; overdue Every records form one batch. Delivery queues a later `followup` and appends dispatch only while the owner is idle. Cron expressions are not accepted.

### Hook protocol and bridges

`dsh-hook-protocol` owns Claude literal-or-regex matchers, Codex unanchored regex, exit-2 / JSON stdout codecs, deny/block > ask > allow/approve merge, `hook/invoked` / `hook/result` session events, and a generic command runner. Default timeout is 600000 ms; persisted stderr summaries cap at 500 characters. An invalid regex matches nothing and uses the TypeScript `invalid {mode} regex matcher` diagnostic.

`dsh-hooks-claude-code` requires `configPath`. It substitutes `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PROJECT_DIR}`, exports `CLAUDE_PROJECT_DIR`, frames stdin with a trailing newline, and maps SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, SubagentStart, and SubagentStop. UserPromptSubmit and Stop drop matchers. `ask` calls `ctx.approval` when that service is mounted and otherwise fail-closed denies. `updatedInput` and `systemMessage` warn with the TypeScript sentences and are not honored.

`dsh-hooks-codex` maps the five Codex events, skips `async: true` hooks, frames stdin without a trailing newline, and honors deny only. `plainStdoutAsContext` applies on SessionStart and UserPromptSubmit.

Rust waterfalls are synchronous. Both bridges run hook bodies with `block_in_place` plus the Tokio handle. `agent/turn-stopping` carries only `{turn}`; each bridge remembers the last `agent/pre-step` agent. Continuable children emit `subagent/start` after `agents.create` and before `followup`. A one-shot `SubagentRuntime::start` publishes `subagent/start` then `subagent/end` after `provider.start` settles.

## Alternatives considered

**Mount these plugins in the default headless tree.** Official TypeScript headless does not insert those rows. Opt-in overlays own the composition.

**Depend on `rmcp` / `reqwest` for MCP.** Those graphs pull edition 2024 or `hashbrown` 0.17. Hand-written JSON-RPC plus `curl` stay on Rust 1.83.

**Honor `updatedInput`, `systemMessage`, or MCP Resources/Prompts on Rust only.** That would fork the TypeScript Known Limitations. Those paths stay deferred until the TypeScript owner ships them.

**Give each Rust Agent a private `ctx.tools` so Schedule can register on `agent/created`.** The Rust tool registry is process-global. `enabled_for` on a live owner map is the same visibility rule without a second registry.

**Await hook futures from the waterfall thread with `futures::executor::block_on`.** That deadlocks inside Tokio. `block_in_place` plus the runtime handle is the adapter that keeps the TypeScript async hook runner.

## Consequences

Official MCP, schedule, and hook overlays load on the Rust host without changing `dsh-agent-loop`, `SESSION_FORMAT_VERSION`, or SQLite `SCHEMA_VERSION`. Headless dump-config stays free of those rows unless an overlay inserts them. One-shot SubagentStart hooks run after the child settles. Native MCP results are content blocks; Code Mode's separate canonical `{content, structuredContent}` binding is absent because Rust `ToolOutcome` has no second value slot. Inventory status lives in [ts-rust-functional-gaps.md](../../../../rust/docs/ts-rust-functional-gaps.md).
