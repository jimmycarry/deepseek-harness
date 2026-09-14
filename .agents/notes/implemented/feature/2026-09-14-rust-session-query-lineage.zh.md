# Agent Note: 对齐 Rust session-query 谱系与最新优先列表

Status: implemented

[English](2026-09-14-rust-session-query-lineage.md) | 中文

## 问题

Headless、ACP 与 JSON-RPC 都会挂上 `openAt: never` 的 `ctx.sessionQuery`。TypeScript 仍按最新优先列出实时优先语料，报告 `live` / `persisted`，拒绝冲突的不可变 header，并从一次列表追踪父子谱系。Rust 用 `BTreeMap` 顺序列出实时 id 再补持久化 id，返回 `{id, title}` 而不是 `{header, live, persisted}`，并把已找到的会话当成空祖先列表。`SessionHeader` 已经存储 `parentSession` 与 `createdAt`，`PersistenceRuntime::list_headers` 也已存在，因此缺口在查询服务，不在会话日志。

在同一次改动里启用 SQLite FTS，或把 TypeScript 的打包 schema 8 当成 Rust 目标，会重开剩余的搜索行和预发布格式规则。精确列表与谱系不需要索引。

## 决策

`dsh-session-query` 对一次实时优先语料观察做列表与追踪。若已挂载持久化后端，持久化 header 来自 `PersistenceRuntime::list_headers`。实时 `SessionStore` 记录在 `assert_session_headers_compatible` 比较 `version`、`id`、`createdAt`、`cwd`、`parentSession`、`seedLength` 与 `delegationDepth` 之后覆盖同一 id。不比较 `origin`。冲突以 `SESSION_QUERY_SOURCE_CONFLICT` 失败，文句为 TypeScript 的 `session source headers conflict for session "{id}"`。已挂载的列表失败以 `SESSION_QUERY_PERSISTENCE_FAILED` 失败，文句为 `session persistence listing failed: {error}`。

返回记录是 `{header, live, persisted}`。标题仍走 `read_title` / `fold_session_title`，不复制到列表行。排序是 `createdAt` 降序，再按 `id` 升序。

`trace_session` 消费同一份列表。祖先从直接父级向外走。与目标相连的环以 `SESSION_QUERY_INVALID_LINEAGE` 失败，文句为 `session lineage contains a cycle at "{id}"`。缺失父级返回带 `unresolved_parent_id` 的 `SessionLineageTrace::Incomplete`。完整链返回 `SessionLineageTrace::Complete`，其 `root` 是最外层祖先，若无父级则是目标本身。后代用显式栈构建，深子链不会递归。直接子级按 `createdAt` 升序，再按 `id` 排序。缺失目标以 `session "{id}" not found` / `SESSION_QUERY_SESSION_NOT_FOUND` 失败。

`read_session`、`read_event`、`read_title` 与禁用搜索保持原样。`openAt` 仍为 `never`。SQLite FTS 仍为 schema 1。`filterSessions`、`listEvents`、`readSurface` 与 `traceEvent` 仍更薄。`dsh-agent-loop` 不变。`SESSION_FORMAT_VERSION` 保持 `0`。

[TypeScript 追踪决策](2026-07-13-session-query-tracing.zh.md) 仍然拥有关系语义。[移植 Agent Note](../architecture/2026-08-22-rust-harness-port.zh.md) 仍然拥有 1:1 规则。行为真源仍是 TypeScript。

## 考虑过的替代方案

**在同一次改动里启用 FTS 或抬高 `openAt`。** 否决，因为两棵树的 base 都交付 `openAt: never`，且 Rust schema 1 不是 TypeScript schema 8。精确列表与谱系才是已挂载 headless 的缺口。

**保留 `{id, title}` 列表行，且只在调用方已有实时会话时走父级。** 否决，因为 TypeScript 列表行是 header 加可用性位，标题是单独的 fold，而谱系是跨语料列表，因此仅持久化的父级或子级也必须可见。

**构建后代树时递归。** 否决，因为 TypeScript 用显式栈走访，深子链不会消耗调用栈。

**比较整个 header，包括 `origin`。** 否决，因为 TypeScript 的兼容性检查省略 `origin`。

## 测试

`dsh-session-query` crate 测试覆盖最新优先列表顺序与 id 平局、live/persisted 位、header 冲突（含 `delegationDepth` 不匹配与可容忍的 `origin` 不匹配）、完整祖先加已排序后代、根与未解析父级、与目标相连的环、缺失目标、仅持久化的完整追踪在列表不可读后失败，以及 256 层深的后代链。

## 后果

已交付 profile 可以按最新优先列出会话并恢复父子树，而无需打开 FTS。已挂载的持久化中断仍会使列表与谱系失败，即使目标是实时会话，这也匹配 TypeScript 的跨语料语义。精确的事件表面追踪、过滤与 FTS 仍更薄，归属 [剩余工作排序](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md)。
