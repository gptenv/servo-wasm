# Worker browser compatibility matrix

This matrix describes the raw Worker artifact and adapter, not a guarantee that every website works. “Tested” means the production-artifact Node suite or local workerd fixtures exercise the stated path. “Partial” means meaningful behavior exists with named gaps. “Unverified” means this port has no sufficient Worker-artifact evidence. A supported property on `window` alone does not count as conformance.

| Area | Status | Current evidence and boundary |
| --- | --- | --- |
| HTML, DOM, CSSOM, script | Partial | Basic navigation, inline script, DOM/CSSOM and layout fixtures pass. Module/external-script corpus and broad WPT coverage remain open. |
| Timers, microtasks, animation frames | Partial | Nested timers, interval cancellation and one-shot/recurring animation frames pass. Runaway synchronous scripts (loops, generators, async/microtask storms, eval, regexp backtracking) are terminated by the per-turn script work budget; it bounds work, not CPU time or memory. |
| Fetch and redirects | Partial | Same-origin requests, non-credentialed cross-origin CORS (simple requests including POST bodies, and preflighted methods/headers with allow and deny cases checked on the server side), response streaming, cancellation and bounded redirects pass focused tests. Credentialed cross-origin requests, redirects after a preflight, a preflight cache and streaming uploads are unsupported. Response-clone cancellation remains unverified. |
| Cookies | Partial | Script cookies, final/redirect `Set-Cookie`, credentials omit, `HttpOnly` isolation and origin partitioning have focused tests. Complete SameSite site context, prefixes and partitioning policy remain open. |
| WebSockets | Partial | Host bridge handshakes, text/binary messages and close behavior pass focused tests. Service-level egress policy must be enforced by the host. |
| localStorage, sessionStorage | Partial | Read/write and navigation origin isolation pass. Data is lost with the WASM instance; browsing-context lifetime and quota remain open. |
| IndexedDB | Partial | Open/write/read passes. Abort/rollback, indexes, cursors, key ranges, upgrades, concurrency, structured clone and restart coverage remain open. |
| Cache Storage | Partial | `open`, `has`, ordered `keys` and `delete` pass. Cache request/response operations are absent. |
| Storage Manager | Partial | `estimate()` resolves with a metadata-only usage lower bound and unenforced 32 MiB estimate; `persist()` and `persisted()` report false. |
| Canvas 2D, images, fonts | Partial | Canvas drawing, image decoding and font registration pass focused tests. Raster and Worker SVG images have 8 Mpixel per-image limits; aggregate decoded-allocation budgets and malformed-input stress remain open. |
| CPU screenshots | Partial | Backgrounds, borders, text, images, canvas, shadows, filters, masks and scroll captures have deterministic fixtures. Sticky positioning, 3D, backdrop filters and clipping still have gaps. |
| Input | Partial | Mouse, keyboard and scroll dispatch exist and have focused coverage. Accessibility output and broader input semantics remain unverified. |
| History | Partial | Back/forward across documents, reload, and same-document `pushState` traversal with restored `history.state` and `popstate` pass (production artifact). `history.length` always reports 1; `replaceState`, hash navigation and cross-origin entries lack dedicated tests. |
| Blob, File and blob: URLs | Partial | An in-memory blob store answers Servo's file-manager messages. `Blob` reads/slices/structured clones, `File` metadata, `FileReader`, multipart `FormData` bodies, `createObjectURL`/`revokeObjectURL`, `fetch()` and `<img>` loads of blob: URLs, the origin check and revocation pass. Blob URL range requests are unsupported, there is no file picker, and blob memory is not yet counted toward an aggregate limit. |
| Forms and uploads/downloads | Unverified | No declared Worker compatibility corpus yet. Streaming request bodies are unsupported. |
| Streams, messaging, permissions | Unverified | No declared Worker compatibility corpus yet; individual native implementations may exist. |
| Synchronous XHR, Web Audio | Unsupported | Synchronous http(s) XHR throws `NetworkError` and `AudioContext`/`OfflineAudioContext` throw `NotSupportedError` instead of hanging or trapping (regression test runs in a child process with a deadline). |
| Web Crypto (`crypto`) | Partial | `crypto.getRandomValues()` and `crypto.randomUUID()` pass production-artifact coverage in the no-default-features Worker build. `crypto.subtle`/`SubtleCrypto` remains absent because the Worker build omits the heavyweight `webcrypto` feature. |
| Dedicated/shared workers and service workers | Unsupported | No cooperative Worker services are exposed in this port. |
| Media and audio | Unverified | No declared Worker compatibility corpus or host policy yet. |
| WebGL and WebGPU | Unsupported by design | Both are intentional exclusions. A software WebGL shim is a separate project. |

For release, attach a supported-site/workflow corpus and scoped WPT results to this matrix. Move an entry to tested only with a named artifact, revision and passing result. The broader non-GPU compatibility goal requires closing every non-GPU unsupported and unverified area that target sites depend on.
