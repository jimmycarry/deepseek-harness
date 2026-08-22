# DeepSeek Harness — Rust 移植

[English](README.md) | 中文

TypeScript harness 的语义 1:1 移植。`packages/` 下的 TypeScript 树仍是行为真源。本工作区保留插件树、能力 seam 与会话日志约定。组合身份是 TypeScript 行 `id` 加插件 `name`；`dsh --dump-config` 打印这棵组合树。跑任务会挂载同一棵树；`!!js` 只在挂载时求值。会话 JSONL 以 TypeScript 的 header 行开头，每个事件序列化为 `{type, seq, time, data}` 并带 `sourceEventSeqs` 引用；消息携带 `role` 与 UUID `id`，并保留 TypeScript 的 `StreamChunk` 标签。会话标题真实运行：确定性回退加 first-prompt LLM provider，后者在辅助调用之前记录 `session/title-llm-request`。Headless 会挂上 `bash-sandbox`、`glob` / `grep`（经 `ctx.subprocess` 拉起 ripgrep）、`str_replace_editor`、`create_goal` / `get_goal` / `update_goal`、一次性 `subagent` / `subagent_fork`、`workflow`（`return <json>`）、`web_search`（DeepSeek official 或 `replay` 夹具）、`repeat-tool-reminder`、`ctx.attachments`、`ctx.sessionQuery`（`openAt: never`）、spill（`ctx.spillStore` 加 `spill-policy`）、`todo_write`（日志态 `todo/write`）、工具调用超时策略（按工具声明的 `timeout_ms`，`TOOL_TIMEOUT` 失败文案）、压缩期工具结果裁剪器（`compaction/prune` 加 `tool/result` replace）、plan mode（`plan/mode`、`exit_plan_mode`、`/plan`；headless 无 user-questions provider，plan 评审按精确文句失败）、skill 栈（`SKILL.md` 发现、`skill` 工具、按 agent 一次的 `skill-catalog` 消息）、`ctx.tokenMeter`，以及压缩（携带 `compactionId` 的 `compaction/start` / `summary` / `end` 加 `/compact`）。受限制的 bash 在没有可用的 bwrap 或 landlock-run 后端时以 `SANDBOX_UNAVAILABLE` 拒载，禁止无隔离执行。base 的 `tool-web` 保持 `fetch: false`。continuable 后台子 agent、JavaScript workflow worker、打包的 SQLite schema 17、栅格图像归一化、带比例阈值的 LLM 压缩摘要与 skill 文件监听未挂载。

crate 名为 `dsh-<pkg>`；目录对齐 `packages/<group>/<pkg>/`。

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

产品地图见 [docs/architecture.zh.md](../docs/architecture.zh.md)。
