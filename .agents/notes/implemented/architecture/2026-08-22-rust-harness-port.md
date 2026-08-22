# Agent Note: Rust harness port keeps TypeScript as the behavior source

Status: implemented

English | [中文](2026-08-22-rust-harness-port.zh.md)

## Problem

A second implementation language cannot invent a second product. If the Rust tree grew its own loop, event names, or session-log rules, every later change would have two answers and no way to say which one users get. The TypeScript tree under `packages/` already owns those names: there is no privileged kernel, and a running `dsh` is the Cordis plugin tree assembled from profile, bundle, and patch layers.

## Decision

`rust/` is a Cargo workspace that ports the same plugin tree, capability seams, and session-log contract. Crate names are `dsh-<pkg>`; directories mirror `packages/<group>/<pkg>/`. TypeScript remains the behavior source of truth: a Rust change that would diverge from a TypeScript name or log event updates the TypeScript owner first, or it does not ship.

The workspace targets Rust 1.83 and edition 2021. Dependencies that pull edition 2024 or `hashbrown` 0.17 are rejected at the lockfile. Loader YAML is a hand-written subset rather than `serde_yaml` and parses the Include dialect: `- insert:` lists, id-targeted whole-field replacement, `inject`, `|` / `>-` blocks, flow sequences, and `!!js` stored as `{ "__jsExpr" }`. HTTPS provider calls go through `curl` rather than `reqwest`; the host HTTP listener is a hand-written HTTP/1.1 server rather than `axum`. `Branded<B>` implements `Clone` and `Debug` by hand so the brand token does not have to.

A capability seam is complete: Service Definition, Service Provider, and Consumer. Tool Consumers depend only on the Definition crate. Deployment-varying values are `Config` fields supplied at construction; `run` does not hide a default. Model-visible content is logged; compaction advances the surface with `surfaceOp: replace` and does not delete history. Session logs stay on `SESSION_FORMAT_VERSION` `0`; the SQLite backend uses monotonic `SCHEMA_VERSION` `1`. Unknown required-on-read event types refuse resume unless the envelope carries `ignorable: true`.

Composition identity is the row `id` plus the TypeScript plugin `name` (`@deepseek-ai/dsh-*` or `@deepseek-ai/cordis-plugin-*`), not the Rust crate name. `dsh --dump-config` and `compose_profile` share one flattened `apply_entry_patches` call over the TypeScript `dsh-base` then `dsh-headless` patch files; `!!js` prints verbatim and is not evaluated. A missing patch target or a name mismatch fails loud. The default driver lives in `dsh-agent-loop` and stays a plugin. `max-tokens` is sticky for the open turn. `agent/turn-stopping` may `steer` another step. The first `cancel` reason wins. Tool bodies overlap up to `ToolRuntimeConfig.max_parallel`; `tools/post-execute` commits in model order. A task run composes that same tree, registers every plugin name, and mounts it. `!!js` on `disabled` and `config` is evaluated at mount (`process.env.*`, `process.platform`, `process.cwd()`, `dshHomePath`, `ctx.<service>.<field>`). Spine rows, default-model, persistence, sandbox-policy, approval, permission, credentials, settings, fs-sandbox, and the headless startup/runner apply for real; remaining names mount as no-ops. dump-config never mounts and never evaluates. `apply_world` remains for crate tests that exercise the spine without the profile tree. A slash command is dispatched by `ctx.commands` and does not enter the model.

## Alternatives considered

- **Port the loop as one `async fn` and pluginize later** — the TypeScript rule is the opposite: new behavior lands on documented extension points. A monolith would drop `agent/pre-step`, `agent/request`, `agent/request-error`, and `agent/turn-stopping` as the place plugins actually run.
- **Rewrite the Web UI in Rust** — the host only has to speak the existing client protocol. A second UI is a product fork, not a port.
- **Fold compaction into the loop** — TypeScript compaction is a Consumer of `agent/pre-step`, `agent/request-error`, and idle maintenance. Putting it in `step` would make every later engine a loop change.
- **Treat the Python SDK as a second loop** — both SDKs project the TypeScript loop. A Rust loop that those SDKs also reimplement would be a third driver.
- **`serde_yaml` / `reqwest` / `axum`** — each pulls an edition-2024 or `hashbrown` 0.17 graph that Rust 1.83 cannot build. The hand-written YAML subset, `curl` transport, and HTTP/1.1 listener keep the workspace on the declared toolchain.
- **Compatibility shims for older on-disk formats** — the pre-release stance rejects old backends. A shim would hide a format mistake until the first tagged release.

## Consequences

The repository carries two language trees until a tagged release. `cargo test --workspace` under `rust/` is the Rust evidence; keyless headless snapshots live in `rust/apps/cli/tests/headless_snapshot.rs` and cover a text turn, a `bash` turn, and a `write_file` turn that rereads the file from the host. `--dump-config` prints the composed TypeScript row ids and plugin names; `dsh-app-boot` pins the headless id sequence and a replay turn that flushes JSONL under `$DSH_HOME/sessions`.

A later crate that needs a default must take it as `Config` at construction. A later event type that older Rust readers must skip has to set `ignorable: true`; omitting the marker refuses resume. Changing `dsh-agent-loop` still requires updating [docs/architecture.md](../../../../docs/architecture.md).
