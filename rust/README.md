# DeepSeek Harness — Rust port

English | [中文](README.zh.md)

1:1 semantic port of the TypeScript harness. The TypeScript tree under `packages/` remains the behavior source of truth. This workspace preserves the plugin tree, capability seams, and session-log contract. Composition identity is the TypeScript row `id` plus plugin `name`; `dsh --dump-config` prints that composed tree. A task run mounts the same tree; `!!js` is evaluated only at mount. Session JSONL leads with the TypeScript header line and serializes each event as `{type, seq, time, data}` with `sourceEventSeqs` citations; messages carry `role` and a UUID `id`, and the TypeScript `StreamChunk` tags are preserved. Session titles run for real: the deterministic fallback plus a first-prompt LLM provider that logs `session/title-llm-request` before its auxiliary call. Headless mounts `bash-sandbox`, `glob` / `grep` (ripgrep over `ctx.subprocess`), `str_replace_editor`, `create_goal` / `get_goal` / `update_goal`, one-shot `subagent` / `subagent_fork`, `workflow` (`return <json>`), `web_search` (DeepSeek official or a `replay` fixture), `repeat-tool-reminder`, `ctx.attachments`, `ctx.sessionQuery` (`openAt: never`), spill (`ctx.spillStore` plus `spill-policy`), `todo_write` (log-only `todo/write`), the tool-call timeout policy (per-tool declared `timeout_ms`, `TOOL_TIMEOUT` failure text), the compaction-time tool-result pruner (`compaction/prune` plus a `tool/result` replace), plan mode (`plan/mode`, `exit_plan_mode`, `/plan`; headless has no user-questions provider so plan review fails with the exact sentence), the skill stack (`SKILL.md` discovery, the `skill` tool, and the once-per-agent `skill-catalog` message), `ctx.tokenMeter`, and compaction (`compactionId`-carrying `compaction/start` / `summary` / `end` plus `/compact`). Confined bash fails closed with `SANDBOX_UNAVAILABLE` when no bwrap or landlock-run backend is usable. Base `tool-web` keeps `fetch: false`. Continuable background subagents, JavaScript workflow workers, packed SQLite schema 17, raster image normalization, LLM-backed compaction summaries, and skill file watching are not mounted.

Crate names are `dsh-<pkg>`; directories mirror `packages/<group>/<pkg>/`.

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

See [docs/architecture.md](../docs/architecture.md) for the product map this port follows.
