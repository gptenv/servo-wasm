# Worker production readiness

This document tracks the 2026-09-25 review of revision `7f6bbd4a89d7f20b8350ce4b49e19714a3b52e5c`. A passing local build or test suite is evidence for the measured artifact only. The current Worker is a controlled-evaluation browser engine, not a general-purpose untrusted browsing service.

## Release profiles

| Profile | Scope | Gate |
| --- | --- | --- |
| Controlled evaluation | Single isolated runtime for trusted test pages, in-memory storage, bounded host fetches, CPU screenshots. | Local production-artifact suite and workerd fixtures. Do not promise durable data or script interruption. |
| Persistent browser service | Untrusted pages, recoverable sessions, declared supported-site corpus and durable state. | Every P0/P1 item below, green CI at the release commit, host integration and remote resource measurements. |
| Broad non-GPU compatibility | All requested non-GPU browser features. WebGL and WebGPU are excluded. | Scoped WPT and site corpus for every supported API; worker, media, storage and renderer gaps closed. |

## Review findings and disposition

| ID | Current status | Required completion evidence |
| --- | --- | --- |
| R1 execution and resources | **Open P0.** Synchronous JS can loop without an engine interrupt. Per-response caps and a storage estimate do not bound aggregate memory. | SpiderMonkey interrupt/deadline design, loop/promise/decompression/image/storage stress tests, concurrent isolate memory and recovery measurements. |
| R2 network security | **In progress P0.** Outgoing and response cookie decisions now share a resolved client-origin rule; outgoing preexisting Cookie headers are removed before jar attachment. CORS preflight, credentialed cross-origin fetches and complete SameSite context remain unsupported. | Browser-differential request/response tests, redirect and cookie policy matrix, CSP/mixed-content/header audit, service-layer authentication and egress policy. |
| R3 lifecycle and isolation | **Open P0.** Reset navigates and cancels but does not erase state or reclaim the WASM instance. | Explicit close/erase APIs, generation-safe completions across all operations, trapped-instance replacement, long-session and cross-user stress tests. |
| R4 persistent storage | **Open P1.** Storage is instance-local; `persist()` reports false and quota is unenforced. | Versioned WASM host contract; session/origin/tab-keyed DO transactions and hydration; crash/retry/migration/quota tests. The hosting integration belongs to the separate `servo-mcp` owner. |
| R5 Cache and IndexedDB | **Open P1.** Cache lifecycle and a basic IndexedDB read/write path work. | Cache request/response operations and storage accounting; IndexedDB abort, indexes, cursors, upgrade, concurrency, clone and restart tests. |
| R6 ABI | **In progress P1.** ABI was bumped to 5 for the redirect-cookie export; adapter checks that export at initialization. Internal `RequestBuilder` JSON is still the fetch wire format. | Narrow versioned DTO, malformed-message tests and pairwise old/new compatibility tests. |
| R7 page operations | **In progress P1.** Evaluation has one result slot and does not await promises. `beginInvocation()` now separates the 50-subrequest invocation counter from a 10,000-subrequest session cap; the host must call it between serialized MCP operations. | Correlated operations, deadlines, history/redirect/stream cancellation tests and host integration of the invocation boundary. |
| R8 compatibility | **In progress P1.** `worker-compatibility-matrix.md` records tested paths and known partial, unsupported and unverified areas. | Supported-site corpus, scoped WPT results and inventory covering workers, media, forms, streams, permissions, accessibility and renderer behavior. |
| R9 release evidence | **In progress P1.** CI now installs a pinned WASI sysroot; it records logs, the artifact, a complete Wrangler dry-run bundle and a JSON manifest with hashes, ABI, tool versions and locked Git revisions. | Green CI at exact release commit and remote CPU/memory/latency measurements. |
| R10 forks | **In progress P1/P2.** Cargo.lock records revisions; CI now emits a reachable-package license/source inventory. Fork patch/security inventories and native-path validation are incomplete. | Upstream baselines, patch owners, native tests, standard SBOM and notices for the distributed artifact. |
| R11 operations | **Open P2.** Trap rejection and some callbacks are explicit; no complete support runbook exists. | Structured session/operation telemetry, expiry/deletion, rollback/schema compatibility and failure-injection tests without logging secrets. |

## Next execution order

1. Get the pinned-sysroot CI run green and retain its manifest, build log and test log. Check the complete Wrangler upload size and runtime behavior on the intended account.
2. Implement an engine-side execution interrupt and aggregate resource budgets. Validate failure isolation before accepting arbitrary pages.
3. Complete cookie/CORS semantics and lifecycle cancellation, then introduce the versioned persistence bridge with the hosting-project owner.
4. Implement Cache request/response operations, broaden IndexedDB and page-operation semantics, and close the declared compatibility matrix with scoped WPT/site evidence.
5. Maintain the forks as a locked release set, add SBOM/notices and operational recovery, and run measured remote soak tests.

No routine clean build is required. Use the production profile and `cargo build --locked` with incremental caches.
