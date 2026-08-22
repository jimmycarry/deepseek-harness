# DeepSeek Harness — Rust port

English | [中文](README.zh.md)

1:1 semantic port of the TypeScript harness. The TypeScript tree under `packages/` remains the behavior source of truth. This workspace preserves the plugin tree, capability seams, and session-log contract. Composition identity is the TypeScript row `id` plus plugin `name`; `dsh --dump-config` prints that composed tree. A task run mounts the same tree; `!!js` is evaluated only at mount. Session JSONL uses `{type, data}` events and the TypeScript `StreamChunk` tags. Headless mounts `bash-sandbox`, `glob` / `grep` (ripgrep over `ctx.subprocess`), and `str_replace_editor`. Confined bash fails closed with `SANDBOX_UNAVAILABLE` when no bwrap or landlock-run backend is usable.

Crate names are `dsh-<pkg>`; directories mirror `packages/<group>/<pkg>/`.

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

See [docs/architecture.md](../docs/architecture.md) for the product map this port follows.
