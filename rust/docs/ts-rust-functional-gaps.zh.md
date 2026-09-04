# TypeScript ↔ Rust 功能差距与移植优先级

[English](ts-rust-functional-gaps.md) | 中文

本页是每条 `packages/<group>/<pkg>` 叶与 `rust/crates/<group>/<pkg>` 的对照清单，以及剩余移植工作的排序。`packages/` 下的 TypeScript 仍是行为真源。本页不改变该规则；它记录 Rust 树在何处更薄、是桩、已改名，或有意缺席。排序决策见 [proposed Agent Note](../../.agents/notes/proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md)。已交付的 Rust 行为仍以 [rust/README.zh.md](../README.zh.md) 与 [移植 Agent Note](../../.agents/notes/implemented/architecture/2026-08-22-rust-harness-port.zh.md) 为准。

取证日期：2026-09-04。计数：**227** 个 TypeScript 包，`rust/crates/*/*/` 下 **117** 个 crate，**109** 个相同的 `(group, pkg)` 对，**118** 个仅 TypeScript 叶，**8** 个仅 Rust 叶（改名或额外 patch crate）。只比叶名会误报：`typert/protocol` 不是 `sdk/protocol`，`acp/acp` 也不是 `bundle/acp`。

## 如何阅读优先级

| 标签 | 含义 |
|---|---|
| **P0** | Headless、ACP 或 JSON-RPC 在 Linux 上的产品用户或模型可见正确性——即已交付的 Rust profile |
| **P1** | 这些 profile 上的耐久性、运维或次要表面 |
| **P2** | 其他平台、会讲现有 TypeScript 客户端协议的 Rust 宿主，或相邻的可选 provider |
| **P3** | 可选示例与额外 agent 能力 |
| **P4** | 实验、云或 hook 桥 |
| **P5** | 工具库，随第一个消费者一起移植 |
| **skip** | 不是 Rust 移植目标，或已记录的独立格式 |

若后续 crate 要在 Rust 里重写 TypeScript Web UI，那是产品分叉，不是移植（[已否决的替代方案](../../.agents/notes/implemented/architecture/2026-08-22-rust-harness-port.zh.md#alternatives-considered)）。`dsh-agent-loop` 保持为插件；finish-chunk 恢复只在此报告，不靠改 loop 关闭。会话日志保持 `SESSION_FORMAT_VERSION` `0`。Rust SQLite 使用单调的 `SCHEMA_VERSION` `2`，并拒绝其他 `user_version`；追到 TypeScript 打包 schema 17 是 **skip**，不是 P0。

## 名称改写

这些 Rust 目录不共享 TypeScript 文件夹名。它们不是缺失的包。

| TypeScript | Rust | 说明 |
|---|---|---|
| `core/agent-default-model` | `core/agent` 里的 `dsh-agent::AgentDefaultModel` | 在 `app-boot` 中真实挂载 |
| `compaction/compaction-tool-result-pruner` | `compaction/tool-result-pruner` | 同一 Consumer |
| `subagent/subagent-spawn-in-process` + `subagent-fork-in-process` + `subagent-in-process-driver` | `subagent/subagent-inprocess` | 合并的 spawn/fork provider |
| `workflow/workflow-worker-thread` | `workflow/workflow-local` | 进程内 `return <json>`；不是 JS worker |
| `terminal/terminal-bash` | `terminal/terminal-local` | 内存桩，不是 PTY |
| `test-support/llm-replay` | `llm/llm-replay` | 快照使用的 replay adapter |
| `examples/agent-spine-demo` | `examples/agent-spine` | spine demo crate |
| `examples/acp-demo` / `examples/jsonrpc-demo` | `bundle/acp` / `bundle/jsonrpc` | 盖在 `packages/acp/acp` 与 `packages/sdk/server` 上的 patch-layer crate |

## 有意不做 Rust crate

| 区域 | TypeScript 叶 | 为何是 skip |
|---|---|---|
| Web UI | 整个 `packages/client/*`（40 个包） | 宿主以后可以讲现有客户端协议；第二套 UI 是分叉 |
| 测试基础设施 | `test-support/*`，已改名的 replay adapter 除外 | Vitest/tsx harness 留在 TypeScript |
| Typert generator | `typert/generator` | TypeScript 构建期分析器 |
| 运行时诊断 | `runtime-diagnostics/invariants` | TypeScript 包不变量配套 |

`app-boot` 仍把其中一些名字挂成空操作，以便组合出的 TypeScript 树能加载：`@deepseek-ai/cordis-plugin-hmr`、`@deepseek-ai/dsh-typert-registry`、`@deepseek-ai/dsh-typert-loader`、`@deepseek-ai/dsh-api-gateway`、`@deepseek-ai/dsh-llm-pi-ai`、`@deepseek-ai/dsh-skill-badge`，以及 `@deepseek-ai/dsh-code-runtime-worker-thread`（仅 marker `codeRuntime`）。未知名字也返回 `Ok(())`。

## 优先级摘要

### P0 — 已交付 profile 的正确性（Linux）

1. **会话 persistence 协调器** — **已关闭。** `PersistenceRuntime` 的 write-behind（`writeBatchMaxDelayMs`，默认 200，后续 append 不重置窗口）、公开的 `create` / `append` / `prepare` / `readFrom`，以及 JSONL（截到最后一个完整 `\n`）与 SQLite（从第一个无法解码或出现缺口的 seq 起 `DELETE`）上的耐久 `commitRepair`。SQLite 保持 schema `2`。`inspect` 仍在内存中合成 closer（[`rust/crates/session/session-persistence/src/lib.rs`](../crates/session/session-persistence/src/lib.rs)）。
2. **LLM DeepSeek 传输** — SSE 与图像块 **已关闭。** `"stream": true` 加 `stream_options.include_usage`；解析 `data:` / `[DONE]`；缺少 `[DONE]` 为 `STREAM_CLOSED`；`finish` / `usage` 只在 `[DONE]` 之后发出。vision 模型（`model` 含 `vision`）把用户图像做成 `image_url` data-URL。Files API 上传未挂载（[`rust/crates/llm/llm-deepseek/src/lib.rs`](../crates/llm/llm-deepseek/src/lib.rs)）。
3. **附件栅格流水线** — **已关闭。** `request_image` 解码、按最长边缩小（`normalizedImageMaxDimension`，默认 2048），并在 `normalizedImageMaxBytes`（默认 4 MiB）下重编码为 JPEG。质量 85 再 80 后仍超上限则为 `IMAGE_TOO_LARGE`（[`rust/crates/attachment/attachment-local/src/lib.rs`](../crates/attachment/attachment-local/src/lib.rs)）。
4. **ACP 图像提示** — **已关闭。** 仅当挂了 `ctx.attachments` 且 `AgentDefaultModel` 为 vision 时广告 `image: true`；经 `save_image` 接纳 `image` 块。默认 headless 仍为 `image: false`，并以 `inline image prompts were not advertised by this connection` 拒绝（[`rust/crates/acp/acp/src/lib.rs`](../crates/acp/acp/src/lib.rs)）。
5. **settings Service Definition** — 在 `settings-file`（`ctx.settings`）上 **已关闭**：`register` / `watch` / `revision` / `mutate` / `describe`，以及 `settings/updated`（`ns`、`revision`、`value`）与 `settings/document-updated`（`revision`）。独立的 `settings` crate 仍缺席。
6. **headless 上的 plan-mode 评审** — **已关闭。** 默认无 provider，以 `no user-questions provider is registered` 失败。Config `reviewProvider: "auto"` 挂载选取第一项的 approver；该选项在没有 `ctx.userQuestions` 时于 install 失败。默认 headless patch 不设置 `reviewProvider`（[`rust/crates/plan/plan-mode/src/lib.rs`](../crates/plan/plan-mode/src/lib.rs)）。
7. **agent-loop 的 finish-chunk 错误** — loop 在 `stream()` `Err` 时结束 turn，且只有 `FinishReason::MaxTokens` 改变 turn 结束；流内 `finish { kind: error \| aborted }` 被记录后 turn 仍完成（[`rust/crates/core/agent-loop/src/lib.rs`](../crates/core/agent-loop/src/lib.rs)）。**只报告；不要改 `dsh-agent-loop` 来关闭本行。**

### P1 — 已交付 profile 上的耐久性与运维

8. 从插件 `install` Config 接通 `preparedSessionCacheSize` / `writeBatchMaxDelayMs` — 随 P0-1 **已关闭**（`dsh-app-boot` / `PersistenceRuntime`）。
9. `readFrom` / 后缀读取 — 随 P0-1 **已关闭**（`PersistenceRuntime::read_from`）。
10. `session-projection-cache`（无 crate）。
11. session-query FTS（[`rust/crates/bundle/base/cordis.patch.yml`](../crates/bundle/base/cordis.patch.yml) 里 `openAt: never`；SQLite FTS schema 1 对 TypeScript 8）。
12. 在需要取 URL 的 profile 里启用 `web-fetch-http`（crate 已在；base 为 `fetch: false`）。
13. skill 文件系统监听 / 轮询到根出现（Rust 只在 `agent/pre-step` 与 `fs/observed` 上重扫）。
14. OTel seam 的 flush 提示（metrics 保持 skip）。
15. SDK 客户端助手（`Session.run`、后代通知合并）对比瘦的 stdio 包装。
16. 外部子代理 provider（ACP / Codex / Claude / `dsh-sdk`）。
17. 盖在现有 `user-questions` 服务上的 `tool-ask-user` Consumer。

### P2 — 平台与相邻产品

18. 会讲现有 TypeScript 客户端协议的 Rust HTTP 宿主（`host/webserver` + 完整 `host/apiproxy` BFF + `boot/cmdline` + 作为 TypeScript SPA 宿主的 `bundle/web-app`——不是重写 UI）。
19. Typert registry / loader / protocol 与 `api/gateway` + `api/remotes`（仅当该宿主要交付时才需要）。
20. macOS Seatbelt 与 Win32 `CreateRestrictedToken`（Linux 的 bwrap/landlock 已交付；Windows crate 只给 Node ACL runner 加前缀，不调用 token API）。
21. 真 PTY（`terminal-bash`）与 stdio LSP。
22. JavaScript workflow worker。
23. `llm-pi-ai`、Exa、Perplexity。
24. storage / workspace / agent-presets / persona（Web 数据面）。
25. Code Mode（`code-runtime*` + `agent-tool-presentation`）。

### P3 — 可选能力

26. `schedule` — **已关闭。** `time-context`、`tmux-context` 仍缺席。
27. `tool-session-query`、`session-log-export`、`session-stats`、`session-title-all-prompts-llm`。
28. 持久 shell 工具（`tool-bash-persistent` / `tool-pwsh-persistent`）。
29. `skill-badge`（空操作，base 中禁用）。
30. `message-feedback`（Web）；`command-feedback` 已交付。

### P4 — 实验 / 云 / hook

31. `e2b` / `fs-e2b` / `subprocess-e2b`。
32. `experimental/agent-team` + `tool-agent-team`。
33. `hooks-claude-code` / `hooks-codex` / `hook-protocol` — **已关闭。**
34. `mcp-client` — **已关闭。**
35. 动态 Cordis（`tool-cordis`、host/client runner）。

### P5 — 随第一个消费者走的工具库

36. `launch-environment`、`native-command`、`output-retention`。
37. 仅当第二个 title provider 需要共享路由时，才抽出 `session-title-llm`。

## 建议关闭顺序

1. persistence 协调器、DeepSeek SSE + 图像块、附件归一化、settings Service Definition、headless plan 评审 — **已关闭**（P0 第 1–6 项）。
2. 记录 loop 的 finish-chunk 差距；此处不改 `dsh-agent-loop`（P0 第 7 项，只报告）。
3. DeepSeek Files API 上传（内联 `image_url` data-URL 已交付）。
4. session-query FTS 可选开启、web fetch 启用、skill 监听、OTel flush、SDK 助手、外部子代理。
5. 仅当那些 headless 宿主进入范围时，再做平台沙箱与 PTY/LSP。
6. 仅在上面的 headless spine 差距关闭之后，才让 Rust 做现有 TypeScript Web 客户端的宿主。

## 分组对照表

状态值：**aligned**（headless 约定匹配）、**thinner**（crate 在，约定不完整）、**stub**（已挂但是内存 / marker）、**no-op**（`app-boot` 的 `Ok(())` 或 marker）、**absent**、**remap**、**skip**。

### acp

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `acp` | 图像提示 aligned | 仅在有 `ctx.attachments` + vision 默认模型时广告 `image: true`；audio / embeddedContext 仍为 false | remaining（audio / resource） |

### api

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `gateway` | no-op | `dsh-api-gateway` 行以 `Ok(())` 加载；无 Typert RPC gateway | P2 |
| `remotes` | absent | 无 Remote 贡献组装 | P2 |

### attachment

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `attachment` | aligned | 存储类型已在 | — |
| `attachment-local` | aligned | 魔数准入加 `request_image` JPEG 归一化 | remaining（Files API 在 DeepSeek 侧） |

### boot

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `app-boot` | thinner | headless 树真实挂载；剩余名字空操作；persistence Config（`preparedSessionCacheSize` / `writeBatchMaxDelayMs`）已接通；组合树点名时挂上可选的 `schedule` / `mcp-client` / hook 桥 | P1 空操作见上 |
| `cmdline` | absent | TypeScript 共享 argv 解析；Rust 把任务旗标内联进 headless 启动 | P2 |

### bundle

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `base` / `headless` | aligned | 同一份 TypeScript patch 文件；已记录 `fetch: false` 与 `openAt: never` | P1（产品旗标） |
| `web-app` | absent | 无 Rust web bundle；`profile_templates()` 可能点名它 | P2 |
| `bundle/acp` / `bundle/jsonrpc` | remap | Rust patch crate；TypeScript 服务器在 `acp` / `sdk` 下 | — |

### client

全部 40 个包（**skip**）。不要把 SPA、slots 或 `ui-*` 插件移植到 Rust。

### code-runtime

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `code-runtime` | absent | 无 `run` Service Definition crate | P2 |
| `code-runtime-python` | absent | 无 CPython 后端 | P2 |
| `code-runtime-worker-thread` | no-op | 仅 marker `codeRuntime` | P2 |

### compaction

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `compaction` / `compaction-basic` / `command-compact` | aligned | 压力、溢出、`/compact`、检查点成帧 | P1（SSE 之后回归） |
| `compaction-tool-result-pruner` | remap | `tool-result-pruner` | — |

### context

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `agent-instructions` | aligned | 基线 + 更新消息 | — |
| `file-reference` / `file-reference-local` | absent | `@file` 发现 | P2（Web） |
| `session-reference` | absent | `@session` 注入 | P2 |
| `time-context` | absent | 请求时钟消息 | P3 |
| `tmux-context` | absent | tmux pane 上下文 | P3 |

### core

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `session` / `agent` / `tools` / `system-prompt` / `scope` | aligned | headless spine | — |
| `agent-loop` | thinner | 缺流内 finish-error / aborted 的 turn 结束 | P0 只报告 |
| `agent-default-model` | remap | 从 `dsh-agent` 安装 | — |
| `agent-tool-presentation` | absent | Code Mode 的 `presentAs` | P2 |

### credentials

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `credentials` / `authorization` / `credentials-local` | aligned | 环境 / YAML / `.env` 顺序；Unix 模式拒绝 | P1（再核 Unix 文件模式） |

### e2b

三个包全部 **absent** / **P4**。

### examples

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `agent-spine-demo` | remap | `examples/agent-spine` | — |
| `acp-demo` / `jsonrpc-demo` | remap | `bundle/acp` / `bundle/jsonrpc` + `apps/cli` | — |

### experimental

`agent-team` / `tool-agent-team`：**absent** / **P4**。

### extensions

`tool-cordis` / `ui-cordis` / `cordis-host-runner` / `cordis-client-runner`：**absent** / **P4**。

### feedback

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `command-feedback` | aligned | 仅日志的 `/feedback` | — |
| `message-feedback` | absent | 存储上的逐条赞/踩 | P3 |

### fs

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `fs` / `fs-local` / `fs-sandbox` / `tool-fs` / `tool-fs-search` / `tool-str-replace-editor` | aligned | 观察 + 沙箱 write/edit | — |
| `fs-observation-policy` | aligned | 与 TypeScript 相同的 resume 限制（观察不跨 resume 存活） | P1 双方暂缓 |

### goal

`goal` / `goal-round-driver` / `command-goal` / `tool-goal`：**aligned**。

### guard

`repeat-tool-reminder` / `timeout-policy`：**aligned**。

### hooks

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `hook-protocol` | aligned | matcher、codec、merge、`hook/invoked` / `hook/result`、detached drain | remaining（解析 `updatedInput` 但不兑现——与 TypeScript 相同） |
| `hooks-claude-code` | aligned | SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / Stop / SubagentStart / SubagentStop；`ask` 走 `ctx.approval`，否则失败闭合 deny | remaining（`updatedInput` / `systemMessage` / `allow` 预批准——与 TypeScript 相同） |
| `hooks-codex` | aligned | 五个 Codex 事件；跳过 `async: true`；只兑现 deny | remaining（`updatedInput` / `additionalContext`——与 TypeScript 相同） |

### host

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `webserver` | thinner | `/health` + POST `/rpc`；无 SPA 回退、SSE 或升级表；`apply_named` 未挂载 | P2 |
| `apiproxy` | thinner | 仅 HTTP POST 转发；不是 TypeScript BFF；`apply_named` 未挂载 | P2 |
| `frontend-static` / `plugin-inventory` / `directory-picker*` | absent | Web chrome | P2 |

### identity

`anonymous-user-id`：**aligned**。

### interaction

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `commands` / `permission-presets` / `user-approval` | aligned | headless 的 `never` / `ask` 失败闭合 | — |
| `user-questions` | thinner | 有服务、无默认 provider；plan-mode 可选择 `reviewProvider: "auto"` | remaining（交互式 provider） |
| `tool-ask-user` | absent | 模型的 `ask_user_question` | P1 |

### jobs

`jobs` / `jobs-local` / `tool-jobs`：**aligned**。

### llm

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `llm` | aligned | chunk 标签含 `FinishReason::Error` | P0 消费者必须遵守 |
| `llm-deepseek` | thinner | SSE + 用户 `image_url` data-URL 已对齐；无 Files API 上传；retry/classify/`Retry-After` 已对齐 | remaining（Files API） |
| `llm-retry` / `token-meter` | aligned | `providerRetryAfterMs` 超上限规则 | — |
| `llm-pi-ai` | no-op | bundle 行，无 crate | P2 |
| `llm-replay` | remap | 在 `llm/` 下 | — |

### lsp

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `lsp` / `lsp-stdio` / `tool-lsp` | stub | 记录 initialize；静态 capabilities；无 language-server 进程 | P2 |

### mcp

`mcp-client`：**aligned**。stdio 与 streamable-http 工具桥、重连监督器、公开名 `mcp__<server>__<raw>`、经路由证明后的图像准入。成功 body 把 `ToolOutcome.value` 设为 `{content, structuredContent?}`。剩余：MCP Resources / Prompts（TypeScript 同样暂缓）。

### plan

`plan-mode`：`/plan` + `exit_plan_mode` 与可选 `reviewProvider: "auto"` **aligned**。默认评审保持失败闭合。

### preset

`agent-presets` / `persona`：**absent** / **P2**。

### runtime-diagnostics

`invariants`：**skip**（TypeScript 测试配套）。

### sandbox

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `sandbox` / `sandbox-local` / `sandbox-policy` | Linux 上 aligned | bwrap + landlock-run；无 `sandbox-exec` | P2 Seatbelt |
| `sandbox-windows-acl` | thinner | 仅 Node runner argv；无 `CreateRestrictedToken` | P2 |

### schedule

`schedule`：**aligned**。版本 1 的 `schedule/change` fold、`schedule_create` / `schedule_list` / `schedule_delete`、idle 阶段到期投递。剩余：`time-context` 是单独的 P3 crate；Rust 把三个工具登记为全局工具，并用 `enabled_for` 限制到加载后创建的存活根 owner。

### sdk

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `protocol` / `server` | aligned | stdio JSON-RPC 身份 `deepseek-harness-sdk-runtime` | — |
| `client` | thinner | `initialize` / `prompt` / `shutdown` + 通知排空；无 `Session.run` 助手 | P1 |

### session

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `session-persistence` | aligned | write-behind、`create` / `append` / `prepare` / `readFrom`、`load` 的耐久 `commitRepair`；`inspect` 仍是内存 closer | remaining（read-repair / incarnation） |
| `session-persistence-jsonl` | aligned | header + 事件 + `list` + 撕毁最后一行的 `commitRepair` | remaining（read-repair） |
| `session-persistence-sqlite` | aligned | schema 2 加撕毁行 `commitRepair`；schema 17 为 **skip** | remaining（read-repair） |
| `session-projection` | aligned | registry 已在 | — |
| `session-checkpoint-policy` | aligned | 模型请求与顶层工具分派前刷盘 | P1（与 write-behind 一起回归） |
| `session-telemetry` / `session-telemetry-otel` | thinner | OTLP 日志 + `Retry-After` + keepAlive；无 flush 提示；无 metrics | P1 flush；metrics **skip** |
| `session-title` / `session-title-first-prompt-llm` | aligned | 回退 + first-prompt LLM | — |
| `session-projection-cache` | absent | 耐久 projection 检查点 | P1 |
| `session-stats` | absent | 聊天统计 projection | P3 |
| `session-title-llm` | absent | 共享库；逻辑在 first-prompt crate 里 | P5 |
| `session-title-all-prompts-llm` | absent | 全部人类消息标题 | P3 |

### session-query

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `session-query` | thinner | 精确读取；lineage 为空；默认禁用搜索 | P1 |
| `session-query-sqlite` | thinner | FTS schema 1；`openAt: never` | P1 |
| `session-log-export` | absent | ZIP 导出命令 | P3 |
| `tool-session-query` | absent | 模型搜索/读取工具 | P3 |

### settings

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `settings-file` | aligned | 文件文档、YAML 叶级 `update` / `replace`，以及 `register` / `watch` / `revision` / `mutate` / `describe` 与 settings 事件 | remaining（本 crate 无剩余） |
| `settings` | absent | 方法在 `settings-file`（`ctx.settings`）上；无独立 Definition crate | remaining（仅当角色独立演化时再拆） |

### shell

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `shell` / `shell-env` / `bash-local` / `bash-sandbox` / `tool-bash` | Linux 上 aligned | confine + jobs | P2 其他 OS runner |
| `pwsh-local` / `pwsh-sandbox` / `tool-pwsh` | aligned | 非 win32 上由 `!!js` 禁用 | — |
| `tool-bash-persistent` / `tool-pwsh-persistent` | absent | 有状态 PTY 工具 | P3 |

### skill

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `skill` / `tool-skill` | aligned | catalog 消息 + `<skill_content>` | — |
| `skill-filesystem` | thinner | 在 `agent/pre-step` 与 skill 路径的 `fs/observed` 上重扫；无 Chokidar / 轮询 | P1 |
| `skill-badge` | no-op | base 中禁用 | P3 |

### spill

`spill` / `spill-local` / `spill-policy`：**aligned**。

### storage

四个包全部 **absent** / **P2**。

### subagent

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `subagent` / `tool-subagent` / `tool-subagent-control` / `tool-subagent-report` | aligned | continuable spawn、冷 resume、`list_agents` 诊断、`report` | P1（冷 inspect 依赖 `commitRepair`） |
| `subagent-inprocess` | remap | 合并的 spawn/fork | P1 driver wake latch |
| `subagent-acp` / `subagent-claude-code` / `subagent-codex` / `subagent-dsh-sdk` | absent | 进程外 provider | P1 |

### subprocess

`subprocess` / `subprocess-local`：**aligned**。

### terminal

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `terminal` / `tool-terminal` | stub | 内存写历史 | P2 |
| `terminal-local` | remap / stub | 不是 `terminal-bash` PTY | P2 |
| `terminal-bash` | absent | 真 PTY + 沙箱策略 | P2 |

### test-support

**skip**，已改名的 `llm-replay` 除外。

### todo

`tool-todo`：**aligned**。

### typert

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `registry` / `loader` | no-op | 行会加载；无 `ctx.typert` | P2 |
| `protocol` | absent | Remote/Gateway 类型 | P2 |
| `generator` | skip | 构建期 TS | skip |

### util

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `brand` / `timeout` / `atomic-write` / `home-paths` | aligned | — | — |
| `launch-environment` / `native-command` / `output-retention` | absent | 库 | P5 |

### web

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `web` / `web-search-deepseek` / `tool-web` | aligned | official + `replay`；`fetch: false` | P1 fetch 旗标 |
| `web-fetch-http` | thinner | crate 已在；base 未启用 | P1 |
| `web-search-exa` / `web-search-perplexity` | absent | 可选 provider | P2 |

### workflow

| 包 | 状态 | 差距 | 优先级 |
|---|---|---|---|
| `workflow` / `tool-workflow` / `tool-ralph` | `return <json>` / Ralph-over-spawn 对齐 | 隔离不同 | — |
| `workflow-local` | remap / thinner | 进程内求值 | P2 worker |
| `workflow-worker-thread` | 作为 JS worker 不存在 | TypeScript 默认引擎 | P2 |

### workspace

`workspace`：**absent** / **P2**。

## 已经对齐（不要再当成差距打开）

凭据解析、`llm-retry` 的 `retryPolicy` + `providerRetryAfterMs`（delay-seconds 与 HTTP-date，超上限的 `normal`/`always`）、sandbox-policy / approval / permission-presets、带冷 resume 与 `list_agents` 诊断的 continuable 进程内子代理、persistence write-behind / `append` / 耐久 `commitRepair` / inspect LRU `preparedSessionCacheSize`、Windows ACL 的 Node runner argv、OTel keepAlive + `Retry-After` HTTP-date、compaction-basic 主路径、goal / todo / plan 工具（含可选 `reviewProvider: "auto"`；默认 headless 评审仍失败闭合）、jobs、spill、agent-instructions、fs 观察门、skill catalog 工具、token-meter、标题、附件 `request_image`、在 store + vision 挂载时的 ACP 图像提示、DeepSeek SSE + `image_url` data-URL、settings 的 `register` / `watch` / `revision` / `mutate`、`schedule` 版本 1 工具与 idle 投递、`mcp-client` 工具桥、`hook-protocol` 以及 Claude Code 与 Codex 桥。

## 验证

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

在提升某一行之前，重读 `install()` / `apply_named` 与 TypeScript 包 README 的 **Known Limitations**。更薄 crate 上的绿灯单元测试不能证明 TypeScript 对等。
