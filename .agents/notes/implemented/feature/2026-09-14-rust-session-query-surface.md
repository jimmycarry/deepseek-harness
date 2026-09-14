# Agent Note: Align Rust session-query surface traces and filters

Status: implemented

English | [中文](2026-09-14-rust-session-query-surface.zh.md)

## Problem

Headless, ACP, and JSON-RPC mount `ctx.sessionQuery` with `openAt: never`. TypeScript still classifies every event through one `foldSurface` pass that retains replacement history, then exposes `listEvents`, `readSurface`, `traceEvent`, `filterSessions`, and `filterEvents`. Rust `SessionSurface` kept only current `nodes` and `replace_generation`. Exact list and lineage already shipped in [the lineage alignment Agent Note](2026-09-14-rust-session-query-lineage.md); event-surface traces and provider-independent filters stayed thinner. Enabling SQLite FTS, or treating TypeScript packed schema 8 as a Rust target, would reopen the remaining search row and the pre-release format rule.

## Decision

`dsh-session::fold_surface` replays one complete contiguous log through the TypeScript eligibility, provenance, and tool-result rewrite checks and returns current nodes plus `replacements[{seq,start,end,shadowed_seqs}]`. Incremental live append still uses `SessionSurface::apply`. A detached fold failure is `SessionError::InvalidSurface` with the TypeScript sentence.

`dsh-session-query` loads surface, event-trace, and event-filter sources through `load_logical`. A known live session returns a detached snapshot and does not consult persistence. Otherwise the service lists persisted headers, fails with `session "{id}" not found` / `SESSION_QUERY_SESSION_NOT_FOUND` when the id is absent, inspects the stored log, prefers a session that became live during inspect, and then runs `assert_session_headers_compatible` on the inspected and listed headers. A mounted listing failure stays `session persistence listing failed: {error}` / `SESSION_QUERY_PERSISTENCE_FAILED`. An inspect failure is `failed to inspect session "{id}": {error}` / `SESSION_QUERY_PERSISTENCE_FAILED`. A fold failure is `invalid session surface: {error}` / `SESSION_QUERY_INVALID_SURFACE`.

`list_events` classifies each raw event as `current`, `shadowed`, or `log-only`. `read_surface` returns the cloned header, `captured_through_seq` (the last raw seq, or `None` on an empty log), and the current surface events. `trace_event` checks that `events[seq]` exists and `event.seq == seq` before folding, then returns `replaced_by`, `replacement_chain`, `replaced_event_seqs`, `source_event_seqs`, and `derived_event_seqs`. A missing target is `session "{id}" has no event at seq {seq}` / `SESSION_QUERY_EVENT_NOT_FOUND`.

`filter_sessions` and `filter_events` AND clauses and OR list values. Session clauses are `id`, `cwd`, `created-at`, `parent`, and `availability` (`live` | `persisted`). Event clauses are `seq`, `time`, `type`, `surface`, and `text`. Ranges require finite `from` / `to` and `from <= to`. Text is a literal, case-insensitive, whitespace-flexible scan over `extract_session_event_text`; empty text is `session text filter must contain non-whitespace text` / `SESSION_QUERY_INVALID_FILTER`. An unknown kind is `session unknown filter kind "{kind}"`. The matcher does not add a `regex` crate.

`read_session`, `read_event`, and `read_title` still reconstruct through `PersistenceRuntime::load`. `openAt` stays `never`. SQLite FTS stays schema 1. `dsh-agent-loop` is unchanged. `SESSION_FORMAT_VERSION` stays `0`.

[The TypeScript tracing decision](2026-07-13-session-query-tracing.md) still owns relationship semantics. [The lineage alignment Agent Note](2026-09-14-rust-session-query-lineage.md) still owns newest-first list rows and parent/child traces. [The port Agent Note](../architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule. TypeScript remains the behavior source.

## Alternatives considered

**Enable FTS or lift `openAt` in the same change.** Rejected because both trees ship `openAt: never` on base, and Rust schema 1 is not TypeScript schema 8. Surface traces and filters do not need an index.

**Route persisted surface reads through `Session::append_logged`.** Rejected because that rebuild refuses malformed logs before `fold_surface` can emit `SESSION_QUERY_INVALID_SURFACE`. `load_logical` inspects the stored events and folds them in place.

**Keep replacement history on live `SessionSurface::apply`.** Rejected because TypeScript's incremental surface also drops history; only the detached fold returns `replacements`. Query APIs fold a complete observation.

**Add the workspace `regex` crate for the text clause.** Rejected because TypeScript compiles an escaped literal with Unicode case folding and flexible whitespace. A hand-written token matcher preserves that contract without a new dependency.

## Testing

`dsh-session` crate tests cover an empty fold, recorded `shadowed_seqs`, a non-surface citation, a replacement missing a shadowed source, and duplicate sources. `dsh-session-query` crate tests cover current/shadowed/log-only classification, a detached current surface with `captured_through_seq`, an empty surface with `None`, replacement chain `[4, 8]`, live-preferred inspect after listing, list and inspect persistence failures, listed-versus-inspected header conflict, target-not-found before invalid-surface, malformed persisted logs on both `list_events` and `trace_event`, and session/event filters including empty text and unknown kind sentences.

## Consequences

Shipped profiles can read the current surface, classify raw-log events, trace positional replacements, and apply provider-independent filters without opening FTS. A malformed persisted log fails as `SESSION_QUERY_INVALID_SURFACE` instead of a Session rebuild error. Full-text search remains thinner and stays on [the remaining-work ranking](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md).
