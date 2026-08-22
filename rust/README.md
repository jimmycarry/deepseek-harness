# DeepSeek Harness — Rust port

English | [中文](README.zh.md)

1:1 semantic port of the TypeScript harness. The TypeScript tree under `packages/` remains the behavior source of truth. This workspace preserves the plugin tree, capability seams, and session-log contract.

Crate names are `dsh-<pkg>`; directories mirror `packages/<group>/<pkg>/`.

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

See [docs/architecture.md](../docs/architecture.md) for the product map this port follows.
