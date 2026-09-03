# TypeScript ↔ Rust functional gap (scoped packages)

Audit date: 2026-09-03. Scope: packages listed in the TS↔Rust porting brief that have **no Rust crate** or **only a no-op / marker mount** in `rust/crates/boot/app-boot/src/plugins.rs`. Evidence: TS `src/index.ts` entry contracts; Rust `rust/crates/**/Cargo.toml` inventory; `apply_named` match arms; shipped `cordis.patch.yml` rows in `packages/bundle/` and `rust/crates/bundle/`.

**Already ported (excluded from gap tables):** `@deepseek-ai/dsh-agent-default-model` (`dsh_agent::AgentDefaultModel`), `@deepseek-ai/dsh-subagent-spawn-in-process` / `@deepseek-ai/dsh-subagent-fork-in-process` (`dsh_subagent_inprocess`), `@deepseek-ai/dsh-agent-instructions` (`dsh_agent_instructions`), `@deepseek-ai/dsh-session-title-first-prompt-llm` (`dsh_session_title_first_prompt_llm`).

**Rust crates that exist but are not wired in `app-boot`:** `dsh-apiproxy`, `dsh-webserver` (minimal HTTP/JSON-RPC; not mounted by `apply_named` today).

---

## `app-boot` no-op / marker mounts (scoped plugin names)

| Plugin name | Mount behavior in `plugins.rs` | In shipped Rust base patch? |
| --- | --- | --- |
| `@deepseek-ai/cordis-plugin-hmr` | Explicit `Ok(())` — no service | yes (disabled in TS web patch) |
| `@deepseek-ai/dsh-typert-registry` | `_ => Ok(())` no-op | yes |
| `@deepseek-ai/dsh-typert-loader` | no-op | yes |
| `@deepseek-ai/dsh-api-gateway` | no-op | yes |
| `@deepseek-ai/dsh-llm-pi-ai` | no-op | yes |
| `@deepseek-ai/dsh-skill-badge` | no-op | yes (`disabled: true`) |
| `@deepseek-ai/dsh-code-runtime-worker-thread` | Marker only (`provide_marker::<CodeRuntime>`) | yes (headless/acp/jsonrpc/web TS patches) |

Web-surface plugin names in TS `dsh-web-app` patch (storage, workspace, apiproxy, client `ui-*`, etc.) are **not** in Rust bundle crates yet; Rust `profile_templates()` references `dsh-web-app` but only `dsh-base`, `dsh-headless`, `dsh-acp`, and `dsh-jsonrpc` exist under `rust/crates/bundle/`.

---

## Gap tables

### packages/api

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| api | `gateway` — `packages/api/gateway/src/index.ts` exports `TypertGateway` service: live Remote dispatch over Connection with codec/lookup policy | no-op mount (`dsh-api-gateway` in base patch) | No Typert RPC gateway; Remotes cannot invoke host services from browser/ACP | yes (Web, any Remote client) |
| api | `remotes` — `packages/api/remotes/src/index.ts` exports BFF shell, forwarded-event allowlist, `createApiRemoteAgentResolver` | absent | No Remote contribution assembly or compile-time event-shape gate for client bundles | yes (Web client half) |

### packages/typert

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| typert | `registry` — `packages/typert/registry/src/index.ts` default-exports `TypertRegistry`: schema/package registry on `ctx.typert` | no-op mount | No runtime type graph registry; generator output has nowhere to land at boot | headless-only (blocks Web/API) |
| typert | `loader` — `packages/typert/loader/src/index.ts` `apply()` scans Loader entries for `./typert` exports and registers manifests | no-op mount | Plugins' Typert artifacts never auto-register on mount/HMR | headless-only |
| typert | `protocol` — `packages/typert/protocol/src/index.ts` Remote decorators, `TypertGatewayBinding`, lookup failure types | absent (library) | No shared Remote/Gateway protocol types or decorators for Rust codegen | headless-only |
| typert | `generator` — `packages/typert/generator/src/index.ts` `WorkspaceTypertGenerator` / `FaceModelEmitter` build-time analyzer | absent | No Rust Typert artifact pipeline (build gate only in TS) | no |

### packages/mcp

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| mcp | `mcp-client` — `packages/mcp/mcp-client/src/index.ts` `apply()` connects MCP servers and registers `mcp__*` tools on `ctx.tools` | absent | External MCP tool namespaces unavailable | yes (when composed) |

### packages/schedule

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| schedule | `schedule` — `packages/schedule/schedule/src/index.ts` `apply()` installs `ScheduleRuntime` + reminder tools over session log | absent | No durable agent-scoped reminders (`after`/`at`/`every`) | yes (example `web-schedule`) |

### packages/storage

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| storage | `storage` — `packages/storage/storage/src/index.ts` `Storage` hub: named backend registry on `ctx.storage` | absent (TS web patch only) | No generic storage hub for domain plugins | yes (Web: feedback, workspace, projection cache) |
| storage | `storage-domain` — `packages/storage/storage-domain/src/index.ts` `apply()` mounts validated KV domains with change events | absent | No domain data-form layer (`ctx.storage.domain`) | yes |
| storage | `storage-json` — `packages/storage/storage-json/src/index.ts` registers JSON file backend | absent | No JSON backend for `$DSH_HOME/storages` | yes |
| storage | `storage-sqlite` — `packages/storage/storage-sqlite/src/index.ts` registers SQLite backend | absent | No SQLite storage backend (distinct from session-query SQLite) | headless-only |

### packages/workspace

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| workspace | `workspace` — `packages/workspace/workspace/src/index.ts` `WorkspaceRegistry` service: durable workspace records + session membership | absent (TS web patch) | No multi-workspace registry or sidebar workspace model | yes |

### packages/preset

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| preset | `agent-presets` — `packages/preset/agent-presets/src/index.ts` standing preset mounts, discovery, scope join on agent create | absent (TS web patch) | Web sessions cannot compose per-preset tool/prompt planes | yes |
| preset | `persona` — `packages/preset/persona/src/index.ts` scope-only persona row shadowing deployment persona | absent | Presets cannot override agent identity text | yes |

### packages/code-runtime

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| code-runtime | `code-runtime` — `packages/code-runtime/code-runtime/src/index.ts` `CodeRuntime` Service Definition (`run` seam) | absent | No code-execution capability interface | yes (Code Mode) |
| code-runtime | `code-runtime-python` — `packages/code-runtime/code-runtime-python/src/index.ts` CPython subprocess wire protocol | absent | No Python code backend | yes (when composed) |
| code-runtime | `code-runtime-worker-thread` — `packages/code-runtime/code-runtime-worker-thread/src/index.ts` `apply()` registers worker-thread `CodeRuntime` provider | no-op marker only | Code Mode tools cannot execute TS programs (marker service only) | yes |

### packages/hooks

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| hooks | `hook-protocol` — `packages/hooks/hook-protocol/src/index.ts` matcher, runner, merge, durable hook events (library) | absent | No shared hook execution library | headless-only |
| hooks | `hooks-claude-code` — `packages/hooks/hooks-claude-code/src/index.ts` Claude Code hook bridge on agent/tool extension points | absent | Claude Code hooks cannot run | headless-only (ACP example) |
| hooks | `hooks-codex` — `packages/hooks/hooks-codex/src/index.ts` Codex hook bridge (regex-only, blocking decisions) | absent | Codex hooks cannot run | headless-only |

### packages/extensions

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| extensions | `tool-cordis` — `packages/extensions/tool-cordis/src/index.ts` model tools to inspect/define/run dynamic Cordis plugins | absent | No self-modification / dynamic-plugin tools | yes (examples) |
| extensions | `ui-cordis` — `packages/extensions/ui-cordis/src/index.ts` empty host `apply()`; browser card via `./client` | absent | No dynamic-plugin UI surface | yes |
| extensions | `cordis-host-runner` — `packages/extensions/cordis-host-runner/src/index.ts` `DynamicCordisRegistry` + host sandbox/eval | absent (TS web patch) | No host-side dynamic Cordis package lifecycle | yes |
| extensions | `cordis-client-runner` — `packages/extensions/cordis-client-runner/src/index.ts` empty host `apply()`; browser runner | absent | No client-side dynamic plugin activation | yes |

### packages/e2b

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| e2b | `e2b` — `packages/e2b/e2b/src/index.ts` shared E2B sandbox ownership service | absent | No remote sandbox capability | headless-only (E2B examples) |
| e2b | `fs-e2b` — `packages/e2b/fs-e2b/src/index.ts` E2B filesystem provider for `ctx.fs` | absent | No cloud FS adapter | headless-only |
| e2b | `subprocess-e2b` — `packages/e2b/subprocess-e2b/src/index.ts` E2B subprocess provider | absent | No cloud process adapter | headless-only |

### packages/experimental

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| experimental | `agent-team` — `packages/experimental/agent-team/src/index.ts` `AgentTeams` service: roster, mailbox, tasks | absent | No multi-agent team runtime | headless-only |
| experimental | `tool-agent-team` — `packages/experimental/tool-agent-team/src/index.ts` scoped team collaboration tools | absent | No team tools on `ctx.tools` | headless-only |

### packages/client (Web UI half — summary)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| client | **Web UI stack** — `packages/client/runtime/src/index.ts` empty host stub; **`dsh-client-runtime`** + **`dsh-client-modules`** boot chain; **30 `ui-*` plugins** in `packages/bundle/web-app/cordis.patch.yml` roster (+ **`ui-primitives`**, **`ui-slots`** as static platform modules); plus **`connection`**, **`locale`**, **`hmr`**, **`cordis-client-runner`**, **`api-remotes`** | absent (entire web bundle unported) | No browser SPA, slots, session UI, settings UI, or SSE transport — Rust `web` profile references missing `dsh-web-app` bundle | yes |

### packages/host (remaining)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| host | `frontend-static` — `packages/host/frontend-static/src/index.ts` `apply()` serves built SPA via webserver fallback seat | absent | Cannot serve frontend static assets | yes |
| host | `plugin-inventory` — `packages/host/plugin-inventory/src/index.ts` Remote snapshot of Loader plugin entries | absent (TS web patch) | Settings/plugin UI cannot list live composition | yes |
| host | `directory-picker` — `packages/host/directory-picker/src/index.ts` `DirectoryPicker` Service Definition (native vs browse capability union) | absent | No workspace directory-picking seam | yes |
| host | `directory-picker-native` — `packages/host/directory-picker-native/src/index.ts` OS chooser backend | absent | No native folder dialog | yes |
| host | `directory-picker-browse` — `packages/host/directory-picker-browse/src/index.ts` in-app filesystem browser backend | absent | Remote clients cannot browse host paths | yes |
| host | `directory-picker-auto` — `packages/host/directory-picker-auto/src/index.ts` boot probe + mounts native or browse pair | absent (TS web patch) | Web cannot adapt picker backend to environment | yes |

Note: TS Web patch mounts `@deepseek-ai/dsh-host-apiproxy` (full BFF). Rust has **`dsh-apiproxy`** crate (minimal HTTP forwarder) and **`dsh-webserver`** crate, neither wired in `app-boot`.

### packages/bundle/web-app

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| bundle | `web-app` — `packages/bundle/web-app/src/index.ts` web glue: dist resolution, `frontend-static`, prompt sections, URL line, browser open | absent (no `rust/crates/bundle/web-app`) | Rust `web` profile cannot boot browser surface | yes |

### packages/boot/cmdline

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| boot | `cmdline` — `packages/boot/cmdline/src/index.ts` `CmdlineArgs` service + `parseCmdline()` for app-owned flags | absent | Headless/web startup plugins cannot share launcher argv parsing contract (Rust inlines task in `HeadlessStartup` config) | headless-only |

### packages/feedback

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| feedback | `message-feedback` — `packages/feedback/message-feedback/src/index.ts` durable per-message like/dislike + notes on storage domain | absent (TS web patch) | No message feedback persistence or Remote API (`command-feedback` exists in Rust) | yes |

### packages/context

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| context | `file-reference` — `packages/context/file-reference/src/index.ts` `FileReferenceService` Remote: `@` path candidate discovery | absent | Composer `@file` discovery unavailable | yes |
| context | `file-reference-local` — `packages/context/file-reference-local/src/index.ts` local search impl + system-prompt section | absent (TS web patch) | No workspace file search for `@` mentions | yes |
| context | `session-reference` — `packages/context/session-reference/src/index.ts` cross-session snapshot prep + Remote | absent (TS web patch) | No `@session` reference injection | yes |
| context | `time-context` — `packages/context/time-context/src/index.ts` opt-in request clock messages on pre-step | absent | Model lacks timestamp/timezone context | headless-only (examples) |
| context | `tmux-context` — `packages/context/tmux-context/src/index.ts` tmux pane layout via `ctx.shell` on step 1 | absent | No tmux location context for terminal workflows | headless-only |

### packages/session (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| session | `session-stats` — `packages/session/session-stats/src/index.ts` registers `sessionStats` projection unit (turn/step/timing fold) | absent (TS web patch) | Chat stats strip lacks whole-log metrics | yes |
| session | `session-projection-cache` — `packages/session/session-projection-cache/src/index.ts` `SessionProjectionCache` durable checkpoint store | absent (TS web patch) | Cold session reads replay full projection tails | yes |
| session | `session-title-llm` — `packages/session/session-title-llm/src/index.ts` shared LLM title route/framing library (not a plugin) | partial (logic lives in `session-title-first-prompt-llm` crate only) | Shared title-LLM policy module not reusable for alternate providers | headless-only |
| session | `session-title-all-prompts-llm` — `packages/session/session-title-all-prompts-llm/src/index.ts` registers all-human-messages title provider | absent | Only first-prompt LLM titles in Rust; no all-prompts variant | headless-only |

### packages/session-query (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| session-query | `session-log-export` — `packages/session-query/session-log-export/src/index.ts` registers Web `/export` command | absent (TS web patch) | No session log ZIP download trigger | yes |
| session-query | `tool-session-query` — `packages/session-query/tool-session-query/src/index.ts` model tools for authorized session search/read | absent | Model cannot search/read other sessions via tools | yes (when search enabled) |

### packages/subagent (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| subagent | `subagent-acp` — `packages/subagent/subagent-acp/src/index.ts` out-of-process ACP child provider | absent | No ACP-backed subagents | headless-only |
| subagent | `subagent-claude-code` — `packages/subagent/subagent-claude-code/src/index.ts` Claude Code SDK one-shot provider | absent | No Claude Code delegation backend | headless-only |
| subagent | `subagent-codex` — `packages/subagent/subagent-codex/src/index.ts` Codex app-server one-shot provider | absent | No Codex delegation backend | headless-only |
| subagent | `subagent-dsh-sdk` — `packages/subagent/subagent-dsh-sdk/src/index.ts` out-of-process harness JSON-RPC child | absent | No nested full-runtime subagents | headless-only |
| subagent | `subagent-in-process-driver` — `packages/subagent/subagent-in-process-driver/src/index.ts` `startInProcessRun()` shared one-shot driver (library) | absorbed (`dsh_subagent_inprocess` implements providers; no separate crate name) | Library surface not exported for third-party in-process providers | headless-only |

### packages/web (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| web | `web-search-exa` — `packages/web/web-search-exa/src/index.ts` registers Exa `WebSearchProvider` | absent | No Exa search route | yes (when configured) |
| web | `web-search-perplexity` — `packages/web/web-search-perplexity/src/index.ts` registers Perplexity search provider | absent | No Perplexity search route | yes (when configured) |

### packages/llm

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| llm | `llm-pi-ai` — `packages/llm/llm-pi-ai/src/index.ts` multi-provider pi-ai adapter registering routes into `ctx.llm` | no-op mount | Models page / multi-provider chat unavailable (only DeepSeek + replay in Rust) | yes |

### packages/settings

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| settings | `settings` — `packages/settings/settings/src/index.ts` full `Settings` Service Definition: namespace registration, schema merge, live/restart applies, redaction | partial (`dsh-settings-file` only) | Rust lacks namespace schema registration, composed defaults, `installSettingsSection`, and live plugin reactions | yes |

### packages/core (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| core | `agent-tool-presentation` — `packages/core/agent-tool-presentation/src/index.ts` preset row calling `ctx.tools.presentAs()` for native/code/both | absent | Code Mode per-preset tool presentation unavailable (Web uses `DSH_TOOLS_MODE` env workaround) | yes |

### packages/interaction

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| interaction | `tool-ask-user` — `packages/interaction/tool-ask-user/src/index.ts` registers `ask_user_question` tool on `ctx.userQuestions` | absent | Model cannot block on structured user Q&A (`user-questions` service exists without tool consumer) | yes |

### packages/shell

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| shell | `tool-bash-persistent` — `packages/shell/tool-bash-persistent/src/index.ts` persistent bash tool over PTY seam | absent | No stateful bash sessions across tool calls | yes (when composed) |
| shell | `tool-pwsh-persistent` — `packages/shell/tool-pwsh-persistent/src/index.ts` persistent pwsh tool over PTY seam | absent | No stateful PowerShell sessions | yes (when composed) |

### packages/terminal

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| terminal | `terminal-bash` — `packages/terminal/terminal-bash/src/index.ts` PTY backend plugin for `ctx.terminals` with sandbox policy | absent (`dsh-terminal-local` is in-memory stub, not wired in `app-boot`) | Persistent shell tools lack real PTY/subprocess backend | yes (when composed) |

### packages/skill

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| skill | `skill-badge` — `packages/skill/skill-badge/src/index.ts` bundled `dsh-badge` skill provider | no-op mount (disabled in base) | Official badge skill content not registered | yes (when enabled) |

### packages/util (extras)

| Group | TS package | Rust status | Functional gap | Product-visible? |
| --- | --- | --- | --- | --- |
| util | `launch-environment` — `packages/util/launch-environment/src/index.ts` layered env snapshot (`process` / project / user `.env`) | absent (library) | Providers cannot resolve credentials/endpoints through launch-layer precedence | headless-only |
| util | `native-command` — `packages/util/native-command/src/index.ts` `runNativeCommand` no-shell execFile helper | absent (library) | Host native integrations (picker, browser open) lack shared runner | headless-only |
| util | `output-retention` — `packages/util/output-retention/src/index.ts` `ItemRetainer` / `TextRetainer` bounded output helpers | absent (library) | Tools must hand-roll truncation metadata | headless-only |

---

## Priority tiers

Priorities reflect **dependency order** and **shipped composition impact**. Counts are scoped packages above (~70 line items).

### P0 — Unblocks any browser/Web profile boot

1. **Typert spine** (`protocol` library → `registry` → `loader` → `generator` build hook): without it, Remotes, apiproxy, and client codegen cannot compile or dispatch.
2. **API surface** (`remotes`, full `host/apiproxy` BFF, wire `dsh-webserver`): Web transport stops at missing gateway.
3. **`bundle/web-app` patch crate** + **`boot/cmdline`**: Rust `web` profile currently references a non-existent bundle.
4. **Client stack** (`runtime`, `modules`, `connection`, 30× `ui-*`): entire product UI.

### P1 — Shipped base-patch no-ops that affect every profile

1. **`llm-pi-ai`** (multi-provider models).
2. **`settings` Service Definition** completeness atop existing `settings-file`.
3. **`code-runtime` + `code-runtime-worker-thread`** (replace marker with real worker runtime).
4. **`agent-tool-presentation`** (remove `DSH_TOOLS_MODE` env shim).
5. **`tool-ask-user`** (complete `user-questions` seam).

### P2 — Shipped Web host rows (TS `dsh-web-app` patch)

1. **Storage stack** (`storage`, `storage-json`, `storage-domain`).
2. **Workspace + agent-presets + persona**.
3. **Session UX** (`session-projection-cache`, `session-stats`, `session-reference`, `file-reference` + local, `session-log-export`, `message-feedback`).
4. **Host chrome** (`frontend-static`, `plugin-inventory`, `directory-picker*`).
5. **`cordis-host-runner` + `tool-cordis` + client runners** (dynamic plugins).

### P3 — Agent capability extensions (opt-in / examples)

1. **`schedule`**, **`time-context`**, **`tmux-context`**.
2. **`tool-session-query`**, extra **web search providers** (Exa, Perplexity).
3. **Persistent shell** (`terminal-bash`, `tool-bash-persistent`, `tool-pwsh-persistent`).
4. **Out-of-process subagents** (ACP, Claude Code, Codex, SDK).
5. **`session-title-all-prompts-llm`**, **`skill-badge`**.

### P4 — Experimental / cloud / hooks (defer)

1. **`e2b`*** , **`experimental/agent-team`*, **`hooks-*`**, **`mcp-client`**.

### P5 — Util libraries (port with first consumer)

1. **`launch-environment`**, **`native-command`**, **`output-retention`**, **`hook-protocol`**, **`session-title-llm`** (extract shared crate from first-prompt impl).

---

## Suggested next milestones

| Milestone | Outcome | Primary packages |
| --- | --- | --- |
| M1 Headless parity++ | Typert registry/loader no-ops replaced; settings namespace API; pi-ai or explicit defer | typert/*, settings, llm-pi-ai |
| M2 Web host skeleton | `dsh-web-app` Rust bundle, webserver + apiproxy wired, cmdline, frontend-static | bundle/web-app, host/*, boot/cmdline |
| M3 Web data plane | storage*, workspace, agent-presets, projection-cache, message-feedback | storage/*, workspace, preset/*, session extras, feedback |
| M4 Web UX | client runtime + top ui-* (conversation, sidebar, settings, connection) | client/* |
| M5 Code Mode | code-runtime*, agent-tool-presentation, tool-ask-user | code-runtime/*, core/agent-tool-presentation, interaction/tool-ask-user |
