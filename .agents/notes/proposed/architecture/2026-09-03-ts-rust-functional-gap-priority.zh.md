# Agent Note: 按 headless 优先的功能差距排列剩余 Rust 移植工作

Status: proposed

[English](2026-09-03-ts-rust-functional-gap-priority.md) | 中文

## 问题

Rust 树已经在 [移植 Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.zh.md) 的 1:1 规则下交付 headless、ACP 与 JSON-RPC profile。TypeScript 仍有 227 个包；Rust 有 112 个 crate。若没有约定顺序，下一个 crate 可以去追 Web UI、打包的 SQLite schema 17，或去改 loop，看起来仍像进展，而已交付的 Linux profile 在 persistence、流式、附件与 settings 上仍然更薄。

只比叶名也会说谎：`typert/protocol` 不是 `sdk/protocol`。维护者若按「每个缺失文件夹名」移植，会把已经存在于另一 group 下的服务器再做一遍。

## 提案

采用 [rust/docs/ts-rust-functional-gaps.md](../../../../rust/docs/ts-rust-functional-gaps.md) 作为工作清单，并以本排序作为剩余 Rust 工作的顺序。`packages/` 下的 TypeScript 仍是行为真源。状态为 **aligned** 或 **skip** 的行不是缺失 crate。

P0 是已交付 headless / ACP / JSON-RPC profile 在 Linux 上的正确性。P1 是这些 profile 上的耐久性与运维。P2 是其他平台，或会讲现有 TypeScript 客户端协议的 Rust 宿主。P3–P5 是可选、实验与工具库。在 Rust 里重写 `packages/client/*` 仍被否决。`dsh-agent-loop` 不是关闭 finish-chunk 那一行的地方。`SESSION_FORMAT_VERSION` 保持 `0`。Rust SQLite 的 `SCHEMA_VERSION` `2` 保持为独立的预发布格式。

[移植 Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.zh.md) 仍然拥有 1:1 规则与已交付集合。本 note 只拥有剩余工作的顺序。

## 优先级分档

| 档 | 覆盖 | 首批条目 |
|---|---|---|
| P0 | 已交付 profile 在 Linux 上的正确性 | persistence 协调器（write-behind、耐久 `commitRepair`、append）；DeepSeek SSE + 图像块；附件栅格归一化；随后的 ACP 图像；settings Service Definition；headless plan 评审；报告 loop 的 finish-chunk 差距 |
| P1 | 这些 profile 上的耐久性与运维 | `readFrom`；projection cache；session-query FTS；启用 `web-fetch-http`；skill 监听；OTel flush 提示；SDK 客户端助手；外部子代理 provider；`tool-ask-user` |
| P2 | 平台与相邻产品 | 现有 TypeScript SPA 的 Rust 宿主；Typert/API；Seatbelt / `CreateRestrictedToken`；PTY / LSP；JS workflow worker；`llm-pi-ai`；Exa / Perplexity；storage / workspace / presets；Code Mode |
| P3 | 可选能力 | `schedule`；额外 context；持久 shell；额外 title / query / feedback 包；`skill-badge` |
| P4 | 实验 / 云 / hook | e2b；agent-team；Claude Code / Codex hook；MCP；动态 Cordis |
| P5 | 工具库 | `launch-environment`；`native-command`；`output-retention`；仅在有第二个消费者时抽出 `session-title-llm` |

## 不在范围内

- 改 `dsh-agent-loop` 以遵守流内 `finish { kind: error \| aborted }`（记录差距；改 loop 是单独的架构更新）。
- 提升 `SESSION_FORMAT_VERSION`，或把 TypeScript 打包 SQLite schema 17 当成 Rust 目标。
- 在 Linux 或 Wine 上假造 Win32 `CreateRestrictedToken`。
- OTel SDK metrics，以及 TypeScript crate 同样记录为并发 flush 时拒绝的 flush 实现。
- 用 Rust 再写一套 Web UI。

## 考虑过的替代方案

- **把 Typert、API gateway 与 Web 客户端栈标成 P0** — 这会颠倒已交付 profile，并重开已被否决的「用 Rust 重写 Web UI」替代方案。后续 Rust 宿主可以讲现有客户端协议；那是 P2，排在 headless spine 之后。
- **把每个仅 TypeScript 的包都当成同等缺失工作** — 123 个叶包含测试 harness、UI 插件与构建期生成器。同等优先级会盖住 Linux 上的产品用户差距。
- **只把本排序折进移植 Agent Note** — 该 note 已经拥有 1:1 规则与已挂载的树。再塞一张 227 行的表会同时淹没决策与清单。清单放在 `rust/docs/`；本 note 只保留顺序。
- **只生成计数、不定优先级** — 没有 P0–P5 的普查仍让下一个 PR 可以挑最好写的 crate。

## 验收标准

- `rust/docs/ts-rust-functional-gaps.zh.md` 列出每一个 `packages/` group，写明 aligned / thinner / stub / no-op / absent / remap / skip，并给出优先级或 skip 理由。
- 后续 Rust 移植 PR 引用一行 P0 或 P1，或解释为何更低档进入范围。
- 任何声称遵循本 note 的 PR 都不改 `dsh-agent-loop`、不提升 `SESSION_FORMAT_VERSION`、不实现打包 schema 17。
- [移植 Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.zh.md) 与 [rust/README.zh.md](../../../../rust/README.zh.md) 链到该清单。

## 风险

crate 落地后清单会漂移。要在同一个 PR 里更新这些文件，否则排序会变成第二份过期真源。

读者仍可能把「缺 crate」当成「必须移植」。skip 表与 Web UI 否决必须留在 P0 列表旁边。

只关闭 persistence 或 SSE、不做附件，会让 ACP 图像继续广告为 false。清单里的关闭顺序考虑了依赖；跳过前提会把 P0 行重新打开。
