# Raw Worker ABI, version 3

The JavaScript adapter and WASM artifact are a matched pair. The adapter checks
`servo_worker_abi_version() === 3` before running constructors or bootstrap. Rebuild
the artifact whenever the interface or serialized request representation changes.
This is a project-internal protocol, not an MCP protocol or a stable upstream Servo API.

## Host imports

Exactly five function imports exist, all in `env`:

| Import | Contract |
| --- | --- |
| `worker_fetch_request(ptr, len)` | Copy and decode a UTF-8 JSON command synchronously; do network I/O asynchronously. |
| `worker_getrandom(ptr, len)` | Fill every requested byte using a CSPRNG; return zero on success, nonzero on failure. Never use a deterministic fallback. |
| `worker_log_error(ptr, len)` | Consume a UTF-8 diagnostic synchronously. |
| `worker_monotonic_now_ns()` | Return monotonic nanoseconds as a JavaScript `bigint`. |
| `worker_unix_time_now_ns()` | Return Unix-epoch nanoseconds as a JavaScript `bigint`. |

The fetch import carries either `{version:3, kind:"fetch", request:...}` or
`{version:3, kind:"cancel", request_ids:[...]}`. IDs serialize as UUID strings.
The request uses Servo's `RequestBuilder` serialization (including URL, method,
byte-string headers, destination, mode, credentials and redirect policy). A body
is currently supported only via `body.worker_bytes`, limited to 256 KiB.
Cancellation retires the Rust callback first, then cancels queued/active host I/O.

## Response lifecycle

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
Response limits: IDs 64 bytes, URLs 16 KiB, headers 64 KiB, error messages 4096
bytes, chunks 256 KiB at the ABI (the adapter uses 64 KiB). The adapter defaults
to 8 MiB total response bytes and releases unread bodies on error.

Allocation is explicit: `servo_js_alloc` / `servo_js_free`. Hosts must pass valid,
in-bounds allocated buffers; this is a trusted host ABI, not a memory-safe FFI for
arbitrary pointer values. Never retain a typed-array view across an export that
could grow WASM memory. The adapter copies each input and frees it after the call.

## Browser and scheduling lifecycle

`runtime.capabilities()` returns a frozen `{abiVersion, supported, partial,
unsupported, unverified}` report for the host-facing feature set. Treat an
unverified entry as unavailable until it has a passing runtime characterization.
The report describes this adapter's support contract; it does not replace
host-side URL/SSRF policy.

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
`loadHtml(html, {url})` stages a bounded HTML response for one HTTP(S) document URL,
without a host network fetch; relative resources still use the host adapter.
Only one supplied document may be staged at a time. A normal load or reset clears
any staged document.

`evaluatePage(source)` queues a main-page-realm evaluation. `pumpUntilSettled()`
does not report settlement while an accepted evaluation has no result yet; if the
result never arrives, the budget is exhausted and it returns `settled:false`. `pageResult()` returns
the serialized Servo result once available. This is a low-level, single-result
slot: the adapter rejects a second evaluation until the first result is read.
Reading it consumes the adapter's result slot. The raw WASM ABI does not enforce
this sequencing. Evaluation does not await returned JavaScript promises or
implement a script timeout.

`pumpStatus()` returns `{fetches, progressed}` from a bit-packed export (bit zero
is progress, remaining bits count host fetch dispatches). One pump advances the
cooperative browser loop; it is not an instruction/time-preemptible unit.
`nextTimerDelayMs()` exposes the next browser timer deadline.

`pumpUntilSettled({maxDurationMs, maxTurns, until})` waits for host response activity,
browser work and timers. It requires eight quiet turns and an optional synchronous
predicate to report settlement. Exhausted budgets return `settled:false`.
Concurrent settling calls are rejected. This is a quiescence heuristic, not a
browser load event or proof that no future page work is possible. It cannot stop
an infinite page script inside a synchronous WASM call.

With `networkIdleMs`, pending or recurring timers alone no longer keep the page
busy: once no fetch has been queued or in flight for that long, it returns
`{settled:true, timersPending:true}`. Use it for real sites with polling,
carousels or analytics timers, which otherwise never settle. `screenshot()`
uses `networkIdleMs: 500` by default.

`reset()` cancels network work, retires callbacks, clears results and queues
`about:blank`; callers must pump the reset or queue a replacement load. It is not
a secure erase of all storage or a full engine destroy. A fresh WASM instance is
required for isolation between unrelated users. Drop host references when the
invocation ends; Servo's native blocking shutdown path is not used.

## Navigation and input

`loadPage(url)` performs native top-level navigation. `goBack()`, `goForward()`,
and `reload()` expose session-history traversal and reload; each returns whether
Servo accepted the action. Pump the runtime afterward to complete navigation.

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
`fullPage` the whole document from its top, at the viewport width (height capped
at 8 Mpixels to fit the 128 MB isolate). It asks the page to update its rendering
once (layout otherwise stays idle on the Worker), pumps until settled, and repeats
while a frame starts new image, font or canvas loads (up to `maxPasses`, default 4),
then rasterizes on the CPU. The legacy `servo_worker_render_png(flags)` export
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
basic response in every mode) and never become host subrequests. Images use
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
adds a TTF/OTF/TTC/OTC file of up to 32 MiB and returns its face count, or throws
for data that is not a font. Registered fonts are usable by name and as fallback
for characters the requested font lacks (for example CJK or emoji). Register
them before loading pages that need them. Fonts are held in WASM memory, which
counts toward the 128 MB isolate limit. Web fonts (`@font-face`) are not
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

The default host limits are six simultaneous fetches, fifty pending requests and
fifty actual network fetch calls (redirects count). The network-call budget lasts
for the runtime's lifetime, including resets. Synthetic supplied HTML costs no
network fetch. These bounds do not prove Cloudflare CPU or total-memory compliance.

Only simple non-credentialed direct cross-origin GET/HEAD requests have CORS
support. Preflighted/credentialed requests and cross-origin script-fetch redirects
fail closed. Cookies and general request-body streams are not implemented. Full
Fetch redirect/manual-redirect behavior, response-reader and cloned-response
cancellation semantics, WebSocket/service-worker/IndexedDB/Cache Storage support,
and hard script deadlines remain unsupported or uncharacterized. Screenshots
are implemented with the CPU renderer described above; backdrop filters and
non-rounded clip paths have rendering gaps, and only the tested mask cases are
covered. The eventual host must add its own destination/SSRF policy before
accepting arbitrary users.
