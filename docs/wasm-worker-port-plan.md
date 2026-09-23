# Servo WASM Worker Port: Completion Plan

Status: active port; DOM/JS/CSS, streaming fetch cancellation, timers, inline HTML, page reset, repeated loads, canvas 2D and image loading verified locally (2026-09-23). Next goal: page screenshots.

Target: a raw `wasm32-unknown-unknown` Servo module instantiated directly by a Cloudflare Worker. The MCP server and OAuth layer remain a separate repository and are intentionally out of scope for this port.

## 1. Current state

The port builds as a raw `wasm32-unknown-unknown` module with no WASI or wasm-bindgen imports. The current production-stripped artifact is **57,836,481 bytes** (about 55.2 MiB), below the 64 MiB target (the same limit applies on Workers Free and Paid). The Worker bundle remains about **54,637 KiB** uncompressed (see the smoke-test README for the latest exact dry-run measurement). It imports exactly these five host functions:

- `worker_fetch_request`
- `worker_getrandom`
- `worker_log_error`
- `worker_monotonic_now_ns`
- `worker_unix_time_now_ns`

The Worker adapter instantiates the module, creates a Servo instance, installs the fetch bridge, and advances the cooperative event loop. Deterministic raw-WASM integration coverage includes navigation and subresource fetches, JS `fetch()` success and 404 responses, in-memory POST bodies and clean rejection of oversized bodies, response headers, responses over 1 MiB via chunked delivery, bounded response size, failed fetches, navigation and JS fetch redirects, rejection of cross-origin script-fetch redirects before forwarding credentials, CSSOM parsing and cross-origin stylesheet-rule access control, a resolved CSS color, inline scripts, `setTimeout`, cross-origin page-global separation, aborting an in-flight host fetch on page reset, and four sequential page loads with a bounded linear-memory check. The expanded suite has **36 passing tests including subtests** against the production-stripped artifact (run `npm test` in `ports/servo-js-wasm`), including canvas 2D pixel readback and `<img>` decoding. A host-facing timer deadline export and `pumpUntilSettled()` use Worker `scheduler.wait()` where available, with a cancellable timer fallback. The pump reports internal browser event progress and wakes on response headers/chunks, preventing false idle and allowing page code to consume or abort a response before its body completes.

The latest batch adds a checked **version-1 host ABI**, `loadHtml(html, {url})`, deterministic initial `about:blank` bootstrapping before the async factory returns, reset-then-load navigation coalescing, and cancellation of active/queued host fetches from page `AbortController`s. Response delivery enforces header/chunk/terminal ordering. Mid-body failures now reject body consumers instead of succeeding with truncated content, and already-errored/canceled streams ignore duplicate failure transitions and late chunks. Tests cover these cases, unread response/redirect-body cleanup, subrequest limits across redirects and resets, and invalid/concurrent settling calls. A small original web-platform-style corpus covers templates, selectors, DOM fragments/clones, event propagation, CSS rule mutation/computed-style invalidation, shadow DOM, and microtask/timer ordering. It is not the upstream WPT runner or a claim of complete web conformance. The exact contract is in [WORKER-ABI.md](../ports/servo-js-wasm/WORKER-ABI.md).

The host adapter queues requests above six concurrent outbound connections and caps actual host `fetch()` calls (including redirect hops) at 50 per runtime by default, matching Workers Free's current per-invocation limits. The many-page stress test raises the latter cap explicitly. One runtime must represent one incoming Worker invocation for this accounting to be meaningful.

The browser-engine path is not complete yet. Worker pipelines currently share one script event loop because creating a second SpiderMonkey runtime on the same WASM thread traps; a basic cross-origin global-separation test passes, but browsing-context/security coverage remains limited. The reset API navigates to `about:blank`, aborts host fetches, and clears pending results; it does not destroy Servo or its SpiderMonkey runtime, because Servo's native shutdown path blocks on OS-thread services and is not Worker-safe. Replacing a pending navigation now retires its pipeline, but cleanup of incomplete-load records and cancellation on ordinary navigation (without reset) need a broader lifetime audit. The Worker rendering context is still a null-GL placeholder and Paint target paths do not create WebRender painters, so screenshots, animation frames, and rendering-dependent observer delivery are not implemented. `getComputedStyle(...).color` works with a Worker-safe empty system-font lookup, but layout-dependent measurements and text rasterization need a real font and paint backend. Request bodies are supported only when the script layer already holds at most 256 KiB in memory; general request-body streaming must be ported. The response bridge streams 64 KiB chunks with an 8 MiB default limit. The Worker bridge has a limited CORS path for simple, non-credentialed direct cross-origin GET/HEAD requests: it checks `Access-Control-Allow-Origin`, filters exposed response headers, and rejects denied responses before body delivery. Cross-origin no-CORS subresources receive opaque metadata while parsers can still consume the internal body, preventing cross-origin stylesheet rules from leaking via CSSOM. Preflighted/credentialed cross-origin requests and cross-origin JS redirects fail closed; full Fetch-standard CORS, `no-cors` script fetches, cookies, manual-redirect filtering, body-reader cancellation and cloned-response abort semantics remain incomplete or unverified. A local Wrangler/workerd smoke Worker verifies inline HTML, computed CSS, script fetch and open-stream abort; a clean-target build remains outstanding.

**Hosting decision (2026-09-23): target Workers Paid first.** Paid allows up to 5 minutes of CPU per request (30 s default, configurable), so the numbers below no longer block the first release. Bundle size (64 MiB) and memory (128 MB per isolate) are the same on both plans and remain hard constraints. A Free-tier variant is a later goal; for it, the following measurements still apply. [Cloudflare's current limits](https://developers.cloudflare.com/workers/platform/limits/) list 64 MiB for the Worker bundle, 128 MB memory per isolate, and only 10 ms CPU time per HTTP request on Workers Free. After fixing the async factory to finish its initial document before returning, the reproducible `node ports/servo-js-wasm/tests/cpu-benchmark.mjs` diagnostic measured about **402 ms CPU** for ready-to-use bootstrap and **252 ms CPU** for a tiny HTML page load and DOM evaluation; four individual page pumps exceeded 10 ms and the slowest used about 37 ms. The earlier 61 ms bootstrap figure measured construction only, not a ready initial document, and is not comparable. This is a Node process measurement, not a Cloudflare production CPU measurement, but it is far beyond the free-tier budget. Local workerd does not enforce the account's CPU quota. Remote validation requires explicit authorization. Yielding between pumps inside one request does not reset its accumulated CPU budget; resumable execution is useful for responsiveness but is not by itself a Free-tier solution. Per the user's decision, continue the engine port while investigating this limit.

Work is committed to `gptenv/servo-wasm` `main`; see git history and Section 14 for the latest changes.

## 2. Definition of “finished”

The port should not be considered complete merely because it links or because a JavaScript expression evaluates. Completion means all of the following are demonstrated in a clean build:

1. A raw `WebAssembly.instantiate` call with only the documented `env` imports succeeds.
2. A Worker can create one browser isolate, load deterministic HTML, pump it to completion, and inspect the resulting DOM.
3. JavaScript executes inside the page realm, including script elements, DOM mutation, promises/microtasks, exceptions, and `fetch()`.
4. CSS parses and computed style/layout-facing DOM APIs behave consistently for the supported subset.
5. Fetch requests cross the Worker boundary with correct method, headers, redirects, status, body, errors, and cancellation behavior.
6. Repeated page loads and evaluations have bounded memory growth and a defined reset/destroy lifecycle.
7. Unsupported capabilities fail explicitly rather than hanging, silently dropping work, or trapping with an unexplained native-platform panic.
8. The production artifact passes import, size, security, and integration tests in a clean target directory.

Screenshots and full rasterization are optional for the first usable text/DOM engine. If visual capture is required, it is a separate gated workstream described below; it must not be confused with the current DOM-only bootstrap.

## 3. Workstream A — stabilize the dependency and target policy

### A1. Stylo WASM clock fork — implemented; broaden validation

The pinned Stylo fork contains the Worker-safe monotonic clock adaptation. The earlier style-traversal clock trap is no longer the immediate blocker. Keep the fork synchronized and test its broader cascade, computed-style, and mutation behavior. Its implementation should continue to:

- uses Servo’s Worker-safe `CrossProcessInstant` where the API can depend on Servo;
- otherwise defines a small Stylo-local monotonic clock abstraction backed by `worker_monotonic_now_ns` on `wasm32` and `std::time::Instant` elsewhere;
- keeps style-statistics timing disabled or functional without a native clock;
- avoids adding a second incompatible host ABI;
- preserves native builds unchanged.

The workspace uses the GitHub fork at one pinned revision across its Stylo crates; dependency-fork changes must be committed and pushed before rebuilding Servo.

### A2. Audit all transitive raw-WASM assumptions before feature work

Run a source and dependency audit after Stylo is wired:

- `std::thread`, `thread::spawn`, `JoinHandle`, `std::sync::mpsc`;
- `std::time::{Instant,SystemTime}` and crates that call them internally;
- filesystem, environment, process, socket, DNS, native TLS, and OS signal APIs;
- `ipc-channel` and any channel implementation that assumes a process boundary;
- `glow`, `web_sys`, `wasm-bindgen`, WASI, and C/C++ archives;
- unconditional WebRender/WebGPU/WebGL initialization;
- resource readers, font discovery, and certificate loading.

For each result, classify it as: Worker implementation, compile-time exclusion, explicit unsupported error, or future feature. Do not leave accidental “works until called” behavior.

### A3. Establish a target-specific feature profile

Define one documented Worker feature profile that excludes desktop shell, multiprocess, WebDriver server, devtools server, native media backends, WebGL/WebGPU, filesystem caches, and platform font discovery unless they have a real Worker implementation. Keep DOM, HTML parsing, CSS parsing/style, SpiderMonkey, URL, fetch, cookies as explicitly enabled capabilities.

Add a CI check that builds exactly this profile and rejects newly introduced WASI, wasm-bindgen, or non-`env` imports.

## 4. Workstream B — finish the cooperative execution model

The same-thread script handle and constellation pump are the correct direction, but the model needs a complete contract.

### B1. Define pump semantics

Document and implement:

- what one `pump()` may execute;
- whether it is bounded by task count, wall time, or both;
- how it reports pending work and pending network requests;
- how a page reaches a stable/idle state;
- how errors and shutdown are surfaced;
- how reentrant calls from a Worker request are rejected.

Avoid an unbounded loop inside a Cloudflare request. A host-facing pump should have a budget and return a status such as `Idle`, `Progress`, `PendingFetch`, `PendingTimer`, `Complete`, or `Failed`.

### B2. Replace native-only background services

The current no-op background-hang monitor and disabled paint/timer paths are acceptable temporary bootstraps, not final semantics. For each service, either implement a cooperative version or remove it from the Worker profile. In particular, verify constellation, script, layout, image cache, profiler, storage, and media initialization under repeated creation and teardown.

### B3. Make lifecycle explicit

Add exports and host-adapter methods for:

- create/bootstrap;
- load or navigate;
- pump;
- read result/error/status;
- cancel outstanding work;
- destroy/reset the isolate.

Do not rely on thread-local statics surviving indefinitely. Clear callback maps, DOM roots, fetch state, page results, and JS runtime state on reset. Test two sequential isolates in one Worker and multiple page loads in one isolate.

## 5. Workstream C — make DOM, HTML, CSS, and JavaScript page execution real

### C1. Deterministic about:blank/inline document loading — implemented

`loadHtml(html, {url})` supplies one bounded synthetic HTML response without a host network fetch. Inline script, relative fetch resolution, computed CSS and immediate load after factory creation are covered. Expand this into fixture files for parser edge cases and repeated canceled-navigation stress; do not mistake the supplied URL/origin for an authorization or SSRF policy.

### C2. Fix Stylo and validate CSS

After the Stylo clock patch, test:

- selectors and cascade;
- inline and stylesheet CSS;
- computed style values;
- stylesheet loading and parse errors;
- media-independent layout-facing values;
- DOM mutations that trigger style invalidation;
- custom elements and shadow DOM where supported.

Keep the first CSS corpus small and deterministic. Then add a curated WPT-style subset rather than attempting Servo’s desktop `test-wpt` cross-target harness.

### C3. Define page-evaluation behavior

The current evaluation exports are useful probes but are not yet a complete page-evaluation API. Define whether evaluation runs:

- in the page’s main realm;
- after the document is loaded or immediately;
- with a promise result or synchronous JSON result;
- with structured-clone values, exceptions, console output, and timeouts.

Add tests for script elements, DOM mutation, promise/microtask ordering, `setTimeout`, `fetch`, thrown errors, rejected promises, Unicode, typed arrays, and detached/reused documents. Keep the low-level int32 SpiderMonkey smoke export as a separate test.

## 6. Workstream D — implement the Worker fetch adapter fully

The version-1 ABI wraps `RequestBuilder` JSON in tagged fetch/cancel envelopes and accepts bounded, chunked response delivery. The adapter rejects mismatched versions before bootstrap. Continue hardening the protocol without reintroducing the removed whole-response shortcuts.

### D1. Request protocol

Versioned envelopes, IDs, URL, method, headers, body, credentials/mode/cache/redirect policy, destination and cancellation commands are implemented. Next isolate a stable request DTO from Servo's internal serialization, centralize boundary validation, and cover malformed/truncated payloads and callback reentrancy. Pointer validity remains a trusted-host requirement, not arbitrary-pointer safety.

### D2. Response protocol

Headers, status, final URL, chunks, EOF, errors, callback retirement and duplicate/out-of-order rejection are implemented and tested. Preserve meaningful timing information, broaden redirect tests and audit cancellation races involving response clones, navigation and body readers.

### D3. Cloudflare `fetch()` behavior

Implement the Worker adapter with explicit redirect policy, request-header filtering, response-size limits, abort handling, and bounded concurrency. Do not forward forbidden headers blindly. Decide how `Request` bodies are buffered and how streaming is represented.

### D4. Test matrix

Use a deterministic mock `fetchImpl` for:

- HTML and CSS subresources;
- redirects;
- 404/500 responses;
- empty and binary bodies;
- malformed content types;
- delayed responses;
- rejected fetches;
- cancellation and reset while a fetch is pending.

Then add a small real-Worker/workerd integration test, without deploying to Cloudflare.

## 7. Workstream E — timers, promises, and scheduling

The host-pull Worker timer scheduler is implemented. Basic timers, cancellation and promise/microtask ordering pass. Remaining work is hard execution interruption, nested/interval timer stress, and rendering-dependent scheduling; a host deadline cannot interrupt a synchronous infinite page script.

Choose a host-pull model: Servo returns the next timer deadline, and the Worker schedules a continuation using `setTimeout`/`scheduler.wait`; or Servo exposes a host timer request callback. The simpler first design is host-pull:

1. `pump()` returns the next deadline and pending-work status.
2. JavaScript schedules the next Worker continuation.
3. The next continuation calls `pump()`.

Implement timer IDs, cancellation, minimum delays, microtask checkpoints, promise jobs, animation-frame behavior (or explicit unsupported status), and a maximum execution budget. Add ordering tests for synchronous code, microtasks, timers, fetch completion, and nested scheduling.

## 8. Workstream F — storage and other browser services

The current storage target gating and in-memory fallback must be documented as a choice, not treated as browser-complete.

- Decide whether `localStorage`, session storage, IndexedDB, Cache Storage, cookies, and SQLite are required in the first release.
- For ephemeral operation, implement per-isolate in-memory stores with clear quotas and reset semantics.
- For persistence, keep Cloudflare D1/KV/R2 integration in the future MCP/Worker host repository, behind an explicit host service interface. Do not couple Servo core to Cloudflare SDK types.
- Verify cryptographic randomness remains fail-closed and CSPRNG-backed.
- Route wall-clock use through the Worker host bridge wherever browser-visible timestamps require Unix time.

## 9. Workstream G — rendering, fonts, and screenshots

The current WASM path intentionally bypasses Painter/WebRender. Decide this product requirement before investing in it.

### G1. Text/DOM-first milestone

For the first usable MCP backend, omit screenshots and expose structured DOM/text/results. Keep rendering disabled and make that capability visible in the API.

### G2. Screenshot milestone, if required

If visual page inspection is required, implement in this order:

1. A Worker-safe software framebuffer and a stable pixel-buffer export.
2. Stylo layout integration without native threads.
3. The existing swash/WOFF/WOFF2 font path and deterministic font registration.
4. A headless WebRender configuration only if it can run without Surfman, native threads, WebGL, or platform GPU APIs.
5. PNG encoding on the host or in a small WASM-compatible encoder.
6. Screenshot tests for glyphs, boxes, colors, overflow, and deterministic output.

Do not re-enable the current native Painter path until its worker-thread and Surfman assumptions have been audited. The local WebRender experiment is not integrated or pushed and must not be treated as the production solution.

## 10. Workstream H — ABI, security, and Cloudflare operational limits

Before exposing the module to an MCP server:

- preserve the versioned ABI/host protocol and reject incompatible adapters (implemented for version 1);
- centralize pointer/length validation and allocation ownership;
- prevent stale pointers across memory growth;
- enforce URL, redirect, response-size, CPU-turn, and memory budgets;
- add SSRF policy controls in the eventual host layer;
- reject non-HTTP(S) navigation unless explicitly supported;
- define cookie and credential isolation per MCP user/session;
- ensure panic messages do not leak response bodies or secrets;
- make all host callbacks exception-safe and terminal-state-safe;
- test Worker CPU duration, memory growth, subrequest limits, and concurrent request behavior;
- keep OAuth, token storage, MCP JSON-RPC, and user authorization in the separate server repository.

## 11. Workstream I — test and CI strategy

Build the test pyramid before adding more features:

### Layer 1: static artifact checks

- clean production build using `./mach build`;
- exact import allowlist;
- no WASI or wasm-bindgen imports;
- size budget;
- exported ABI snapshot;
- dependency audit for native-only target edges.

### Layer 2: engine unit tests

- Worker clock and entropy adapters;
- fetch envelope validation;
- response state machine;
- timer deadline/cancellation logic;
- reset/lifecycle state machine;
- DOM/CSS fixture helpers.

### Layer 3: Node raw-WASM integration

- bootstrap and pump;
- inline HTML/CSS fixtures;
- script elements and DOM mutation;
- promise/timer ordering;
- fetch request/response delivery;
- errors, cancellation, reset, and repeated loads.

### Layer 4: workerd/Wrangler integration

- instantiate the same artifact in a local Worker runtime;
- exercise actual `fetch()`, `crypto.getRandomValues()`, timers, and request limits;
- verify no Node-only globals are required.

### Layer 5: curated web compatibility suite

Add a small versioned fixture corpus covering HTML parsing, CSS cascade, DOM APIs, JavaScript language behavior, fetch, cookies, and failure cases. Record unsupported features explicitly rather than silently skipping them.

Every layer should run after a clean target build at least once in CI. Incremental builds are useful during development but cannot be the only regression signal for dependency-source or linker changes.

## 12. Recommended execution order

1. Audit incomplete-navigation teardown, ordinary navigation cancellation, response-reader cancellation and cloned-response aborts as one lifetime batch. Test many canceled loads, retained callbacks/DOM roots and linear-memory growth, not only four successful navigations.
2. Finish the page-evaluation API: correlated results, serialization, exceptions, awaited promises and explicit unsupported/timeout semantics. Keep the existing single-result probe documented until replaced.
3. Expand Fetch policy as one reviewed batch: manual redirects, request streaming, CORS preflight/credentials, cookie scope, and security-sensitive subresource behavior. Do not remove current fail-closed checks piecemeal.
4. Broaden the independent fixture corpus: modules/external scripts, custom elements, mutation observers, nested/interval timers, CSS cascade and layout-facing APIs. Keep rendering-dependent expectations separate.
5. Audit and implement or explicitly exclude storage, service workers, workers, media, WebSockets, WebGL and WebGPU. Report supported capabilities in the host API.
6. Execute the optional rendering/font/screenshot workstream if required for the release; current DOM/CSS success does not imply visible pixels.
7. Add CI for the exact Worker profile and run a clean-target build with import, size and local workerd checks. Preserve existing build artifacts and protect this memory-constrained machine; never delete its target tree just to test cache independence. Existing native-target gating warnings remain, and these changes have not been validated in a native build.
8. Investigate production CPU and total-isolate memory honestly alongside porting. No unapproved deployment or alternate paid hosting is part of this plan.
9. Only then create the separate MCP server repository with OAuth, MCP JSON-RPC, tool schemas, session policy and authorized Cloudflare deployment configuration.

## 13. Exit checklist

- [x] Stylo WASM clock fork/patch is reproducible and pinned.
- [x] Raw Worker adapter can fetch deterministic HTML, parse its DOM/CSSOM, execute an inline script, and return a page-evaluation result.
- [ ] `about:blank` and multiple sequential full-page loads pump to stable completion with bounded retained memory.
- [ ] Page scripts, DOM mutation, promises, and exceptions work broadly; inline scripts, DOM reads, and `setTimeout` have a deterministic smoke test.
- [ ] Worker fetch request/response/error/cancel protocol is complete; navigation, subresources, in-memory POST, headers, chunks, same-origin redirects, abort before/after headers, queued aborts, body failure, response ordering and reset cancellation are covered. Streaming request bodies, full CORS/cookie/manual-redirect behavior, reader/clone cancellation and the wider redirect matrix remain. Simple permitted CORS reads work; unsupported credential/preflight paths fail closed.
- [x] ABI version mismatch is rejected; immediate navigation after factory creation and supplied inline HTML are tested.
- [x] Curated DOM/event/CSS/shadow-DOM and promise/timer-ordering cases pass.
- [x] One deterministic DOM/CSSOM/fetch fixture passes in raw Node WASM tests.
- [ ] Expanded DOM/CSS/fetch failure and standards-compatibility fixtures pass in raw Node WASM tests; computed color, failed and oversized fetches, and a navigation redirect now have fixtures.
- [x] Page reset-to-`about:blank` and a four-page repeated-load memory-bound test pass (this is not full engine destruction).
- [ ] Unsupported APIs return explicit errors/statuses. (Partial: `canvas.getContext("2d")`, the one confirmed hang, is now fixed — see Section 14. `WebSocket` and `requestAnimationFrame` accept calls without throwing but haven't been verified to ever progress or fail; `indexedDB`/`caches` are absent but inert.)
- [x] Rendering limitation is documented: the WASM context is null-GL and screenshot pixels are not rendered.
- [x] Local Wrangler/workerd smoke test passes with inline HTML, computed CSS, deterministic script fetch and open-stream abort (no deployment).
- [ ] Clean production build passes import and size gates. (Latest successful production build was incremental; a clean target build remains required.)
- [ ] The real Cloudflare Workers Free CPU budget is met; local CPU measurements currently suggest this is not feasible for a full page load in one request.
- [ ] MCP/OAuth implementation begins only in the separate server repository.

## 14. Characterization pass findings (2026-09-23)

**Update (later on 2026-09-23): canvas 2D and images now work on the Worker.**
The stopgap below (`getContext("2d")` returning `null`) has been replaced by a
real implementation: script owns an in-process `WorkerCanvasPaintThread`
(components/canvas/canvas_paint_thread.rs) that runs Servo's own canvas code and
the `vello_cpu` CPU rasterizer on the script thread, draining its command
queue after every send so readback replies exist before the blocking `recv()`.
Related root causes found and fixed while doing it:

- `vello_cpu`'s `RenderSettings::default()` unwraps `available_parallelism()`
  when its `multithreading` feature is on, which panics on wasm32; Servo now
  builds the settings explicitly and always single-threaded on wasm32.
- `servo_base::threadpool::ThreadPool` built a rayon pool; on wasm32 its work is
  now queued and run by the Worker pump (`run_worker_deferred_work`).
- The Worker port had replaced Servo's image cache with a stub that always
  answered `FailedToLoadOrDecode`, so **every `<img>` failed without a fetch**.
  The real cache is enabled on wasm32. It then hung at startup because
  `CrossProcessPaintApi::generate_image_key_blocking` / `fetch_font_keys` block
  on Paint, which only runs between script turns; on wasm32 they now return
  the same placeholder keys the Worker Paint returns without a painter.
- `data:` URLs were forwarded to the host adapter and failed its CORS check;
  they are now decoded in-process (ports/servo-js-wasm/lib.rs).
- The script `TaskQueue` per-iteration throttle budget was only reset in the
  native blocking `select()` path, so on the Worker it never reset and
  throttled tasks were held back; each Worker pump now resets it.
- `pumpUntilSettled` could report settlement before an `evaluatePage` result
  arrived; it now waits for outstanding evaluations. The adapter also refuses
  all calls after a WASM trap instead of producing misleading secondary panics.

Verified at that point: 36/36 Node tests (`npm test` in ports/servo-js-wasm, which also sets
the larger stack Node's defaults need; real workerd did not overflow), exact
five-import allowlist, artifact 57,836,481 bytes. Local workerd `/cases` runs the
shared fixture corpus repeatedly.

**Blocking `recv()` sites: first batch fixed.** The canvas and image-key hangs
share one pattern: script blocks on a reply from a component that only runs
after the current script turn. Probing common APIs on a real `https:` page found
nine that hung the Worker forever and one that trapped it. All now answer
without waiting, as a headless browser would:

| API | Worker answer |
| --- | --- |
| `screen.*`, `outerWidth`/`outerHeight`, `screenX`/`screenY` | the viewport (no physical screen or window) |
| `alert` / `confirm` / `prompt` | dismissed via the spec's "cannot show simple dialogs" step: no-op, `false`, `null` |
| `window.open` | popup blocked: `null` |
| `history.length` | `1` (session history lives in the constellation) |
| `localStorage` / `sessionStorage` | previously panicked (storage thread absent) and trapped; now Servo's own web-storage manager with in-memory SQLite, driven in-process |

Still unaudited from the same pattern: history traversal state
(`CoreResourceMsg::GetHistoryState` in history.rs, used by back/forward), window
features that clone storage for auxiliary contexts, and the less common sites
listed by `grep -rn "\.recv()" components/script/dom`. `history.length` is not
tracked across in-page navigations.

**Build memory.** This machine has 15 GB and no swap. Building the `script` crate
alone at this profile needs several GB; with other large apps open, even
`--jobs 1` can exhaust memory. Build with a memory watchdog and close heavy
apps; `--jobs 2`-`4` is safe with roughly 6-7 GB available.

A no-rebuild pass against the existing production-stripped artifact, probing
each surface Sections 9/10 and WORKER-ABI.md call unsupported/incomplete, to
check whether each one actually fails explicitly (the Section 2 #7 and
checklist "Unsupported APIs return explicit errors/statuses" requirement) or
instead hangs/no-ops silently. Methodology note for whoever extends this:
`evaluatePage` runs each call in a **fresh global scope** (confirmed by the
existing "fresh globals prevent state leaking between evaluations" test) —
state must be threaded through `document.body.dataset`, not a `globalThis`
variable, to observe results across pump turns. An early version of this pass
mis-read two CORS cases as "hung forever" purely from checking before enough
pump turns had elapsed; both actually reject immediately and correctly (see
below). Cite that mistake as a reason to re-verify with generous pump budgets
before concluding something hangs.

**Critical finding, now fixed: `canvas.getContext("2d")` hung the WASM
instance indefinitely.** `document.createElement("canvas")` alone was fine;
calling `.getContext("2d")` on it never returned — confirmed reproducible in
isolation with a hard OS-level `timeout`, not just an unsettled promise
(100% CPU, unkillable by any JS-level budget, matching Section 7's "a host
deadline cannot interrupt a synchronous infinite page script" caveat, except
this wasn't a contrived infinite loop — it was a single trivial call any
page script can make). `getContext("webgl")` was fine by contrast: it
returns `null` immediately, the documented null-GL behavior.

Root cause, traced after the initial no-rebuild pass: `CanvasState::new()`
(`components/script/dom/canvas/2d/canvas_state.rs`) sends
`ScriptToConstellationMessage::CreateCanvasPaintThread` to the constellation
and blocks on `receiver.recv()`. The constellation's handler
(`handle_create_canvas_paint_thread_msg` in `components/constellation/constellation.rs`)
in turn calls `CanvasPaintThread::start`
(`components/canvas/canvas_paint_thread.rs`), which spawns an OS thread via
`std::thread::Builder::spawn` whose body is an infinite
`loop { select! { .. } }` servicing exactly this kind of request. wasm32 has
no real OS threads, so that "spawn" never produces a second thread to
service the call — the whole cooperative runtime deadlocks on itself. This
is a concrete instance of the general native-thread-assumption class of bug
Workstream A2 calls out for audit (`std::thread`, `thread::spawn`,
`JoinHandle`); this one wasn't caught by that audit yet because nothing had
exercised the canvas 2D path until this characterization pass.

**Fix applied and verified**: `CanvasState::new()` now returns `None`
immediately under `#[cfg(target_arch = "wasm32")]`, before ever reaching the
constellation/thread-spawn path — the same fail-closed behavior
`getContext("webgl")` already had. Rebuilt clean (`--jobs 4`, ~30 minutes —
touching this file forced a full recompile of the `script` crate and
everything downstream, much longer than the "seconds" a typical one-file
incremental rebuild takes per the prior handoff doc; budget accordingly if
you touch a file this central again). All 31 existing tests still pass, the
artifact stayed within budget (55,912,978 bytes, versus 55,923,533 before —
slightly smaller), and the isolated hang reproduction now returns
`{"ctx-type":"null"}` immediately instead of hanging. This is a stopgap that
trades "hangs" for "explicitly unsupported," not a rendering implementation
— canvas 2D drawing still doesn't work; see Workstream G before attempting
that.

Other surfaces checked (fresh `about:blank` runtime, no page load unless noted):

| Surface | Behavior today |
| --- | --- |
| `localStorage.setItem/getItem` | Throws `SecurityError: Cannot access localStorage from opaque origin` — explicit, matches spec intent for an opaque/blank origin. |
| `indexedDB` | `typeof indexedDB === "undefined"` — not implemented, referencing it is inert. |
| `caches` (Cache Storage) | `typeof caches === "undefined"` — same. |
| `document.cookie` | Silent no-op: assignment doesn't throw, read-back is always `""`. Matches WORKER-ABI.md's "cookies... not provided," but it's a silent no-op rather than an explicit error — a script that depends on cookie persistence fails confusingly later rather than immediately. |
| `new WebSocket(url)` | Constructs without throwing. Not exercised further (no `.send`/`onopen` check) — unknown whether it silently never connects or eventually errors; worth a follow-up probe before relying on this line. |
| `canvas.getContext("webgl")` | Returns `null` immediately — correct, spec-compliant "unsupported" signal. |
| `requestAnimationFrame(cb)` | Accepts the callback without throwing. Whether `cb` ever actually fires was not verified — same "unknown, needs a follow-up probe" caveat as WebSocket. |
| `fetch(..., {credentials:"include"})` cross-origin | Rejects immediately with `TypeError: Network error: CORS check failed` once given enough pump turns to settle. Fails closed as WORKER-ABI.md claims. |
| Cross-origin `fetch` with no `Access-Control-Allow-Origin` header | Same explicit `TypeError` rejection. Fails closed as claimed. |
| `fetch(..., {redirect:"manual"})` | Call itself doesn't throw; actual redirect-handling behavior under `manual` mode was not exercised. |
| `getBoundingClientRect()` on an appended `<div>` | Returns real, non-placeholder-looking numbers (e.g. width matching the 1280px viewport minus margins) rather than zeros or an error — layout is doing real work here, not a stub; useful to know before assuming layout-dependent APIs are all unimplemented. |

Not yet characterized this pass: streaming/large request bodies past the
256 KiB in-memory limit, preflighted (non-simple) CORS requests, nested
`Worker`/`ServiceWorker` construction (the one attempt here hit
an unrelated URL-parsing `SyntaxError` before reaching the real question),
and whether `requestAnimationFrame`/`WebSocket` ever progress past
construction. These would be reasonable next targets for another no-rebuild
characterization pass before spending a build cycle on any of them.
