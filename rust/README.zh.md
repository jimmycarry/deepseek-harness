# DeepSeek Harness — Rust 移植

[English](README.md) | 中文

TypeScript harness 的语义 1:1 移植。`packages/` 下的 TypeScript 树仍是行为真源。本工作区保留插件树、能力 seam 与会话日志约定。组合身份是 TypeScript 行 `id` 加插件 `name`；`dsh --dump-config` 打印这棵组合树。跑任务会挂载同一棵树；没有 Rust apply 的插件名会拒载。

crate 名为 `dsh-<pkg>`；目录对齐 `packages/<group>/<pkg>/`。

```sh
cargo test --workspace
cargo run -p dsh -- --dump-config
cargo run -p dsh -- --profile headless "reply with the word pong"
```

产品地图见 [docs/architecture.zh.md](../docs/architecture.zh.md)。
