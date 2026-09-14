# Agent Note: Align Rust SDK Session.run and descendant notification merge

Status: implemented

English | [中文](2026-09-14-rust-sdk-session-run.zh.md)

## Problem

TypeScript `DeepSeekHarness` / `HarnessSession.run` owns one activity interval: subscribe the session tree, queue `session/prompt`, drop notifications until the durable `agent/inbox/spliced` receipt for that `messageId`, then collect through the next root `session.status` `idle`. `events` stays root-scoped. `notifications` keeps every in-tree frame in wire order, including descendants discovered from `subagent.started`. The parent map lives for the client process and resets on a new runtime. Malformed root `session.event` envelopes and `assistant/message` content reject as `SdkProtocolError`. `finalResponse` is the last root assistant text in that interval, or `""`.

Rust `dsh-sdk-client` only spawned stdio, called `initialize` / `prompt` / `shutdown`, and scanned every buffered `session.event` for the last non-empty assistant text. It did not wait for the receipt, did not stop at root idle, did not filter by session id, and did not merge descendant notifications.

Porting the Node dispose ladder, request timeouts, and subscription fan-out would delay the interval contract. Those stay thinner.

## Decision

`dsh-sdk-client` keeps `JsonRpcClient` as the stdio transport and adds `DeepSeekHarness` / `HarnessSession` plus a pull collector.

`record_session_relationship` records a `subagent.started` edge only when both ids are non-empty and differ. `in_session_tree` matches TypeScript `subscribeSessionTree`: started/finished frames match when the parent is a descendant of the root, or when `childSessionId` equals the root; other methods match when `params.sessionId` is a descendant. A parent-map cycle stops the walk.

`RunCollector` / `collect_run` drop out-of-tree frames and every notification before the matching inbox receipt. After the receipt, root `session.event` payloads are validated and stored on `events`; every collected frame is stored on `notifications`. Collection ends on root `idle`. A finite stream that ends first fails with `notification stream ended before session idle`. `final_response` reads only those root events and returns `""` when no assistant message exists.

`DeepSeekHarness` memoizes `initialize` (absolute workspace cwd, provider, model, optional `maxTokens`). A failed handshake reaps the child and retries on a later `start` unless `close` already ended the harness. The parent map lives on the harness for the runtime process and clears when that process is replaced or closed. Default route is `deepseek-official` / `deepseek-v4-flash`. Omitted session ids mint `session-` plus a hyphen-stripped UUID.

Request timeout, stderr-tail `TransportClosedError`, and the EOF → SIGTERM → SIGKILL dispose ladder stay thinner. `dsh-agent-loop` is unchanged.

[The Python/TypeScript session-tree decision](../bug-fix/2026-07-24-recursive-python-sdk-session-notifications.md) still owns relationship semantics. [The TypeScript SDK decision](2026-07-27-typescript-sdk-and-sdk-subagent-backend.md) still owns the Node client. TypeScript remains the behavior source.

## Alternatives considered

**Port the full Node `HarnessClient` (filter subscriptions, request timeout, EOF → SIGTERM → SIGKILL) in the same change.** Rejected because the open P1 row is the owned run interval and descendant merge. The thin stdio transport already drives `initialize` / `prompt` / `shutdown`.

**Keep scanning every notification for the last non-empty assistant text.** Rejected because TypeScript `finalResponse` is the last root `assistant/message` in the receipt-to-idle interval, including empty text, and never a child message.

**Give each `run` a fresh parent map.** Rejected because TypeScript keeps ancestry for the client lifetime so a descendant discovered on an earlier turn remains in-tree, and resets the map only when a new runtime process starts.

**Validate every in-tree `session.event`, including descendants.** Rejected because TypeScript validates only root `session.event` envelopes that enter `events`.

## Testing

`dsh-sdk-client` crate tests cover receipt-before drop, root-only `events` with descendant `notifications`, last-root `final_response`, empty-interval `""`, cycles and empty/self-loop edges, a parent map that survives a second interval, malformed root envelopes, stream end before idle, handshake retry after a failed `initialize`, and a scripted stdio runtime that exercises `DeepSeekHarness.run` including a child session.

## Consequences

A Rust SDK consumer can own the same receipt-to-idle interval as TypeScript without attributing a child assistant message to the root. Request timeout and the Node dispose ladder remain thinner and stay on [the remaining-work ranking](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md).
