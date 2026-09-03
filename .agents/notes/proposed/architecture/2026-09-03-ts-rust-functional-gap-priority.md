# Agent Note: Rank remaining Rust port work by headless-first functional gaps

Status: proposed

English | [中文](2026-09-03-ts-rust-functional-gap-priority.zh.md)

## Problem

The Rust tree already ships headless, ACP, and JSON-RPC profiles under the 1:1 rule in [the port Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.md). TypeScript still has 227 packages; Rust has 112 crates. Without an agreed order, the next crate can chase Web UI, packed SQLite schema 17, or a loop edit and still look like progress while remaining P0 is the reported finish-chunk gap and remaining thinner paths (DeepSeek Files API, session-query FTS, skill watch) stay open.

Leaf-name matching also lies: `typert/protocol` is not `sdk/protocol`. A maintainer who ports “every missing folder name” will duplicate servers that already exist under another group.

## Proposal

Adopt [rust/docs/ts-rust-functional-gaps.md](../../../../rust/docs/ts-rust-functional-gaps.md) as the working inventory and this ranking as the sequence for remaining Rust work. TypeScript under `packages/` stays the behavior source. A row that is **aligned** or **skip** is not a missing crate.

P0 is Linux correctness for the shipped headless / ACP / JSON-RPC profiles. P1 is durability and ops on those profiles. P2 is other platforms or a Rust host that speaks the existing TypeScript client protocol. P3–P5 are opt-in, experimental, and utilities. Rewriting `packages/client/*` in Rust stays rejected. `dsh-agent-loop` is not the place that closes the finish-chunk row. `SESSION_FORMAT_VERSION` stays `0`. Rust SQLite `SCHEMA_VERSION` `2` stays an independent pre-release format.

[The port Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule and the shipped set. This note owns only the remaining-work order.

## Priority bands

| Band | Owns | First items |
|---|---|---|
| P0 | Shipped-profile correctness on Linux | Items 1–6 closed (persistence coordinator, DeepSeek SSE + image blocks, attachment normalize, ACP images, settings Service Definition, headless plan review). Remaining: report the loop finish-chunk gap |
| P1 | Durability and ops on those profiles | DeepSeek Files API upload; projection cache; session-query FTS; enable `web-fetch-http`; skill watch; OTel flush hint; SDK client helpers; external subagent providers; `tool-ask-user` |
| P2 | Platforms and adjacent products | Rust host of the existing TypeScript SPA; Typert/API; Seatbelt / `CreateRestrictedToken`; PTY / LSP; JS workflow workers; `llm-pi-ai`; Exa / Perplexity; storage / workspace / presets; Code Mode |
| P3 | Opt-in capabilities | `schedule`; extra context; persistent shell; extra title / query / feedback packages; `skill-badge` |
| P4 | Experimental / cloud / hooks | e2b; agent-team; Claude Code / Codex hooks; MCP; dynamic Cordis |
| P5 | Utilities | `launch-environment`; `native-command`; `output-retention`; extract `session-title-llm` only with a second consumer |

## Out of scope

- Editing `dsh-agent-loop` to honor in-stream `finish { kind: error \| aborted }` (record the gap; a loop change is its own architecture update).
- Bumping `SESSION_FORMAT_VERSION` or treating TypeScript packed SQLite schema 17 as a Rust target.
- Fake Win32 `CreateRestrictedToken` on Linux or Wine.
- OTel SDK metrics and a flush implementation that the TypeScript crate also documents as refused on a concurrent flush.
- A second Web UI written in Rust.

## Alternatives considered

- **Rank Typert, API gateway, and the Web client stack as P0** — that inverts the shipped profiles and reopens the rejected “rewrite the Web UI in Rust” alternative. A later Rust host may speak the existing client protocol; that is P2, after the headless spine.
- **Treat every TypeScript-only package as equal missing work** — 123 leaves include test harnesses, UI plugins, and build-time generators. Equal priority hides the Linux product-user gaps.
- **Fold this ranking into the port Agent Note only** — that note already owns the 1:1 rule and the mounted tree. Adding a 227-row matrix would bury both the decision and the inventory. The inventory lives under `rust/docs/`; this note keeps the order.
- **Generate counts without priorities** — a census without P0–P5 still leaves the next PR free to pick the easiest crate.

## Acceptance criteria

- `rust/docs/ts-rust-functional-gaps.md` lists every `packages/` group, states aligned / thinner / stub / no-op / absent / remap / skip, and names a priority or skip reason.
- Later Rust port PRs cite a P0 or P1 row, or explain why a lower band is in scope.
- No PR that claims to follow this note edits `dsh-agent-loop`, bumps `SESSION_FORMAT_VERSION`, or implements packed schema 17.
- [The port Agent Note](../../implemented/architecture/2026-08-22-rust-harness-port.md) and [rust/README.md](../../../../rust/README.md) link the inventory.

## Risks

The inventory will drift when a crate lands. Update the same files in that PR, or the ranking becomes a second, stale source.

A reader can still treat “absent crate” as “must port.” The skip table and the Web UI rejection have to stay next to the P0 list.

ACP images advertise `true` only when attachments and a vision default model are mounted. DeepSeek Files API upload remains a thinner path; inline `image_url` data-URLs already ship. The inventory records closed P0 rows so a later PR does not reopen them.
