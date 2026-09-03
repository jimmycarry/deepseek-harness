# TypeScript ↔ Rust functional gaps and port priorities

English | [中文](ts-rust-functional-gaps.zh.md)

Reference inventory of every `packages/<group>/<pkg>` leaf against `rust/crates/<group>/<pkg>`, plus the ranking for remaining port work. TypeScript under `packages/` remains the behavior source. This page does not change that rule; it records where the Rust tree is thinner, stubbed, remapped, or intentionally absent. The ranking decision lives in [the proposed Agent Note](../../.agents/notes/proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md). Shipped Rust behavior stays in [rust/README.md](../README.md) and [the port Agent Note](../../.agents/notes/implemented/architecture/2026-08-22-rust-harness-port.md).

Evidence date: 2026-09-03. Counts: **227** TypeScript packages, **112** Rust crates under `rust/crates/*/*/`, **104** same `(group, pkg)` pairs, **123** TypeScript-only leaves, **8** Rust-only leaves (name remaps or extra patch crates). Leaf-name-only matching is wrong: `typert/protocol` is not `sdk/protocol`, and `acp/acp` is not `bundle/acp`.

## How to read the ranking

| Label | Meaning |
|---|---|
| **P0** | Headless, ACP, or JSON-RPC product-user or model-visible correctness on Linux — the shipped Rust profiles |
| **P1** | Durability, ops, or secondary surfaces on those same profiles |
| **P2** | Other platforms, a Rust host that speaks the existing TypeScript client protocol, or adjacent optional providers |
| **P3** | Opt-in examples and extra agent capabilities |
| **P4** | Experimental, cloud, or hook bridges |
| **P5** | Utility libraries, ported with their first consumer |
| **skip** | Not a Rust port target, or a documented independent format |

A later crate that would rewrite the TypeScript Web UI in Rust is a product fork, not a port ([rejected alternative](../../.agents/notes/implemented/architecture/2026-08-22-rust-harness-port.md#alternatives-considered)). `dsh-agent-loop` stays a plugin; finish-chunk recovery is reported here and is not closed by editing the loop. Session logs stay on `SESSION_FORMAT_VERSION` `0`. Rust SQLite uses monotonic `SCHEMA_VERSION` `2` and refuses other `user_version` values; reaching TypeScript packed schema 17 is **skip**, not P0.

## Name remaps

These Rust directories do not share the TypeScript folder name. They are not missing packages.

| TypeScript | Rust | Notes |
|---|---|---|
| `core/agent-default-model` | `dsh-agent::AgentDefaultModel` in `core/agent` | Mounted for real in `app-boot` |
| `compaction/compaction-tool-result-pruner` | `compaction/tool-result-pruner` | Same Consumer |
| `subagent/subagent-spawn-in-process` + `subagent-fork-in-process` + `subagent-in-process-driver` | `subagent/subagent-inprocess` | Combined spawn/fork provider |
| `workflow/workflow-worker-thread` | `workflow/workflow-local` | In-process `return <json>`; not a JS worker |
| `terminal/terminal-bash` | `terminal/terminal-local` | In-memory stub, not a PTY |
| `test-support/llm-replay` | `llm/llm-replay` | Replay adapter used by snapshots |
| `examples/agent-spine-demo` | `examples/agent-spine` | Spine demo crate |
| `examples/acp-demo` / `examples/jsonrpc-demo` | `bundle/acp` / `bundle/jsonrpc` | Patch-layer crates over `packages/acp/acp` and `packages/sdk/server` |

## Intentionally not a Rust crate

| Area | TypeScript leaves | Why this is skip |
|---|---|---|
| Web UI | entire `packages/client/*` (40 packages) | Host may later speak the existing client protocol; a second UI is a fork |
| Test infrastructure | `test-support/*` except the remapped replay adapter | Vitest/tsx harnesses stay in TypeScript |
| Typert generator | `typert/generator` | TypeScript build-time analyzer |
| Runtime diagnostics | `runtime-diagnostics/invariants` | TypeScript package-invariant companion |

`app-boot` still mounts some of these names as no-ops so the composed TypeScript tree can load: `@deepseek-ai/cordis-plugin-hmr`, `@deepseek-ai/dsh-typert-registry`, `@deepseek-ai/dsh-typert-loader`, `@deepseek-ai/dsh-api-gateway`, `@deepseek-ai/dsh-llm-pi-ai`, `@deepseek-ai/dsh-skill-badge`, and `@deepseek-ai/dsh-code-runtime-worker-thread` (marker `codeRuntime` only). Unknown names also return `Ok(())`.

## Priority summary

### P0 — shipped-profile correctness (Linux)

1. **Session persistence coordinator** — **closed.** `PersistenceRuntime` write-behind (`writeBatchMaxDelayMs`, default 200, not reset by further appends), public `create` / `append` / `prepare` / `readFrom`, and durable `commitRepair` on JSONL (truncate to the last complete `\n`) and SQLite (`DELETE` from the first undecodable or gapped seq). SQLite stays at schema `2`. `inspect` still synthesizes closers in memory ([`rust/crates/session/session-persistence/src/lib.rs`](../crates/session/session-persistence/src/lib.rs)).
2. **LLM DeepSeek transport** — **closed** for SSE and image blocks. `"stream": true` plus `stream_options.include_usage`; parse `data:` / `[DONE]`; a stream without `[DONE]` is `STREAM_CLOSED`; `finish` / `usage` emit only after `[DONE]`. A vision model (`model` contains `vision`) sends user images as `image_url` data-URLs. Files API upload is not mounted ([`rust/crates/llm/llm-deepseek/src/lib.rs`](../crates/llm/llm-deepseek/src/lib.rs)).
3. **Attachment raster pipeline** — **closed.** `request_image` decodes, longest-edge downscales (`normalizedImageMaxDimension`, default 2048), and JPEG-re-encodes under `normalizedImageMaxBytes` (default 4 MiB). Over-cap after quality 85 then 80 is `IMAGE_TOO_LARGE` ([`rust/crates/attachment/attachment-local/src/lib.rs`](../crates/attachment/attachment-local/src/lib.rs)).
4. **ACP image prompts** — **closed.** Advertise `image: true` only when `ctx.attachments` and a vision `AgentDefaultModel` are mounted; admit `image` blocks through `save_image`. Default headless stays `image: false` and rejects with `inline image prompts were not advertised by this connection` ([`rust/crates/acp/acp/src/lib.rs`](../crates/acp/acp/src/lib.rs)).
5. **Settings Service Definition** — **closed** on `settings-file` (`ctx.settings`): `register` / `watch` / `revision` / `mutate` / `describe` plus `settings/updated` (`ns`, `revision`, `value`) and `settings/document-updated` (`revision`). A standalone `settings` crate is still absent.
6. **Plan-mode review on headless** — **closed.** Default review has no provider and fails with `no user-questions provider is registered`. Config `reviewProvider: "auto"` mounts a first-option approver; that option without `ctx.userQuestions` fails at install. The default headless patch does not set `reviewProvider` ([`rust/crates/plan/plan-mode/src/lib.rs`](../crates/plan/plan-mode/src/lib.rs)).
7. **Agent-loop finish-chunk errors** — the loop ends a turn on `stream()` `Err`, and only `FinishReason::MaxTokens` changes turn end; in-stream `finish { kind: error \| aborted }` is logged and the turn still completes ([`rust/crates/core/agent-loop/src/lib.rs`](../crates/core/agent-loop/src/lib.rs)). **Report only; do not edit `dsh-agent-loop` to close this row.**

### P1 — durability and ops on shipped profiles

8. Wire `preparedSessionCacheSize` / `writeBatchMaxDelayMs` from plugin `install` Config — **closed** with P0-1 (`dsh-app-boot` / `PersistenceRuntime`).
9. `readFrom` / suffix reads — **closed** with P0-1 (`PersistenceRuntime::read_from`).
10. `session-projection-cache` (no crate).
11. Session-query FTS (`openAt: never` in [`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml); SQLite FTS schema 1 vs TypeScript 8).
12. Enable `web-fetch-http` in profiles that need URL retrieval (crate exists; base `fetch: false`).
13. Skill filesystem watch / poll-until-root (Rust rescans on `agent/pre-step` and `fs/observed` only).
14. OTel seam flush hint (metrics stay skip).
15. SDK client helpers (`Session.run`, descendant notification merge) versus the thin stdio wrapper.
16. External subagent providers (ACP / Codex / Claude / `dsh-sdk`).
17. `tool-ask-user` Consumer over the existing `user-questions` service.

### P2 — platforms and adjacent products

18. Rust HTTP host that speaks the existing TypeScript client protocol (`host/webserver` + full `host/apiproxy` BFF + `boot/cmdline` + `bundle/web-app` as a host of the TypeScript SPA — not a rewritten UI).
19. Typert registry / loader / protocol and `api/gateway` + `api/remotes` (needed only if that host ships).
20. macOS Seatbelt and Win32 `CreateRestrictedToken` (Linux bwrap/landlock already ship; Windows crate prefixes the Node ACL runner and does not call the token API).
21. Real PTY (`terminal-bash`) and stdio LSP.
22. JavaScript workflow workers.
23. `llm-pi-ai`, Exa, Perplexity.
24. Storage / workspace / agent-presets / persona (Web data plane).
25. Code Mode (`code-runtime*` + `agent-tool-presentation`).

### P3 — opt-in capabilities

26. `schedule`, `time-context`, `tmux-context`.
27. `tool-session-query`, `session-log-export`, `session-stats`, `session-title-all-prompts-llm`.
28. Persistent shell tools (`tool-bash-persistent` / `tool-pwsh-persistent`).
29. `skill-badge` (no-op, disabled in base).
30. `message-feedback` (Web); `command-feedback` already ships.

### P4 — experimental / cloud / hooks

31. `e2b` / `fs-e2b` / `subprocess-e2b`.
32. `experimental/agent-team` + `tool-agent-team`.
33. `hooks-claude-code` / `hooks-codex` / `hook-protocol`.
34. `mcp-client`.
35. Dynamic Cordis (`tool-cordis`, host/client runners).

### P5 — utilities with the first consumer

36. `launch-environment`, `native-command`, `output-retention`.
37. Extract `session-title-llm` only if a second title provider needs the shared route.

## Recommended closure order

1. Persistence coordinator, DeepSeek SSE + image blocks, attachment normalization, settings Service Definition, and headless plan-review — **closed** (P0 items 1–6).
2. Record the loop finish-chunk gap; do not change `dsh-agent-loop` here (P0 item 7, report only).
3. DeepSeek Files API upload (inline `image_url` data-URLs already ship).
4. Session-query FTS opt-in, web fetch enablement, skill watching, OTel flush, SDK helpers, external subagents.
5. Platform sandbox and PTY/LSP only when those headless hosts are in scope.
6. Rust-as-host for the existing TypeScript Web client only after the headless spine gaps above are closed.

## Per-group matrix

Status values: **aligned** (headless contract matches), **thinner** (crate exists, contract incomplete), **stub** (mounted but in-memory / marker), **no-op** (`app-boot` `Ok(())` or marker), **absent**, **remap**, **skip**.

### acp

| Package | Status | Gap | Pri |
|---|---|---|---|
| `acp` | aligned for image prompts | Advertise `image: true` only with `ctx.attachments` + vision default model; audio / embeddedContext stay false | remaining (audio / resource) |

### api

| Package | Status | Gap | Pri |
|---|---|---|---|
| `gateway` | no-op | `dsh-api-gateway` row loads as `Ok(())`; no Typert RPC gateway | P2 |
| `remotes` | absent | No Remote contribution assembly | P2 |

### attachment

| Package | Status | Gap | Pri |
|---|---|---|---|
| `attachment` | aligned | Store types present | — |
| `attachment-local` | aligned | Magic-byte admit plus `request_image` JPEG normalization | remaining (Files API is DeepSeek-side) |

### boot

| Package | Status | Gap | Pri |
|---|---|---|---|
| `app-boot` | thinner | Real mounts for the headless tree; leftover names no-op; persistence Config (`preparedSessionCacheSize` / `writeBatchMaxDelayMs`) wired | P1 no-ops listed above |
| `cmdline` | absent | TypeScript shared argv parser; Rust inlines task flags on headless startup | P2 |

### bundle

| Package | Status | Gap | Pri |
|---|---|---|---|
| `base` / `headless` | aligned | Same TypeScript patch files; documented `fetch: false` and `openAt: never` | P1 (product flags) |
| `web-app` | absent | No Rust web bundle; `profile_templates()` may name it | P2 |
| `bundle/acp` / `bundle/jsonrpc` | remap | Rust patch crates; TypeScript servers live under `acp` / `sdk` | — |

### client

All 40 packages (**skip**). Do not port the SPA, slots, or `ui-*` plugins to Rust.

### code-runtime

| Package | Status | Gap | Pri |
|---|---|---|---|
| `code-runtime` | absent | No `run` Service Definition crate | P2 |
| `code-runtime-python` | absent | No CPython backend | P2 |
| `code-runtime-worker-thread` | no-op | Marker `codeRuntime` only | P2 |

### compaction

| Package | Status | Gap | Pri |
|---|---|---|---|
| `compaction` / `compaction-basic` / `command-compact` | aligned | Pressure, overflow, `/compact`, checkpoint framing | P1 (re-test after SSE) |
| `compaction-tool-result-pruner` | remap | `tool-result-pruner` | — |

### context

| Package | Status | Gap | Pri |
|---|---|---|---|
| `agent-instructions` | aligned | Baseline + update messages | — |
| `file-reference` / `file-reference-local` | absent | `@file` discovery | P2 (Web) |
| `session-reference` | absent | `@session` injection | P2 |
| `time-context` | absent | Request clock messages | P3 |
| `tmux-context` | absent | tmux pane context | P3 |

### core

| Package | Status | Gap | Pri |
|---|---|---|---|
| `session` / `agent` / `tools` / `system-prompt` / `scope` | aligned | Headless spine | — |
| `agent-loop` | thinner | Missing in-stream finish-error / aborted turn end | P0 report only |
| `agent-default-model` | remap | Installed from `dsh-agent` | — |
| `agent-tool-presentation` | absent | Code Mode `presentAs` | P2 |

### credentials

| Package | Status | Gap | Pri |
|---|---|---|---|
| `credentials` / `authorization` / `credentials-local` | aligned | Env / YAML / `.env` order; Unix mode refuse | P1 (re-verify file mode on Unix) |

### e2b

All three packages **absent** / **P4**.

### examples

| Package | Status | Gap | Pri |
|---|---|---|---|
| `agent-spine-demo` | remap | `examples/agent-spine` | — |
| `acp-demo` / `jsonrpc-demo` | remap | `bundle/acp` / `bundle/jsonrpc` + `apps/cli` | — |

### experimental

`agent-team` / `tool-agent-team`: **absent** / **P4**.

### extensions

`tool-cordis` / `ui-cordis` / `cordis-host-runner` / `cordis-client-runner`: **absent** / **P4**.

### feedback

| Package | Status | Gap | Pri |
|---|---|---|---|
| `command-feedback` | aligned | `/feedback` log-only | — |
| `message-feedback` | absent | Per-message like/dislike on storage | P3 |

### fs

| Package | Status | Gap | Pri |
|---|---|---|---|
| `fs` / `fs-local` / `fs-sandbox` / `tool-fs` / `tool-fs-search` / `tool-str-replace-editor` | aligned | Observation + sandbox write/edit | — |
| `fs-observation-policy` | aligned | Same resume limitation as TypeScript (observation does not survive resume) | P1 shared defer |

### goal

`goal` / `goal-round-driver` / `command-goal` / `tool-goal`: **aligned**.

### guard

`repeat-tool-reminder` / `timeout-policy`: **aligned**.

### hooks

All three packages **absent** / **P4**.

### host

| Package | Status | Gap | Pri |
|---|---|---|---|
| `webserver` | thinner | `/health` + POST `/rpc`; no SPA fallback, SSE, or upgrade table; not mounted by `apply_named` | P2 |
| `apiproxy` | thinner | HTTP POST forward only; not the TypeScript BFF; not mounted by `apply_named` | P2 |
| `frontend-static` / `plugin-inventory` / `directory-picker*` | absent | Web chrome | P2 |

### identity

`anonymous-user-id`: **aligned**.

### interaction

| Package | Status | Gap | Pri |
|---|---|---|---|
| `commands` / `permission-presets` / `user-approval` | aligned | Headless `never` / `ask` fail-closed | — |
| `user-questions` | thinner | Service without a default provider; plan-mode can opt in `reviewProvider: "auto"` | remaining (interactive provider) |
| `tool-ask-user` | absent | Model `ask_user_question` | P1 |

### jobs

`jobs` / `jobs-local` / `tool-jobs`: **aligned**.

### llm

| Package | Status | Gap | Pri |
|---|---|---|---|
| `llm` | aligned | Chunk tags include `FinishReason::Error` | P0 consumers must honor it |
| `llm-deepseek` | thinner | SSE + user `image_url` data-URLs aligned; no Files API upload; retry/classify/`Retry-After` aligned | remaining (Files API) |
| `llm-retry` / `token-meter` | aligned | `providerRetryAfterMs` over-cap rules | — |
| `llm-pi-ai` | no-op | Bundle row, no crate | P2 |
| `llm-replay` | remap | Lives under `llm/` | — |

### lsp

| Package | Status | Gap | Pri |
|---|---|---|---|
| `lsp` / `lsp-stdio` / `tool-lsp` | stub | Records initialize; static capabilities; no language-server process | P2 |

### mcp

`mcp-client`: **absent** / **P4**.

### plan

`plan-mode`: **aligned** for `/plan` + `exit_plan_mode` and opt-in `reviewProvider: "auto"`. Default review stays fail-closed.

### preset

`agent-presets` / `persona`: **absent** / **P2**.

### runtime-diagnostics

`invariants`: **skip** (TypeScript test companion).

### sandbox

| Package | Status | Gap | Pri |
|---|---|---|---|
| `sandbox` / `sandbox-local` / `sandbox-policy` | aligned on Linux | bwrap + landlock-run; no `sandbox-exec` | P2 Seatbelt |
| `sandbox-windows-acl` | thinner | Node runner argv only; no `CreateRestrictedToken` | P2 |

### schedule

`schedule`: **absent** / **P3**.

### sdk

| Package | Status | Gap | Pri |
|---|---|---|---|
| `protocol` / `server` | aligned | stdio JSON-RPC identity `deepseek-harness-sdk-runtime` | — |
| `client` | thinner | `initialize` / `prompt` / `shutdown` + notification drain; no `Session.run` helpers | P1 |

### session

| Package | Status | Gap | Pri |
|---|---|---|---|
| `session-persistence` | aligned | write-behind, `create` / `append` / `prepare` / `readFrom`, `load` durable `commitRepair`; `inspect` stays in-memory closers | remaining (read-repair / incarnation) |
| `session-persistence-jsonl` | aligned | Header + events + `list` + torn last-line `commitRepair` | remaining (read-repair) |
| `session-persistence-sqlite` | aligned | Schema 2 plus torn-row `commitRepair`; schema 17 is **skip** | remaining (read-repair) |
| `session-projection` | aligned | Registry present | — |
| `session-checkpoint-policy` | aligned | Flush before model request and top-level tool dispatch | P1 (re-test with write-behind) |
| `session-telemetry` / `session-telemetry-otel` | thinner | OTLP logs + `Retry-After` + keepAlive; no flush hint; no metrics | P1 flush; metrics **skip** |
| `session-title` / `session-title-first-prompt-llm` | aligned | Fallback + first-prompt LLM | — |
| `session-projection-cache` | absent | Durable projection checkpoints | P1 |
| `session-stats` | absent | Chat stats projection | P3 |
| `session-title-llm` | absent | Shared library; logic lives in the first-prompt crate | P5 |
| `session-title-all-prompts-llm` | absent | All-human-messages titles | P3 |

### session-query

| Package | Status | Gap | Pri |
|---|---|---|---|
| `session-query` | thinner | Exact reads; lineage empty; search disabled by default | P1 |
| `session-query-sqlite` | thinner | FTS schema 1; `openAt: never` | P1 |
| `session-log-export` | absent | ZIP export command | P3 |
| `tool-session-query` | absent | Model search/read tools | P3 |

### settings

| Package | Status | Gap | Pri |
|---|---|---|---|
| `settings-file` | aligned | File document, YAML leaf `update` / `replace`, plus `register` / `watch` / `revision` / `mutate` / `describe` and settings events | remaining (none on this crate) |
| `settings` | absent | Methods live on `settings-file` (`ctx.settings`); no standalone definition crate | remaining (split only if roles diverge) |

### shell

| Package | Status | Gap | Pri |
|---|---|---|---|
| `shell` / `shell-env` / `bash-local` / `bash-sandbox` / `tool-bash` | aligned on Linux | Confine + jobs | P2 other OS runners |
| `pwsh-local` / `pwsh-sandbox` / `tool-pwsh` | aligned | Disabled on non-win32 via `!!js` | — |
| `tool-bash-persistent` / `tool-pwsh-persistent` | absent | Stateful PTY tools | P3 |

### skill

| Package | Status | Gap | Pri |
|---|---|---|---|
| `skill` / `tool-skill` | aligned | Catalog messages + `<skill_content>` | — |
| `skill-filesystem` | thinner | Rescan on `agent/pre-step` and skill-path `fs/observed`; no Chokidar / poll | P1 |
| `skill-badge` | no-op | Disabled in base | P3 |

### spill

`spill` / `spill-local` / `spill-policy`: **aligned**.

### storage

All four packages **absent** / **P2**.

### subagent

| Package | Status | Gap | Pri |
|---|---|---|---|
| `subagent` / `tool-subagent` / `tool-subagent-control` / `tool-subagent-report` | aligned | Continuable spawn, cold resume, `list_agents` diagnostic, `report` | P1 (cold inspect depends on `commitRepair`) |
| `subagent-inprocess` | remap | Combined spawn/fork | P1 driver wake latch |
| `subagent-acp` / `subagent-claude-code` / `subagent-codex` / `subagent-dsh-sdk` | absent | Out-of-process providers | P1 |

### subprocess

`subprocess` / `subprocess-local`: **aligned**.

### terminal

| Package | Status | Gap | Pri |
|---|---|---|---|
| `terminal` / `tool-terminal` | stub | In-memory write history | P2 |
| `terminal-local` | remap / stub | Not `terminal-bash` PTY | P2 |
| `terminal-bash` | absent | Real PTY + sandbox policy | P2 |

### test-support

**skip** except remapped `llm-replay`.

### todo

`tool-todo`: **aligned**.

### typert

| Package | Status | Gap | Pri |
|---|---|---|---|
| `registry` / `loader` | no-op | Rows load; no `ctx.typert` | P2 |
| `protocol` | absent | Remote/Gateway types | P2 |
| `generator` | skip | Build-time TS | skip |

### util

| Package | Status | Gap | Pri |
|---|---|---|---|
| `brand` / `timeout` / `atomic-write` / `home-paths` | aligned | — | — |
| `launch-environment` / `native-command` / `output-retention` | absent | Libraries | P5 |

### web

| Package | Status | Gap | Pri |
|---|---|---|---|
| `web` / `web-search-deepseek` / `tool-web` | aligned | Official + `replay`; `fetch: false` | P1 fetch flag |
| `web-fetch-http` | thinner | Crate present; not enabled in base | P1 |
| `web-search-exa` / `web-search-perplexity` | absent | Optional providers | P2 |

### workflow

| Package | Status | Gap | Pri |
|---|---|---|---|
| `workflow` / `tool-workflow` / `tool-ralph` | aligned for `return <json>` / Ralph-over-spawn | Isolation differs | — |
| `workflow-local` | remap / thinner | In-process eval | P2 worker |
| `workflow-worker-thread` | absent as a JS worker | TypeScript default engine | P2 |

### workspace

`workspace`: **absent** / **P2**.

## Already aligned (do not re-open as gaps)

Credentials resolution, `llm-retry` `retryPolicy` + `providerRetryAfterMs` (delay-seconds and HTTP-date, over-cap `normal`/`always`), sandbox-policy / approval / permission-presets, continuable in-process subagents with cold resume and `list_agents` diagnostics, persistence write-behind / `append` / durable `commitRepair` / inspect LRU `preparedSessionCacheSize`, Windows ACL Node runner argv, OTel keepAlive + `Retry-After` HTTP-date, compaction-basic main path, goal / todo / plan tools (including opt-in `reviewProvider: "auto"`; default headless review stays fail-closed), jobs, spill, agent-instructions, fs observation gate, skill catalog tool, token-meter, titles, attachment `request_image`, ACP image prompts when store + vision are mounted, DeepSeek SSE + `image_url` data-URLs, settings `register` / `watch` / `revision` / `mutate`.

## Verification

```sh
python3 - <<'PY'
from pathlib import Path
ts=len(list(Path('packages').glob('*/*/package.json')))
rs=len(list(Path('rust/crates').glob('*/*/Cargo.toml')))
print(ts, rs)
PY
cargo test --workspace
cargo run -p dsh -- --dump-config
```

Re-read `install()` / `apply_named` and the TypeScript package README **Known Limitations** before promoting a row. A green unit test on a thinner crate does not prove TypeScript parity.
