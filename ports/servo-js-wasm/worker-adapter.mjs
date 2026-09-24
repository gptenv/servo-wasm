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
const FREE_TIER_SUBREQUESTS = 50;
const WORKER_ABI_VERSION = 2;
const encoder = new TextEncoder();

function bytesToString(bytes) {
  return new TextDecoder().decode(bytes);
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
  width = 1280,
  height = 720,
  url = "about:blank",
  log = console.error,
  maxResponseBytes = MAX_RESPONSE_BYTES,
  maxSubrequests = FREE_TIER_SUBREQUESTS,
} = {}) {
  if (!Number.isSafeInteger(maxResponseBytes) || maxResponseBytes < 0 ||
      maxResponseBytes > 64 * 1024 * 1024) {
    throw new RangeError('maxResponseBytes must be between 0 and 64 MiB');
  }
  if (!Number.isSafeInteger(maxSubrequests) || maxSubrequests < 1) {
    throw new RangeError('maxSubrequests must be a positive integer');
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
        const message = JSON.parse(bytesToString(bytes));
        if (message.version !== WORKER_ABI_VERSION) {
          throw new Error('Servo Worker host protocol version mismatch');
        }
        if (message.kind === 'fetch') runtime.dispatchFetch(message.request);
        else if (message.kind === 'cancel') runtime.cancelFetches(message.request_ids);
        else throw new Error('Unknown Servo Worker host command');
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
  runtime = new ServoWorkerRuntime(instance, fetchImpl, log, maxResponseBytes, maxSubrequests);
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
  #log;
  #maxResponseBytes;
  #maxSubrequests;
  #subrequestCount = 0;
  #inFlightFetches = new Set();
  #fetchControllers = new Map();
  #fetchQueue = [];
  #generation = 0;
  #inlinePage = null;
  #activityVersion = 0;
  #activityWaiters = new Set();
  #settling = false;
  #trap = null;
  #evaluationPending = false;

  constructor(instance, fetchImpl, log, maxResponseBytes, maxSubrequests) {
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
    this.#log = log;
    this.#maxResponseBytes = maxResponseBytes;
    this.#maxSubrequests = maxSubrequests;
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
      if (++this.#subrequestCount > this.#maxSubrequests) {
        throw new Error(`Servo Worker exceeded ${this.#maxSubrequests} host subrequests`);
      }
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
      const headersBytes = encoder.encode(JSON.stringify([...response.headers.entries()]));
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
      if (accepted) this.#inlinePage = null;
      return accepted;
    } finally {
      this.#free(ptr, len);
    }
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

  evaluatePage(source) {
    const [ptr, len] = this.#write(source);
    try {
      const accepted = this.instance.exports.servo_worker_evaluate_page(ptr, len) === 1;
      if (accepted) this.#evaluationPending = true;
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

  /** Pump queued work and wait for Worker fetches and browser timers to progress. */
  async pumpUntilSettled({ maxDurationMs = 10_000, maxTurns = 1_000, until } = {}) {
    if (!Number.isFinite(maxDurationMs) || maxDurationMs < 0 ||
        !Number.isSafeInteger(maxTurns) || maxTurns < 0) {
      throw new RangeError('Pump budgets must be finite and non-negative; maxTurns must be an integer');
    }
    if (until !== undefined && typeof until !== 'function') {
      throw new TypeError('until must be a synchronous predicate');
    }
    if (this.#settling) throw new Error('A Servo settling operation is already running');
    this.#settling = true;
    try {
      return await this.#pumpUntilSettled(maxDurationMs, maxTurns, until);
    } finally {
      this.#settling = false;
    }
  }

  async #pumpUntilSettled(maxDurationMs, maxTurns, until) {
    const startedAt = performance.now();
    let turns = 0;
    let quietTurns = 0;
    while (turns < maxTurns && performance.now() - startedAt < maxDurationMs) {
      turns++;
      const activityVersion = this.#activityVersion;
      const { progressed } = this.pumpStatus();
      await Promise.resolve();

      if (progressed || this.#activityVersion !== activityVersion) {
        quietTurns = 0;
        await this.#wait(0);
        continue;
      }

      const timerDelay = this.nextTimerDelayMs();
      if (this.#evaluationPending && this.instance.exports.servo_worker_page_result_len()) {
        this.#evaluationPending = false;
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

      const remaining = maxDurationMs - (performance.now() - startedAt);
      if (remaining <= 0) break;
      const controller = new AbortController();
      const activity = this.#waitForActivity();
      const waits = [...this.#inFlightFetches, activity.promise];
      if (timerDelay !== null) waits.push(this.#wait(Math.min(timerDelay, remaining), controller.signal));
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
    for (const controller of this.#fetchControllers.values()) controller.abort();
    this.#fetchControllers.clear();
    this.#inFlightFetches.clear();
    this.#fetchQueue.length = 0;
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
  async screenshot({ maxDurationMs = 5_000, maxPasses = 4, fullPage = false } = {}) {
    // A frame can start loads (CSS background images, web fonts, canvas
    // frames) that only a later frame shows; repeat until a frame adds none.
    const exports = this.instance.exports;
    for (let pass = 0; pass < maxPasses; pass++) {
      const resourcesBefore = exports.servo_worker_frame_resource_generation();
      if (exports.servo_worker_request_frame() !== 1) {
        throw new Error('Servo has not been bootstrapped');
      }
      await this.pumpUntilSettled({ maxDurationMs });
      if (exports.servo_worker_frame_resource_generation() === resourcesBefore) break;
    }
    const length = this.instance.exports.servo_worker_render_png(fullPage ? 1 : 0);
    if (!length) throw new Error('Servo could not render the page (see the host log)');
    const ptr = this.instance.exports.servo_worker_frame_png_ptr();
    return new Uint8Array(this.instance.exports.memory.buffer, ptr, length).slice();
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
    return JSON.parse(bytesToString(new Uint8Array(this.instance.exports.memory.buffer, ptr, len)));
  }
}
