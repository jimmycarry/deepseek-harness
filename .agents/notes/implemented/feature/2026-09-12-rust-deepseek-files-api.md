# Agent Note: Align Rust DeepSeek Files API upload

Status: implemented

English | [中文](2026-09-12-rust-deepseek-files-api.zh.md)

## Problem

The TypeScript `deepseek-official` route prefers the OpenAI-compatible Files API so repeated vision turns reuse uploaded request bytes. The Rust adapter already sent user images as `image_url` data-URLs. Without Files upload, every vision request repeated base64, ignored expiry and quota recovery, and left the P1 durability row open even though inline transport already shipped.

A Files path that mixed `file_id` with inline images, or that remembered a process-wide outage, would diverge from [the TypeScript fallback decision](../bug-fix/2026-08-21-deepseek-files-inline-fallback.md). Waiting for the full TypeScript `readImageRequest` pipeline would delay reuse and recovery until route pixel budgets existed.

## Decision

`dsh-llm-deepseek` prefers Files when a `DeepSeekFileStore` is mounted. Headless `dsh-app-boot` shares one default store at `$DSH_HOME/llm-deepseek/files-v3.json`. A constructor without a store still sends inline data-URLs and does not call Files.

Each retained user image is uploaded through `POST /files` and appears as `{type:"file",file_id}` after its handle text (`Image {id}; request image {w}x{h}px.`). HTTPS uses `curl`; plaintext `http://` uses a hand-rolled HTTP/1.1 client. Chat and Files requests both send the harness `User-Agent`. Files HTTP 401/403 is `AUTH`, 429 is `RATE_LIMIT`, status 500 and above is `SERVER`, and other non-2xx is `FILES_API`. `resolve_file_runtime` validates `filesApiTimeoutMs`, `fileExpiresAfterSeconds`, `fileRefreshMarginSeconds`, and `fileQuotaCleanupBatch` with the TypeScript sentences. Plugin install fails on an invalid document. A later invalid live settings merge uses `FileRuntimeConfig` defaults and does not fail the request.

The index format is `formatVersion` `3`. Scope is SHA-256 of `trim(baseURL) + NUL + apiKey`; the file never stores the key. A missing or malformed index is an empty cache. `commit` / `remove` / `clear` take the atomic-write file lock. An upload is indexed only after a complete file object, matching byte count, and `expires_at`. A mapping with no more than `fileRefreshMarginSeconds` remaining is replaced without a retrieve. Concurrent `ensure_uploaded` calls for one scoped `variantId` share one upload.

Owned filenames are `dsh-{attachment[16]}-{variant[8]}.{ext}`. Chat images above 32MiB fail with the TypeScript sentence. One quota error lists the configured oldest harness-owned `dsh-` files, deletes that set, and retries the upload once. Public `release_all` deletes owned remotes and clears that scope.

A Files resolution failure or an expired `filesApiTimeoutMs` rebuilds the same prepared bytes as a whole-request `image_url` payload. A chat request never mixes `file_id` with inline images. The next request tries Files again. A stale chat file id invalidates the named mappings, or every mapping used by that attempt when the response names none, then retries chat once. A second stale rejection returns the error. A generic chat failure does not switch transports.

Rust `variant_id` is SHA-256 of `attachment_id + NUL + media_type + NUL + data` over the JPEG already produced by `attachment-local::request_image`. Catalog vision detection still uses `model.contains("vision")`. Assistant and tool images remain `UNSUPPORTED_CONTENT`. Route `readImageRequest` budgets, offload quanta, `maxInlineRequestImageBytes`, and tool-result follow-up user messages stay on [the unified image pipeline](2026-08-20-unified-image-request-pipeline.md) and [the inventory](../../../../rust/docs/ts-rust-functional-gaps.md).

[The port Agent Note](../architecture/2026-08-22-rust-harness-port.md) still owns the 1:1 rule. TypeScript remains the behavior source for the request-version pipeline.

## Alternatives considered

**Keep inline data-URLs as the only Rust vision transport.** Rejected because successful Files uploads reuse deterministic request bytes across turns and because TypeScript is the behavior source.

**Mix resolved file ids with inline images after one upload fails.** Rejected because the request would still depend on the failing Files service and would carry two independent image budgets.

**Remember a process-wide Files outage.** Rejected because recovery timing and shared failure state are unnecessary; the next request retries Files.

**Defer Files until `readImageRequest` and catalog `inputModalities` exist.** Rejected because upload reuse, expiry, quota recovery, and all-inline fallback do not require route pixel budgets. The prepared JPEG bytes already have a stable identity.

**Apply the 128MiB Files bound to inline fallback.** Rejected for the same request-body reason as [the TypeScript fallback note](../bug-fix/2026-08-21-deepseek-files-inline-fallback.md). Rust does not apply the 20MiB inline high watermark; that bound remains with the request-version pipeline.

## Verification

`dsh-llm-deepseek` crate tests cover multipart upload plus chat `file_id`, Files HTTP 503 and `filesApiTimeoutMs` whole-request fallback, generic chat 503 without a transport switch, one stale-id re-upload, a second stale rejection, index `commit` / `get` / `remove`, a corrupt index as an empty cache, and oversize or illegal expiry without a network call.

## Consequences

Repeated vision turns reuse file ids under one DSH home. A Files outage still completes an image chat that fits the inline payload. Indexed mappings survive a later image's resolution failure. Remaining request-version work stays thinner and is owned by [the unified image pipeline](2026-08-20-unified-image-request-pipeline.md) and [the remaining-work ranking](../../proposed/architecture/2026-09-03-ts-rust-functional-gap-priority.md). The TypeScript Files Agent Notes remain authority for that pipeline; this Agent Note owns only the Rust alignment.
