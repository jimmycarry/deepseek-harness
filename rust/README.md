# DeepSeek Harness — Rust port

English | [中文](README.zh.md)

1:1 semantic port of the TypeScript harness. The TypeScript tree under `packages/` remains the behavior source of truth. This workspace preserves the plugin tree, capability seams, and session-log contract. Composition identity is the TypeScript row `id` plus plugin `name`; `dsh --dump-config` prints that composed tree. A task run mounts the same tree; `!!js` is evaluated only at mount. Session JSONL uses `{type, data}` events and the TypeScript `StreamChunk` tags. Headless mounts `bash-sandbox`, `glob` / `grep` (ripgrep over `ctx.subprocess`), `str_replace_editor`, `create_goal` / `get_goal` / `update_goal`, one-shot `subagent` / `subagent_fork`, `workflow` (`return <json>`), `web_search` (DeepSeek official or a `replay` fixture), `repeat-tool-reminder`, `ctx.attachments`, `ctx.sessionQuery` (`openAt: never`), and spill (`ctx.spillStore` plus `spill-policy`). Confined bash fails closed with `SANDBOX_UNAVAILABLE` when no bwrap or landlock-run backend is usable. Base `tool-web` keeps `fetch: false`. Continuable background subagents, JavaScript workflow workers, packed SQLite schema 17, and raster image normalization are not mounted.

Crate names are `dsh-<pkg>`; directories mirror `packages/<group>/<pkg>/`.

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

See [docs/architecture.md](../docs/architecture.md) for the product map this port follows.
