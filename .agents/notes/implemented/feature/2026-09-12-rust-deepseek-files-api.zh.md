# Agent Note: 对齐 Rust DeepSeek Files API 上传

Status: implemented

[English](2026-09-12-rust-deepseek-files-api.md) | 中文

## 问题

TypeScript 的 `deepseek-official` 路由优先走 OpenAI 兼容 Files API，好让重复的 vision 轮次复用已上传的请求字节。Rust 适配器（adapter）已经把用户图像做成 `image_url` data-URL。没有 Files 上传时，每个 vision 请求都会重复 base64，忽略有效期与配额回收，即使内联传输已经交付，P1 耐久性那一行仍开着。

若 Files 路径把 `file_id` 与内联图像混在同一请求里，或记住进程级故障，就会偏离 [TypeScript 回退决策](../bug-fix/2026-08-21-deepseek-files-inline-fallback.zh.md)。等到完整的 TypeScript `readImageRequest` 流水线齐了再挂 Files，会把复用与恢复拖到路由像素预算存在之后。

## 决策

`dsh-llm-deepseek` 在挂了 `DeepSeekFileStore` 时优先走 Files。Headless 的 `dsh-app-boot` 共享一份默认 store，路径为 `$DSH_HOME/llm-deepseek/files-v3.json`。没有 store 的构造函数仍发送内联 data-URL，并且不调用 Files。

每张保留的用户图像经 `POST /files` 上传，并在 handle 文本（`Image {id}; request image {w}x{h}px.`）之后以 `{type:"file",file_id}` 出现。HTTPS 使用 `curl`；明文 `http://` 使用手写 HTTP/1.1 客户端。chat 与 Files 请求都发送 harness 的 `User-Agent`。Files HTTP 401/403 为 `AUTH`，429 为 `RATE_LIMIT`，500 及以上为 `SERVER`，其余非 2xx 为 `FILES_API`。`resolve_file_runtime` 用 TypeScript 文句校验 `filesApiTimeoutMs`、`fileExpiresAfterSeconds`、`fileRefreshMarginSeconds` 与 `fileQuotaCleanupBatch`。插件安装时非法文档拒载。之后现场 settings 合并若非法，则使用 `FileRuntimeConfig` 默认值，并不使该请求失败。

索引格式是 `formatVersion` `3`。作用域是 `trim(baseURL) + NUL + apiKey` 的 SHA-256；文件从不存储密钥。缺失或损坏的索引按空缓存处理。`commit` / `remove` / `clear` 持有 atomic-write 文件锁。只有在响应返回完整文件对象、匹配的字节数和 `expires_at` 之后才写入索引。本地映射剩余时间不超过 `fileRefreshMarginSeconds` 时会替换，且不先 retrieve。同一作用域 `variantId` 上并发的 `ensure_uploaded` 共享一次上传。

自有文件名为 `dsh-{attachment[16]}-{variant[8]}.{ext}`。超过 32MiB 的 chat 图像按 TypeScript 文句失败。一次配额错误会列出配置数量的最旧 harness 自有 `dsh-` 文件，删除该集合后重试上传一次。公开的 `release_all` 删除自有远端文件并清空该作用域。

Files 解析失败或 `filesApiTimeoutMs` 到期时，用同一组已准备字节整请求重建为 `image_url` 载荷。同一次 chat 请求从不把 `file_id` 与内联图像混用。下一次请求会再次尝试 Files。chat 报失效 file id 时，适配器作废被点名的映射；响应未点名任何 id 时作废该次尝试用到的全部映射，然后重试 chat 一次。第二次失效拒绝直接返回错误。普通 chat 失败不切换传输方式。

Rust 的 `variant_id` 是对 `attachment-local::request_image` 已产出 JPEG 做 `attachment_id + NUL + media_type + NUL + data` 的 SHA-256。catalog 的 vision 判定仍用 `model.contains("vision")`。assistant 与 tool 图像仍是 `UNSUPPORTED_CONTENT`。路由 `readImageRequest` 预算、offload 量子、`maxInlineRequestImageBytes` 以及工具结果图像的后续 user 消息仍由 [统一图像流水线](2026-08-20-unified-image-request-pipeline.zh.md) 与 [清单](../../../../rust/docs/ts-rust-functional-gaps.md) 拥有。

[移植 Agent Note](../architecture/2026-08-22-rust-harness-port.zh.md) 仍然拥有 1:1 规则。请求版本流水线的行为真源仍是 TypeScript。

## 考虑过的替代方案

**只把内联 data-URL 当作 Rust 的 vision 传输。** 否决，因为成功的 Files 上传能在后续轮次复用确定性请求字节，且 TypeScript 是行为真源。

**一张图上传失败后把已解析的 file id 与内联图像混在同一请求。** 否决，因为该请求仍依赖正在失败的 Files 服务，并且会带上两套独立的图像预算。

**记住进程级 Files 故障。** 否决，因为不需要恢复计时与共享故障状态；下一次请求会再试 Files。

**等到 `readImageRequest` 与 catalog `inputModalities` 存在再挂 Files。** 否决，因为上传复用、有效期、配额回收与全量内联回退并不要求路由像素预算。已准备的 JPEG 字节已有稳定身份。

**把 128MiB 的 Files 上限套到内联回退上。** 否决理由与 [TypeScript 回退 Agent Note](../bug-fix/2026-08-21-deepseek-files-inline-fallback.zh.md) 相同，都是请求体限制。Rust 不套用 20MiB 内联高水位；该上限仍属于请求版本流水线。

## 验证

`dsh-llm-deepseek` crate 测试覆盖 multipart 上传加 chat `file_id`、Files HTTP 503 与 `filesApiTimeoutMs` 的整请求回退、普通 chat 503 不切换传输、失效 id 重传一次、第二次失效拒绝、配额回收后重试上传一次、没有 harness 自有文件时保留配额错误、索引的 `commit` / `get` / `remove`、损坏索引当空缓存，以及超限或非法 expiry 不发网络请求。

## 后果

同一 DSH home 下的重复 vision 轮次会复用 file id。Files 故障时，仍能完成装得进内联载荷的图像 chat。较晚一张图解析失败时，已提交的索引映射仍可复用。剩余的请求版本工作仍更薄，由 [统一图像流水线](2026-08-20-unified-image-request-pipeline.zh.md) 与 [剩余工作排序](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.zh.md) 拥有。TypeScript 的 Files Agent Note 仍是该流水线的权威；本 Agent Note 只拥有 Rust 对齐。
