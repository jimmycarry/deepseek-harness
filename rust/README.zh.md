# DeepSeek Harness — Rust 移植

[English](README.md) | 中文

TypeScript harness 的语义 1:1 移植。`packages/` 下的 TypeScript 树仍是行为真源。本工作区保留插件树、能力 seam 与会话日志约定。

crate 名为 `dsh-<pkg>`；目录对齐 `packages/<group>/<pkg>/`。

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

产品地图见 [docs/architecture.zh.md](../docs/architecture.zh.md)。
