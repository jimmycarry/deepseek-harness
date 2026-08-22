# Agent Note: Rust harness 移植以 TypeScript 为行为真源

Status: implemented

[English](2026-08-22-rust-harness-port.md) | 中文

## Problem

第二种实现语言不能发明第二套产品。如果 Rust 树自造 loop、事件名或会话日志规则，之后每次改动都会有两个答案，也无法说明用户拿到的是哪一套。`packages/` 下的 TypeScript 树已经拥有这些名字：没有特权内核，正在运行的 `dsh` 就是由 profile、bundle 与 patch 层组装出来的 Cordis 插件树。

## Decision

`rust/` 是一个 Cargo workspace，移植同一棵插件树、同一组能力 seam 和同一套会话日志约定。crate 名为 `dsh-<pkg>`；目录对齐 `packages/<group>/<pkg>/`。TypeScript 仍是行为真源：Rust 侧若要偏离某个 TypeScript 名字或日志事件，必须先改 TypeScript 真源，否则不能落地。

工作区目标为 Rust 1.83 与 edition 2021。会拉入 edition 2024 或 `hashbrown` 0.17 的依赖在 lockfile 处拒绝。Loader YAML 是手写子集，不使用 `serde_yaml`；HTTPS 提供方调用走 `curl`，不使用 `reqwest`；宿主 HTTP 监听器是手写 HTTP/1.1，不使用 `axum`。`Branded<B>` 手写 `Clone` 与 `Debug`，品牌标记类型因此不必实现这些 trait。

能力 seam 必须完整：Service Definition、Service Provider、Consumer。工具 Consumer 只依赖 Definition crate。随部署变化的值是构造时传入的 `Config` 字段；`run` 不隐藏默认值。模型可见内容必须记入日志；压缩用 `surfaceOp: replace` 推进 surface，不删除历史。会话日志保持 `SESSION_FORMAT_VERSION` `0`；SQLite 后端使用单调递增的 `SCHEMA_VERSION` `1`。未知且 required-on-read 的事件类型会拒绝 resume，除非信封带 `ignorable: true`。

默认驱动在 `dsh-agent-loop`，并且保持为插件。`max-tokens` 在当前 turn 内粘滞。`agent/turn-stopping` 可以 `steer` 再开一步。第一次 `cancel` 的原因获胜。工具 body 的重叠上限是 `ToolRuntimeConfig.max_parallel`；`tools/post-execute` 按模型顺序提交。`apply_world` 在 spine 上挂载 sandbox、文件系统、shell 及其工具。`dsh --dump-config` 打印 `shipped_bundles` 给出的已交付 bundle 栈。斜杠命令由 `ctx.commands` 分派，不进入模型。

## Alternatives considered

- **先把 loop 做成一个 `async fn`，以后再插件化** — TypeScript 规则正好相反：新行为落在已文档化的扩展点上。单体实现会丢掉 `agent/pre-step`、`agent/request`、`agent/request-error` 和 `agent/turn-stopping` 这些插件真正运行的位置。
- **用 Rust 重写 Web UI** — 宿主只需讲现有客户端协议。第二套 UI 是产品分叉，不是移植。
- **把压缩做进 loop** — TypeScript 压缩是 `agent/pre-step`、`agent/request-error` 与空闲 maintenance 的 Consumer。放进 `step` 会让之后每个引擎都变成 loop 改动。
- **把 Python SDK 当成第二套 loop** — 两个 SDK 都投影 TypeScript loop。再让 Rust loop 被这些 SDK 重实现，就会变成第三套驱动。
- **`serde_yaml` / `reqwest` / `axum`** — 各自会拉入 edition 2024 或 `hashbrown` 0.17 图，Rust 1.83 编不过。手写 YAML 子集、`curl` 传输和 HTTP/1.1 监听器把工作区钉在已声明的工具链上。
- **为旧磁盘格式做兼容垫片** — 预发布立场拒绝旧后端。垫片会把格式错误藏到第一次打 tag。

## Consequences

在打 tag 发布之前，仓库会同时带着两棵语言树。`rust/` 下的 `cargo test --workspace` 是 Rust 侧证据；无密钥 headless 快照在 `rust/apps/cli/tests/headless_snapshot.rs`，覆盖文本轮次、`bash` 轮次，以及从宿主重读文件的 `write_file` 轮次。`--dump-config` 是组合转储，不是插件身份的第二来源。

之后的 crate 若需要默认值，必须在构造时作为 `Config` 传入。之后的事件类型若要让旧的 Rust 读取器跳过，必须设 `ignorable: true`；漏标会拒绝 resume。改 `dsh-agent-loop` 仍然要同步更新 [docs/architecture.zh.md](../../../../docs/architecture.zh.md)。
