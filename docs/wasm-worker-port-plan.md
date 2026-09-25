# Servo WASM Worker Port: Completion Plan

Status: active port; review reconciled against the current source on 2026-09-25. DOM/JS/CSS, streaming fetch responses, timers, inline HTML, page reset, repeated loads, canvas 2D, image loading, bundled/registerable fonts, input, navigation and CPU-rendered viewport/full-page screenshots are implemented and have existing Node/workerd coverage. This document is the release gap list, not a claim of complete browser conformance.

Target: a raw `wasm32-unknown-unknown` Servo module instantiated directly by a Cloudflare Worker. The separate MIT-licensed `servo-mcp` repository now hosts a stateless MCP App over this runtime; its production deployment and any OAuth/access-control policy are tracked in that repository, not in Servo core.

## 1. Current state

The production-stripped artifact is **62,373,083 bytes** (about 59.46 MiB); the previous local Wrangler dry run bundled **60,945.86 KiB** uncompressed. The source and newly built production artifact report ABI 4 and import exactly five `env` functions, with no WASI or wasm-bindgen imports:

- `worker_fetch_request`
- `worker_getrandom`
- `worker_log_error`
- `worker_monotonic_now_ns`
- `worker_unix_time_now_ns`

The Worker adapter instantiates the module, creates a Servo instance, installs the fetch and WebSocket bridges, and advances the cooperative event loop. `npm test` passes **55 tests** against the current artifact, including custom-element upgrade, MutationObserver records, repeated canceled-navigation stress, and nested timer/interval coverage. Local workerd smoke checks pass, including the root integration response, **45/45** shared fixture runs over three rounds (15 distinct fixtures), and a screenshot response. The host timer deadline export and `pumpUntilSettled()` use Worker `scheduler.wait()` where available with a cancellable timer fallback.

The source implements checked **version-4 host ABI**, adding Worker-host WebSocket event/action envelopes and Worker-pumped `requestAnimationFrame` callbacks. It also has `loadHtml(html, {url})`, deterministic initial `about:blank` bootstrapping, navigation coalescing and cancellation of active/queued host fetches. Response delivery enforces header/chunk/terminal ordering. Mid-body failures reject body consumers instead of succeeding with truncated content. The original web-platform-style corpus covers templates, selectors, DOM fragments/clones, event propagation, CSS rule mutation/computed-style invalidation, shadow DOM, and microtask/timer ordering. It is not the upstream WPT runner or a claim of complete web conformance. The exact contract is in [WORKER-ABI.md](../ports/servo-js-wasm/WORKER-ABI.md).

The host adapter queues requests above six concurrent outbound connections and caps actual host `fetch()` calls (including redirect hops) at 50 per runtime by default, matching Workers Free's current per-invocation limits. The many-page stress test raises the latter cap explicitly. One runtime must represent one incoming Worker invocation for this accounting to be meaningful.

The runtime remains cooperative and reset is not destruction: it cancels fetches and navigates to `about:blank`, but Servo and SpiderMonkey stay alive for the lifetime of the WASM instance. The current renderer uses `vello_cpu` over Servo display lists; it is not WebRender's native painter and has documented gaps (backdrop filters and non-rounded clip paths; masks are only covered by selected fixtures). Request bodies are buffered up to 256 KiB; responses stream in 64 KiB chunks with an 8 MiB default limit. CORS support is limited to simple, non-credentialed direct cross-origin GET/HEAD; unsupported credentialed/preflight paths fail closed. Cookies, general request streaming, complete redirect behavior, reader/clone cancellation, script timeouts and several browser APIs remain unsupported or uncharacterized. Current ABI details and renderer coverage live in [WORKER-ABI.md](../ports/servo-js-wasm/WORKER-ABI.md).

**Hosting decision (2026-09-23): target Workers Paid first.** Paid allows up to 5 minutes of CPU per request (30 s default, configurable), so the numbers below no longer block the first release. Bundle size (64 MiB) and memory (128 MB per isolate) are the same on both plans and remain hard constraints. A Free-tier variant is a later goal; for it, the following measurements still apply. [Cloudflare's current limits](https://developers.cloudflare.com/workers/platform/limits/) list 64 MiB for the Worker bundle, 128 MB memory per isolate, and only 10 ms CPU time per HTTP request on Workers Free. After fixing the async factory to finish its initial document before returning, the reproducible `node ports/servo-js-wasm/tests/cpu-benchmark.mjs` diagnostic measured about **402 ms CPU** for ready-to-use bootstrap and **252 ms CPU** for a tiny HTML page load and DOM evaluation; four individual page pumps exceeded 10 ms and the slowest used about 37 ms. The earlier 61 ms bootstrap figure measured construction only, not a ready initial document, and is not comparable. This is a Node process measurement, not a Cloudflare production CPU measurement, but it is far beyond the free-tier budget. Local workerd does not enforce the account's CPU quota. Remote validation requires explicit authorization. Yielding between pumps inside one request does not reset its accumulated CPU budget; resumable execution is useful for responsiveness but is not by itself a Free-tier solution. Per the user's decision, continue the engine port while investigating this limit.

The current Servo checkout is on `main` at `2065a23b90d`, synchronized with `origin/main` at the start of this completion pass. Dependency fork branches and remotes are audited separately; do not infer that an untracked branch is pushed just because its commit is in Cargo.lock. Stylo and html5ever are pinned by commit. The other dependency forks are selected by branch name, so keep their resolved revisions in build provenance because those names can move.

## 2. Definition of “finished”

The port should not be considered complete merely because it links or because a JavaScript expression evaluates. Completion means the following are demonstrated against the current production artifact; validation may use incremental builds, and a clean build is explicitly not a release requirement:

1. A raw `WebAssembly.instantiate` call with only the documented `env` imports succeeds.
2. A Worker can create one browser isolate, load deterministic HTML, pump it to completion, and inspect the resulting DOM.
3. JavaScript executes inside the page realm, including script elements, DOM mutation, promises/microtasks, exceptions, and `fetch()`.
4. CSS parses and computed style/layout-facing DOM APIs behave consistently for the supported subset.
5. Fetch requests cross the Worker boundary with correct method, headers, redirects, status, body, errors, and cancellation behavior.
6. Repeated page loads and evaluations have bounded memory growth and a defined reset/destroy lifecycle.
7. Unsupported capabilities fail explicitly rather than hanging, silently dropping work, or trapping with an unexplained native-platform panic.
8. The production artifact passes import, size, security, and integration checks, with its build provenance and dependency revisions recorded.

CPU-rendered viewport and full-page screenshots are implemented. Their rendering coverage and known gaps are specified in `WORKER-ABI.md`; visual correctness beyond existing fixtures remains a compatibility workstream.

## 3. Workstream A — stabilize the dependency and target policy

### A1. Stylo WASM clock fork — implemented; broaden validation

The pinned Stylo fork contains the Worker-safe monotonic clock adaptation. The earlier style-traversal clock trap is no longer the immediate blocker. Keep the fork synchronized and test its broader cascade, computed-style, and mutation behavior. Its implementation should continue to:

- uses Servo’s Worker-safe `CrossProcessInstant` where the API can depend on Servo;
- otherwise defines a small Stylo-local monotonic clock abstraction backed by `worker_monotonic_now_ns` on `wasm32` and `std::time::Instant` elsewhere;
- keeps style-statistics timing disabled or functional without a native clock;
- avoids adding a second incompatible host ABI;
- preserves native builds unchanged.

The workspace uses the GitHub fork at one pinned revision across its Stylo crates; dependency-fork changes must be committed and pushed before rebuilding Servo.

### A2. Audit transitive raw-WASM assumptions — ongoing

Stylo is wired, but the platform audit is not complete. Search and classify remaining occurrences of:

- `std::thread`, `thread::spawn`, `JoinHandle`, `std::sync::mpsc`;
- `std::time::{Instant,SystemTime}` and crates that call them internally;
- filesystem, environment, process, socket, DNS, native TLS, and OS signal APIs;
- `ipc-channel` and any channel implementation that assumes a process boundary;
- `glow`, `web_sys`, `wasm-bindgen`, WASI, and C/C++ archives;
- unconditional WebRender/WebGPU/WebGL initialization;
- resource readers, font discovery, and certificate loading.

For each result, classify it as: Worker implementation, compile-time exclusion, explicit unsupported error, or future feature. Do not leave accidental “works until called” behavior.

### A3. Establish a target-specific feature profile

Define one documented Worker feature profile that excludes desktop shell, multiprocess, WebDriver server, devtools server, native media backends, WebGL/WebGPU, filesystem caches, and platform font discovery unless they have a real Worker implementation. DOM, HTML parsing, CSS parsing/style, SpiderMonkey, URL, and fetch are enabled. Cookies are not currently implemented.

Add a CI check that builds exactly this profile and rejects newly introduced WASI, wasm-bindgen, or non-`env` imports.

## 4. Workstream B — finish the cooperative execution model

The same-thread script handle and constellation pump are the correct direction, but the model needs a complete contract.

### B1. Define pump semantics — implemented, stress gaps remain

`pumpStatus()` advances one cooperative turn and reports progress/fetch dispatches; `pumpUntilSettled()` applies caller budgets, waits on fetch activity and timer deadlines, and returns unsettled on budget exhaustion. It is a quiescence heuristic and cannot interrupt synchronous script execution. Add stress/edge coverage for nested timers and host callback/reentrancy behavior; keep callers' finite budgets explicit.

### B2. Replace native-only background services

Worker-safe paths now cover script pumping, deferred threadpool work, image decode, in-memory storage, fonts, canvas and CPU screenshot rendering. The native Servo shutdown path is still unavailable; reset does not destroy the runtime. Finish the source/API audit for unexercised blocking receives and classify every service as implemented, excluded, explicitly unsupported or future work. Stress lifetime paths without implying full destroy semantics.

### B3. Make lifecycle explicit — partial

Bootstrap, navigation/history/input, pumping, results, fetch cancellation and reset exports exist. Reset retires fetch callbacks and navigates to `about:blank`, but cannot destroy Servo/SpiderMonkey or guarantee a secure erase. The host adapter now aborts queued and in-flight fetches and retires their Rust callbacks when an accepted top-level navigation supersedes them; the regression test verifies the underlying request signal is aborted and the callback count returns to zero. Continue auditing incomplete loads and retained roots/callbacks; use a fresh WASM instance for user/session isolation.

## 5. Workstream C — make DOM, HTML, CSS, and JavaScript page execution real

### C1. Deterministic about:blank/inline document loading — implemented

`loadHtml(html, {url})` supplies one bounded synthetic HTML response without a host network fetch. Inline script, relative fetch resolution, computed CSS and immediate load after factory creation are covered. Expand this into fixture files for parser edge cases and repeated canceled-navigation stress; do not mistake the supplied URL/origin for an authorization or SSRF policy.

### C2. Fix Stylo and validate CSS — implementation is present; broaden fixtures

The clock patch, cascade, computed styles, mutation invalidation, shadow DOM and CSS mask properties are present. Continue testing:

- selectors and cascade;
- inline and stylesheet CSS;
- computed style values;
- stylesheet loading and parse errors;
- media-independent layout-facing values;
- DOM mutations that trigger style invalidation;
- custom elements and shadow DOM where supported.

Keep the first CSS corpus small and deterministic. Then add a curated WPT-style subset rather than attempting Servo’s desktop `test-wpt` cross-target harness.

### C3. Define page-evaluation behavior

The adapter currently enforces a serialized, single-result slot. Define and implement whether evaluation:

- in the page’s main realm;
- after the document is loaded or immediately;
- with a promise result or synchronous JSON result;
- with structured-clone values, exceptions, console output, and timeouts.

Add tests for script elements, DOM mutation, promise/microtask ordering, `setTimeout`, `fetch`, thrown errors, rejected promises, Unicode, typed arrays, and detached/reused documents. Keep the low-level int32 SpiderMonkey smoke export as a separate test.

## 6. Workstream D — implement the Worker fetch adapter fully

The version-4 ABI wraps `RequestBuilder` JSON in tagged fetch/cancel and WebSocket envelopes and accepts bounded, chunked response delivery. The adapter rejects mismatched versions before bootstrap. Continue hardening the protocol without reintroducing whole-response shortcuts.

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

The host-pull scheduler, timer IDs/cancellation, deadline export and basic promise/microtask ordering are implemented. The ABI 4 change adds a Worker-pumped refresh timer so `requestAnimationFrame` can run without a timer thread. Remaining work: nested/interval timer stress and improve execution interruption. A host deadline cannot interrupt a synchronous infinite page script.

## 8. Workstream F — storage and other browser services

Storage is in-memory for local/session storage per WASM instance. Reset does not clear all instance storage; a new instance provides isolation. Cookies, IndexedDB and Cache Storage are not implemented. Decide quotas and persistence semantics before exposing storage to users.

- Decide whether IndexedDB, Cache Storage, cookies and SQLite are required in the first release.
- For ephemeral local/session storage, document per-instance lifetime and establish quotas; persistence belongs behind a host service interface.
- For persistence, keep Cloudflare D1/KV/R2 integration in the future MCP/Worker host repository, behind an explicit host service interface. Do not couple Servo core to Cloudflare SDK types.
- Verify cryptographic randomness remains fail-closed and CSPRNG-backed.
- Route wall-clock use through the Worker host bridge wherever browser-visible timestamps require Unix time.

## 9. Workstream G — rendering, fonts, and screenshots

Font registration, text shaping/layout, canvas rasterization, display-list capture, `vello_cpu` rasterization and PNG streaming are implemented. Expand deterministic screenshot coverage and record unsupported visuals: backdrop filters and non-rounded clip paths have gaps, while only selected mask cases are covered; 3D transforms flatten and sticky positioning uses static position. Do not re-enable native Painter/Surfman paths without auditing their thread and platform assumptions.

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

- incremental production-profile build using `./mach build` (clean builds are not required);
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

Every layer should run against the production profile in CI. Preserve build artifacts and use incremental builds; do not remove the target tree or require a clean build. When dependency sources or linker configuration change, record the resolved revisions and verify the resulting artifact's imports and ABI.

## 12. Recommended execution order

1. Audit incomplete-navigation teardown, response-reader cancellation and cloned-response aborts as one lifetime batch. Ordinary navigation cancellation now has regression coverage; next test many canceled loads, retained callbacks/DOM roots and linear-memory growth.
2. Finish the page-evaluation API: correlated results, serialization, exceptions, awaited promises and explicit unsupported/timeout semantics. Keep the existing single-result probe documented until replaced.
3. Expand Fetch policy as one reviewed batch: manual redirects, request streaming, CORS preflight/credentials, cookie scope, and security-sensitive subresource behavior. Do not remove current fail-closed checks piecemeal.
4. Broaden the independent fixture corpus: modules/external scripts, custom elements, mutation observers, nested/interval timers, CSS cascade and layout-facing APIs. Keep rendering-dependent expectations separate.
5. Audit and implement or explicitly exclude storage, service workers, workers, media, WebGL and WebGPU. WebSockets now use the Worker host WebSocket API; report supported capabilities in the host API.
6. Execute the optional rendering/font/screenshot workstream if required for the release; current DOM/CSS success does not imply visible pixels.
7. A GitHub Actions workflow now builds the exact Worker profile incrementally and runs import/size checks through the Node suite plus local workerd smoke routes. Its first run exposed missing Servo uv setup and exited before the build; the workflow now uses Servo's Python/uv setup action, and the corrected remote run is pending. Preserve existing build artifacts and do not require a clean build. Existing native-target gating warnings remain, and the forked native-target changes need validation where relevant.
8. Investigate production CPU and total-isolate memory honestly alongside porting. No unapproved deployment or alternate paid hosting is part of this plan.
9. The separate `servo-mcp` Worker App is scaffolded and locally verified. Choose/implement access control for its intended audience, measure it under the intended Workers plan, and deploy only after bundle/CPU/memory limits are confirmed.

## 13. Exit checklist

- [x] Worker ABI, exact five-import allowlist, deterministic navigation, DOM/CSSOM, inline scripts, fetch, timers, canvas, fonts, native input and CPU screenshots are implemented; existing Node/workerd coverage exists.
- [x] Storage is in-memory per WASM instance; reset is documented as navigation/cancellation, not runtime destruction.
- [x] CPU renderer limitations and unsupported service classes are documented in `WORKER-ABI.md`.
- [x] Incremental production build passes; the module reports ABI 4, imports exactly the five allowed `env` functions, has no WASI/wasm-bindgen imports, and stays below the size limit.
- [x] Current artifact passes the 55-test Node suite, local workerd root smoke, 45 shared fixture runs over three rounds (15 distinct fixtures), and screenshot response.
- [x] Resolved fork revisions are recorded in Cargo.lock and each remote named branch matched local HEAD at the audit.
- [x] Ordinary accepted top-level navigation aborts superseded host fetches and retires their Rust callbacks; the adapter suite covers a slow request.
- [ ] Audit incomplete-navigation teardown and response-reader/clone cancellation. Ordinary navigation cancellation is covered, and this pass adds repeated canceled-navigation stress with callback and memory bounds. A cloned-reader cancellation probe did not settle and is documented as unsupported until the engine path is fixed.
- [ ] Upgrade evaluation beyond its serialized single-result slot: promise awaiting, result/error contract, timeout semantics and correlation.
- [ ] Complete request streaming and Fetch semantics where required; until then fail closed for credentialed/preflight CORS and other unsupported paths.
- [x] Verify ABI 4 Worker-driven one-shot and recurring `requestAnimationFrame` callbacks and WebSocket handshakes/messages/close against the production artifact. History traversal and other native blocking/API assumptions still need probes.
- [x] Add a CI workflow for the Worker profile, exact imports, ABI, size, Node tests and local workerd, using incremental builds only. The first run caught missing uv setup; the corrected Actions run remains pending. The fixture matrix now covers custom elements, MutationObserver records, nested timers, and repeated canceled navigation.
- [ ] Measure production CPU and total isolate memory on the intended Workers plan. The repeatable Node diagnostic now reports 331.447 ms CPU bootstrap, 250.463 ms page work, 36.313 ms maximum pump CPU, and four pumps over 10 ms; these are not production quota measurements. Workers Paid remains the recorded initial target.
- [x] Keep the MCP host separate. The new MIT-licensed `servo-mcp` project passes TypeScript checks, 16 network-policy tests, Wrangler deploy dry-run and local MCP initialize/tool-call/screenshot smoke. Its deployment and access-control policy remain pending in that repository.

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

**Fonts and text (Worker ABI version 2).** The wasm font backend was a
placeholder: it never read font bytes (every glyph 10 px wide, glyph ID =
code point) and its shaper returned no glyphs, so no text had glyphs or height,
and canvas `measureText`/`fillText` panicked ("couldn't find font") and trapped
the instance. Now:

- `fonts_traits::worker_fonts` is an in-memory registry; faces appear as local
  fonts (`worker-font:<n>`). Noto Sans (regular, bold), Noto Serif and Noto
  Sans Mono (OFL 1.1, ~2.6 MB; the user's preferred font) are bundled and back
  the generic families; hosts add more with `registerFont()`.
- `platform/wasm` implements `PlatformFont` on skrifa (charmap, advances,
  bounds, metrics) and the font list on the registry; missing glyphs fall back
  to every registered family.
- Text is shaped with HarfRust (already linked via usvg), adapted from Servo's
  earlier HarfRust backend (unmerged branch `origin/wr-skrifa`), so Servo's
  normal shaping path is used on wasm32 instead of a byte-wise ASCII path.
- The real `SystemFontService` runs in-process (same drain-before-`recv()`
  pattern as canvas and storage) and refreshes when fonts are registered.

Verified: 41/41 Node tests and 39/39 workerd case runs; 16 px sans-serif text
lays out with real font metrics (18.4 px line height with Liberation Sans, as Chrome/Firefox give for Arial; the bundled fonts were then switched to Noto, whose taller line height is expected). Artifact
60,349,728 bytes (about 6.7 MB below the 64 MiB limit). Font sanitization
(fontsan) remains unavailable on wasm32.

**Screenshots.** The Worker has no WebRender painter, so a CPU renderer interprets
display lists instead:

- `update_the_rendering` was disabled on wasm32; it now runs once per host frame
  request (`servo_worker_request_frame`). Layout's display list build then
  panicked in WebRender's `zeitstempel::now()` (std `Instant` on wasm32), fixed in
  `gptenv/webrender-wasm` `defce4362`. `servo_base::worker_trace` breadcrumbs,
  printed by the panic hook, located it in a stripped build.
- Worker Paint keeps each pipeline's latest display list and the images, fonts
  and font instances layout registers (`components/paint/worker_frame.rs`);
  resource keys were all `0` on the Worker and now come from a counter.
- `components/paint/worker_render.rs` draws them with vello_cpu and encodes PNG
  via `pixels`; see WORKER-ABI.md for coverage and gaps.
- Canvas frames reach Paint (the Worker canvas uses the page's paint API), and
  Paint answers canvas frame-delay requests immediately; previously a page with
  a canvas never rendered again after its first frame.

Verified with pixel-exact Node tests (44/44) and in workerd (a 640×360 page renders
in ~80 ms). The second pass added box/text shadows (vello_cpu's blurred rounded
rects and Gaussian blur filter layers), dotted/dashed/double/groove/ridge/
inset/outset borders, `mix-blend-mode`, CSS filters (color filters via an
offscreen color matrix, since vello_cpu implements only blur and drop-shadow),
scroll offsets and full-page capture. The page's scroll offset lives on the
implicit root scroll node, known only through Servo's scroll tree
(`ScrollTreeNode::webrender_id`), and Worker Paint applies layout's scroll
messages to captured scroll trees.

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
| `new WebSocket(url)` | Constructs in `CONNECTING` state (`readyState === 0`) but no `open` or `error` event fires after 30 Worker pump turns; the WebSocket transport is reported unsupported. |
| `canvas.getContext("webgl")` | Returns `null` immediately — correct, spec-compliant "unsupported" signal. |
| `requestAnimationFrame(cb)` | Accepts the callback without throwing, but callback did not fire after 30 Worker pump turns; reported unsupported. |
| `fetch(..., {credentials:"include"})` cross-origin | Rejects immediately with `TypeError: Network error: CORS check failed` once given enough pump turns to settle. Fails closed as WORKER-ABI.md claims. |
| Cross-origin `fetch` with no `Access-Control-Allow-Origin` header | Same explicit `TypeError` rejection. Fails closed as claimed. |
| `fetch(..., {redirect:"manual"})` | Call itself doesn't throw; actual redirect-handling behavior under `manual` mode was not exercised. |
| `getBoundingClientRect()` on an appended `<div>` | Returns real, non-placeholder-looking numbers (e.g. width matching the 1280px viewport minus margins) rather than zeros or an error — layout is doing real work here, not a stub; useful to know before assuming layout-dependent APIs are all unimplemented. |

Not yet characterized this pass: streaming/large request bodies past the
256 KiB in-memory limit, preflighted (non-simple) CORS requests, nested
`Worker`/`ServiceWorker` construction (the one attempt here hit
an unrelated URL-parsing `SyntaxError` before reaching the real question),
and whether history traversal completes safely. The runtime probes now classify
`requestAnimationFrame` and WebSocket transport as unsupported rather than
leaving them unverified.
