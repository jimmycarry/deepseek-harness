# Agent Note: Align Rust skill incomplete last-good catalog

Status: implemented

English | [中文](2026-09-14-rust-skill-incomplete-snapshot.zh.md)

## Problem

TypeScript skill discovery treats unexpected I/O as an incomplete observation. `tool-skill` does not publish that observation, so the session keeps the last complete `skill-catalog` message. Confirmed missing roots and deleted skill files remain complete empty state.

Rust `apply_scan` replaced `ctx.skills` from every scan. `load_dir` errors, including `SKILL.md` that is a directory, were skipped as if the skill were absent. A transient root failure therefore unregistered last-good names and let `tool-skill` publish a smaller catalog. Flat `ctx.skills` also had no completeness bit, so the model catalog could not tell a finished empty scan from a failed one.

Rewriting `dsh-skill` into TypeScript's provider-scoped `invalidate()` and uncached incomplete candidate lists would delay the catalog-publication fix. Watch already updates the flat registry.

## Decision

`SkillRuntime` stores a completeness bit next to the flat name map. `snapshot()` returns `{skills, complete}`. A new registry starts complete.

`dsh-skill-filesystem` `scan` returns `SkillScan { skills, complete }`. A missing root (`NotFound`) is empty complete state for that root. Any other `read_dir` or skill-file read failure marks the observation incomplete. Malformed frontmatter still skips that entry without failing the scan.

`apply_scan` on an incomplete observation calls `set_complete(false)` and leaves the previous registrations in place. A complete observation replaces the owned names and sets `complete` true. Confirmed deletion is a complete scan and unregisters the missing skill.

`dsh-tool-skill` returns the current pre-step payload without appending a catalog message while `ctx.skills` is incomplete. The last published digest stays. `dsh-agent-loop` is unchanged.

Rust still uses one flat registry rather than per-provider uncached candidates. `get` during an incomplete observation therefore still returns last-good bodies. TypeScript `snapshot()` can show empty candidates from the failed listing while the session catalog stays last-good. That listing-vs-registry difference stays thinner.

[The watch alignment](2026-09-13-rust-skill-filesystem-watch.md) still owns polling. [The TypeScript hot-refresh decision](2026-07-27-skill-catalog-hot-refresh.md) still owns Chokidar options. TypeScript remains the behavior source for publication.

## Alternatives considered

**Replace the registry with the failed scan's candidates (often empty) and mark incomplete.** Rejected because `skill` `get` would lose last-good bodies while the model catalog still named them. TypeScript last-good lives in the session message; Rust last-good also stays in `ctx.skills` so the loader matches the catalog.

**Rewrite `dsh-skill` for provider `invalidate()` and uncached incomplete lists.** Rejected because catalog publication only needs a completeness bit and a non-replacing incomplete `apply_scan`.

**Treat every `read_dir` error as a missing root.** Rejected because `NotADirectory` and permission failures are not confirmed absence, and TypeScript marks those incomplete.

## Testing

`dsh-skill` covers `snapshot()` completeness after `set_complete`. `dsh-skill-filesystem` covers a missing custom root as complete empty, a file used as a skill root as incomplete last-good, `SKILL.md` that is a directory as incomplete, and a confirmed bundle deletion as complete removal. `dsh-tool-skill` covers a later pre-step that does not publish while incomplete.

## Consequences

A transient host I/O failure no longer deletes the model catalog. Confirmed removals still republish. Provider-scoped invalidation and TypeScript's empty incomplete `snapshot()` candidates stay thinner.
