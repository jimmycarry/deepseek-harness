# Agent Note: 对齐 Rust SDK Session.run 与后代通知合并

Status: implemented

[English](2026-09-14-rust-sdk-session-run.md) | 中文

## 问题

TypeScript 的 `DeepSeekHarness` / `HarnessSession.run` 拥有一段活动区间：订阅会话树，入队 `session/prompt`，丢掉通知直到该 `messageId` 出现在耐久的 `agent/inbox/spliced` 回执，再收集到下一次根会话 `session.status` `idle`。`events` 只含根会话。`notifications` 按线上顺序保留树内每一帧，包括从 `subagent.started` 发现的后代。父映射活在客户端进程上，并在新运行时上重置。畸形的根 `session.event` 信封和 `assistant/message` 内容以 `SdkProtocolError` 拒绝。`finalResponse` 是该区间最后一条根助手文本，没有则为 `""`。

Rust 的 `dsh-sdk-client` 只拉起 stdio、调用 `initialize` / `prompt` / `shutdown`，并在已缓冲的全部 `session.event` 里扫描最后一段非空助手文本。它不等回执、不停在根 `idle`、不按 session id 过滤，也不合并后代通知。

把 Node 的释放阶梯、请求超时和订阅扇出一并移植，会推迟这段区间约定。那些仍更薄。

## 决策

`dsh-sdk-client` 保留 `JsonRpcClient` 作为 stdio 传输，并加上 `DeepSeekHarness` / `HarnessSession` 以及拉取式收集器。

`record_session_relationship` 只在两个 id 都非空且不相等时记录 `subagent.started` 边。`in_session_tree` 对齐 TypeScript `subscribeSessionTree`：started/finished 在父级已是根的后代时匹配，或在 `childSessionId` 等于根时匹配；其他方法在 `params.sessionId` 是根的后代时匹配。父映射成环则停止上溯。

`RunCollector` / `collect_run` 丢掉树外帧，以及匹配回执之前的每一条通知。回执之后，根会话的 `session.event` payload 经校验后写入 `events`；每条已收集帧写入 `notifications`。收集在根 `idle` 结束。有限流先结束则失败，文句为 `notification stream ended before session idle`。`final_response` 只读这些根事件，没有助手消息时返回 `""`。

`DeepSeekHarness` 记忆化 `initialize`（绝对工作区 cwd、provider、model、可选 `maxTokens`）。握手失败会回收子进程，并在之后的 `start` 重试，除非 `close` 已经结束该 harness。父映射挂在 harness 上跟随当前运行时进程，并在该进程被替换或关闭时清空。默认路由是 `deepseek-official` / `deepseek-v4-flash`。省略的 session id 铸成 `session-` 加去掉连字符的 UUID。

请求超时、带 stderr 尾的 `TransportClosedError`，以及 EOF → SIGTERM → SIGKILL 释放阶梯仍更薄。`dsh-agent-loop` 不变。

[Python/TypeScript 会话树决策](../bug-fix/2026-07-24-recursive-python-sdk-session-notifications.zh.md) 仍然拥有关系语义。[TypeScript SDK 决策](2026-07-27-typescript-sdk-and-sdk-subagent-backend.zh.md) 仍然拥有 Node 客户端。行为仍以 TypeScript 为真源。

## 考虑过的替代方案

**在同一次改动里移植完整的 Node `HarnessClient`（过滤订阅、请求超时、EOF → SIGTERM → SIGKILL）。** 否决，因为未关的 P1 行是这段被拥有的 run 区间与后代合并。瘦的 stdio 传输已经能驱动 `initialize` / `prompt` / `shutdown`。

**继续在全部通知里扫描最后一段非空助手文本。** 否决，因为 TypeScript 的 `finalResponse` 是回执到 idle 区间里最后一条根 `assistant/message`（包括空文本），从不是子会话消息。

**每次 `run` 使用一张新的父映射。** 否决，因为 TypeScript 在客户端生命周期内保留祖先，使更早一轮发现的后代仍在树内，并且只在新的运行时进程启动时重置该映射。

**校验树内每一条 `session.event`，包括后代。** 否决，因为 TypeScript 只校验进入 `events` 的根 `session.event` 信封。

## 测试

`dsh-sdk-client` 的 crate 测试覆盖回执前丢弃、仅根 `events` 且 `notifications` 含后代、最后一条根 `final_response`、空区间 `""`、环与空/自环边、跨第二段区间仍在的父映射、畸形根信封、idle 前流出错、失败 `initialize` 之后的握手重试，以及脚本化 stdio 运行时上的 `DeepSeekHarness.run`（含子会话）。

## 后果

Rust SDK 消费方可以拥有与 TypeScript 相同的回执到 idle 区间，而不会把子会话助手消息算到根上。请求超时与 Node 释放阶梯仍更薄，见 [剩余工作排序](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md)。
