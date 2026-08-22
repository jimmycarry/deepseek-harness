# Agent Note: Rust harness 移植以 TypeScript 为行为真源

Status: implemented

[English](2026-08-22-rust-harness-port.md) | 中文

## Problem

第二种实现语言不能发明第二套产品。如果 Rust 树自造 loop、事件名或会话日志规则，之后每次改动都会有两个答案，也无法说明用户拿到的是哪一套。`packages/` 下的 TypeScript 树已经拥有这些名字：没有特权内核，正在运行的 `dsh` 就是由 profile、bundle 与 patch 层组装出来的 Cordis 插件树。

## Decision

`rust/` 是一个 Cargo workspace，移植同一棵插件树、同一组能力 seam 和同一套会话日志约定。crate 名为 `dsh-<pkg>`；目录对齐 `packages/<group>/<pkg>/`。TypeScript 仍是行为真源：Rust 侧若要偏离某个 TypeScript 名字或日志事件，必须先改 TypeScript 真源，否则不能落地。

工作区目标为 Rust 1.83 与 edition 2021。会拉入 edition 2024 或 `hashbrown` 0.17 的依赖在 lockfile 处拒绝。Loader YAML 是手写子集，不使用 `serde_yaml`，并解析 Include 方言：`- insert:` 列表、按 id 整字段替换、`inject`、`|` / `>-` 块、flow 序列，以及存成 `{ "__jsExpr" }` 的 `!!js`。HTTPS 提供方调用走 `curl`，不使用 `reqwest`；宿主 HTTP 监听器是手写 HTTP/1.1，不使用 `axum`。`Branded<B>` 手写 `Clone` 与 `Debug`，品牌标记类型因此不必实现这些 trait。

能力 seam 必须完整：Service Definition、Service Provider、Consumer。工具 Consumer 只依赖 Definition crate。随部署变化的值是构造时传入的 `Config` 字段；`run` 不隐藏默认值。模型可见内容必须记入日志；压缩用 `surfaceOp: replace` 推进 surface，不删除历史。会话日志保持 `SESSION_FORMAT_VERSION` `0`；SQLite 后端使用单调递增的 `SCHEMA_VERSION` `2`，并把会话 header 与事件行一并存储。未知且 required-on-read 的事件类型会拒绝 resume，除非信封带 `ignorable: true`。

组合身份是行 `id` 加上 TypeScript 插件 `name`（`@deepseek-ai/dsh-*` 或 `@deepseek-ai/cordis-plugin-*`），不是 Rust crate 名。`dsh --dump-config` 与 `compose_profile` 对 TypeScript 的 `dsh-base` 再 `dsh-headless` patch 文件共用一次扁平的 `apply_entry_patches`；`!!js` 原文打印、不求值。缺目标 id 或 name 不匹配会拒载。默认驱动在 `dsh-agent-loop`，并且保持为插件。`max-tokens` 在当前 turn 内粘滞。`agent/turn-stopping` 可以 `steer` 再开一步。第一次 `cancel` 的原因获胜。工具 body 的重叠上限是 `ToolRuntimeConfig.max_parallel`；`tools/post-execute` 按模型顺序提交。跑任务时组合同一棵树、登记每个插件名并挂载。`disabled` 与 `config` 上的 `!!js` 在挂载时求值（`process.env.*`、`process.platform`、`process.cwd()`、`dshHomePath`、`ctx.<service>.<field>`）。spine 行、default-model、persistence、sandbox-policy、approval、permission、credentials、settings、fs-sandbox、bash-sandbox、tool-fs-search、tool-str-replace-editor、goal / goal-round-driver / command-goal / tool-goal、subagent spawn/fork、tool-subagent、workflow-worker-thread / tool-workflow、repeat-tool-reminder、web / web-search-deepseek / tool-web、attachment-local、session-query-sqlite、spill-local / spill-policy 以及 headless startup/runner 有真实 apply；其余名字以 no-op 挂上。`ctx.sandbox.confine` 先经 bwrap、再经 landlock-run 包装 argv；两者都不可用时按 TypeScript 的 `SANDBOX_UNAVAILABLE` 文句拒载，禁止无隔离执行。`danger-full-access` 下 bash 不隔离。`glob` 与 `grep` 经 `ctx.subprocess` 拉起 `rg`。`str_replace_editor` 只用 `ctx.fs`，且要求绝对路径。`ctx.goals` 是事件源：变更追加 `goal/change`；`goal-round-driver` 在 `goal/changed` 上为已武装的 active goal 排队 `<goal_round>` followup。模型工具是 `create_goal` / `get_goal` / `update_goal`；`/goal` 对模型不可见。`tools/post-execute` 携带 `name`、`args`、`agentId`、`content` 和 `additionalContexts`；被拒绝的调用仍走这条瀑布。`repeat-tool-reminder` 在配置的阈值（base 为 `[3, 5, 8]`）前置插件 notice，并在 `agent/pre-step` 看到人类 `source.kind === "user"` 消息时重置该 agent 的链。`web_search` 经 `ctx.web` 走 provider id `deepseek-official`；实搜用 `curl` POST `${baseURL}/messages`，`replay` 配置给无密钥快照。base 的 `tool-web` 保持 `fetch: false`。一次性 `subagent` / `subagent_fork` 在同一 context 上跑子会话；`run_in_background: true` 会失败，因为 continuable 子 agent 未挂载。`workflow` 求值顶层 `return <json>` 并写入 `tool-workflow/run-start` / `run-end`，不拉起 JavaScript worker。`ctx.attachments` 在魔数与头部尺寸校验后把获准的源图像存到 `$DSH_HOME/attachments/v1/objects/<aa>/<sha256>`，不做栅格解码或缩小。`ctx.sessionQuery` 从 live 与已持久化日志提供精确读取、标题和谱系；headless 的 `openAt: never` 让 `searchSessions` / `searchEvents` 以 `SESSION_QUERY_SEARCH_DISABLED` 失败，并且从不打开 SQLite。`session-persistence-sqlite` 写入单调递增的 `SCHEMA_VERSION` `2`：session 行携带 JSON 会话 header，另加每个 seq 一行 JSON 事件，并拒绝任何其他 `user_version`。`spill-policy` 监听携带 `content` 的 `tools/post-execute`：纯文本结果超过 `maxInlineBytes` 时经 `ctx.spillStore` 保存全文，并把模型可见结果替换为头尾预览加 locator；跳过 `read`，保存失败则保留内联结果。dump-config 从不挂载、不求值。`apply_world` 留给不经过 profile 树的 crate 测试。斜杠命令由 `ctx.commands` 分派，不进入模型。会话 JSONL 以 TypeScript 的 header 行开头（`type: "session"`、`version`、`id`、`createdAt`、`cwd`、`delegationDepth`）；每个事件序列化为 `{type, seq, time, data}`，且只在存在时携带 `sourceEventSeqs`、`surfaceOp` 与 `ignorable`。消息携带 `role` 与 UUID `id`；`assistant/message` 通过 `sourceEventSeqs` 引用它的 `assistant/chunk` seq，`tool/result` 引用它的 `tool/call` seq。`StreamChunk` 使用 TypeScript 的 `type` 标签（`block-start`、`text-delta`、`tool-call-delta`、`block-end`、`usage`、`finish`），`FinishReason` 为 `{kind}`。`TokenUsage` 使用 `inputTokens` / `outputTokens`。用户消息带 `source.kind`。loop 会记录 `agent/inbox/spliced`、`request/header`、`request/context`，以及 system-prompt 的运行时上下文快照；`request/header` 遵循 TypeScript `canonicalHeader()` 的省略规则（先 `config`，`adapterDefaults` / `system` / `tools` 只在有值时出现）。`ctx.sessionTitle` 写入确定性的回退 `session/title`；first-prompt provider 在发出 `purpose: "session-title"` 辅助调用之前，记录模型可见的精确 `session/title-llm-request`；辅助调用失败时保留现有标题。Headless 在第一次 followup 之前写入 `permission/preset`、`sandbox/mode` 和 `approval/policy`。

## Alternatives considered

- **先把 loop 做成一个 `async fn`，以后再插件化** — TypeScript 规则正好相反：新行为落在已文档化的扩展点上。单体实现会丢掉 `agent/pre-step`、`agent/request`、`agent/request-error` 和 `agent/turn-stopping` 这些插件真正运行的位置。
- **用 Rust 重写 Web UI** — 宿主只需讲现有客户端协议。第二套 UI 是产品分叉，不是移植。
- **把压缩做进 loop** — TypeScript 压缩是 `agent/pre-step`、`agent/request-error` 与空闲 maintenance 的 Consumer。放进 `step` 会让之后每个引擎都变成 loop 改动。
- **把 Python SDK 当成第二套 loop** — 两个 SDK 都投影 TypeScript loop。再让 Rust loop 被这些 SDK 重实现，就会变成第三套驱动。
- **`serde_yaml` / `reqwest` / `axum`** — 各自会拉入 edition 2024 或 `hashbrown` 0.17 图，Rust 1.83 编不过。手写 YAML 子集、`curl` 传输和 HTTP/1.1 监听器把工作区钉在已声明的工具链上。
- **为旧磁盘格式做兼容垫片** — 预发布立场拒绝旧后端。垫片会把格式错误藏到第一次打 tag。

## Consequences

在打 tag 发布之前，仓库会同时带着两棵语言树。`rust/` 下的 `cargo test --workspace` 是 Rust 侧证据；无密钥 spine 快照在 `rust/apps/cli/tests/headless_snapshot.rs`，覆盖文本轮次、`bash` 轮次、`write_file` 轮次、`glob` 轮次、从宿主重读文件的 `str_replace_editor` create、`create_goal` / `get_goal` 及已接纳的 `<goal_round>`、对着 replay provider 的 `web_search`、`workflow` 的 `return <json>`、前台 `subagent` 子会话、第三次相同 `bash` 调用注入的温和 repeat-tool-reminder notice，以及 60 000 字节 `bash` 结果被 spill-policy 替换为可从宿主重读的 locator。`rust/tests/snapshots` 下的 profile 路径快照组合 headless 树，并对类型序列与关键 payload 对照 TypeScript headless-profile 词表，包括这些编排与 spill 轮次、`session/title-llm-request` 载荷，以及 `assistant/message` 与 `tool/result` 上的 `sourceEventSeqs` 引用。`--dump-config` 打印组合后的 TypeScript 行 id 与插件名；`dsh-app-boot` 钉住 headless 的 id 序列，以及一轮会把 JSONL flush 到 `$DSH_HOME/sessions` 的 replay。continuable 子 agent 续跑、JavaScript workflow worker、TypeScript 打包的 SQLite schema 17、栅格图像归一化，以及没有 `replay` 配置的实网 DeepSeek 搜索仍未挂载。

之后的 crate 若需要默认值，必须在构造时作为 `Config` 传入。之后的事件类型若要让旧的 Rust 读取器跳过，必须设 `ignorable: true`；漏标会拒绝 resume。改 `dsh-agent-loop` 仍然要同步更新 [docs/architecture.zh.md](../../../../docs/architecture.zh.md)。
