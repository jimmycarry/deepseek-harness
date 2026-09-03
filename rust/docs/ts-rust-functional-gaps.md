# TypeScript ↔ Rust functional gap analysis

Scope: every leaf under `rust/crates/` that mirrors `packages/<group>/<pkg>/`. Style and implementation mechanics are out of scope; only product-visible behavior, durability, and wire contracts.

Priority key:

| Label | Meaning |
|---|---|
| **P0** | Headless product-user or model-visible correctness on Linux |
| **P1** | Durability, ops, or secondary headless surfaces |
| **P2** | Other platforms, GUI-only, or adjacent products |

Evidence sources: TypeScript package READMEs (`## Known Limitations and Deferred Work`), `install()` / public APIs in Rust `lib.rs`, and [`rust/README.md`](../README.md).

---

## Priority summary

### P0 — headless spine (Linux)

1. **Session persistence coordinator** — no write-behind batching, `prepare`/`commitRepair`, `readFrom`, append-only SQLite; full-log rewrite on save ([`packages/session/session-persistence`](../../packages/session/session-persistence), [`rust/crates/session/session-persistence-sqlite/src/lib.rs`](../crates/session/session-persistence-sqlite/src/lib.rs)).
2. **LLM DeepSeek adapter** — `stream: false` only; no SSE, no image blocks, no Files API / inline recovery ([`packages/llm/llm-deepseek`](../../packages/llm/llm-deepseek), [`rust/crates/llm/llm-deepseek/src/lib.rs`](../crates/llm/llm-deepseek/src/lib.rs)).
3. **Agent loop finish-chunk path** — Rust loop does not translate in-stream `finish { kind: error \| aborted }` into turn errors (report only; do not change loop in this task) ([`packages/core/agent-loop/src/agent.ts`](../../packages/core/agent-loop/src/agent.ts), [`rust/crates/core/agent-loop/src/lib.rs`](../crates/core/agent-loop/src/lib.rs)).
4. **Attachment raster pipeline** — store-only; no Sharp normalization or request-image projection ([`packages/attachment/attachment-local`](../../packages/attachment/attachment-local), [`rust/crates/attachment/attachment-local/src/lib.rs`](../crates/attachment/attachment-local/src/lib.rs)).
5. **ACP image prompts** — advertised `promptCapabilities.image: false`; inline images reject ([`packages/acp/acp`](../../packages/acp/acp), [`rust/crates/acp/acp/src/lib.rs`](../crates/acp/acp/src/lib.rs)).
6. **Plan mode review** — `exit_plan_mode` needs `ctx.userQuestions` provider; headless fails closed ([`packages/plan/plan-mode`](../../packages/plan/plan-mode), [`rust/crates/plan/plan-mode/src/lib.rs`](../crates/plan/plan-mode/src/lib.rs)).
7. **Settings seam** — file-backed sections only; no `ctx.settings` Service Definition (register/watch/revision/mutate) ([`packages/settings/settings`](../../packages/settings/settings), [`rust/crates/settings/settings-file/src/lib.rs`](../crates/settings/settings-file/src/lib.rs)).

### P1 — durability / ops

8. **SQLite schema parity** — session store schema 17 vs 2; session-query FTS schema 8 vs 1; incompatible on-disk artifacts.
9. **Session projection cache** — no Rust crate; cold list/history projections lack durable checkpoint ladder ([`packages/session/session-projection-cache`](../../packages/session/session-projection-cache)).
10. **Session query surface** — simplified reads; lineage empty; FTS disabled by default (`openAt: never`) ([`packages/session-query/session-query`](../../packages/session-query/session-query), [`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml)).
11. **Web fetch + providers** — base bundle sets `fetch: false`; no Exa/Perplexity crates ([`packages/web/web-search-exa`](../../packages/web/web-search-exa), [`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml)).
12. **Skill filesystem watching** — rescan on `agent/pre-step` and `fs/observed` only; no Chokidar / poll-until-root ([`packages/skill/skill-filesystem`](../../packages/skill/skill-filesystem), [`rust/crates/skill/skill-filesystem/src/lib.rs`](../crates/skill/skill-filesystem/src/lib.rs)).
13. **OTEL telemetry** — no `forceFlush` on seam hint; no SDK metrics ([`packages/session/session-telemetry-otel`](../../packages/session/session-telemetry-otel), [`rust/crates/session/session-telemetry-otel/src/lib.rs`](../crates/session/session-telemetry-otel/src/lib.rs)).
14. **Subagent providers** — in-process spawn/fork only; no ACP/Codex/Claude/driver split packages ([`packages/subagent`](../../packages/subagent)).
15. **SDK / Python** — Rust client is thin stdio wrapper; Python SDK has `Session.run()`, subagent notification merge, bundled runtime ([`python/sdk/README.md`](../../python/sdk/README.md), [`rust/crates/sdk/client/src/lib.rs`](../crates/sdk/client/src/lib.rs)).

### P2 — platform / GUI-adjacent

16. **Workflow worker thread** — in-process `return <json>` only; no JS worker isolation ([`packages/workflow/workflow-worker-thread`](../../packages/workflow/workflow-worker-thread), [`rust/crates/workflow/workflow/src/lib.rs`](../crates/workflow/workflow/src/lib.rs)).
17. **Sandbox macOS Seatbelt + Win32 token** — Linux bwrap/landlock only; Windows ACL runner without `CreateRestrictedToken` ([`packages/sandbox/sandbox-local`](../../packages/sandbox/sandbox-local), [`rust/crates/sandbox/sandbox-windows-acl/src/lib.rs`](../crates/sandbox/sandbox-windows-acl/src/lib.rs)).
18. **Terminal / LSP** — in-memory write history and stub initialize; no PTY or stdio LSP ([`packages/terminal/terminal-bash`](../../packages/terminal/terminal-bash), [`packages/lsp/lsp-stdio`](../../packages/lsp/lsp-stdio)).
19. **Host webserver / apiproxy** — minimal `/health` + `/rpc`; no GUI RPC surface, SSE, session export, settings plane ([`packages/host/apiproxy`](../../packages/host/apiproxy), [`rust/crates/host/webserver/src/lib.rs`](../crates/host/webserver/src/lib.rs)).
20. **llm-pi-ai** — referenced in Rust bundle patch but no Rust crate ([`packages/llm/llm-pi-ai`](../../packages/llm/llm-pi-ai), [`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml)).

---

## Focus-area deep dive

### 1. Session persistence

| Aspect | TypeScript | Rust |
|---|---|---|
| Write path | `PersistenceCoordinator` + per-live-session write-behind (`writeBatchMaxDelayMs`, mandatory flush at `turn/end` and dispose) | `save()` on backends; JSONL atomic rewrite; SQLite delete-all-events + reinsert |
| Repair | `commitRepair(meta, tornMarker, closers)` durable; cold `load` closes interrupted turns with synthetic tool results | In-memory `repair_open_turn` on inspect/load only; not durably committed |
| Prepare | `prepare(id)` reserves unpublished `Session`, LRU of `preparedSessionCacheSize` (config, default 5) | `inspect()` + internal `SessionPreparations` LRU; no public `prepare()` |
| Suffix read | `readFrom(id, fromSeq)`; SQLite `loadStoredFrom` | **Absent** |
| SQLite schema | `SCHEMA_VERSION = 17` packed layout ([`packages/session/session-persistence-sqlite/src/schema.ts`](../../packages/session/session-persistence-sqlite/src/schema.ts)) | `SCHEMA_VERSION = 2`, one JSON row per event ([`rust/crates/session/session-persistence-sqlite/src/lib.rs`](../crates/session/session-persistence-sqlite/src/lib.rs)) |

**Gaps:** P0 write-behind + live append batching; P0 `commitRepair` / torn-tail truncation; P1 `readFrom`; P1 config-driven `preparedSessionCacheSize` on plugin install; P1 cross-format SQLite interop.

### 2. LLM (+ agent-loop finish recovery)

| Aspect | TypeScript | Rust |
|---|---|---|
| DeepSeek transport | SSE streaming, finish-chunk errors, reasoning passback, vision Files API | Non-streaming POST, `stream: false`, text-only `blocks_text` ([`rust/crates/llm/llm-deepseek/src/lib.rs`](../crates/llm/llm-deepseek/src/lib.rs) L310–314) |
| llm-pi-ai | Multi-provider twin | **No crate**; dormant in bundle patch only |
| Finish-chunk errors | Loop ends turn on `finish { kind: error \| aborted }` ([`packages/core/agent-loop/src/agent.ts`](../../packages/core/agent-loop/src/agent.ts)) | Stream loop logs all chunks; only `MaxTokens` finish affects turn end ([`rust/crates/core/agent-loop/src/lib.rs`](../crates/core/agent-loop/src/lib.rs) L391–430). HTTP failures use `agent/request-error` waterfall instead |

**Gaps:** P0 SSE + streaming semantics; P0 image input blocks; P1 llm-pi-ai; P1 EMPTY_RESPONSE / unknown finish_reason as finish chunks (adapter + loop parity — **report loop gap; do not change agent-loop in gap-closure task**).

### 3. Workflow + ralph

| Aspect | TypeScript | Rust |
|---|---|---|
| Engine | `workflow-worker-thread`: Node worker, `agent()` / `parallel()` / host protocol | `workflow-local`: in-process script eval, `return <json>` ([`rust/crates/workflow/workflow/src/lib.rs`](../crates/workflow/workflow/src/lib.rs)) |
| Ralph | JS workflow over worker engine | Rust loop over `ctx.subagents` ([`rust/crates/workflow/tool-ralph/src/lib.rs`](../crates/workflow/tool-ralph/src/lib.rs)) — functionally similar, different isolation |

**Gaps:** P2 JS worker isolation and cancellation via `worker.terminate()`; P2 workflow observer events parity under load.

### 4. Web

| Aspect | TypeScript | Rust |
|---|---|---|
| Fetch tool | `web-fetch-http` registered; configurable | `web-fetch-http` crate exists; **base bundle `fetch: false`** ([`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml) L417) |
| Search providers | DeepSeek, Exa, Perplexity | DeepSeek (+ replay fixture) only |
| Live vs replay | Real API + test replay | `web-search-deepseek` supports `replay` config for keyless runs ([`rust/crates/web/web-search-deepseek/src/lib.rs`](../crates/web/web-search-deepseek/src/lib.rs)) |

**Gaps:** P1 enable fetch in headless when product needs URL retrieval; P2 Exa/Perplexity; P1 live search without replay fixture for CI snapshots.

### 5. Skill watching

TypeScript uses Chokidar with `watchPollIntervalMs` for missing roots ([`packages/skill/skill-filesystem/README.md`](../../packages/skill/skill-filesystem/README.md)). Rust rescans on `agent/pre-step` and skill-path `fs/observed` ([`rust/crates/skill/skill-filesystem/src/lib.rs`](../crates/skill/skill-filesystem/src/lib.rs) L5–7, L237–245).

**Gaps:** P1 native directory watch / poll-until-root; P2 catalog refresh latency vs IDE save.

### 6. Attachment

TypeScript: Sharp normalization (sRGB, dimension cap, format ladder), request-image cache ([`packages/attachment/attachment-local/README.md`](../../packages/attachment/attachment-local/README.md)). Rust: magic-byte verify + raw store ([`rust/crates/attachment/attachment-local/src/lib.rs`](../crates/attachment/attachment-local/src/lib.rs) L4–5).

**Gaps:** P0 full raster decode/normalize; P0 request-image projection for vision routes; P1 `imageCompressionConcurrency`.

### 7. Terminal + LSP

| Package | TypeScript | Rust |
|---|---|---|
| terminal | `terminal-bash` / node-pty backends | `TerminalRuntime`: in-memory write history ([`rust/crates/terminal/terminal/src/lib.rs`](../crates/terminal/terminal/src/lib.rs)) |
| lsp | `lsp-stdio` real language server process | `LspRuntime`: records initialize, returns static capabilities ([`rust/crates/lsp/lsp/src/lib.rs`](../crates/lsp/lsp/src/lib.rs)) |

**Gaps:** P2 real PTY and LSP stdio (headless spine does not require them today).

### 8. Sandbox

| Platform | TypeScript | Rust |
|---|---|---|
| Linux | bwrap, landlock-run, seatbelt on macOS | bwrap + landlock-run ([`rust/crates/sandbox/sandbox-local/src/lib.rs`](../crates/sandbox/sandbox-local/src/lib.rs)); **no `sandbox-exec`** |
| Windows | `CreateRestrictedToken` via koffi ([`packages/sandbox/sandbox-windows-acl/src/token.ts`](../../packages/sandbox/sandbox-windows-acl/src/token.ts)) | Node runner prefix only; **no token APIs** ([`rust/crates/sandbox/sandbox-windows-acl/src/lib.rs`](../crates/sandbox/sandbox-windows-acl/src/lib.rs)) |

**Gaps:** P2 Seatbelt; P2 Win32 restricted token (Rust documents Wine/Linux cannot prove NTFS/DACL).

### 9. OTEL

TypeScript: OTLP exporter with documented `forceFlush` refusal on concurrent flush ([`packages/session/session-telemetry-otel/src/index.ts`](../../packages/session/session-telemetry-otel/src/index.ts)). Rust matches log export + retry policy; explicitly no flush hint and no metrics ([`rust/crates/session/session-telemetry-otel/src/lib.rs`](../crates/session/session-telemetry-otel/src/lib.rs) L12–15).

**Gaps:** P1 seam `flush` hint behavior parity; P2 metrics instruments.

### 10. ACP

TypeScript: image prompts when attachments + vision route ([`packages/acp/acp/README.md`](../../packages/acp/acp/README.md)). Rust: `promptCapabilities.image: false` ([`rust/crates/acp/acp/src/lib.rs`](../crates/acp/acp/src/lib.rs)).

**Gaps:** P0 image prompts (blocked on attachment normalization); P1 resume/list/fork (both mark fresh-session-only).

### 11. SDK (Python vs Rust)

Python: high-level `DeepSeekHarness.run()`, `Session.run()` activity boundary, descendant notifications, bundled `dsh-jsonrpc-agent` ([`python/sdk/README.md`](../../python/sdk/README.md)). Rust: `JsonRpcClient` with `initialize` / `prompt` / `shutdown` and notification drain ([`rust/crates/sdk/client/src/lib.rs`](../crates/sdk/client/src/lib.rs)).

**Gaps:** P1 Rust/Python client feature parity (session helpers, turn boundaries); protocol server in Rust is adequate for stdio spine.

### 12. Host webserver / apiproxy

TypeScript apiproxy: full GUI RPC (sessions, settings, credentials, search, export ZIP, …) ([`packages/host/apiproxy/README.md`](../../packages/host/apiproxy/README.md)). Rust: `WebServer` serves `/health` + POST `/rpc` without SSE ([`rust/crates/host/webserver/src/lib.rs`](../crates/host/webserver/src/lib.rs)); `ApiProxy` is HTTP POST forward only ([`rust/crates/host/apiproxy/src/lib.rs`](../crates/host/apiproxy/src/lib.rs)).

**Gaps:** P2 entire web-app carrier (intentionally out of headless scope).

### 13. Compaction

Rust `compaction-basic` implements pressure, overflow, summarization stream, tool-result pruner hook ([`rust/crates/compaction/compaction-basic/src/lib.rs`](../crates/compaction/compaction-basic/src/lib.rs)). Aligns with TS README contract; same known limitations (heuristic meter, indivisible overflow).

**Gaps:** P1 verify KV-cache replay byte parity on summarization route; no missing major capability identified.

### 14. Subagent

TypeScript: spawn/fork/driver packages, ACP/Codex/Claude providers, continuable background, projection-backed `list_agents` ([`packages/subagent/subagent/README.md`](../../packages/subagent/subagent/README.md)). Rust: `subagent-inprocess` merged spawn+fork ([`rust/crates/subagent/subagent-inprocess/src/lib.rs`](../crates/subagent/subagent-inprocess/src/lib.rs)); continuable path exists per [`rust/README.md`](../README.md).

**Gaps:** P1 external subagent providers (ACP/Codex/Claude); P1 `subagent-in-process-driver` wake latch parity; P2 process-local residency limits.

### 15. Settings

TypeScript Service Definition: `register`, `watch`, `revision`, `mutate`, `describe` ([`packages/settings/settings/README.md`](../../packages/settings/settings/README.md)). Rust: `settings-file` exposes `section()`, `document()`, debounced reload, YAML patch `update` ([`rust/crates/settings/settings-file/src/lib.rs`](../crates/settings/settings-file/src/lib.rs)).

**Gaps:** P0 typed namespace registration and revision/conflict protocol for adapters that call `ctx.settings.register`; P1 watcher events (`settings/updated`).

### 16. Session-query + FTS

Default Rust headless: `openAt: never` → `SESSION_QUERY_SEARCH_DISABLED` ([`rust/crates/session-query/session-query/src/lib.rs`](../crates/session-query/session-query/src/lib.rs) L154–158). SQLite FTS schema v1 vs TS v8. Rust omits `filterSessions`, `readSurface`, `traceEvent`, conflict codes, cancellation on batch title reads.

**Gaps:** P1 opt-in FTS (`openAt: startup`); P1 full read/trace API; P2 GUI `session.search` gateway.

### 17. Plan mode + user-questions

Plan tool and logged `plan/mode` exist. Headless has `user-questions` service but no provider ([`rust/crates/interaction/user-questions/src/lib.rs`](../crates/interaction/user-questions/src/lib.rs)); plan review fails with exact sentence ([`rust/crates/plan/plan-mode/src/lib.rs`](../crates/plan/plan-mode/src/lib.rs) L6–8).

**Gaps:** P0 automation provider or explicit plan-exit bypass for headless CI; P2 Web `plan-review` renderer.

### 18. Filesystem observation

Rust mounts `fs-observation-policy` with event gate ([`rust/crates/fs/fs-observation-policy/src/lib.rs`](../crates/fs/fs-observation-policy/src/lib.rs)). Same limitations as TS: observation does not survive resume ([`packages/fs/fs-observation-policy/README.md`](../../packages/fs/fs-observation-policy/README.md)); no directory watch beyond skill rescan triggers.

**Gaps:** P1 durable observation across resume (both defer); P1 repo-wide fs watch for context plugins (TS also lacks unified watch — skill-specific only).

---

## Per-crate matrix

Format: **TS contract** → **Rust shipped** → **Gaps**.

### Session group

#### `session-persistence`
- **TS:** Full `ctx.sessionPersistence` with coordinator, write-behind, `prepare`/`load`/`inspect`/`readFrom`/`listSnapshots`.
- **Rust:** Slimmer `PersistenceRuntime`: `save`, `load`, `inspect`, list/revision helpers ([`rust/crates/session/session-persistence/src/lib.rs`](../crates/session/session-persistence/src/lib.rs)).
- **Gaps:** P0 coordinator + write-behind; P0 `append`/`create`/`prepare`/`readFrom`/`readRaw`; P1 `commitRepair`.

#### `session-persistence-sqlite`
- **TS:** Append-only store, schema 17, seek reads, repair transactions.
- **Rust:** Schema 2, full replace on `save` ([`rust/crates/session/session-persistence-sqlite/src/lib.rs`](../crates/session/session-persistence-sqlite/src/lib.rs) L161–173).
- **Gaps:** P0 append-only durability; P1 schema 17 interop.

#### `session-persistence-jsonl`
- **TS:** Byte-offset torn tail + coordinator hooks.
- **Rust:** Atomic full-file rewrite; inspect repair in memory ([`rust/crates/session/session-persistence-jsonl/src/lib.rs`](../crates/session/session-persistence-jsonl/src/lib.rs)).
- **Gaps:** P0 torn-tail `commitRepair`; P1 incremental append.

#### `session-projection`
- **TS:** Projection registry + units.
- **Rust:** Registry present ([`rust/crates/session/session-projection`](../crates/session/session-projection)).
- **Gaps:** P1 without `session-projection-cache` cold ladder (TS companion package absent in Rust).

#### `session-projection-cache`
- **TS:** Durable projection checkpoints, throttled write-behind.
- **Rust:** **No crate.**
- **Gaps:** P1 entire package.

#### `session-checkpoint-policy`
- **TS:** Flush before model request and tool dispatch.
- **Rust:** Installed in headless bundle ([`rust/crates/session/session-checkpoint-policy`](../crates/session/session-checkpoint-policy)).
- **Gaps:** None significant (verify e2e with new persistence write path).

#### `session-telemetry-otel`
- **TS:** OTLP logs, flush semantics documented.
- **Rust:** OTLP logs; no flush hint / metrics ([`rust/crates/session/session-telemetry-otel/src/lib.rs`](../crates/session/session-telemetry-otel/src/lib.rs)).
- **Gaps:** P1 flush; P2 metrics.

#### `session-title` / `session-title-first-prompt-llm`
- **TS/Rust:** Both implement fallback + first-prompt LLM path per [`rust/README.md`](../README.md).
- **Gaps:** P2 minor byte/limit parity audits only.

### Core group

#### `session` (core)
- **TS:** Live store, fork, surface fold, crash repair vocabulary.
- **Rust:** Ported session model ([`rust/crates/core/session`](../crates/core/session)).
- **Gaps:** P1 fork/resume edge cases tied to persistence gaps.

#### `agent-loop`
- **TS:** Turn driver, parallel tools, finish-error turns, wake latch.
- **Rust:** Turn driver; missing in-stream finish-error handling ([`rust/crates/core/agent-loop/src/lib.rs`](../crates/core/agent-loop/src/lib.rs)).
- **Gaps:** P0 finish-chunk error turn end (**report only**); P1 wake latch (#1838 class).

#### `agent` / `tools` / `system-prompt` / `scope`
- **TS/Rust:** Headless spine mounted.
- **Gaps:** P2 presentation/UI-only consumers.

### LLM group

#### `llm`
- **TS:** Stream chunk protocol, adapter registry, model info.
- **Rust:** Matching chunk types including `FinishReason::Error` ([`rust/crates/llm/llm/src/lib.rs`](../crates/llm/llm/src/lib.rs) L905–921).
- **Gaps:** P0 consumers must honor error finishes (loop gap above).

#### `llm-deepseek`
- **TS:** SSE, vision, reasoning, error taxonomy.
- **Rust:** Blocking JSON, text-only ([`rust/crates/llm/llm-deepseek/src/lib.rs`](../crates/llm/llm-deepseek/src/lib.rs)).
- **Gaps:** P0 streaming; P0 images; P1 reasoning passback; P1 tool schema on wire.

#### `llm-replay` / `llm-retry` / `token-meter`
- **TS/Rust:** Present for tests and retry waterfall.
- **Gaps:** P2 pi-ai replay scenarios N/A until pi-ai crate exists.

#### `llm-pi-ai`
- **TS:** Multi-provider adapter.
- **Rust:** **No crate** (bundle reference only).
- **Gaps:** P2 entire package.

### Workflow group

#### `workflow` + `workflow-local`
- **TS:** Worker-thread engine default.
- **Rust:** In-process `return <json>` ([`rust/crates/workflow/workflow/src/lib.rs`](../crates/workflow/workflow/src/lib.rs)).
- **Gaps:** P2 worker-thread isolation.

#### `tool-workflow` / `tool-ralph`
- **TS/Rust:** Tool surfaces; Ralph loop semantics aligned ([`rust/crates/workflow/tool-ralph/src/lib.rs`](../crates/workflow/tool-ralph/src/lib.rs)).
- **Gaps:** P2 under worker-based engine only.

### Web group

#### `web`
- **TS:** Provider registry, ambiguous/unavailable errors.
- **Rust:** Same selection logic ([`rust/crates/web/web/src/lib.rs`](../crates/web/web/src/lib.rs)).
- **Gaps:** P2 observation surface (both defer per TS README).

#### `web-fetch-http`
- **TS:** Production fetch provider.
- **Rust:** Crate present; disabled in default bundle.
- **Gaps:** P1 mount + enable in profiles needing fetch.

#### `web-search-deepseek`
- **TS:** Live search API.
- **Rust:** Live + `replay` fixture ([`rust/crates/web/web-search-deepseek/src/lib.rs`](../crates/web/web-search-deepseek/src/lib.rs)).
- **Gaps:** P1 keyless CI without replay config.

#### `web-search-exa` / `web-search-perplexity`
- **TS:** Optional providers.
- **Rust:** **No crates.**
- **Gaps:** P2 providers.

#### `tool-web`
- **TS:** `web_search` + optional `web_fetch`.
- **Rust:** Same; `fetch: false` in base patch ([`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml)).
- **Gaps:** P1 fetch enablement.

### Skill group

#### `skill` / `tool-skill`
- **TS/Rust:** Catalog tool + messages parity.
- **Gaps:** P1 catalog refresh tied to filesystem watching.

#### `skill-filesystem`
- **TS:** Chokidar + poll for missing roots.
- **Rust:** Event-driven rescan only ([`rust/crates/skill/skill-filesystem/src/lib.rs`](../crates/skill/skill-filesystem/src/lib.rs)).
- **Gaps:** P1 watch/poll.

### Attachment group

#### `attachment` / `attachment-local`
- **TS:** Full normalize + request images.
- **Rust:** Store + header verify only ([`rust/crates/attachment/attachment-local/src/lib.rs`](../crates/attachment/attachment-local/src/lib.rs)).
- **Gaps:** P0 normalization pipeline.

### Terminal / LSP

#### `terminal` / `terminal-local` / `tool-terminal`
- **TS:** PTY backends.
- **Rust:** Stub history ([`rust/crates/terminal/terminal/src/lib.rs`](../crates/terminal/terminal/src/lib.rs)).
- **Gaps:** P2 PTY.

#### `lsp` / `lsp-stdio` / `tool-lsp`
- **TS:** Stdio language server.
- **Rust:** Stub ([`rust/crates/lsp/lsp/src/lib.rs`](../crates/lsp/lsp/src/lib.rs)).
- **Gaps:** P2 real LSP.

### Sandbox group

#### `sandbox` / `sandbox-local` / `sandbox-policy`
- **TS:** Linux + macOS + Windows paths.
- **Rust:** Linux confiners; classification includes seatbelt rules unused ([`rust/crates/sandbox/sandbox/src/classify.rs`](../crates/sandbox/sandbox/src/classify.rs)).
- **Gaps:** P2 macOS Seatbelt execution.

#### `sandbox-windows-acl`
- **TS:** `CreateRestrictedToken` enforcement.
- **Rust:** Runner argv only ([`rust/crates/sandbox/sandbox-windows-acl/src/lib.rs`](../crates/sandbox/sandbox-windows-acl/src/lib.rs)).
- **Gaps:** P2 token APIs.

### Subagent group

#### `subagent`
- **TS:** Continuation manager, projections, multi-provider.
- **Rust:** Core runtime + continuable path ([`rust/README.md`](../README.md)).
- **Gaps:** P1 external providers; P1 projection cache integration.

#### `subagent-inprocess`
- **TS:** Split spawn/fork/driver packages.
- **Rust:** Combined provider ([`rust/crates/subagent/subagent-inprocess/src/lib.rs`](../crates/subagent/subagent-inprocess/src/lib.rs)).
- **Gaps:** P1 driver wake semantics; P2 package split.

#### `tool-subagent` / `tool-subagent-control` / `tool-subagent-report`
- **TS/Rust:** Tools mounted in headless.
- **Gaps:** P1 cold child inspect without `commitRepair` (Rust claims parity — verify with persistence gaps).

### Compaction group

#### `compaction` / `compaction-basic` / `tool-result-pruner` / `command-compact`
- **TS/Rust:** Pressure, overflow, pruner, `/compact`.
- **Gaps:** P1 regression tests after LLM streaming lands.

### Settings

#### `settings-file`
- **TS:** Provider for `ctx.settings` service.
- **Rust:** Standalone `SettingsRuntime` on `ctx.settings` key without register/watch API ([`rust/crates/settings/settings-file/src/lib.rs`](../crates/settings/settings-file/src/lib.rs)).
- **Gaps:** P0 Service Definition parity.

### Session-query group

#### `session-query`
- **TS:** Full read/filter/trace/search contract.
- **Rust:** Subset ([`rust/crates/session-query/session-query/src/lib.rs`](../crates/session-query/session-query/src/lib.rs)).
- **Gaps:** P1 API surface; P1 lineage from headers.

#### `session-query-sqlite`
- **TS:** FTS schema v8, startup/first-search indexing.
- **Rust:** FTS schema v1; `openAt: never` default ([`rust/crates/session-query/session-query-sqlite/src/lib.rs`](../crates/session-query/session-query-sqlite/src/lib.rs)).
- **Gaps:** P1 schema + default indexing policy.

### Plan / interaction

#### `plan-mode`
- **TS:** Plan review via user-questions.
- **Rust:** Same; fails without provider ([`rust/crates/plan/plan-mode/src/lib.rs`](../crates/plan/plan-mode/src/lib.rs)).
- **Gaps:** P0 headless review path.

#### `user-questions` / `user-approval` / `permission-presets` / `commands`
- **TS/Rust:** Services present; providers UI-specific.
- **Gaps:** P0 provider for plan/ask in automation.

### FS group

#### `fs` / `fs-local` / `fs-sandbox` / `fs-observation-policy`
- **TS/Rust:** Observation gate + sandbox write/edit.
- **Gaps:** P1 observation durability (shared defer).

#### `tool-fs` / `tool-fs-search` / `tool-str-replace-editor`
- **TS/Rust:** Headless tools mounted (glob/grep via ripgrep per [`rust/README.md`](../README.md)).
- **Gaps:** P2 landlock e2e on agent-spine only.

### Host / SDK / ACP

#### `host/webserver`
- **TS:** Route registry, SPA fallback, upgrades.
- **Rust:** Minimal HTTP ([`rust/crates/host/webserver/src/lib.rs`](../crates/host/webserver/src/lib.rs)).
- **Gaps:** P2 GUI server.

#### `host/apiproxy`
- **TS:** Full BFF.
- **Rust:** JSON-RPC forward ([`rust/crates/host/apiproxy/src/lib.rs`](../crates/host/apiproxy/src/lib.rs)).
- **Gaps:** P2 GUI RPC.

#### `sdk/protocol` / `sdk/server` / `sdk/client`
- **TS/Python:** Rich client ergonomics.
- **Rust:** Protocol + stdio server + thin client.
- **Gaps:** P1 client helpers.

#### `acp`
- **TS:** Stdio ACP with optional images.
- **Rust:** Stdio ACP without images ([`rust/crates/acp/acp/src/lib.rs`](../crates/acp/acp/src/lib.rs)).
- **Gaps:** P0 images (after attachments).

### Shell / subprocess / jobs / spill / credentials / boot / bundle

These crates largely match headless contracts per [`rust/README.md`](../README.md).

| Crate | Gaps |
|---|---|
| `bash-local` / `bash-sandbox` / `tool-bash` | P2 Windows/macOS sandbox runners |
| `pwsh-local` / `pwsh-sandbox` / `tool-pwsh` | P2 non-win32 disabled via patch (expected) |
| `subprocess-local` | None significant |
| `jobs-local` / `tool-jobs` | None significant |
| `spill-*` | None significant |
| `credentials-local` | P1 Unix credential file mode parity (verify) |
| `boot/app-boot` | P1 load `preparedSessionCacheSize` from cordis config |
| `bundle/headless` / `base` | P1 patch flags (`fetch: false`, `openAt: never`) document product choices |

### Util crates (`brand`, `timeout`, `atomic-write`, `home-paths`)

- **TS/Rust:** Utility parity for headless.
- **Gaps:** None.

### Goal / guard / context / identity / feedback / todo / examples

- **TS/Rust:** Mounted in Rust headless ([`rust/README.md`](../README.md)).
- **Gaps:** P2 GUI-only feedback surfaces.

---

## Recommended closure order (dependency-aware)

1. **Session persistence coordinator** — unblocks correct resume, subagent cold inspect, projection cache later.
2. **LLM DeepSeek SSE + images** — unblocks vision, ACP images, accurate streaming errors from adapter.
3. **Attachment normalization** — prerequisite for vision and ACP.
4. **Settings Service Definition** — unblocks adapter/settings merges (`llm-deepseek`, `agent-default-model`) without ad hoc file reads.
5. **Agent-loop finish-chunk turn errors** — report in adapter work; loop change is separate PR (explicitly out of scope for adapter-only tasks).
6. **Session-query FTS opt-in** — ops/debugging value once persistence stable.
7. **Web fetch enable + live search policy** — product config decision.
8. **Skill watching** — developer UX.
9. **Platform sandbox (macOS/Windows)** — P2 unless targeting those headless platforms.

---

## Verification commands

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

Cross-check TS Known Limitations in `packages/**/README.md` and Rust crate headers under `rust/crates/**/src/lib.rs`.
