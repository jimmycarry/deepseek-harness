# Agent Note: Rust 把 MCP、schedule 与 hook 桥作为可选插件移植

Status: implemented

[English](2026-09-04-rust-mcp-schedule-hooks.md) | 中文

## Problem

TypeScript 树已经拥有三组默认 headless patch 不该插入的可选能力 seam：`@deepseek-ai/dsh-mcp-client` 把一台外部 MCP 服务器的工具登记到 `ctx.tools`；`@deepseek-ai/dsh-schedule` 给之后创建的存活根 Agent 三件会话日志提醒工具；`@deepseek-ai/dsh-hook-protocol` 加上 Claude Code 与 Codex 桥在 harness 拦截点上跑 command hook。Rust 宿主若省略这些名字，会把官方 overlay（`examples/mcp-memory`、`examples/web-schedule`、`examples/acp-agent`）当成空操作加载，并静默丢掉模型可见工具、提醒 follow-up 以及 hook 的 deny/ask 决定。在 Rust 侧另造一套 MCP SDK、cron 调度器，或单独兑现 `updatedInput`，都会分叉 TypeScript 的 Known Limitations。

## Decision

组合树点名时，`dsh-app-boot` 通过 `apply_named` 挂上 `@deepseek-ai/dsh-schedule`、`@deepseek-ai/dsh-mcp-client`、`@deepseek-ai/dsh-hooks-claude-code` 与 `@deepseek-ai/dsh-hooks-codex`。默认 headless 树不插入这些行。`packages/` 下的 TypeScript 仍是行为真源；crate 目录对齐 `packages/<group>/<pkg>/`。[移植 Agent Note](../architecture/2026-08-22-rust-harness-port.zh.md) 仍拥有 1:1 规则。[差距排序](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md) 把这些 P3/P4 行记为已关闭。

### MCP client

每个插件实例拥有一个 `serverName`（`/^[A-Za-z0-9_-]{1,32}$/`）。第二个同名存活实例在加载时按 TypeScript 文句失败。线上客户端身份为 `{ name: 'dsh-mcp-client', version: '0.0.1' }`，协议 `2025-03-26`。`callTool` 永远发送 MCP 原始名。公开登记名是 `mcp__<server>__<raw>`；`[A-Za-z0-9_-]` 以外的字符变成 `_`，有损改写再追加 `sha256(server + '\0' + raw)` 的前 12 个十六进制字符。stdio 继承 `scrubbed_parent_env()` 再叠显式 `env`。Streamable HTTP 走 `curl`。重连默认值为 `enabled: true`、`initialDelayMs: 500`、`maxDelayMs: 30000`、`maxAttempts: 10`。`failOnStartupError: true` 以 `mcp-client(<server>): initial connection or tool synchronization failed` 拒绝激活。图像块只有在 `ctx.attachments` 与精确默认模型路由声明图像输入之后才成为耐久附件；被拒绝的图像、音频、嵌入资源与未知块保留 TypeScript 诊断句。受支持的广告 `outputSchema` 成为 `Tool::output_schema` 上的 `structuredContent`；不受支持的词表回退为无约束 JSON。MCP Resources 与 Prompts 仍不桥接。

### Schedule

版本 1 的 `schedule/change` create、delete 与 dispatch 记录从会话日志 fold。工具是 `schedule_create`、`schedule_list` 与 `schedule_delete`。Rust 没有按 agent 的 `ctx.tools`，也没有 `agent/created` 事件：插件把三件工具登记一次，用 `Tool::enabled_for` 只对加载后发布的存活根 owner 可见，并监听 `agent/session-start`。每次管理预检与到期决策都等待 `ctx.sessionPersistence` flush；缺失或被拒绝的屏障返回 `persistence_uncertain`。到期 one-shot 优先；过期的 Every 记录组成一批。投递只在 owner idle 时排队后续 `followup` 并追加 dispatch。不接受 cron 表达式。

### Hook 协议与桥

`dsh-hook-protocol` 拥有 Claude 的字面量或正则 matcher、Codex 的无锚正则、exit-2 / JSON stdout codec、deny/block > ask > allow/approve 合并、`hook/invoked` / `hook/result` 会话事件，以及通用 command runner。默认超时 600000 ms；持久化 stderr 摘要上限 500 字符。非法正则不匹配任何值，并使用 TypeScript 的 `invalid {mode} regex matcher` 诊断。

`dsh-hooks-claude-code` 要求 `configPath`。它替换 `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PROJECT_DIR}`，导出 `CLAUDE_PROJECT_DIR`，stdin 带尾换行，并映射 SessionStart、UserPromptSubmit、PreToolUse、PostToolUse、Stop、SubagentStart 与 SubagentStop。UserPromptSubmit 与 Stop 丢掉 matcher。`ask` 在已挂 `ctx.approval` 时走该服务，否则失败闭合 deny。`updatedInput` 与 `systemMessage` 按 TypeScript 文句告警，不兑现。

`dsh-hooks-codex` 映射五个 Codex 事件，跳过 `async: true` hook，stdin 不带尾换行，并且只兑现 deny。`plainStdoutAsContext` 作用于 SessionStart 与 UserPromptSubmit。

Rust waterfall 是同步的。两座桥用 `block_in_place` 加 Tokio handle 跑 hook body。`agent/turn-stopping` 只携带 `{turn}`；每座桥记住最近一次 `agent/pre-step` 的 agent。continuable 子级在 `agents.create` 之后、`followup` 之前发出 `subagent/start`。一次性 `SubagentRuntime::start` 在 `provider.start` 结算之后先发布 `subagent/start` 再发布 `subagent/end`。

## Alternatives considered

**把这些插件挂进默认 headless 树。** 官方 TypeScript headless 不插入这些行。可选 overlay 拥有组合。

**MCP 依赖 `rmcp` / `reqwest`。** 那些依赖图会拉入 edition 2024 或 `hashbrown` 0.17。手写 JSON-RPC 加 `curl` 保持在 Rust 1.83。

**只在 Rust 侧兑现 `updatedInput`、`systemMessage` 或 MCP Resources/Prompts。** 那会分叉 TypeScript 的 Known Limitations。这些路径保持暂缓，直到 TypeScript 真源交付它们。

**给每个 Rust Agent 一套私有 `ctx.tools`，好让 Schedule 在 `agent/created` 上登记。** Rust 工具注册表是进程全局的。在存活 owner 表上用 `enabled_for` 得到同一套可见性规则，无需第二套注册表。

**在 waterfall 线程里用 `futures::executor::block_on` 等待 hook future。** 这会在 Tokio 内死锁。`block_in_place` 加运行时 handle 才是保住 TypeScript 异步 hook runner 的适配。

## Consequences

官方 MCP、schedule 与 hook overlay 可以在 Rust 宿主上加载，且不改 `dsh-agent-loop`、`SESSION_FORMAT_VERSION` 或 SQLite `SCHEMA_VERSION`。除非 overlay 插入这些行，headless dump-config 不含它们。一次性 SubagentStart hook 在子级结算之后运行。Native MCP 结果是 content block；因为 Rust `ToolOutcome` 没有第二个值槽，Code Mode 那份独立的规范 `{content, structuredContent}` 绑定不存在。清单状态见 [ts-rust-functional-gaps.md](../../../../rust/docs/ts-rust-functional-gaps.md)。
