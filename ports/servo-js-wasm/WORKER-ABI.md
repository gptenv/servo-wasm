# Raw Worker ABI, version 1

The JavaScript adapter and WASM artifact are a matched pair. The adapter checks
`servo_worker_abi_version() === 1` before running constructors or bootstrap. Rebuild
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

The fetch import carries either `{version:1, kind:"fetch", request:...}` or
`{version:1, kind:"cancel", request_ids:[...]}`. IDs serialize as UUID strings.
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

`evaluatePage(source)` queues a main-page-realm evaluation. `pageResult()` returns
the serialized Servo result once available. This is a low-level, single-result
slot: serialize evaluations and read each result before starting the next one.
It does not await returned JavaScript promises or implement a script timeout.

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

`reset()` cancels network work, retires callbacks, clears results and queues
`about:blank`; callers must pump the reset or queue a replacement load. It is not
a secure erase of all storage or a full engine destroy. A fresh WASM instance is
required for isolation between unrelated users. Drop host references when the
invocation ends; Servo's native blocking shutdown path is not used.

## Limits and intentionally incomplete behavior

The default host limits are six simultaneous fetches, fifty pending requests and
fifty actual network fetch calls (redirects count). The network-call budget lasts
for the runtime's lifetime, including resets. Synthetic supplied HTML costs no
network fetch. These bounds do not prove Cloudflare CPU or total-memory compliance.

Only simple non-credentialed direct cross-origin GET/HEAD requests have CORS
support. Preflighted/credentialed requests and cross-origin script-fetch redirects
fail closed. Cookies, general request-body streams, full redirect semantics,
screenshots, and hard script deadlines are not provided by this ABI. The eventual
host must add its own destination/SSRF policy before accepting arbitrary users.
