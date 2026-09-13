# Agent Note: Align Rust skill-filesystem watch without Chokidar

Status: implemented

English | [中文](2026-09-13-rust-skill-filesystem-watch.zh.md)

## Problem

TypeScript `@deepseek-ai/dsh-skill-filesystem` observes host skill roots so a catalog change from an IDE, Git, or shell is visible at the next model step without a restart. Rust already scanned on `agent/pre-step` and on any skill-path `fs/observed`. That missed `{agentsHome}/skills`, skipped no `.system` child under `{dshHome}/skills`, treated the configured cwd as the project root instead of the nearest `.git` ancestor, and left missing roots unobserved until some other event rescanned.

Porting Chokidar into Rust would add a Node watcher or a native `notify` crate. [The TypeScript hot-refresh decision](2026-07-27-skill-catalog-hot-refresh.md) already deferred a generic Cordis file-watch service. A native watcher that TypeScript does not use on this path would be a second observation mechanism, not a 1:1 adapter.

## Decision

`dsh-skill-filesystem` polls catalog-relevant host paths. `Config::resolve` accepts the TypeScript watch fields and the same positive-integer sentences: `watch` (default `true`), `watchUsePolling` (default `false`), `watchStabilityThresholdMs` (default `200`), `watchPollIntervalMs` (default `100`), `watchMaxProjects` (default `128`), and `watchFollowSymlinks` (default `true`). `watchUsePolling` is stored so cordis.yml matches TypeScript; both values poll. There is no Chokidar, `notify`, or inotify watcher.

Discovery roots stay in rank order: project `.dsh/skills` and `.agents/skills` from the nearest `.git` ancestor (else the supplied cwd), then `customSkillDirs`, then `{dshHome}/skills` (skip `.system`) and `{agentsHome}/skills`, then `bundledSkillDir` or `$DSH_BUNDLED_SKILL_DIR` when default roots are included. `includeDefaultRoots: false` drops project, user, and the bundled environment default.

An existing root is sampled at `watchPollIntervalMs`. The snapshot is direct child directories plus their `SKILL.md` content fingerprint, and direct `*.md` files. Bundle `references` / `scripts` / `assets` do not change that snapshot. A changed snapshot must remain stable for `watchStabilityThresholdMs` before `apply_scan`. A missing root is followed from the nearest existing ancestor one segment at a time on the same interval; when the real root appears, observation switches to the existing-root snapshot. Project roots use a `watchMaxProjects` LRU. `watch: false` starts no poll thread and still retains roots so first-party observation can match.

`write` and `edit` `fs/observed` events rescan immediately when the path is a catalog-relevant skill entry. `FsObservationActor` carries optional `name` so those tools publish `{name:"write"|"edit"}`. `read` and unnamed actors do not take the fast path. `agent/pre-step` still rescans and refreshes the observed project set. Catalog publication remains `dsh-tool-skill` on the next pre-step digest change. `dsh-agent-loop` is unchanged.

Invalidation is `apply_scan` on the flat `ctx.skills` registry. Rust has no provider-scoped `invalidate()` or incomplete snapshot bit. A transient `read_dir` failure keeps the last snapshot and does not treat the root as deleted. Effect teardown sets the stop flag, drops the wake channel, and joins the poll thread. `Drop` does the same if the context is discarded without `dispose`.

[The port Agent Note](../architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule. TypeScript remains the behavior source for Chokidar options and incomplete last-good catalogs.

## Alternatives considered

**Depend on Chokidar or wrap it from Node.** Rejected because the Rust tree must not take a Node watcher dependency, and the user constraint for this alignment forbids introducing Chokidar.

**Use the `notify` crate as a native Chokidar stand-in.** Rejected because TypeScript's missing-root path is already `fs.watchFile` polling, existing-root Chokidar can be `usePolling: true`, and a second native event source would diverge on teardown, symlink, and Windows 8.3 behavior. Interval polling is the adapter, not a leftover gap.

**Keep `agent/pre-step` and `fs/observed` as the only refresh.** Rejected because IDEs, Git, and shell writes never cross that path, and a missing skills directory at startup would stay invisible until an unrelated rescan.

**Extract a generic Cordis file-watch service in this change.** Rejected for the same deferral as [the TypeScript hot-refresh note](2026-07-27-skill-catalog-hot-refresh.md): no second consumer has established that service contract.

**Rewrite `dsh-skill` for provider-scoped invalidation and incomplete snapshots here.** Rejected because watch can update the flat registry with `apply_scan`. Last-good retention on unexpected I/O remains a thinner registry row, not a blocker for host observation.

## Testing

`dsh-skill-filesystem` crate tests cover default and rejected watch integers, `{agentsHome}/skills`, skipped `.system`, `.git` project-root walk, `includeDefaultRoots: false`, existing-root add/remove/frontmatter after the stability window, ignored bundle resource edits, a missing root that later appears, `watch: false` with no poll thread, project LRU eviction, joined teardown, and `write` versus `read` `fs/observed`. `dsh-fs` covers actor `name` round-trip. `dsh-tool-fs` write and edit publish that name.

## Consequences

External skill-root edits become visible before the next `agent/pre-step` catalog digest without a Node watcher. Detection latency is bounded by `watchPollIntervalMs` plus `watchStabilityThresholdMs`. Headless default `watch: true` starts one poll thread per mounted provider; dispose joins it. Incomplete last-good catalog publication stays on the TypeScript registry contract and is still thinner in Rust.
