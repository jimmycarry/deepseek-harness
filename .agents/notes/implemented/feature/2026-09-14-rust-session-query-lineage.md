# Agent Note: Align Rust session-query lineage and newest-first list

Status: implemented

English | [中文](2026-09-14-rust-session-query-lineage.zh.md)

## Problem

Headless, ACP, and JSON-RPC all mount `ctx.sessionQuery` with `openAt: never`. TypeScript still lists the live-preferred corpus newest-first, reports `live` / `persisted`, rejects conflicting immutable headers, and traces parent/child lineage from one listing. Rust listed live ids then persisted ids in `BTreeMap` order, returned `{id, title}` instead of `{header, live, persisted}`, and treated a found session as an empty ancestor list. `SessionHeader` already stores `parentSession` and `createdAt`, and `PersistenceRuntime::list_headers` already exists, so the gap was the query service, not the session log.

Enabling SQLite FTS, or treating TypeScript packed schema 8 as a Rust target, would reopen the remaining search row and the pre-release format rule. Exact list and lineage do not need an index.

## Decision

`dsh-session-query` lists and traces one live-preferred corpus observation. Persisted headers come from `PersistenceRuntime::list_headers` when that backend is mounted. Live `SessionStore` records overwrite the same id after `assert_session_headers_compatible` compares `version`, `id`, `createdAt`, `cwd`, `parentSession`, `seedLength`, and `delegationDepth`. `origin` is not compared. A conflict fails with `SESSION_QUERY_SOURCE_CONFLICT` and the TypeScript sentence `session source headers conflict for session "{id}"`. A mounted listing failure fails with `SESSION_QUERY_PERSISTENCE_FAILED` and `session persistence listing failed: {error}`.

Returned records are `{header, live, persisted}`. Titles stay on `read_title` / `fold_session_title` and are not copied onto list rows. Sort is `createdAt` descending, then `id` ascending.

`trace_session` consumes that same listing. Ancestors walk from the immediate parent outward. A target-connected cycle fails with `SESSION_QUERY_INVALID_LINEAGE` and `session lineage contains a cycle at "{id}"`. A missing parent returns `SessionLineageTrace::Incomplete` with `unresolved_parent_id`. A complete chain returns `SessionLineageTrace::Complete` whose `root` is the outermost ancestor, or the target when it has no parent. Descendants are built with an explicit stack so a deep child chain does not recurse. Direct children sort by `createdAt` ascending, then `id`. A missing target fails with `session "{id}" not found` / `SESSION_QUERY_SESSION_NOT_FOUND`.

`read_session`, `read_event`, `read_title`, and disabled search stay as they were. `openAt` stays `never`. SQLite FTS stays schema 1. `filterSessions`, `listEvents`, `readSurface`, and `traceEvent` stay thinner. `dsh-agent-loop` is unchanged. `SESSION_FORMAT_VERSION` stays `0`.

[The TypeScript tracing decision](2026-07-13-session-query-tracing.md) still owns relationship semantics. [The port Agent Note](../architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule. TypeScript remains the behavior source.

## Alternatives considered

**Enable FTS or lift `openAt` in the same change.** Rejected because both trees ship `openAt: never` on base, and Rust schema 1 is not TypeScript schema 8. Exact list and lineage are the headless-mounted gap.

**Keep `{id, title}` list rows and walk parents only when a caller already has a live session.** Rejected because TypeScript list rows are header plus availability bits, titles are a separate fold, and lineage is a cross-corpus listing so a persisted-only parent or child is visible.

**Recurse when building descendant trees.** Rejected because the TypeScript walk is an explicit stack so a deep child chain does not consume the call stack.

**Compare whole headers, including `origin`.** Rejected because TypeScript's compatibility check omits `origin`.

## Testing

`dsh-session-query` crate tests cover newest-first list order and id ties, live/persisted bits, header conflict including a `delegationDepth` mismatch with a tolerated `origin` mismatch, complete ancestry plus sorted descendants, a root and an unresolved parent, a target-connected cycle, a missing target, a persisted-only complete trace that fails after listing becomes unreadable, and a 256-deep descendant chain.

## Consequences

Shipped profiles can list sessions newest-first and recover parent/child trees without opening FTS. A mounted persistence outage still fails list and lineage even when the target is live, matching TypeScript cross-corpus semantics. Exact event-surface traces, filters, and FTS remain thinner and stay on [the remaining-work ranking](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md).
