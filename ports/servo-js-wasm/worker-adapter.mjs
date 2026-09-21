/*
 * Cloudflare Worker host adapter for servo_js_wasm.
 *
 * This file deliberately contains no WASI shim. The Rust module imports only
 * the small `env` surface below, and outbound navigation/subresource requests
 * are fulfilled by the Worker's standard fetch() implementation.
 */

const MAX_RESPONSE_BYTES = 64 * 1024 * 1024;

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
  if (Array.isArray(source)) {
    for (const entry of source) {
      if (Array.isArray(entry) && entry.length === 2) headers.append(entry[0], entry[1]);
    }
  } else {
    for (const [name, value] of Object.entries(source)) {
      if (Array.isArray(value)) {
        for (const item of value) headers.append(name, String(item));
      } else {
        headers.set(name, String(value));
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
 * `wasmModule` may be a WebAssembly.Module, an ArrayBuffer, or a module
 * response accepted by WebAssembly.instantiate. Call this once per Worker
 * isolate, not once per incoming request.
 */
export async function createServoWorkerRuntime(wasmModule, {
  fetchImpl = globalThis.fetch,
  width = 1280,
  height = 720,
  url = "about:blank",
  log = console.error,
} = {}) {
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
        const request = JSON.parse(bytesToString(bytes));
        void runtime.fulfillFetch(request);
      },
    },
  };

  const instantiated = await WebAssembly.instantiate(wasmModule, imports);
  const instance = instantiated instanceof WebAssembly.Instance
    ? instantiated
    : instantiated.instance;
  runtime = new ServoWorkerRuntime(instance, fetchImpl);
  runtime.instance.exports.__wasm_call_ctors?.();
  runtime.instance.exports.servo_worker_install_fetch_adapter();
  if (!runtime.bootstrap(width, height, url)) {
    throw new Error("Servo Worker bootstrap failed");
  }
  return runtime;
}

class ServoWorkerRuntime {
  #fetchImpl;

  constructor(instance, fetchImpl) {
    this.instance = instance;
    this.#fetchImpl = fetchImpl;
  }

  #write(value) {
    const bytes = new TextEncoder().encode(value);
    const ptr = this.instance.exports.servo_js_alloc(bytes.length);
    if (!ptr) throw new Error("Servo wasm allocation failed");
    new Uint8Array(this.instance.exports.memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  }

  #free(ptr, len) {
    this.instance.exports.servo_js_free(ptr, len);
  }

  async fulfillFetch(request) {
    const requestId = request.id;
    try {
      const response = await this.#fetchImpl(requestUrl(request), {
        method: requestMethod(request),
        headers: requestHeaders(request),
        redirect: "manual",
      });
      const body = new Uint8Array(await response.arrayBuffer());
      if (body.byteLength > MAX_RESPONSE_BYTES) {
        throw new Error(`Servo response exceeds ${MAX_RESPONSE_BYTES} bytes`);
      }
      const idBytes = new TextEncoder().encode(requestId);
      const urlBytes = new TextEncoder().encode(response.url || requestUrl(request));
      const contentTypeBytes = new TextEncoder().encode(response.headers.get("content-type") || "");
      const idPtr = this.instance.exports.servo_js_alloc(idBytes.length);
      const urlPtr = this.instance.exports.servo_js_alloc(urlBytes.length);
      const contentTypePtr = contentTypeBytes.length
        ? this.instance.exports.servo_js_alloc(contentTypeBytes.length) : 0;
      const bodyPtr = body.length ? this.instance.exports.servo_js_alloc(body.length) : 0;
      if (!idPtr || !urlPtr || (contentTypeBytes.length && !contentTypePtr) || (body.length && !bodyPtr)) {
        throw new Error("Servo wasm allocation failed");
      }
      try {
        const memory = new Uint8Array(this.instance.exports.memory.buffer);
        memory.set(idBytes, idPtr);
        memory.set(urlBytes, urlPtr);
        if (contentTypeBytes.length) memory.set(contentTypeBytes, contentTypePtr);
        if (body.length) memory.set(body, bodyPtr);
        this.instance.exports.servo_worker_deliver_http_response(
          idPtr, idBytes.length, urlPtr, urlBytes.length, response.status,
          contentTypePtr, contentTypeBytes.length,
          bodyPtr, body.length,
        );
      } finally {
        this.#free(idPtr, idBytes.length);
        this.#free(urlPtr, urlBytes.length);
        if (contentTypeBytes.length) this.#free(contentTypePtr, contentTypeBytes.length);
        if (body.length) this.#free(bodyPtr, body.length);
      }
    } catch (error) {
      const idBytes = new TextEncoder().encode(requestId);
      const messageBytes = new TextEncoder().encode(String(error));
      const idPtr = this.instance.exports.servo_js_alloc(idBytes.length);
      const messagePtr = this.instance.exports.servo_js_alloc(messageBytes.length);
      if (idPtr && messagePtr) {
        try {
          const memory = new Uint8Array(this.instance.exports.memory.buffer);
          memory.set(idBytes, idPtr);
          memory.set(messageBytes, messagePtr);
          this.instance.exports.servo_worker_deliver_http_error(
            idPtr, idBytes.length, messagePtr, messageBytes.length,
          );
        } finally {
          this.#free(idPtr, idBytes.length);
          this.#free(messagePtr, messageBytes.length);
        }
      }
      console.error(`Servo fetch ${requestId} failed`, error);
    }
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
      return this.instance.exports.servo_worker_load_page(ptr, len) === 1;
    } finally {
      this.#free(ptr, len);
    }
  }

  evaluatePage(source) {
    const [ptr, len] = this.#write(source);
    try {
      return this.instance.exports.servo_worker_evaluate_page(ptr, len) === 1;
    } finally {
      this.#free(ptr, len);
    }
  }

  pump() {
    return Number(this.instance.exports.servo_worker_pump());
  }

  pageResult() {
    const ptr = this.instance.exports.servo_worker_page_result_ptr();
    const len = this.instance.exports.servo_worker_page_result_len();
    if (!len) return undefined;
    return JSON.parse(bytesToString(new Uint8Array(this.instance.exports.memory.buffer, ptr, len)));
  }
}
