# Agent Note: 对齐 Rust skill-filesystem 监视（不引入 Chokidar）

Status: implemented

[English](2026-09-13-rust-skill-filesystem-watch.md) | 中文

## 问题

TypeScript 的 `@deepseek-ai/dsh-skill-filesystem` 会观察宿主 skill 根，使 IDE、Git 或 shell 造成的 catalog 变更能在下一模型步骤可见，而无需重启。Rust 已经在 `agent/pre-step` 以及任意 skill 路径的 `fs/observed` 上扫描。这条路径漏了 `{agentsHome}/skills`，未跳过 `{dshHome}/skills` 下的 `.system` 子项，把配置的 cwd 当成项目根而不是最近的 `.git` 祖先，并且在缺失根出现之前，除非另有事件重扫，否则不会观察该根。

把 Chokidar 移植进 Rust 会引入 Node watcher 或原生 `notify` crate。[TypeScript 热刷新决策](2026-07-27-skill-catalog-hot-refresh.zh.md) 已经推迟通用 Cordis 文件监视服务。一条 TypeScript 在此路径上并不使用的原生 watcher，会变成第二种观察机制，而不是 1:1 适配器。

## 决策

`dsh-skill-filesystem` 轮询与 catalog 相关的宿主路径。`Config::resolve` 接受 TypeScript 的监视字段及相同的正整数字句：`watch`（默认 `true`）、`watchUsePolling`（默认 `false`）、`watchStabilityThresholdMs`（默认 `200`）、`watchPollIntervalMs`（默认 `100`）、`watchMaxProjects`（默认 `128`）和 `watchFollowSymlinks`（默认 `true`）。`watchUsePolling` 被保存，使 cordis.yml 与 TypeScript 一致；两个取值都走轮询。没有 Chokidar、`notify` 或 inotify watcher。

发现根仍按 rank 顺序：从最近 `.git` 祖先（否则为给定 cwd）得到的项目 `.dsh/skills` 与 `.agents/skills`，然后是 `customSkillDirs`，然后是 `{dshHome}/skills`（跳过 `.system`）与 `{agentsHome}/skills`，最后在包含默认根时是 `bundledSkillDir` 或 `$DSH_BUNDLED_SKILL_DIR`。`includeDefaultRoots: false` 去掉项目根、用户根和 bundled 环境默认值。

已有根按 `watchPollIntervalMs` 采样。快照是直属子目录及其 `SKILL.md` 内容指纹，以及直属 `*.md` 文件。bundle 的 `references` / `scripts` / `assets` 不改变该快照。变更后的快照必须在 `watchStabilityThresholdMs` 内保持稳定，才会 `apply_scan`。缺失根从最近的现有祖先起，按同一间隔一次跟一段；真实根出现后，观察切到已有根快照。项目根使用 `watchMaxProjects` LRU。`watch: false` 不启动轮询线程，但仍保留根，以便第一方观察能够匹配。

当路径是与 catalog 相关的 skill 条目时，`write` 和 `edit` 的 `fs/observed` 事件立即重扫。`FsObservationActor` 带有可选 `name`，因此这些工具发布 `{name:"write"|"edit"}`。`read` 与未命名 actor 不走快路径。`agent/pre-step` 仍会重扫并刷新被观察的项目集合。catalog 发布仍由 `dsh-tool-skill` 在下一次 pre-step digest 变化时完成。`dsh-agent-loop` 不变。

失效就是对扁平 `ctx.skills` 注册表做 `apply_scan`。Rust 没有提供方作用域的 `invalidate()`，也没有不完整快照位。短暂的 `read_dir` 失败保留上一份快照，不把该根当作已删除。effect teardown 设置停止标志、丢弃唤醒 channel，并 join 轮询线程。若上下文在没有 `dispose` 的情况下被丢弃，`Drop` 做同样的事。

[移植 Agent Note](../architecture/2026-08-22-rust-harness-port.zh.md) 仍然拥有 1:1 规则。Chokidar 选项与不完整的 last-good catalog 仍以 TypeScript 为行为真源。

## 考虑过的替代方案

**依赖 Chokidar 或从 Node 包装它。** 否决，因为 Rust 树不得引入 Node watcher 依赖，且本次对齐的约束禁止引入 Chokidar。

**用 `notify` crate 充当原生 Chokidar 替代。** 否决，因为 TypeScript 的缺失根路径已经是 `fs.watchFile` 轮询，已有根的 Chokidar 也可以 `usePolling: true`，第二条原生事件源会在 teardown、符号链接和 Windows 8.3 行为上分叉。间隔轮询是适配器，不是遗漏的缺口。

**只保留 `agent/pre-step` 与 `fs/observed` 作为刷新。** 否决，因为 IDE、Git 和 shell 写入不会经过该路径，而且启动时缺失的 skills 目录会一直不可见，直到一次无关的重扫。

**在本次改动中提取通用 Cordis 文件监视服务。** 否决，理由与 [TypeScript 热刷新 note](2026-07-27-skill-catalog-hot-refresh.zh.md) 的推迟相同：还没有第二个消费方确立该服务约定。

**为此在这里重写 `dsh-skill`，补上提供方作用域失效与不完整快照。** 否决，因为监视可以用 `apply_scan` 更新扁平注册表。意外 I/O 时保留 last-good 仍是更薄的注册表行，不是宿主观察的阻塞项。

## 测试

`dsh-skill-filesystem` crate 测试覆盖默认与被拒绝的监视整数、`{agentsHome}/skills`、跳过 `.system`、`.git` 项目根上溯、`includeDefaultRoots: false`、稳定窗之后已有根的新增/删除/frontmatter、被忽略的 bundle 资源编辑、随后出现的缺失根、`watch: false` 且无轮询线程、项目 LRU 驱逐、已 join 的 teardown，以及 `write` 对比 `read` 的 `fs/observed`。`dsh-fs` 覆盖 actor `name` 往返。`dsh-tool-fs` 的 write 与 edit 发布该 name。

## 后果

外部对 skill 根的编辑会在下一次 `agent/pre-step` catalog digest 之前可见，且不需要 Node watcher。检测延迟由 `watchPollIntervalMs` 加 `watchStabilityThresholdMs` 限定。Headless 默认 `watch: true` 为每个挂载的提供方启动一条轮询线程；dispose 会 join 它。不完整的 last-good catalog 发布仍属于 TypeScript 注册表约定，在 Rust 里仍然更薄。
