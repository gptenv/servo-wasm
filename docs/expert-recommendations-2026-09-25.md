# Expert recommendations: servo-wasm production readiness

**Review date:** 2026-09-25  
**Reviewed revision:** `7f6bbd4a89d7f20b8350ce4b49e19714a3b52e5c`  
**Assessment:** substantial functional prototype; suitable for controlled evaluation, with unresolved blockers for a general-purpose production browser service.

## 1. Scope and evidence

This is a source and evidence review of the Worker port, adapter, storage/cookie integration, ABI documentation, dependency declarations, tests and CI workflow. The checkout changed during inspection as the main task committed its work; the revision above was clean at the final source snapshot. This report is the only file created by this review.

The existing `/tmp/servo-wasm-npm-test.log` reports **62 passing tests, zero failures**, in approximately 125 seconds. The port plan also records successful local workerd fixtures. Those are useful existing results, not tests independently executed for this report. No build, deployment, security exploit attempt or benchmark was run. A passing local log does not establish which exact binary will be deployed without an artifact hash and build manifest. Current remote CI success and production resource compliance were not established.

This is not an exhaustive audit of Servo or every fork. Dependency recommendations below follow the root manifest and lockfile, rather than a line-by-line review of each dependency. `servo-mcp` and `servo-fetch` were not modified or audited.

### What already works

The project has a raw WASM engine and a small host import surface, cooperative event pumping, navigation, DOM/JavaScript, timers, input, CPU screenshots, Canvas 2D, font registration, network and WebSocket bridges, and in-memory browser storage. Recent tests exercise IndexedDB read/write, CacheStorage lifecycle, cookie handling and storage origin isolation. Response streaming, cancellation and explicit resource caps are valuable foundations.

### What “complete” should mean

Define two separate milestones:

1. **Production release for a declared compatibility profile:** reliable, isolated, bounded and durable for explicitly supported workflows.
2. **Browser feature completeness:** the requested web-platform surface, backed by conformance results and documented exclusions.

WebGL and WebGPU can remain explicit exclusions. A software WebGL shim would be a separate, substantial compatibility project; it should not delay core correctness. “Everything else” needs an API and behavior inventory. Presence of a JavaScript property or one happy-path test is insufficient evidence of full support.

## 2. Priority recommendations

**P0:** required before exposing arbitrary untrusted pages as a general production service.  
**P1:** required for the intended persistent, broadly compatible release.  
**P2:** maintenance, performance and operational maturity. Priorities reflect release impact, not measured exploit severity.

### R1 — Bound synchronous script execution and total resource consumption (P0)

**Evidence:** `worker-adapter.mjs` explicitly reports that synchronous scripts cannot be interrupted. `WORKER-ABI.md` documents that pump deadlines do not implement script timeouts. Storage quota is currently an estimate, and per-response caps do not bound aggregate live memory.

**Recommendation:** implement an engine-side interrupt/deadline mechanism that works while WASM is executing. A JavaScript timer around a synchronous export cannot provide that mechanism. Establish whether SpiderMonkey interrupt checks can consult the host monotonic clock on this target; otherwise design a supported termination boundary and explicit recovery behavior. Also cap aggregate buffered data, decoded image dimensions, fonts, DOM/layout growth where feasible, outstanding work and storage. Count decoded and temporary allocations, not just incoming bytes.

**Acceptance:** infinite loops, recursive scripts, promise storms, decompression/image expansion and storage growth terminate or fail within declared budgets. A failed session cannot prevent other sessions from progressing. Record peak total-isolate memory and resource errors under concurrent load, with a measured safety margin.

### R2 — Complete the browser security contract around host networking (P0)

**Evidence:** current CORS support intentionally covers only simple, noncredentialed cross-origin GET/HEAD. Cookie SameSite context is incomplete. In `components/net/lib.rs`, `attach_worker_cookies()` treats `Origin::Client` as same-origin, while `worker_accepts_response_cookies()` in the port resolves the client origin explicitly.

**Recommendation:** centralize request-origin and credentials decisions; resolve client origins consistently and fail closed when unavailable. Trace whether the permissive cookie-attachment branch is reachable for cross-origin requests. This is a concrete audit target, not a demonstrated leak. Review preexisting Cookie headers when credentials forbid cookies, redirect origin transitions, and final-response cookie decisions.

Implement the intended CORS/preflight and credentials behavior; retain explicit rejection until each path is correct. Cover SameSite site/navigation context, Secure/HttpOnly, expiry, domain/path matching, cookie prefixes and partitioning policy. Audit Worker-specific preservation of CSP, mixed-content rules, opaque responses, forbidden request headers and response-header exposure. Do not assume native paths automatically cover the host bridge.

**Acceptance:** differential tests against browser behavior for same-origin/cross-origin/same-site/cross-site requests; omit/same-origin/include credentials; redirect chains; failed preflights; and cookies on success/error responses. Verify both what script sees and what the remote server actually receives. At the service boundary, authenticate sessions and apply destination/egress policy to initial URLs, redirects and WebSockets so a page cannot inherit unintended host privileges.

### R3 — Make lifecycle and session isolation explicit (P0)

**Evidence:** reset cancels work and loads `about:blank`; it does not destroy Servo/SpiderMonkey or erase stored state. The adapter recommends fresh instances for unrelated users.

**Recommendation:** distinguish navigation, reset, close and erase-profile operations. Serialize operations that mutate one runtime; give asynchronous operations generation/operation IDs and reject stale completions. Specify cancellation for response readers/clones, sockets, screenshot streams, storage work and pending evaluation. Retire a trapped instance and recreate it from committed state. Avoid claiming full memory reclamation merely because reset succeeded.

**Acceptance:** tests for cancellation during each phase, repeated create/close cycles, late callbacks after reset, simultaneous calls and separate users. No cross-session cookie, storage, page-result or screenshot leakage; resource usage remains bounded over long sessions.

### R4 — Finish persistent browser storage with transactional semantics (P1; release requirement for persistent sessions)

**Evidence:** cookies, localStorage, sessionStorage, IndexedDB and CacheStorage are instance-local. `client_storage.rs` calculates Worker usage from database-name lengths and advertises an unenforced 32 MiB quota. `persist()` correctly declines durability today.

**Recommendation:** expose a versioned host persistence contract in servo-wasm. Use the proposed per-session SQLite-backed Durable Object as authoritative state in the hosting project, with the WASM instance holding live state. Key records by session, origin and storage area; include browsing-context identity for sessionStorage. Define tab closure, session expiration and explicit deletion separately from engine eviction.

Preserve IndexedDB transaction atomicity, aborts, upgrade/version-change coordination, indexes, structured cloning and blob data. For asynchronous operations, do not acknowledge a durable commit before the host commits it. Synchronous `localStorage` and `document.cookie` cannot simply await a host storage call: hydrate before page execution, journal changes, and define the checkpoint acknowledged by the outer operation. Document the crash window rather than promising impossible synchronous durability.

Enforce real byte accounting and quotas across storage endpoints. Support migration, recovery, idempotent replay and all-or-nothing publication of chunked records. Large cache bodies need chunking: Durable Object SQLite limits a string, BLOB or row to 2 MB. R2 can be a later large-body store; using it would require explicit coordination between metadata and object writes. [Cloudflare Durable Object limits](https://developers.cloudflare.com/durable-objects/platform/limits/).

**Acceptance:** recreate the WASM instance and recover committed data; crash between commit stages; retry operations; reject quota overflow with appropriate browser errors; preserve origin/tab isolation; migrate old storage without silent loss. Integration work belongs in the hosting project, coordinated with its separate owner.

### R5 — Implement usable Cache APIs and broaden IndexedDB verification (P1)

**Evidence:** `Cache.webidl` exposes only `keys()`. `CacheStorage.webidl` exposes has/open/keys/delete. The adapter accurately calls this “cache-storage-lifecycle.”

**Recommendation:** implement Cache match/matchAll/put/add/addAll/delete and CacheStorage.match with request matching, Vary, response cloning/body consumption, ordering and failure semantics. Implement the corresponding backend operations and storage accounting, not only bindings. Expand IndexedDB tests beyond open/write/read to transactions, cursors, indexes, key ranges, abort/rollback, version upgrades, concurrent connections and binary/structured-clone values.

**Acceptance:** relevant upstream web-platform tests pass on the Worker artifact, including negative and restart cases. Feature discovery distinguishes partial support from conformance.

### R6 — Fix ABI compatibility detection and stabilize host messages (P1)

**Evidence:** the factory accepts ABI version 4, but redirect handling unconditionally invokes `servo_worker_process_redirect_cookies`, a newly added export. An older ABI-4 artifact lacking that export can pass the initial version check and fail later. Host fetch commands also depend on serialized internal Servo request types.

**Recommendation:** bump the compatibility version when required exports change, or negotiate capabilities and validate every required export at initialization. Publish the WASM and adapter as one release with hashes. Introduce a narrow, documented request/response DTO independent of internal Servo structs; validate messages and define error codes and size limits.

**Acceptance:** old/new adapter-artifact combinations either work deliberately or fail immediately with a clear diagnostic. Fuzz malformed messages, lengths and state transitions at the trusted host boundary without claiming the pointer ABI is safe for arbitrary callers.

### R7 — Complete evaluation, navigation and network operation contracts (P1)

**Evidence:** evaluation uses a serialized single-result slot and does not await returned promises; history traversal is marked unverified. The host subrequest count survives reset and defaults to 50 for the entire runtime lifetime.

**Recommendation:** support correlated structured values/errors, promise settlement, cancellation and script deadlines. Distinguish DOM readiness, load completion and network-idle heuristics. Verify back/forward/reload and history state. Finish redirect behavior, streaming uploads and response clone/cancellation semantics for the compatibility profile.

Separate explicit session abuse budgets from per-operation and platform invocation budgets. A lifetime limit may be intentional, but a durable session must not unexpectedly stop networking after its fiftieth host fetch. Do not simply reset counters on every navigation and call that platform accounting.

**Acceptance:** long-lived sessions, redirect-heavy pages, concurrent operations, rejected promises and never-settling pages have deterministic outcomes and documented limits.

### R8 — Publish and close a browser compatibility matrix (P1)

**Evidence:** dedicated/shared workers and service workers are unsupported. Renderer gaps include backdrop filters, non-rounded clip paths, selected masks, flattened 3D transforms and static handling of sticky positioning in the port plan.

**Recommendation:** inventory interfaces and behavior as implemented/tested, partial, unsupported or unverified. Include workers/service workers, modules, messaging, media, audio, form submission/uploads/downloads, streams, permissions, navigation, accessibility output and other APIs required by target sites. These are inventory items, not claims that every item is absent.

Port supported services through the cooperative scheduler; audit reachable native thread, blocking-channel, filesystem, socket and process assumptions. Unsupported paths should return defined failures instead of trapping. Prioritize sticky layout, clipping, fonts and common compositing behavior using real target pages and deterministic screenshot comparisons.

**Acceptance:** a published supported-site/workflow corpus and scoped WPT results, with tracked failures. If all non-GPU browser features remain the goal, worker/service-worker/media gaps remain completion blockers even after a narrower production release.

## 3. Release engineering and dependency stewardship

### R9 — Tie every release to verifiable build evidence (P1)

The Worker workflow builds a production artifact, runs Node tests and checks local workerd fixtures. Extend this foundation with retained build/test/workerd logs, artifact hashes, dependency revisions, toolchain and Wrangler versions, and adapter/ABI identity. Require green CI for the exact release revision. Use a canonical locked build command and incremental caches; this review does **not** recommend routine clean builds.

The recorded artifact is approximately 59.9 MiB, close enough to the bundle limit that added functionality needs size monitoring. Validate the complete upload, not only the WASM file. Current published limits include 64 MiB Worker size, 128 MB memory per isolate, and Paid HTTP CPU up to five minutes when configured; the default is 30 seconds. Measure deployment startup separately from browser initialization, and test the actual Worker/DO arrangement. Node measurements and local workerd success are not account-limit validation. [Cloudflare Worker limits](https://developers.cloudflare.com/workers/platform/limits/).

**Acceptance:** the exact uploaded artifact passes deployment checks and representative remote load/soak tests; p50/p95/p99 latency, CPU, memory failures and per-session cost meet an agreed service objective.

### R10 — Maintain the forks as a release set (P1/P2)

`Cargo.toml` pins Stylo/html5ever revisions but several other fork declarations follow branch names. `Cargo.lock` records resolved revisions, so branch declarations alone do not make a locked build nondeterministic. Require locked dependency resolution and record every resolved commit in a release manifest; consider explicit revision pins for reviewed fork updates.

For mozjs, WebRender, rusqlite/SQLite, Stylo, html5ever, UUID and rustls-pki-types forks, maintain: upstream baseline, local patch inventory, security update owner, upstreaming plan and tests for each WASM-specific patch. Audit build scripts for hidden local paths or generated inputs. Test shared native paths when changing code used outside WASM. Review SpiderMonkey, parsers, decoders and font handling for fuzzing coverage and denial-of-service behavior.

Retain applicable upstream licenses and notices; an MIT license for a separate wrapper does not relicense Servo or its dependencies. Generate an SBOM and license inventory for distributed artifacts. This review does not certify dependency security or license compliance.

### R11 — Add operational recovery and useful diagnostics (P2; minimum support before public launch)

Emit operation/session identifiers, release/ABI version, phase timings, resource counters and structured failure classes. Avoid logging cookies, authorization headers or page contents by default. Define rollback with storage-schema compatibility, session expiry/deletion, trap recovery and a support runbook. Test disconnects, host fetch failures, storage failures and deployment replacement. Preserve committed browser state without promising recovery of arbitrary live JavaScript execution.

## 4. Recommended delivery order

1. **Establish the release contract:** compatibility matrix, supported workloads, artifact provenance and green CI.
2. **Harden execution:** interrupts, aggregate limits, isolation, origin/credential handling and lifecycle failure tests.
3. **Finish state:** persistent host bridge, real quotas, IndexedDB transaction coverage and complete Cache operations.
4. **Finish interaction:** structured evaluation, promises, navigation, CORS/redirects and remaining required APIs.
5. **Validate production:** remote resource measurements, concurrency/soak tests, visual regression corpus, recovery and rollback.

Renderer improvements and fork maintenance can proceed alongside these stages. Do not treat a deployment succeeding, or the current 62 tests passing, as completion of the remaining stages.

## 5. Production release gates

- [ ] Scope and intentional exclusions are published; partial APIs are accurately advertised.
- [ ] Exact release revision has green CI and a matching adapter/WASM manifest.
- [ ] Untrusted scripts and aggregate allocations have tested limits and recovery paths.
- [ ] Cookie/CORS/origin behavior passes positive and negative cross-origin tests.
- [ ] Session isolation, cancellation and trap recovery survive stress testing.
- [ ] Promised storage durability survives eviction/recreation and commit failures.
- [ ] Storage quotas reflect real usage and reject overflow predictably.
- [ ] Cache and IndexedDB meet the declared compatibility profile.
- [ ] Supported user workflows pass end to end on the deployed platform.
- [ ] Bundle, startup, CPU and total-isolate memory fit with measured headroom.
- [ ] Fork revisions, security maintenance, notices and rollback are documented.
- [ ] Operators can diagnose failures and erase/expire a session reliably.

**Overall recommendation:** continue toward a clearly scoped production release, with execution bounds, security semantics and durable state as the immediate priorities. The project has meaningful working functionality; the remaining work spans engine behavior and hosting contracts, and cannot honestly be reduced to a few final polish changes.

## Primary repository references

- [Worker port plan](/mnt/claudevm/servo-wasm/docs/wasm-worker-port-plan.md)
- [Worker ABI contract](/mnt/claudevm/servo-wasm/ports/servo-js-wasm/WORKER-ABI.md)
- [Worker adapter and capability declarations](/mnt/claudevm/servo-wasm/ports/servo-js-wasm/worker-adapter.mjs:19)
- [ABI acceptance check](/mnt/claudevm/servo-wasm/ports/servo-js-wasm/worker-adapter.mjs:168)
- [Cookie attachment](/mnt/claudevm/servo-wasm/components/net/lib.rs:116)
- [Response-cookie credentials decision](/mnt/claudevm/servo-wasm/ports/servo-js-wasm/lib.rs:899)
- [Worker storage usage estimate](/mnt/claudevm/servo-wasm/components/storage/client_storage.rs:373)
- [Cache interface](/mnt/claudevm/servo-wasm/components/script_bindings/webidls/Cache.webidl)
- [Production-artifact tests](/mnt/claudevm/servo-wasm/ports/servo-js-wasm/tests/wasm.test.mjs)
- [Worker CI workflow](/mnt/claudevm/servo-wasm/.github/workflows/worker-wasm.yml)
- [Fork declarations](/mnt/claudevm/servo-wasm/Cargo.toml:462)

References describe the reviewed snapshot; ongoing changes may supersede individual findings.
