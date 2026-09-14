# Agent Note: 对齐 Rust session-query 表面追踪与过滤

Status: implemented

[English](2026-09-14-rust-session-query-surface.md) | 中文

## 问题

Headless、ACP 与 JSON-RPC 都会挂上 `openAt: never` 的 `ctx.sessionQuery`。TypeScript 仍用一次保留替换历史的 `foldSurface` 给每条事件分类，并暴露 `listEvents`、`readSurface`、`traceEvent`、`filterSessions` 与 `filterEvents`。Rust 的 `SessionSurface` 只保留当前 `nodes` 与 `replace_generation`。精确列表与谱系已在 [谱系对齐 Agent Note](2026-09-14-rust-session-query-lineage.zh.md) 落地；事件表面追踪与提供方无关的过滤仍更薄。启用 SQLite FTS，或把 TypeScript 的打包 schema 8 当成 Rust 目标，会重开剩余的搜索行和预发布格式规则。

## 决策

`dsh-session::fold_surface` 把一份完整连续日志按 TypeScript 的资格、出处与 tool-result 改写检查重放，并返回当前节点加 `replacements[{seq,start,end,shadowed_seqs}]`。增量实时追加仍走 `SessionSurface::apply`。分离 fold 失败是带 TypeScript 文句的 `SessionError::InvalidSurface`。

`dsh-session-query` 通过 `load_logical` 加载表面、事件追踪与事件过滤的源。已知实时会话返回分离快照，不咨询 persistence。否则服务先列出持久化 header，id 不在则失败为 `session "{id}" not found` / `SESSION_QUERY_SESSION_NOT_FOUND`，再 inspect 存储日志，优先使用 inspect 期间变成实时的会话，然后对 inspect 与列表 header 做 `assert_session_headers_compatible`。已挂载的列表失败仍是 `session persistence listing failed: {error}` / `SESSION_QUERY_PERSISTENCE_FAILED`。inspect 失败是 `failed to inspect session "{id}": {error}` / `SESSION_QUERY_PERSISTENCE_FAILED`。fold 失败是 `invalid session surface: {error}` / `SESSION_QUERY_INVALID_SURFACE`。

`list_events` 把每条原始事件标为 `current`、`shadowed` 或 `log-only`。`read_surface` 返回克隆的 header、`captured_through_seq`（最后一条原始 seq，空日志为 `None`）以及当前表面事件。`trace_event` 在 fold 之前确认 `events[seq]` 存在且 `event.seq == seq`，再返回 `replaced_by`、`replacement_chain`、`replaced_event_seqs`、`source_event_seqs` 与 `derived_event_seqs`。缺失目标是 `session "{id}" has no event at seq {seq}` / `SESSION_QUERY_EVENT_NOT_FOUND`。

`filter_sessions` 与 `filter_events` 对子句做 AND，对列表值做 OR。会话子句是 `id`、`cwd`、`created-at`、`parent` 与 `availability`（`live` | `persisted`）。事件子句是 `seq`、`time`、`type`、`surface` 与 `text`。范围要求有限的 `from` / `to` 且 `from <= to`。文本是对 `extract_session_event_text` 的字面、大小写不敏感、空白灵活扫描；空文本是 `session text filter must contain non-whitespace text` / `SESSION_QUERY_INVALID_FILTER`。未知 kind 是 `session unknown filter kind "{kind}"`。匹配器不新增 `regex` crate。

`read_session`、`read_event` 与 `read_title` 仍通过 `PersistenceRuntime::load` 重建。`openAt` 仍为 `never`。SQLite FTS 仍为 schema 1。`dsh-agent-loop` 不变。`SESSION_FORMAT_VERSION` 保持 `0`。

[TypeScript 追踪决策](2026-07-13-session-query-tracing.zh.md) 仍然拥有关系语义。[谱系对齐 Agent Note](2026-09-14-rust-session-query-lineage.zh.md) 仍然拥有最新优先列表行与父子追踪。[移植 Agent Note](../architecture/2026-08-22-rust-harness-port.zh.md) 仍然拥有 1:1 规则。行为真源仍是 TypeScript。

## 考虑过的替代方案

**在同一次改动里启用 FTS 或抬高 `openAt`。** 否决，因为两棵树的 base 都交付 `openAt: never`，且 Rust schema 1 不是 TypeScript schema 8。表面追踪与过滤不需要索引。

**让持久化表面读取走 `Session::append_logged`。** 否决，因为那样会在 `fold_surface` 发出 `SESSION_QUERY_INVALID_SURFACE` 之前就拒绝畸形日志。`load_logical` inspect 存储事件并就地 fold。

**在实时 `SessionSurface::apply` 上保留替换历史。** 否决，因为 TypeScript 的增量表面同样丢掉历史；只有分离 fold 返回 `replacements`。查询 API 对一次完整观察做 fold。

**为文本子句加入工作区 `regex` crate。** 否决，因为 TypeScript 编译的是带 Unicode 大小写折叠与灵活空白的转义字面量。手写 token 匹配器保持该约定，且不引入新依赖。

## 测试

`dsh-session` crate 测试覆盖空 fold、记录的 `shadowed_seqs`、非表面引用、替换缺少被遮蔽 source，以及重复 source。`dsh-session-query` crate 测试覆盖 current/shadowed/log-only 分类、带 `captured_through_seq` 的分离当前表面、`None` 的空表面、替换链 `[4, 8]`、列表之后的实时优先 inspect、列表与 inspect 的持久化失败、列表与 inspect header 冲突、无效表面之前的目标未找到、`list_events` 与 `trace_event` 上的畸形持久化日志，以及会话/事件过滤（含空文本与未知 kind 文句）。

## 后果

已交付 profile 可以读取当前表面、给原始日志事件分类、追踪位置替换，并应用提供方无关的过滤，而无需打开 FTS。畸形持久化日志会以 `SESSION_QUERY_INVALID_SURFACE` 失败，而不是 Session 重建错误。全文搜索仍更薄，归属 [剩余工作排序](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md)。
