# Agent Note: 对齐 Rust skill 不完整 last-good catalog

Status: implemented

[English](2026-09-14-rust-skill-incomplete-snapshot.md) | 中文

## 问题

TypeScript 的 skill 发现把意外 I/O 当成不完整观察。`tool-skill` 不发布该观察，因此会话保留上一份完整的 `skill-catalog` 消息。已确认缺失的根和已删除的 skill 文件仍是完整的空状态。

Rust 的 `apply_scan` 每次扫描都整表替换 `ctx.skills`。`load_dir` 错误（包括 `SKILL.md` 是目录）被跳过，就像 skill 不存在。根上的短暂失败因此会注销 last-good 名称，并让 `tool-skill` 发布一份更小的 catalog。扁平的 `ctx.skills` 也没有完整性位，因此模型 catalog 无法区分「扫完了但是空」和「扫失败了」。

把 `dsh-skill` 重写成 TypeScript 那种提供方作用域的 `invalidate()` 与不缓存的不完整候选列表，会推迟 catalog 发布修复。监视已经在更新扁平注册表。

## 决策

`SkillRuntime` 在扁平名称表旁边存储完整性位。`snapshot()` 返回 `{skills, complete}`。新注册表从完整开始。

`dsh-skill-filesystem` 的 `scan` 返回 `SkillScan { skills, complete }`。缺失根（`NotFound`）对该根是空的完整状态。任何其他 `read_dir` 或 skill 文件读取失败都会把观察标为不完整。格式错误的 frontmatter 仍跳过该条目，不让扫描失败。

`apply_scan` 遇到不完整观察时调用 `set_complete(false)`，并保留上一份注册。完整观察会替换已拥有的名称并把 `complete` 设为 true。已确认的删除是完整扫描，会注销缺失的 skill。

当 `ctx.skills` 不完整时，`dsh-tool-skill` 直接返回当前 pre-step payload，不追加 catalog 消息。上一份已发布 digest 保留。`dsh-agent-loop` 不变。

Rust 仍使用一张扁平注册表，而不是按提供方不缓存的候选。因此不完整观察期间的 `get` 仍返回 last-good 正文。TypeScript 的 `snapshot()` 可以显示失败列表的空候选，同时会话 catalog 保持 last-good。这份列表与注册表的差异仍更薄。

[监视对齐](2026-09-13-rust-skill-filesystem-watch.zh.md) 仍然拥有轮询。[TypeScript 热刷新决策](2026-07-27-skill-catalog-hot-refresh.zh.md) 仍然拥有 Chokidar 选项。发布行为仍以 TypeScript 为真源。

## 考虑过的替代方案

**用失败扫描的候选（常常为空）替换注册表并标为不完整。** 否决，因为 `skill` 的 `get` 会丢掉 last-good 正文，而模型 catalog 仍在点那些名字。TypeScript 的 last-good 活在会话消息里；Rust 的 last-good 也留在 `ctx.skills`，使加载器与 catalog 一致。

**重写 `dsh-skill` 以支持提供方 `invalidate()` 与不缓存的不完整列表。** 否决，因为 catalog 发布只需要完整性位，以及不替换的不完整 `apply_scan`。

**把每次 `read_dir` 错误都当成缺失根。** 否决，因为 `NotADirectory` 与权限失败不是已确认缺失，TypeScript 把它们标为不完整。

## 测试

`dsh-skill` 覆盖 `set_complete` 之后 `snapshot()` 的完整性。`dsh-skill-filesystem` 覆盖缺失自定义根为完整空、把文件当 skill 根为不完整 last-good、`SKILL.md` 是目录为不完整，以及已确认删除 bundle 为完整移除。`dsh-tool-skill` 覆盖不完整时后续 pre-step 不发布。

## 后果

短暂的宿主 I/O 失败不再删除模型 catalog。已确认的移除仍会重新发布。提供方作用域失效，以及 TypeScript 空的不完整 `snapshot()` 候选，仍更薄。
