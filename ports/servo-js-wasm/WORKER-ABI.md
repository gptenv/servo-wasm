# Raw Worker ABI, version 13

The JavaScript adapter and WASM artifact are a matched pair. The adapter checks
`servo_worker_abi_version() === 13` and checks that every export it requires
(redirect-cookie processing, the script budget, correlated evaluation, and cookie-state export/import) is present before
running constructors or bootstrap. Rebuild
the artifact whenever the interface or serialized request representation changes.
This is a project-internal protocol, not an MCP protocol or a stable upstream Servo API.

## Host imports

Exactly nine function imports exist, all in `env`:

| Import | Contract |
| --- | --- |
| `worker_fetch_request(ptr, len)` | Copy and decode a UTF-8 JSON command synchronously; do network I/O asynchronously. |
| `worker_fetch_body_chunk(id_ptr, id_len, bytes_ptr, bytes_len)` | Copy one request-body chunk into the host stream and resume its pending pull. |
| `worker_fetch_body_done(id_ptr, id_len)` | Close a completed request-body stream. |
| `worker_fetch_body_error(id_ptr, id_len)` | Error a failed request-body stream. |
| `worker_getrandom(ptr, len)` | Fill every requested byte using a CSPRNG; return zero on success, nonzero on failure. Never use a deterministic fallback. |
| `worker_log_error(ptr, len)` | Consume a UTF-8 diagnostic synchronously. |
| `worker_monotonic_now_ns()` | Return monotonic nanoseconds as a JavaScript `bigint`. |
| `worker_unix_time_now_ns()` | Return Unix-epoch nanoseconds as a JavaScript `bigint`. |
| `worker_media_command(operation, player_id, value, ptr, len)` | Copy a media-player command synchronously; schedule parsing, decoding and callbacks asynchronously after returning to Wasm. |

The fetch import carries `{version:13, kind:"fetch", request:...}`,
`{version:13, kind:"cancel", request_ids:[...]}`,
`{version:13, kind:"web_socket_connect", request_id, url, protocols}`, or
`{version:13, kind:"web_socket_action", request_id, action}`. IDs serialize as
UUID strings. The WebSocket commands use the Worker's `WebSocket` host API; the
adapter reports open, message, close and error events through the
`servo_worker_websocket_*` exports and forwards page send/close actions to the
host socket. Binary frames are exposed as `ArrayBuffer` in the page.
The fetch request DTO contains only `id`, `url`, `method`, `headers`, `body`,
`destination`, `redirect_mode` and `cors_preflight`. `headers` is an ordered array of
`[name, byte-array]` entries, preserving repeated fields. `body` is null or
contains exactly one of `worker_bytes: byte-array` and
`worker_stream_id: UUID string`. Small in-memory bodies use the byte field;
large or externally-backed bodies use the stream ID. The host creates a
`ReadableStream` and asks Servo for one chunk each time Fetch pulls. The stream
ends on `worker_fetch_body_done` or fails on `worker_fetch_body_error`; host
cancellation calls `servo_worker_cancel_request_body`. This keeps only a
bounded chunk in transit and preserves backpressure through Cloudflare's fetch
implementation. Since a consumed stream cannot be replayed, followed redirects
that preserve a streamed request body fail rather than silently sending an
empty or partial upload. Servo's internal `RequestBuilder` fields are not part
of this protocol. The adapter validates DTO structure and protocol versions
before dispatch, without a project-defined command or header-size ceiling.
Cancellation retires the Rust callback first, then cancels queued/active host I/O
and releases any active request-body reader.

## Response lifecycle

For a followed same-origin redirect, the adapter first calls
`servo_worker_process_redirect_cookies(payload, output)` with the response
URL, next URL, request ID and `getSetCookie()` values. The export checks the
pending request and its credentials mode, stores permitted cookies, and reports
the required output length before writing the next hop's `Cookie` header. The
adapter allocates the needed buffer and then sends the next request with it.

For each request ID, the host delivers:

1. `servo_worker_begin_http_response(id, url, status, headers, redirected)` once.
   Strings are UTF-8 pointer/length pairs; headers are JSON `[name,value]` pairs.
   Return 1 means accepted, 0 means invalid/stale/out-of-order input, and -1 means
   the response failed CORS checks and has already been terminated.
2. Zero or more `servo_worker_deliver_http_chunk(id, bytes)` calls.
3. Exactly one `servo_worker_finish_http_response(id)` on successful EOF, or
   `servo_worker_finish_http_error(id, message)` on body failure.

Before headers, use `servo_worker_deliver_http_error(id, message)` for failure.
Calls after completion/cancellation and chunks/EOF before headers return zero.
A mid-body error rejects body consumers; partial content is not a success.
Response data and headers have no project-defined size ceiling. Response bodies
stream through 64 KiB adapter chunks; the chunk size is transport segmentation,
not a total-response limit. The adapter releases unread bodies on error.

Allocation is explicit: `servo_js_alloc` / `servo_js_free`. Hosts must pass valid,
in-bounds allocated buffers; this is a trusted host ABI, not a memory-safe FFI for
arbitrary pointer values. Never retain a typed-array view across an export that
could grow WASM memory. The adapter copies each input and frees it after the call.

`servo_worker_cookie_state_len()` refreshes the serialized cookie jar and returns
its byte length, or a negative value if serialization fails;
`servo_worker_cookie_state_ptr()` points to those bytes until the next cookie-state
export. The host must copy them before calling another export that could refresh
the buffer. `servo_worker_restore_cookie_state(ptr, len)` replaces the complete
jar and returns 1 on success or 0 if the bytes are invalid.

Screen recording uses `servo_worker_render_jpeg(max_width, max_height, quality)`
after a render request and pump. The host copies the JPEG from
`servo_worker_recording_frame_ptr/len`; width and height are available through
`servo_worker_recording_frame_width/height`. `servo_worker_decode_jpeg(ptr, len)`
decodes a copied input buffer to RGBA, returned through
`servo_worker_decoded_recording_frame_ptr/len`. Recording frames are viewport
captures, downscaled to the supplied dimensions, and are intended for the
embedding host's video encoder.

## Browser and scheduling lifecycle

`runtime.capabilities()` returns a frozen report with `abiVersion`, `supported`,
`partial`, `unsupported`, `unsupportedReasons`, and `unverified` fields. The
reason map explains known Worker-port exclusions; it is not a complete roadmap.
Treat an unverified entry as unavailable until it has a passing runtime
characterization. The report describes this adapter's support contract, not
every API implemented by Servo's native builds, and it does not replace host-side
URL/SSRF policy.
The broader API inventory and unverified areas are tracked in
[`worker-compatibility-matrix.md`](../../docs/worker-compatibility-matrix.md).

## Media elements

The WASM backend sends encoded `<audio>` and `<video>` response bytes to the
embedding Worker. The host uses Mediabunny for container demuxing and the
browser's WebCodecs decoders. Primary video frames return as BGRA to Servo's
existing paint path. Ordinary audio PCM is sent to the embedding page for
device playback; audio connected to a Servo media-element audio source is sent
through Servo's existing media audio renderer. This host audio sink does not
implement Servo's general Web Audio API graph. Servo's `AudioContext` and
`OfflineAudioContext` constructors explicitly reject WASM Worker builds: Servo's
current media graph creates a dedicated render thread, while Cloudflare Workers
run single-threaded and have no device output. Offline rendering would need a
single-threaded graph backend; real-time audio would additionally need a host
audio output transport.

`worker_media_command` operations are: 0 create player, 1 set MIME type, 2 push
encoded bytes, 3 end input, 4 play, 5 pause, 6 stop, 7 seek, 8 set muted, 9 set
volume, 10 set playback rate, 11 destroy, 12 set input size, 13 set seekable,
and 14 set buffering. `value` carries a number/boolean or the create flags (bit
0: video renderer; bit 1: Servo media audio source; bit 2: seekable stream).
Commands return 0 when accepted, 1 when encoded data needs backpressure, and 2
for unsupported/rejected commands. The command importer has no project-defined
copy-size or player-count ceiling. The host uses stream backpressure while
decoding; buffered source data has no fixed byte quota.

The host calls `servo_worker_media_event` for metadata, playback state, end of
stream, `EnoughData`/`NeedData`, position, errors and duration. It calls
`servo_worker_media_video_frame` with tightly packed BGRA pixels, or
`servo_worker_media_audio_frame` with planar float32 samples. Frame dimensions
have no project-defined byte or dimension cap. They remain subject to the Wasm
address space, JavaScript typed-array limits, decoder constraints, and runtime
memory. The paired `servo_worker_media_alloc/free` exports allocate callback
input buffers without a project-defined size ceiling. The host must invoke these callbacks only after the command import
returns; re-entering Servo while its media code holds locks is invalid.

This is progressive primary-track playback, and codec availability varies with
the browser. MSE, HLS/DASH, DRM and reliable random-access seeking are not
provided. No codec implementation is embedded in Servo-WASM.

`document.cookie` and Worker fetch requests use Servo's RFC 6265 cookie jar.
`servo_worker_cookie_state_len/ptr` exports its complete postcard-serialized
state, and `servo_worker_restore_cookie_state` replaces it from host bytes.
This includes `HttpOnly` cookies and cookie attributes; page script still
cannot read `HttpOnly` cookies. Final responses and followed same-origin
redirects store `Set-Cookie` values when the host exposes
`Headers.getSetCookie()` and the request's credentials mode permits them.
Cross-site SameSite context checks are incomplete.

`localStorage`, `sessionStorage`, and IndexedDB remain runtime-local services
inside this WASM adapter. The Servo MCP Worker persists them in its per-tab
Durable Object: all Web Storage entries, each IndexedDB database's schema, and
records containing the supported structured-clone values. That host persistence
is not part of this raw ABI. Cache Storage is still runtime-local and incomplete:
request/response operations such as `Cache.match`, `Cache.put`, and `Cache.add`
are not implemented by the current Servo Cache API surface. `CacheStorage.keys()`
preserves cache creation order, including after deletion and recreation.
`navigator.storage.estimate()` reports an unbounded quota and a metadata-only
usage lower bound; `persist()` and `persisted()` report false. The WASM instance
and the embedding runtime's available memory remain practical limits.

One browser/SpiderMonkey runtime may be bootstrapped per WASM instance. A second
bootstrap returns false. Use a separate instance for unrelated incoming requests;
do not share mutable runtime state across requests or users.

`createServoWorkerRuntime()` first pumps a deterministic `about:blank` document
to establish the browsing context, then queues the optional requested URL. Callers
may load HTML or navigate as soon as the factory resolves; pump afterward to finish
that navigation. Raw-ABI callers must likewise finish the initial bootstrap before
sending replacement navigation requests.

`loadPage(url)` queues navigation. Host navigations between pumps coalesce to the
last URL. A Worker host navigation replaces a stalled top-level navigation.
`loadHtml(html, {url})` stages an HTML response for one HTTP(S) document URL,
without a host network fetch; relative resources still use the host adapter.
Only one supplied document may be staged at a time. A normal load or reset clears
any staged document.

`await evaluate(source, {maxDurationMs, maxTurns})` evaluates `source` in the
main page realm and, when it returns a promise or thenable, waits for it to
settle. It resolves with `{Ok: value}` or `{Err: error}`: values use the
WebDriver JSON clone (`Number`, `String`, `Array`, `Object`, `Element`, ...)
and a thrown exception or rejection becomes
`{Err: {EvaluationFailure: {message, filename, line_number, column, stack}}}`.
Promise settlement uses Servo's internal promise reactions, so a page that
overrides `Promise.prototype.then` cannot intercept it. `evaluate()` drives the
pump itself and returns as soon as its result arrives. Pump duration and turn
limits default to unlimited; a caller-supplied finite budget that expires
cancels the evaluation and resolves with `{Err: "Timeout"}`. `reset()` resolves
a pending evaluation with `{Err: "Canceled"}`. Script work is unlimited by
default and can be limited explicitly by the host. Like `pumpUntilSettled()`,
only one may run at a time.

The raw exports are `servo_worker_evaluate_page_async(ptr, len)`, which
returns a nonzero evaluation ID (zero if rejected); `servo_worker_poll_page_evaluation(id)`,
which returns 1 when the result JSON is in
`servo_worker_page_evaluation_result_ptr/len()` (the ID is then retired), 0
while pending and -1 for an unknown, canceled or retired ID; and
`servo_worker_cancel_page_evaluation(id)`. A reply for a canceled ID is
discarded. If the document is replaced before the script runs, the result is
`{Err: "WebViewNotReady"}`.

`evaluatePage(source)` is the older interface. It queues a main-page-realm evaluation. `pumpUntilSettled()`
does not report settlement while an accepted evaluation has no result yet; if a
caller-supplied finite budget expires first, it returns `settled:false`. `pageResult()` returns
the serialized Servo result once available. This is a low-level, single-result
slot: the adapter rejects a second evaluation until the first result is read.
Reading it consumes the adapter's result slot. The raw WASM ABI does not enforce
this sequencing. It does not await returned promises; use `evaluate()` for
that. An evaluation terminated by the script budget produces an `Err` result.

`requestAnimationFrame()` is driven by the Worker script timer after a frame is
requested. The adapter's `pumpUntilSettled()` includes its next deadline so
one-shot and recurring frame callbacks continue while a Worker invocation is
active.

`pumpStatus()` returns `{fetches, progressed, scriptsTerminated}`. The first two
come from a bit-packed export (bit zero is progress, remaining bits count host
fetch dispatches); `scriptsTerminated` counts scripts the operation budget
stopped during that pump. One pump advances the cooperative browser loop.
`nextTimerDelayMs()` exposes the next browser timer deadline.

`pumpUntilSettled({maxDurationMs, maxTurns, until})` waits for host response activity,
browser work and timers. Duration and turn budgets default to unlimited. It
requires eight quiet turns and an optional synchronous predicate to report
settlement. A caller-supplied finite budget can return `settled:false`.
Concurrent settling calls are rejected. This is a quiescence heuristic, not a
browser load event or proof that no future page work is possible. Its result
includes `scriptsTerminated` for the pumps it ran. A host time budget cannot
stop a synchronous page script by itself.

`createServoWorkerRuntime()` accepts an optional `onActivity` host callback. The
adapter invokes it when fetch or WebSocket work delivers activity. Hosts can use
it to schedule a later, serialized pump after the current invocation settles;
the callback must not re-enter the WASM runtime synchronously.

## Script operation budget

A Worker has one thread and its clock does not advance during synchronous
execution, so neither a watchdog nor a wall-clock deadline can interrupt a page
script. Instead, the Worker build of SpiderMonkey (see
`js/src/vm/WorkerScriptBudget.h` in the mozjs fork) charges a work budget at
its interrupt checks: one unit per loop back-edge, function or generator entry,
`finally` block, interrupt-polling builtin loop iteration and
regular-expression backtrack; one more unit per four bytes of bytecode a
backward jump crosses, so a long loop body is not free; 16 units per scripted
call; and 8 per native (builtin or DOM) call.
`servo_worker_set_script_budget(units)` (a `bigint`; zero is unlimited) sets
how many units the scripts of one pump turn may use in total. When a turn
exhausts it, the running script is terminated uncatchably (`catch` and
`finally` blocks do not run) and every later script entry in the same turn is
terminated too; the next turn starts with a fresh budget. Work outside pump
turns is not charged. `servo_worker_script_budget_terminations()` returns the
cumulative number of terminated scripts. The runtime stays usable: timers,
events and later evaluations run normally, although a terminated script may
leave page state half-updated. Deep recursion still raises the ordinary
catchable `InternalError` from the stack limit.

The adapter option `scriptBudget` defaults to zero (unlimited), and
`runtime.scriptBudgetTerminations` reports the counter. Hosts can choose a
finite work budget for a defensive runtime. It is a work count, not CPU time,
and it does not bound memory; hosting runtimes can still interrupt work when
their own execution limits are reached.

## Navigation and input

`loadPage(url)` performs native top-level navigation. `goBack()`, `goForward()`,
and `reload()` expose session-history traversal and reload; each returns whether
Servo accepted the action. Pump the runtime afterward to complete navigation.
Traversal between documents reloads them; traversal between `pushState()`
entries of one document keeps it, restores `history.state` and fires
`popstate`. The Worker keeps pushed state data in memory for the life of the
instance. Session history lives in the constellation, which records a
`pushState()` between pumps: settle after a script changes history before
traversing, or the traversal can overtake the new entry. `history.length`
always reports 1.

`pointerMove(x, y)`, `mouseDown(x, y, button)`, `mouseUp(x, y, button)`,
`click(x, y, button)`, `scrollBy(deltaX, deltaY, {x, y})`, `keyDown(key)`,
`keyUp(key)`, `pressKey(key)`, and `typeText(text)` dispatch Servo input events
through `WebView::notify_input_event`.
Coordinates are viewport device pixels, (0, 0) is the top-left, mouse buttons use
DOM numbering, and wheel deltas are pixels. Keyboard names use DOM key values
such as `a`, `Enter`, and `ArrowDown`; the view is focused before key events.
Hold modifiers with `keyDown("Control")`/`keyUp("Control")` (or Shift, Alt,
Meta) while dispatching another key. These are browser input events rather than
DOM events synthesized by page script.
The corresponding exports are `servo_worker_go_back/forward`,
`servo_worker_reload`, `servo_worker_pointer_move`,
`servo_worker_mouse_button`, `servo_worker_scroll_by`, and `servo_worker_key`.

## Screenshots

`await runtime.screenshot({maxDurationMs, maxPasses, fullPage})` returns PNG bytes
of the current page: the viewport at its current scroll position, or with
`fullPage` the whole document from its top, at the viewport width. It asks the
page to update its rendering once (layout otherwise stays idle on the Worker),
pumps until settled, and repeats while a frame starts new image, font or canvas
loads (without a pass limit by default), then rasterizes on the CPU. Rendered
dimensions have no project-defined cap; Wasm and PNG representations and
available runtime memory still apply. The legacy `servo_worker_render_png(flags)` export
(bit 0: full page) returns a complete PNG in the frame result buffer.
`servo_worker_request_frame()`, `servo_worker_frame_resource_generation()`,
`servo_worker_frame_png_ptr/len()`, `servo_worker_frame_item_count()` and
`servo_worker_frame_describe()` (a text dump of the captured spatial trees and
scroll offsets, in the same result buffer) are diagnostics.

For large captures, `await runtime.screenshotStream(options)` returns a PNG
`ReadableStream`. It settles the page before returning, then renders and
compresses one 1024-pixel strip on each pull. Passing the stream directly to
`new Response(stream)` avoids retaining the full PNG in either the WASM heap or
the JavaScript heap. `screenshot()` remains available and collects this stream
into a `Uint8Array` for compatibility. The stream exports are
`servo_worker_stream_png_begin(flags)`, `servo_worker_stream_png_next()`, and
`servo_worker_stream_png_finish()`; the PNG header and each strip are exposed
through `servo_worker_frame_png_ptr/len()` one chunk at a time.

The renderer interprets Servo's WebRender display list with vello_cpu: 2D
transforms, scroll offsets, rect and rounded-rect clips, backgrounds, text, images
(stretched and repeated), canvas content, borders of every style (radii only for
uniform solid borders), text decorations, linear and radial gradients, box
shadows (outset, inset, spread, blur), text shadows, opacity, `mix-blend-mode`,
CSS filters (blur and drop-shadow natively; brightness, contrast, grayscale,
hue-rotate, invert, saturate and sepia via a color matrix on an offscreen layer)
and iframes. 3D transforms are drawn flattened, sticky elements at their static
position, and backdrop-filter, masks and clip-path shapes other than rounded
rectangles are not yet drawn.

## Traps

A WASM trap does not unwind Rust state, so the instance is unusable afterwards.
After the first `WebAssembly.RuntimeError`, every adapter call throws a clear
"unusable after an earlier WASM trap" error and `runtime.trapped` holds the
original error. Create a new runtime (a new instance) to continue.

## In-process resources and rendering

`data:` URLs are decoded inside the module (Fetch "scheme fetch" semantics, a
basic response in every mode) and never become host subrequests. `blob:` URLs
are also resolved inside the module, from an in-memory blob store that stands
in for Servo's file manager (`components/net/worker_blob_store.rs`): only GET
is allowed, the URL must be valid (or held by a request's claim token) and
belong to the requesting origin, and the whole blob is returned (no range
requests). Blobs, slices, `File` objects and object URLs live in WASM memory
for the life of the instance and count toward available runtime memory. Images use
Servo's real image cache; decoding work is queued and run by the pump, not a
thread pool. Canvas 2D is rasterized in-process by `vello_cpu` (single-threaded
on wasm32), including `getImageData`, `putImageData`, `drawImage`, patterns,
gradients and `toDataURL`/`toBlob`. `getContext("webgl")` returns `null`. Canvas
text uses the fonts below. Page screenshots use Servo's display lists and the
Worker CPU renderer described above.

## Fonts

The Worker has no system fonts. Noto Sans (regular and bold), Noto Serif and
Noto Sans Mono are compiled in and registered at bootstrap; they back the
`sans-serif`, `serif` and `monospace` generic families. They are not
metric-compatible with Arial/Helvetica, so pages naming those fonts lay out
slightly differently than in a browser that has them. Text is shaped
with HarfRust (a pure-Rust HarfBuzz port) and measured with skrifa.

`runtime.registerFont(bytes)` (export `servo_worker_register_font(ptr, len)`)
adds a TTF/OTF/TTC/OTC file and returns its face count, or throws
for data that is not a font. Registered fonts are usable by name and as fallback
for characters the requested font lacks (for example CJK or emoji). Register
them before loading pages that need them. Fonts are held in WASM memory, which
counts toward available runtime memory. Web fonts (`@font-face`) are not
sanitized on this target (the native sanitizer is C); they are parsed only by
the memory-safe Rust parsers.

## No-UI browser behavior

The Worker has no screen, window chrome or user. `screen.*`, `outerWidth`,
`outerHeight`, `screenX` and `screenY` report the viewport (so the viewport width
passed at bootstrap is also what media queries and screenshots see as the screen); `alert`, `confirm`
and `prompt` are dismissed (`undefined`, `false`, `null`); `window.open` returns
`null`; `history.length` is `1`. `localStorage` and `sessionStorage` work and are
kept in memory for the lifetime of the WASM instance only.

## Limits and intentionally incomplete behavior

The adapter applies no project-defined response-byte, subrequest, pending-fetch,
connection-concurrency, WebSocket-count, or script-work limit by default.
`createServoWorkerRuntime()` accepts finite `maxResponseBytes`,
`maxSubrequests`, `maxSessionSubrequests`, and `scriptBudget` options for hosts
that want their own defensive profile. Fetch redirects follow Fetch's
20-redirect limit. Hosting, browser, WebAssembly and decoder limits still apply.

Cross-origin requests use CORS. Servo decides, following Fetch, whether a
request needs a preflight; if so the fetch DTO carries
`cors_preflight: {method, headers, credentials}` (the request method, its
sorted lowercase CORS-unsafe header names, and whether the original request
uses `credentials: include`). The adapter sends `OPTIONS` to the request URL
with `Origin`, `Accept: */*`, `Access-Control-Request-Method` and, when there
are unsafe headers, `Access-Control-Request-Headers`, with no body or cookies
and `redirect: "manual"`. It passes the response status and headers to
`servo_worker_check_cors_preflight(id, status, headers)`. Credentialed requests
require the exact `Access-Control-Allow-Origin`,
`Access-Control-Allow-Credentials: true`, and explicit methods and headers;
wildcards do not authorize them. The actual response passes the same CORS check
before cookies are accepted. Cookie sending applies SameSite and Secure rules.
The preflight returns 1 when the actual request may be sent, and -1 after
failing the request with a network error when it may not; the host must then
not send it. Each preflight is a host subrequest and there is no preflight
cache. Redirects of preflighted requests and cross-origin script-fetch
redirects fail closed. The host's destination and SSRF policy
must therefore cover every method, including preflights and cross-origin
`POST`, `PUT` and `DELETE` requests; see `worker-operations.md`. Cookie support is partial as described above; general request-body
streams are not implemented. Full Fetch redirect/manual-redirect behavior,
response-reader and cloned-response cancellation semantics, service-worker
support, IndexedDB transaction behavior and Cache request/response operations
remain unsupported or uncharacterized. The Service Worker DOM and manager code
exist in Servo, but this port leaves the feature disabled; its manager and
service-worker globals require dedicated threads, so exposing them requires a
cooperative scheduler, durable registrations, and host fetch-event routing.
Some APIs would block the Worker's only
thread or spawn a native thread, so they fail explicitly instead: synchronous
`XMLHttpRequest` to http(s) URLs throws `NetworkError` (the host cannot answer
until the script returns; synchronous `data:` and `blob:` requests still
work), and `new Worker()`, `new AudioContext()` and `new OfflineAudioContext()`
throw `NotSupportedError`. The production artifact exposes basic Web Crypto:
`crypto.getRandomValues()` uses the host CSPRNG bridge and `crypto.randomUUID()`
produces version 4 UUIDs; both pass the no-default-features runtime suite.
`SubtleCrypto` remains absent because the Worker build omits the heavyweight
`webcrypto` feature. Script execution is bounded by the
operation budget above, not by CPU time. WebGL and WebGPU
are intentional exclusions. Screenshots
are implemented with the CPU renderer described above; backdrop filters and
non-rounded clip paths have rendering gaps, and only the tested mask cases are
covered. The eventual host must add its own destination/SSRF policy before
accepting arbitrary users.
