/*
 * Cloudflare Worker host adapter for servo_js_wasm.
 *
 * This file deliberately contains no WASI shim. The Rust module imports only
 * the small `env` surface below, and outbound navigation/subresource requests
 * are fulfilled by the Worker's standard fetch() implementation.
 */

const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;
const MAX_RESPONSE_HEADERS_BYTES = 64 * 1024;
const RESPONSE_CHUNK_BYTES = 64 * 1024;
const MAX_REDIRECTS = 10;
const MAX_OUTGOING_CONNECTIONS = 6;
const MAX_PENDING_FETCHES = 50;
const MAX_HOST_MESSAGE_BYTES = 2 * 1024 * 1024;
const FREE_TIER_SUBREQUESTS = 50;
const DEFAULT_SESSION_SUBREQUESTS = 10_000;
const WORKER_ABI_VERSION = 6;
const encoder = new TextEncoder();

const WORKER_CAPABILITIES = Object.freeze({
  abiVersion: WORKER_ABI_VERSION,
  supported: Object.freeze([
    'navigation', 'mouse-keyboard-input', 'html-dom',
    'javascript', 'cssom', 'computed-style', 'layout-measurements',
    'timers', 'microtasks', 'fetch', 'canvas-2d', 'image-decoding',
    'font-registration', 'cpu-screenshots', 'local-session-storage',
    'request-animation-frame', 'websocket-transport',
    'indexeddb', 'cache-storage-lifecycle',
  ]),
  partial: Object.freeze({
    fetch: 'Response bodies stream; request bodies are buffered up to 256 KiB. ' +
      'CORS is limited to simple non-credentialed cross-origin GET/HEAD.',
    cookies: 'document.cookie and Worker fetches use Servo’s in-memory RFC 6265 ' +
      'cookie jar. Final and followed same-origin redirect cookies require ' +
      'Headers.getSetCookie() and the request credentials mode; complete ' +
      'SameSite context checks are missing, and ' +
      'cookies are lost when the WASM instance is discarded.',
    storage: 'localStorage, sessionStorage, IndexedDB and Cache Storage are ' +
      'instance-local and do not persist across WASM instances. ' +
      'navigator.storage.estimate() has a metadata-only usage lower bound and ' +
      'an unenforced 32 MiB quota estimate; persist() reports false. ' +
      'Cache request/response operations ' +
      'are not implemented.',
    screenshots: 'CPU display-list renderer; backdrop filters and non-rounded ' +
      'clip paths are incomplete, and mask coverage is limited to tested cases.',
    lifecycle: 'reset cancels work and navigates to about:blank; it does not ' +
      'destroy Servo or SpiderMonkey. Use a fresh WASM instance for isolation.',
    pageEvaluation: 'Serialized single-result slot; returned promises are not awaited ' +
      'and synchronous scripts cannot be interrupted.',
  }),
  unsupported: Object.freeze([
    'service-workers',
    'dedicated-shared-workers', 'webgl', 'webgpu',
    'credentialed-preflight-cors', 'streaming-request-bodies',
  ]),
  unverified: Object.freeze(['history-traversal']),
});

function bytesToString(bytes) {
  return new TextDecoder().decode(bytes);
}

/** Decode the versioned, bounded command sent by the trusted WASM module. */
export function parseWorkerHostMessage(bytes) {
  if (bytes.byteLength > MAX_HOST_MESSAGE_BYTES) {
    throw new RangeError('Servo Worker host command exceeds 2 MiB');
  }
  const message = JSON.parse(bytesToString(bytes));
  if (!message || typeof message !== 'object' || Array.isArray(message) ||
      message.version !== WORKER_ABI_VERSION) {
    throw new TypeError('Servo Worker host protocol version mismatch');
  }
  const identifier = (id) => typeof id === 'string' && id.length > 0 && id.length <= 64;
  if (message.kind === 'fetch') {
    const request = message.request;
    if (!request || typeof request !== 'object' || Array.isArray(request) ||
        !identifier(request.id) || typeof request.url !== 'string' ||
        request.url.length > 16 * 1024 ||
        typeof request.method !== 'string' || !/^[A-Z]{1,32}$/.test(request.method) ||
        !Array.isArray(request.headers) || request.headers.length > 256 ||
        typeof request.destination !== 'string' ||
        typeof request.redirect_mode !== 'string') {
      throw new TypeError('Malformed Servo Worker fetch command');
    }
    let headerBytes = 0;
    for (const entry of request.headers) {
      if (!Array.isArray(entry) || entry.length !== 2 ||
          typeof entry[0] !== 'string' || !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(entry[0]) ||
          !Array.isArray(entry[1]) ||
          !entry[1].every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
        throw new TypeError('Malformed Servo Worker fetch header');
      }
      headerBytes += entry[0].length + entry[1].length;
    }
    if (headerBytes > MAX_RESPONSE_HEADERS_BYTES) {
      throw new RangeError('Servo Worker request headers exceed 64 KiB');
    }
    if (request.body !== null && request.body !== undefined) {
      const body = request.body?.worker_bytes;
      if (body !== null && body !== undefined &&
          (!Array.isArray(body) || body.length > 256 * 1024 ||
           !body.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255))) {
        throw new TypeError('Malformed Servo Worker request body');
      }
    }
  } else if (message.kind === 'cancel') {
    if (!Array.isArray(message.request_ids) || message.request_ids.length > MAX_PENDING_FETCHES ||
        !message.request_ids.every(identifier)) {
      throw new TypeError('Malformed Servo Worker cancel command');
    }
  } else if (message.kind === 'web_socket_connect' ||
             message.kind === 'web_socket_action') {
    if (!identifier(message.request_id)) {
      throw new TypeError('Malformed Servo Worker WebSocket command');
    }
  } else {
    throw new TypeError('Unknown Servo Worker host command');
  }
  return message;
}

function requestUrl(request) {
  // UrlWithBlobClaim is serialized as an object by serde; keep the string
  // case too so this remains useful with simplified test fixtures.
  if (typeof request.url === "string") return request.url;
  if (request.url && typeof request.url.url === "string") return request.url.url;
  throw new TypeError("Servo request did not contain a usable URL");
}

function requestHeaders(request) {
  const headers = new Headers();
  const source = request.headers;
  if (!source) return headers;
  const decode = (value) => {
    if (typeof value === 'string') return value;
    if (Array.isArray(value) && value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
      // HTTP field values are byte strings, not necessarily UTF-8.
      return value.map((byte) => String.fromCharCode(byte)).join('');
    }
    throw new TypeError('Servo request contained an invalid header value');
  };
  if (Array.isArray(source)) {
    for (const entry of source) {
      if (Array.isArray(entry) && entry.length === 2) headers.append(entry[0], decode(entry[1]));
    }
  } else {
    for (const [name, value] of Object.entries(source)) {
      if (Array.isArray(value) && !value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
        for (const item of value) headers.append(name, decode(item));
      } else {
        headers.set(name, decode(value));
      }
    }
  }
  return headers;
}

function requestMethod(request) {
  return typeof request.method === "string" ? request.method : "GET";
}

/**
 * Instantiate the Servo wasm module and return a Worker-local browser.
 *
 * `wasmModule` may be a WebAssembly.Module or bytes accepted by
 * WebAssembly.instantiate. Each invocation creates an independent WASM
 * instance and Servo browser; do not share the returned runtime between
 * unrelated incoming requests.
 */
export async function createServoWorkerRuntime(wasmModule, {
  fetchImpl = globalThis.fetch,
  webSocketFactory = (url, protocols) => new WebSocket(url, protocols),
  width = 1280,
  height = 720,
  url = "about:blank",
  log = console.error,
  maxResponseBytes = MAX_RESPONSE_BYTES,
  maxSubrequests = FREE_TIER_SUBREQUESTS,
  maxSessionSubrequests = DEFAULT_SESSION_SUBREQUESTS,
} = {}) {
  if (!Number.isSafeInteger(maxResponseBytes) || maxResponseBytes < 0 ||
      maxResponseBytes > 64 * 1024 * 1024) {
    throw new RangeError('maxResponseBytes must be between 0 and 64 MiB');
  }
  if (!Number.isSafeInteger(maxSubrequests) || maxSubrequests < 1) {
    throw new RangeError('maxSubrequests must be a positive integer');
  }
  if (!Number.isSafeInteger(maxSessionSubrequests) || maxSessionSubrequests < 1) {
    throw new RangeError('maxSessionSubrequests must be a positive integer');
  }
  let runtime;
  const imports = {
    env: {
      worker_monotonic_now_ns: () => BigInt(Math.round(performance.now() * 1e6)),
      worker_unix_time_now_ns: () => BigInt(Date.now()) * 1_000_000n,
      worker_getrandom: (ptr, len) => {
        try {
          const memory = new Uint8Array(runtime.instance.exports.memory.buffer);
          for (let offset = 0; offset < len; offset += 65_536) {
            const count = Math.min(65_536, len - offset);
            crypto.getRandomValues(memory.subarray(ptr + offset, ptr + offset + count));
          }
          return 0;
        } catch (error) {
          log(`Servo Worker CSPRNG failed: ${error}`);
          return 1;
        }
      },
      worker_log_error: (ptr, len) => {
        const bytes = new Uint8Array(runtime.instance.exports.memory.buffer, ptr, len);
        log(bytesToString(bytes));
      },
      worker_fetch_request: (ptr, len) => {
        const bytes = new Uint8Array(runtime.instance.exports.memory.buffer, ptr, len);
        const message = parseWorkerHostMessage(bytes);
        if (message.kind === 'fetch') runtime.dispatchFetch(message.request);
        else if (message.kind === 'cancel') runtime.cancelFetches(message.request_ids);
        else if (message.kind === 'web_socket_connect') runtime.connectWebSocket(message);
        else if (message.kind === 'web_socket_action') runtime.webSocketAction(message);
      },
    },
  };

  const instantiated = await WebAssembly.instantiate(wasmModule, imports);
  const instance = instantiated instanceof WebAssembly.Instance
    ? instantiated
    : instantiated.instance;
  if (instance.exports.servo_worker_abi_version?.() !== WORKER_ABI_VERSION) {
    throw new Error('Servo Worker ABI mismatch; rebuild the WASM artifact with this adapter');
  }
  if (typeof instance.exports.servo_worker_process_redirect_cookies !== 'function') {
    throw new Error('Servo Worker artifact lacks redirect-cookie support required by this adapter');
  }
  runtime = new ServoWorkerRuntime(
    instance, fetchImpl, webSocketFactory, log, maxResponseBytes,
    maxSubrequests, maxSessionSubrequests,
  );
  runtime.instance.exports.__wasm_call_ctors?.();
  runtime.instance.exports.servo_worker_install_fetch_adapter();
  // Establish the initial browsing context before callers can queue loads.
  // A native embedder's LoadUrl requires that context to exist; returning
  // immediately after bootstrap makes an immediate load race its creation.
  if (!runtime.bootstrap(width, height, 'about:blank')) {
    throw new Error("Servo Worker bootstrap failed");
  }
  const bootstrapStatus = await runtime.pumpUntilSettled();
  if (!bootstrapStatus.settled) throw new Error('Servo Worker initial document did not settle');
  if (url !== 'about:blank' && !runtime.loadPage(url)) {
    throw new Error('Servo Worker initial navigation was rejected');
  }
  return runtime;
}

class ServoWorkerRuntime {
  #fetchImpl;
  #webSocketFactory;
  #log;
  #maxResponseBytes;
  #maxSubrequests;
  #subrequestCount = 0;
  #maxSessionSubrequests;
  #sessionSubrequestCount = 0;
  #inFlightFetches = new Set();
  #fetchControllers = new Map();
  #responseStartedFetches = new Set();
  #fetchQueue = [];
  #webSockets = new Map();
  #generation = 0;
  #inlinePage = null;
  #activityVersion = 0;
  #activityWaiters = new Set();
  #settling = false;
  #trap = null;
  #evaluationPending = false;
  #evaluationResultReady = false;
  #screenshotStreamActive = false;
  #pressedModifiers = new Set();

  constructor(instance, fetchImpl, webSocketFactory, log, maxResponseBytes,
              maxSubrequests, maxSessionSubrequests) {
    // A WASM trap does not unwind Rust state (held RefCell borrows, partial
    // updates), so after the first trap every later export call would fail
    // with a misleading secondary panic. Refuse them explicitly instead.
    const exports = {};
    for (const [name, value] of Object.entries(instance.exports)) {
      exports[name] = typeof value !== 'function' ? value : (...args) => {
        if (this.#trap) {
          throw new Error('Servo runtime is unusable after an earlier WASM trap; ' +
            'create a new runtime', { cause: this.#trap });
        }
        try {
          return value(...args);
        } catch (error) {
          if (error instanceof WebAssembly.RuntimeError) this.#trap = error;
          throw error;
        }
      };
    }
    this.instance = { exports };
    this.#fetchImpl = fetchImpl;
    this.#webSocketFactory = webSocketFactory;
    this.#log = log;
    this.#maxResponseBytes = maxResponseBytes;
    this.#maxSubrequests = maxSubrequests;
    this.#maxSessionSubrequests = maxSessionSubrequests;
  }

  /** Start a new serialized host invocation after the previous one has settled. */
  beginInvocation() {
    if (this.#trap) {
      throw new Error('Servo runtime is unusable after a WASM trap; create a new runtime',
        { cause: this.#trap });
    }
    if (this.#inFlightFetches.size || this.#fetchQueue.length ||
        this.pendingFetchCount() || this.#settling || this.#screenshotStreamActive ||
        this.#evaluationPending || this.#evaluationResultReady) {
      throw new Error('Finish or cancel pending Worker work before a new invocation');
    }
    this.#subrequestCount = 0;
  }

  #reserveSubrequest() {
    if (this.#subrequestCount >= this.#maxSubrequests) {
      throw new Error(`Servo Worker exceeded ${this.#maxSubrequests} host subrequests in this invocation`);
    }
    if (this.#sessionSubrequestCount >= this.#maxSessionSubrequests) {
      throw new Error(`Servo Worker exceeded ${this.#maxSessionSubrequests} host subrequests in this session`);
    }
    this.#subrequestCount++;
    this.#sessionSubrequestCount++;
  }

  /** Machine-readable support report for this Worker ABI. */
  capabilities() {
    return WORKER_CAPABILITIES;
  }

  #write(value) {
    const bytes = encoder.encode(value);
    if (bytes.length > 1024 * 1024) throw new RangeError('Servo input exceeds 1 MiB');
    const ptr = this.instance.exports.servo_js_alloc(bytes.length);
    if (!ptr) throw new Error("Servo wasm allocation failed");
    new Uint8Array(this.instance.exports.memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  }

  #free(ptr, len) {
    this.instance.exports.servo_js_free(ptr, len);
  }

  #notifyActivity() {
    this.#activityVersion++;
    for (const resolve of this.#activityWaiters) resolve();
    this.#activityWaiters.clear();
  }

  #waitForActivity() {
    let resolve;
    const promise = new Promise((done) => { resolve = done; });
    this.#activityWaiters.add(resolve);
    return { promise, cancel: () => this.#activityWaiters.delete(resolve) };
  }

  #withBytes(byteArrays, callback) {
    const buffers = [];
    try {
      for (const bytes of byteArrays) {
        const ptr = bytes.length ? this.instance.exports.servo_js_alloc(bytes.length) : 0;
        if (bytes.length && !ptr) throw new Error('Servo wasm allocation failed');
        buffers.push({ ptr, len: bytes.length });
        if (bytes.length) {
          new Uint8Array(this.instance.exports.memory.buffer, ptr, bytes.length).set(bytes);
        }
      }
      return callback(buffers);
    } finally {
      for (const { ptr, len } of buffers) if (len) this.#free(ptr, len);
    }
  }

  #deliverError(requestId, message, responseStarted) {
    const id = encoder.encode(requestId);
    const detail = encoder.encode(String(message).slice(0, 2048)).subarray(0, 4096);
    return this.#withBytes([id, detail], ([idBuffer, detailBuffer]) => {
      const method = responseStarted
        ? this.instance.exports.servo_worker_finish_http_error
        : this.instance.exports.servo_worker_deliver_http_error;
      return method(idBuffer.ptr, idBuffer.len, detailBuffer.ptr, detailBuffer.len) === 1;
    });
  }

  #redirectCookieHeader(requestId, responseUrl, nextUrl, setCookies) {
    const payload = encoder.encode(JSON.stringify({
      request_id: requestId, response_url: responseUrl,
      next_url: nextUrl, set_cookies: setCookies,
    }));
    return this.#withBytes([payload], ([input]) => {
      const capacity = 16 * 1024;
      const output = this.instance.exports.servo_js_alloc(capacity);
      if (!output) throw new Error('Servo wasm allocation failed');
      try {
        const length = this.instance.exports.servo_worker_process_redirect_cookies(
          input.ptr, input.len, output, capacity,
        );
        if (length < 0 || length > capacity) {
          throw new Error('Servo rejected redirect cookies');
        }
        if (length === 0) return null;
        return bytesToString(new Uint8Array(this.instance.exports.memory.buffer, output, length));
      } finally {
        this.#free(output, capacity);
      }
    });
  }

  async #fetchRequest(request, body, signal) {
    let url = requestUrl(request);
    let method = requestMethod(request);
    let headers = requestHeaders(request);
    let redirected = false;
    const mode = request.redirect_mode || 'Follow';
    if (request.destination === 'Document' && method === 'GET' &&
        this.#inlinePage?.url === url) {
      const page = this.#inlinePage;
      this.#inlinePage = null;
      return {
        response: new Response(page.bytes, {
          status: 200,
          headers: { 'content-type': 'text/html; charset=utf-8' },
        }),
        url,
        redirected: false,
      };
    }
    for (let redirects = 0; redirects <= MAX_REDIRECTS; redirects++) {
      this.#reserveSubrequest();
      const response = await this.#fetchImpl(url, {
        method, headers, body, redirect: 'manual', signal,
      });
      const location = response.headers.get('location');
      if (![301, 302, 303, 307, 308].includes(response.status) || !location) {
        return { response, url: response.url || url, redirected };
      }
      if (mode !== 'Error' && (mode === 'Manual' || request.destination === 'Document')) {
        return { response, url: response.url || url, redirected };
      }
      try {
        if (mode === 'Error') throw new Error('Servo redirect mode forbids redirects');
        if (redirects === MAX_REDIRECTS) throw new Error('Too many Worker redirects');
        const next = new URL(location, url);
        if (next.protocol !== 'http:' && next.protocol !== 'https:') {
          throw new Error('Worker redirect target is not HTTP(S)');
        }
        if (next.origin !== new URL(requestUrl(request)).origin &&
            request.destination === 'None') {
          throw new Error('Cross-origin page fetch requires CORS filtering');
        }
        if (next.origin !== new URL(url).origin) {
          // Cloudflare forwards *all* headers when fetch follows redirects.
          // Keep only ordinary content negotiation headers across origins.
          const allowed = new Headers();
          for (const name of ['accept', 'accept-language']) {
            const value = headers.get(name);
            if (value !== null) allowed.set(name, value);
          }
          headers = allowed;
        }
        const cookieHeader = this.#redirectCookieHeader(
          request.id, response.url || url, next.href,
          response.headers.getSetCookie?.() ?? [],
        );
        headers.delete('cookie');
        if (cookieHeader !== null) headers.set('cookie', cookieHeader);
        if ((response.status === 303 && method !== 'HEAD' && method !== 'GET') ||
            ((response.status === 301 || response.status === 302) && method === 'POST')) {
          method = 'GET';
          body = undefined;
          headers.delete('content-type');
          headers.delete('content-length');
        }
        url = next.href;
        redirected = true;
      } finally {
        // Also release the connection when a redirect is rejected.
        await response.body?.cancel().catch(() => {});
      }
    }
    throw new Error('Too many Worker redirects');
  }

  async fulfillFetch(request) {
    const requestId = request.id;
    const signal = this.#fetchControllers.get(requestId)?.signal;
    let responseStarted = false;
    let fetchedResponse;
    try {
      const body = request.body?.worker_bytes;
      if (request.body != null && !Array.isArray(body)) {
        throw new Error('Worker request-body streaming is not implemented');
      }
      if (body?.length > 256 * 1024) {
        throw new Error('Worker request body exceeds 256 KiB');
      }
      const fetched = await this.#fetchRequest(
        request, body == null ? undefined : new Uint8Array(body), signal,
      );
      const { response } = fetched;
      fetchedResponse = response;
      if (signal?.aborted) return;
      const contentLength = Number(response.headers.get('content-length'));
      if (Number.isFinite(contentLength) && contentLength > this.#maxResponseBytes) {
        throw new Error(`Servo response exceeds ${this.#maxResponseBytes} bytes`);
      }
      const idBytes = encoder.encode(requestId);
      const urlBytes = encoder.encode(fetched.url);
      const headers = [...response.headers.entries()]
        .filter(([name]) => name.toLowerCase() !== 'set-cookie');
      // The Fetch Headers iterator hides Set-Cookie. Workers that expose the
      // standard getSetCookie() extension can still hand those values to
      // Servo's cookie jar without exposing them to page script.
      const setCookies = response.headers.getSetCookie?.() ?? [];
      for (const cookie of setCookies) headers.push(['set-cookie', cookie]);
      const headersBytes = encoder.encode(JSON.stringify(headers));
      if (headersBytes.length > MAX_RESPONSE_HEADERS_BYTES) {
        throw new Error('Servo response headers exceed 64 KiB');
      }
      const beginResult = this.#withBytes(
        [idBytes, urlBytes, headersBytes],
        ([id, url, headers]) => this.instance.exports.servo_worker_begin_http_response(
          id.ptr, id.len, url.ptr, url.len, response.status,
          headers.ptr, headers.len, Number(fetched.redirected),
        ),
      );
      if (beginResult === -1) {
        await response.body?.cancel().catch(() => {});
        return; // Rust already delivered a terminal CORS denial.
      }
      responseStarted = beginResult === 1;
      if (!responseStarted) throw new Error('Servo rejected response metadata');
      this.#responseStartedFetches.add(requestId);
      this.#notifyActivity();

      let received = 0;
      const reader = response.body?.getReader();
      if (reader) {
        const abortRead = () => { void reader.cancel().catch(() => {}); };
        signal?.addEventListener('abort', abortRead, { once: true });
        try {
          while (true) {
            const { done, value } = await reader.read();
            if (signal?.aborted) return;
            if (done) break;
            received += value.byteLength;
            if (received > this.#maxResponseBytes) {
              throw new Error(`Servo response exceeds ${this.#maxResponseBytes} bytes`);
            }
            for (let offset = 0; offset < value.byteLength; offset += RESPONSE_CHUNK_BYTES) {
              const chunk = value.subarray(offset, offset + RESPONSE_CHUNK_BYTES);
              const delivered = this.#withBytes([idBytes, chunk], ([id, body]) =>
                this.instance.exports.servo_worker_deliver_http_chunk(
                  id.ptr, id.len, body.ptr, body.len,
                ) === 1);
              if (!delivered) throw new Error('Servo rejected a response chunk');
              this.#notifyActivity();
            }
          }
        } catch (error) {
          await reader.cancel().catch(() => {});
          throw error;
        } finally {
          signal?.removeEventListener('abort', abortRead);
          reader.releaseLock();
        }
      }
      this.#withBytes([idBytes], ([id]) =>
        this.instance.exports.servo_worker_finish_http_response(id.ptr, id.len));
      this.#notifyActivity();
    } catch (error) {
      if (signal?.aborted) return;
      this.#deliverError(requestId, error, responseStarted);
      this.#notifyActivity();
      this.#log(`Servo fetch ${requestId} failed: ${error}`);
    } finally {
      // Header/size errors and aborts can happen before a reader is acquired.
      if (fetchedResponse?.body && !fetchedResponse.body.locked && !fetchedResponse.bodyUsed) {
        await fetchedResponse.body.cancel().catch(() => {});
      }
    }
  }

  dispatchFetch(request) {
    if (this.#fetchQueue.length + this.#inFlightFetches.size >= MAX_PENDING_FETCHES) {
      this.#deliverError(request.id, 'Too many pending Worker fetches', false);
      return;
    }
    this.#fetchQueue.push(request);
    this.#drainFetchQueue();
  }

  cancelFetches(requestIds) {
    const canceled = new Set(requestIds);
    this.#fetchQueue = this.#fetchQueue.filter((request) => !canceled.has(request.id));
    for (const id of canceled) this.#fetchControllers.get(id)?.abort();
  }

  connectWebSocket({ request_id: id, url, protocols = [] }) {
    if (this.#webSockets.size >= MAX_PENDING_FETCHES) {
      this.#deliverWebSocketClose(id, 0xffffffff, '', true);
      return;
    }
    try {
      this.#reserveSubrequest();
    } catch {
      this.#deliverWebSocketClose(id, 0xffffffff, '', true);
      return;
    }
    try {
      const socket = this.#webSocketFactory(url, protocols);
      const generation = this.#generation;
      const current = () => generation === this.#generation &&
        this.#webSockets.get(id) === socket;
      socket.binaryType = 'arraybuffer';
      this.#webSockets.set(id, socket);
      socket.addEventListener('open', () => {
        if (!current()) return;
        this.#callWebSocketExport('servo_worker_websocket_open', id,
          encoder.encode(socket.protocol || ''));
      }, { once: true });
      socket.addEventListener('message', (event) => {
        if (!current()) return;
        const isText = typeof event.data === 'string';
        const bytes = isText ? encoder.encode(event.data) :
          event.data instanceof ArrayBuffer ? new Uint8Array(event.data) :
          ArrayBuffer.isView(event.data) ? new Uint8Array(
            event.data.buffer, event.data.byteOffset, event.data.byteLength,
          ) : null;
        if (bytes === null) {
          this.#log('Ignoring unsupported Worker WebSocket message payload');
          return;
        }
        this.#callWebSocketExport('servo_worker_websocket_message', id, bytes,
          Number(isText));
      });
      socket.addEventListener('error', () => {
        if (!current()) return;
        this.#deliverWebSocketClose(id, 0xffffffff, '', true);
        this.#webSockets.delete(id);
      }, { once: true });
      socket.addEventListener('close', (event) => {
        if (!current()) return;
        this.#deliverWebSocketClose(id, event.code === 1005 ? 0xffffffff : event.code,
          event.reason || '', false);
        this.#webSockets.delete(id);
      }, { once: true });
      this.#notifyActivity();
    } catch (error) {
      this.#log(`Servo Worker WebSocket connection failed: ${error}`);
      this.#deliverWebSocketClose(id, 0xffffffff, '', true);
    }
  }

  webSocketAction({ request_id: id, action }) {
    const socket = this.#webSockets.get(id);
    if (!socket) return;
    try {
      if (action.SendMessage) {
        const [kind, data] = Object.entries(action.SendMessage)[0] ?? [];
        if (kind === 'Text') socket.send(data);
        else if (kind === 'Binary') socket.send(Uint8Array.from(data));
        else throw new TypeError('Unknown Servo WebSocket message type');
      } else if (action.Close) {
        const [code, reason] = action.Close;
        socket.close(code ?? undefined, reason ?? undefined);
      }
    } catch (error) {
      this.#log(`Servo Worker WebSocket action failed: ${error}`);
      this.#deliverWebSocketClose(id, 0xffffffff, '', true);
      this.#webSockets.delete(id);
    }
  }

  #callWebSocketExport(name, id, bytes, flag) {
    const idBytes = encoder.encode(id);
    this.#withBytes([idBytes, bytes], ([idBuffer, dataBuffer]) =>
      this.instance.exports[name](idBuffer.ptr, idBuffer.len,
        dataBuffer.ptr, dataBuffer.len, ...(flag === undefined ? [] : [flag])));
    this.#notifyActivity();
  }

  #deliverWebSocketClose(id, code, reason, failed) {
    const idBytes = encoder.encode(id);
    const reasonBytes = encoder.encode(reason);
    this.#withBytes([idBytes, reasonBytes], ([idBuffer, reasonBuffer]) =>
      this.instance.exports.servo_worker_websocket_close(
        idBuffer.ptr, idBuffer.len, code, reasonBuffer.ptr, reasonBuffer.len,
        Number(failed),
      ));
    this.#notifyActivity();
  }

  #closeWebSockets() {
    for (const socket of this.#webSockets.values()) {
      try { socket.close(); } catch (error) {
        this.#log(`Servo Worker WebSocket close failed: ${error}`);
      }
    }
    this.#webSockets.clear();
  }

  #drainFetchQueue() {
    while (this.#inFlightFetches.size < MAX_OUTGOING_CONNECTIONS && this.#fetchQueue.length) {
      this.#startFetch(this.#fetchQueue.shift());
    }
  }

  #startFetch(request) {
    const generation = this.#generation;
    const controller = new AbortController();
    this.#fetchControllers.set(request.id, controller);
    let tracked;
    tracked = this.fulfillFetch(request).catch((error) => {
        this.#log(`Servo fetch ${request.id} adapter failed: ${error}`);
      }).finally(() => {
        this.#inFlightFetches.delete(tracked);
        this.#fetchControllers.delete(request.id);
        this.#responseStartedFetches.delete(request.id);
        if (generation === this.#generation) this.#drainFetchQueue();
      });
    this.#inFlightFetches.add(tracked);
  }

  bootstrap(width, height, url) {
    const [ptr, len] = this.#write(url);
    try {
      return this.instance.exports.servo_worker_bootstrap(width, height, ptr, len) === 1;
    } finally {
      this.#free(ptr, len);
    }
  }

  loadPage(url) {
    const [ptr, len] = this.#write(url);
    try {
      const accepted = this.instance.exports.servo_worker_load_page(ptr, len) === 1;
      if (accepted) {
        this.#inlinePage = null;
        this.#cancelFetchesForNavigation();
        this.#closeWebSockets();
      }
      return accepted;
    } finally {
      this.#free(ptr, len);
    }
  }

  #cancelFetchesForNavigation() {
    const requests = new Map([
      ...this.#fetchQueue.map((request) => [request.id, request]),
      ...[...this.#fetchControllers.keys()].map((id) => [id, null]),
    ]);
    this.#fetchQueue = [];
    for (const id of requests.keys()) {
      this.#fetchControllers.get(id)?.abort();
      this.#deliverError(id, 'Canceled by top-level navigation',
        this.#responseStartedFetches.has(id));
      this.#responseStartedFetches.delete(id);
    }
    if (requests.size) this.#notifyActivity();
  }

  /** Navigate to one host-supplied HTML document without an outbound fetch. */
  loadHtml(html, { url = 'https://servo-inline.invalid/' } = {}) {
    if (this.#inlinePage !== null) {
      throw new Error('A host-supplied document is already awaiting navigation');
    }
    if (typeof html !== 'string') throw new TypeError('HTML must be a string');
    const parsed = new URL(url);
    if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') {
      throw new TypeError('The host-supplied document URL must be HTTP(S)');
    }
    const bytes = encoder.encode(html);
    if (bytes.byteLength > this.#maxResponseBytes) {
      throw new RangeError(`HTML exceeds ${this.#maxResponseBytes} response bytes`);
    }
    try {
      const started = this.loadPage(parsed.href);
      if (started) this.#inlinePage = { url: parsed.href, bytes };
      return started;
    } catch (error) {
      this.#inlinePage = null;
      throw error;
    }
  }

  /** Traverse one entry in the page's session history, if available. */
  goBack() {
    return this.instance.exports.servo_worker_go_back() === 1;
  }

  /** Traverse forward one entry in the page's session history, if available. */
  goForward() {
    return this.instance.exports.servo_worker_go_forward() === 1;
  }

  /** Reload the current page. Pump the runtime afterward to complete loading. */
  reload() {
    return this.instance.exports.servo_worker_reload() === 1;
  }

  /** Move the native pointer in viewport device pixels. */
  pointerMove(x, y) {
    return this.#inputResult(this.instance.exports.servo_worker_pointer_move(x, y));
  }

  /** Press a mouse button at viewport device-pixel coordinates. */
  mouseDown(x, y, button = 0) {
    return this.#mouseButton(0, button, x, y);
  }

  /** Release a mouse button at viewport device-pixel coordinates. */
  mouseUp(x, y, button = 0) {
    return this.#mouseButton(1, button, x, y);
  }

  /** Move to a point and dispatch a complete native mouse click there. */
  click(x, y, button = 0) {
    this.pointerMove(x, y);
    this.mouseDown(x, y, button);
    return this.mouseUp(x, y, button);
  }

  /** Dispatch a wheel scroll in pixels at viewport device-pixel coordinates. */
  scrollBy(deltaX, deltaY, { x = 0, y = 0 } = {}) {
    return this.#inputResult(this.instance.exports.servo_worker_scroll_by(deltaX, deltaY, x, y));
  }

  /** Dispatch a native keydown event. Use keyUp to release it. */
  keyDown(key) {
    return this.#key(key, 0);
  }

  /** Dispatch a native keyup event. */
  keyUp(key) {
    return this.#key(key, 1);
  }

  /** Dispatch a native keyboard key down and up. Examples: "a", "Enter", "ArrowDown". */
  pressKey(key) {
    this.keyDown(key);
    return this.keyUp(key);
  }

  #key(key, state) {
    if (typeof key !== 'string' || !key || encoder.encode(key).length > 64) {
      throw new TypeError('key must be a non-empty string of at most 64 UTF-8 bytes');
    }
    const modifier = key === 'Shift' ? 'shift'
      : key === 'Control' ? 'control'
      : key === 'Alt' ? 'alt'
      : key === 'Meta' ? 'meta'
      : null;
    const modifierBits = { alt: 0x001, control: 0x008, meta: 0x040, shift: 0x200 };
    let modifiers = 0;
    for (const pressed of this.#pressedModifiers) modifiers |= modifierBits[pressed];
    const [ptr, len] = this.#write(key);
    try {
      if (this.instance.exports.servo_worker_key(ptr, len, state, modifiers) !== 1) {
        throw new Error(`Servo rejected keyboard input for ${JSON.stringify(key)}`);
      }
      if (modifier) {
        if (state === 0) this.#pressedModifiers.add(modifier);
        else this.#pressedModifiers.delete(modifier);
      }
      return true;
    } finally {
      this.#free(ptr, len);
    }
  }

  /** Type text by dispatching native character key events. */
  typeText(text) {
    if (typeof text !== 'string' || [...text].length > 4096) {
      throw new TypeError('text must be a string of at most 4096 characters');
    }
    for (const character of text) {
      this.pressKey(character === '\n' || character === '\r'
        ? 'Enter'
        : character === '\t' ? 'Tab' : character);
    }
  }

  #mouseButton(action, button, x, y) {
    if (!Number.isInteger(button) || button < 0 || button > 4) {
      throw new RangeError('button must be between 0 (primary) and 4 (forward)');
    }
    return this.#inputResult(
      this.instance.exports.servo_worker_mouse_button(action, button, x, y));
  }

  #inputResult(result) {
    if (result !== 1) throw new Error('Servo rejected the input event or its coordinates');
    return true;
  }

  evaluatePage(source) {
    if (this.#evaluationPending || this.#evaluationResultReady) {
      throw new Error('Read the previous page evaluation result before starting another');
    }
    const [ptr, len] = this.#write(source);
    try {
      const accepted = this.instance.exports.servo_worker_evaluate_page(ptr, len) === 1;
      if (accepted) {
        this.#evaluationPending = true;
        this.#evaluationResultReady = false;
      }
      return accepted;
    } finally {
      this.#free(ptr, len);
    }
  }

  pump() {
    return Number(this.instance.exports.servo_worker_pump());
  }

  pumpStatus() {
    const status = Number(this.instance.exports.servo_worker_pump_status());
    return { fetches: status >>> 1, progressed: Boolean(status & 1) };
  }

  nextTimerDelayMs() {
    const deadline = BigInt(this.instance.exports.servo_worker_next_timer_deadline_ns());
    if (deadline === 0n) return null;
    const now = BigInt(Math.round(performance.now() * 1e6));
    return Math.max(0, Number(deadline - now) / 1e6);
  }

  async #wait(delayMs, signal) {
    if (typeof globalThis.scheduler?.wait === 'function') {
      await globalThis.scheduler.wait(delayMs, signal ? { signal } : undefined);
    } else {
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, delayMs);
        signal?.addEventListener('abort', () => {
          clearTimeout(timer);
          resolve();
        }, { once: true });
      });
    }
  }

  /**
   * Pump queued work and wait for Worker fetches and browser timers to progress.
   * With `networkIdleMs`, pending timers alone do not keep the page busy: once
   * no fetch has been queued or in flight for that long, the page counts as
   * settled (`{ settled: true, timersPending: true }`). Pages with recurring
   * timers (carousels, analytics, polling) otherwise never settle.
   */
  async pumpUntilSettled({ maxDurationMs = 10_000, maxTurns = 1_000, until, networkIdleMs } = {}) {
    if (!Number.isFinite(maxDurationMs) || maxDurationMs < 0 ||
        !Number.isSafeInteger(maxTurns) || maxTurns < 0) {
      throw new RangeError('Pump budgets must be finite and non-negative; maxTurns must be an integer');
    }
    if (networkIdleMs !== undefined && (!Number.isFinite(networkIdleMs) || networkIdleMs < 0)) {
      throw new RangeError('networkIdleMs must be finite and non-negative');
    }
    if (until !== undefined && typeof until !== 'function') {
      throw new TypeError('until must be a synchronous predicate');
    }
    if (this.#settling) throw new Error('A Servo settling operation is already running');
    this.#settling = true;
    try {
      return await this.#pumpUntilSettled(maxDurationMs, maxTurns, until, networkIdleMs);
    } finally {
      this.#settling = false;
    }
  }

  async #pumpUntilSettled(maxDurationMs, maxTurns, until, networkIdleMs) {
    const startedAt = performance.now();
    let turns = 0;
    let quietTurns = 0;
    let networkIdleSince = null;
    while (turns < maxTurns && performance.now() - startedAt < maxDurationMs) {
      turns++;
      const activityVersion = this.#activityVersion;
      const { progressed } = this.pumpStatus();
      await Promise.resolve();

      if (progressed || this.#activityVersion !== activityVersion) {
        quietTurns = 0;
        // Timer-driven progress with the network idle still counts as idle.
        if (networkIdleMs !== undefined && !this.#evaluationPending &&
            this.#inFlightFetches.size === 0 && this.#fetchQueue.length === 0) {
          networkIdleSince ??= performance.now();
          if (performance.now() - networkIdleSince >= networkIdleMs && (!until || until())) {
            return { settled: true, turns, timersPending: true };
          }
        } else {
          networkIdleSince = null;
        }
        await this.#wait(0);
        continue;
      }

      const timerDelay = this.nextTimerDelayMs();
      if (this.#evaluationPending && this.instance.exports.servo_worker_page_result_len()) {
        this.#evaluationPending = false;
        this.#evaluationResultReady = true;
      }
      if (this.#evaluationPending) {
        // An accepted evaluation has not produced its result yet. Its result
        // can arrive after pumps that report no progress, so these turns are
        // not evidence of quiescence; an exhausted budget reports unsettled.
        quietTurns = 0;
        await this.#wait(0);
        continue;
      }
      if (this.#inFlightFetches.size === 0 && this.#fetchQueue.length === 0 &&
          timerDelay === null) {
        await this.#wait(0);
        if (++quietTurns >= 8 && (!until || until())) {
          return { settled: true, turns };
        }
        continue;
      }
      quietTurns = 0;

      const now = performance.now();
      let idleDeadline = null;
      if (networkIdleMs !== undefined && this.#inFlightFetches.size === 0 && this.#fetchQueue.length === 0) {
        networkIdleSince ??= now;
        if (now - networkIdleSince >= networkIdleMs && (!until || until())) {
          return { settled: true, turns, timersPending: true };
        }
        idleDeadline = networkIdleSince + networkIdleMs - now;
      } else {
        networkIdleSince = null;
      }

      const remaining = maxDurationMs - (now - startedAt);
      if (remaining <= 0) break;
      const controller = new AbortController();
      const activity = this.#waitForActivity();
      const waits = [...this.#inFlightFetches, activity.promise];
      if (timerDelay !== null) waits.push(this.#wait(Math.min(timerDelay, remaining), controller.signal));
      if (idleDeadline !== null) waits.push(this.#wait(Math.min(idleDeadline, remaining), controller.signal));
      waits.push(this.#wait(remaining, controller.signal));
      try {
        await Promise.race(waits);
      } finally {
        activity.cancel();
        controller.abort();
      }
    }
    return {
      // Exhausting the caller's budget is not evidence of quiescence. Do not
      // execute an extra unbudgeted pump or report success after a quiet gap.
      settled: false,
      turns,
    };
  }

  reset() {
    this.#generation++;
    this.#evaluationPending = false;
    this.#evaluationResultReady = false;
    for (const controller of this.#fetchControllers.values()) controller.abort();
    this.#fetchControllers.clear();
    this.#inFlightFetches.clear();
    this.#fetchQueue.length = 0;
    this.#closeWebSockets();
    this.#inlinePage = null;
    this.#notifyActivity();
    return this.instance.exports.servo_worker_reset() === 1;
  }

  /**
   * Register a font file (TTF/OTF/TTC/OTC bytes, up to 32 MiB) for this
   * runtime, e.g. CJK or emoji fonts. Returns the number of faces added.
   * Register fonts before loading pages that need them.
   */
  registerFont(bytes) {
    const data = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    if (data.length === 0 || data.length > 32 * 1024 * 1024) {
      throw new RangeError('Font data must be between 1 byte and 32 MiB');
    }
    const ptr = this.instance.exports.servo_js_alloc(data.length);
    if (!ptr) throw new Error('Servo wasm allocation failed');
    try {
      new Uint8Array(this.instance.exports.memory.buffer, ptr, data.length).set(data);
      const faces = this.instance.exports.servo_worker_register_font(ptr, data.length);
      if (!faces) throw new TypeError('Font data is not a supported font file');
      return faces;
    } finally {
      this.instance.exports.servo_js_free(ptr, data.length);
    }
  }

  /**
   * Render the page as it currently is to PNG bytes (Uint8Array): the viewport
   * at its current scroll position, or with `fullPage` the whole document
   * from the top (height capped to keep memory within Worker limits). Runs "update the rendering" once, pumps until settled, then
   * rasterizes on the CPU. Throws if the page has not produced a rendering.
   */
  async screenshot(options = {}) {
    const response = await new Response(await this.screenshotStream(options)).arrayBuffer();
    return new Uint8Array(response);
  }

  /**
   * Render the current page as a pull-based PNG ReadableStream. Only one
   * 1024-pixel strip is rendered and compressed per pull, so callers can pass
   * this directly as a Worker Response body without retaining the full PNG.
   */
  async screenshotStream({ maxDurationMs = 5_000, maxPasses = 4, fullPage = false, networkIdleMs = 500 } = {}) {
    // A frame can start loads (CSS background images, web fonts, canvas
    // frames) that only a later frame shows; repeat until a frame adds none.
    const exports = this.instance.exports;
    // Resources can also arrive one frame before the display list that uses
    // them (e.g. an SVG background rasterized at its used size), so a pass
    // that changes neither is not enough on its own to stop: a same-document
    // mask reference (mask-image: url(#id)) takes three passes to resolve
    // (one to notice and queue it, one for script to resolve it between
    // frames, one for layout to pick up the resolved source) with an
    // unchanged item count throughout (a mask clip's resource key isn't
    // reflected in the count) and only a one-pass resource-generation bump
    // in the middle -- so require two consecutive quiet passes, not one,
    // before concluding nothing further is arriving.
    let quietPasses = 0;
    for (let pass = 0; pass < maxPasses; pass++) {
      const resourcesBefore = exports.servo_worker_frame_resource_generation();
      const itemsBefore = exports.servo_worker_frame_item_count();
      if (exports.servo_worker_request_frame() !== 1) {
        throw new Error('Servo has not been bootstrapped');
      }
      await this.pumpUntilSettled({ maxDurationMs, networkIdleMs });
      if (exports.servo_worker_frame_resource_generation() === resourcesBefore &&
          exports.servo_worker_frame_item_count() === itemsBefore) {
        if (++quietPasses >= 2) break;
      } else {
        quietPasses = 0;
      }
    }
    if (this.#screenshotStreamActive) {
      throw new Error('A screenshot stream is already active on this runtime');
    }
    const length = exports.servo_worker_stream_png_begin(fullPage ? 1 : 0);
    if (!length) throw new Error('Servo could not start the PNG stream (see the host log)');
    this.#screenshotStreamActive = true;
    const copyResult = (length) => {
      const ptr = exports.servo_worker_frame_png_ptr();
      return new Uint8Array(exports.memory.buffer, ptr, length).slice();
    };
    return new ReadableStream({
      start(controller) {
        controller.enqueue(copyResult(length));
      },
      pull: (controller) => {
        const length = exports.servo_worker_stream_png_next();
        if (length) {
          controller.enqueue(copyResult(length));
          return;
        }
        const finishLength = exports.servo_worker_stream_png_finish();
        if (!finishLength) {
          this.#screenshotStreamActive = false;
          controller.error(new Error('Servo could not finish the PNG stream (see the host log)'));
          return;
        }
        controller.enqueue(copyResult(finishLength));
        this.#screenshotStreamActive = false;
        controller.close();
      },
      cancel: () => {
        this.#screenshotStreamActive = false;
      },
    });
  }

  /** The WebAssembly.RuntimeError that made this runtime unusable, if any. */
  get trapped() {
    return this.#trap;
  }

  pendingFetchCount() {
    return Number(this.instance.exports.servo_worker_pending_fetch_count());
  }

  pageResult() {
    const ptr = this.instance.exports.servo_worker_page_result_ptr();
    const len = this.instance.exports.servo_worker_page_result_len();
    if (!len) return undefined;
    this.#evaluationPending = false;
    const result = JSON.parse(bytesToString(new Uint8Array(this.instance.exports.memory.buffer, ptr, len)));
    this.#evaluationResultReady = false;
    return result;
  }

  /** Alias for pageResult(), which consumes the single result slot. */
  takePageResult() {
    return this.pageResult();
  }
}
