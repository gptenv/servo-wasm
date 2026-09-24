import assert from 'node:assert/strict';
import { randomFillSync } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { inflateSync } from 'node:zlib';
import test from 'node:test';
import { createServoWorkerRuntime } from '../worker-adapter.mjs';
import { webPlatformCases } from './web-platform-cases.mjs';

// No WASI import object here, deliberately: this module used to require 15
// wasi_snapshot_preview1 imports (clock_time_get, fd_read/write, path_open,
// environ_get, proc_exit, ...) purely because wasi-sysroot's libc.a bakes
// them into its precompiled objects for functions like clock_gettime/fopen/
// fprintf. Cloudflare Workers load wasm32-unknown-unknown modules via plain
// WebAssembly.instantiate() with no WASI runtime, so as built, this module
// could not actually have been instantiated in a real Worker -- it only
// worked in earlier versions of this test because node:wasi was polyfilling
// those imports, silently masking the problem. mozjs-sys/build.rs's
// worker_libc_shim.c plus its trimmed copy of wasi-sysroot's libc.a (see
// that file's build_trimmed_wasi_libc) now physically remove every object
// that would otherwise bake those WASI imports in, so the module has
// exactly five `env` imports, all of which a Worker can actually supply.
// Instantiating with exactly this import object (no WASI polyfill, nothing
// extra) is the actual regression test for that -- see the allowlist test
// below, which additionally asserts no *other* imports crept in.
const WORKER_ENV_IMPORT_ALLOWLIST = [
  'worker_fetch_request',
  'worker_getrandom',
  'worker_log_error',
  'worker_monotonic_now_ns',
  'worker_unix_time_now_ns',
];

// Defaults to the production-stripped release artifact -- the one actually
// audited for its import allowlist and measured against the 64 MiB Worker
// bundle budget (see ports/servo-js-wasm/Cargo.toml's `production-stripped`
// profile) -- not a debug build, so this test exercises what would actually
// ship.
const wasmPath = process.env.SERVO_WASM_PATH ??
  new URL('../../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm', import.meta.url);
const wasmBytes = readFileSync(wasmPath);
const wasm = new WebAssembly.Module(wasmBytes);
let instance;
const unixEpochNsAtStartup = BigInt(Date.now()) * 1_000_000n;
instance = new WebAssembly.Instance(wasm, {
  env: {
    worker_monotonic_now_ns: () => process.hrtime.bigint(),
    worker_unix_time_now_ns: () => BigInt(Date.now()) * 1_000_000n,
    worker_log_error: (ptr, len) => {
      const bytes = new Uint8Array(instance.exports.memory.buffer, ptr, len);
      console.error(new TextDecoder().decode(bytes));
    },
    // This low-level instance only runs isolated SpiderMonkey probes. The
    // separate adapter instance below exercises the real fetch protocol.
    worker_fetch_request: (_ptr, _len) => {},
    // Real entropy, not a stub: getrandom's custom wasm32 backend
    // (servo-net-traits' __getrandom_v03_custom) is fail-closed on a
    // non-zero return, so a fake "always succeeds without writing bytes"
    // implementation here would silently hand SpiderMonkey's Math.random
    // and any crypto-adjacent code path uninitialized memory instead of
    // failing loudly. Node's CSPRNG is a reasonable host-side stand-in for
    // a Worker's crypto.getRandomValues() (see net/lib.rs's doc comment on
    // worker_getrandom for why the real host must be CSPRNG-backed too).
    worker_getrandom: (ptr, len) => {
      try {
        randomFillSync(new Uint8Array(instance.exports.memory.buffer, ptr, len));
        return 0;
      } catch {
        return 1;
      }
    },
  },
});
instance.exports.__wasm_call_ctors();

// Decode an 8-bit RGBA PNG into {width, height, pixel(x, y) -> [r, g, b, a]}.
function decodePng(bytes) {
  const data = Buffer.from(bytes);
  assert.deepEqual([...data.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10], 'PNG signature');
  let offset = 8;
  let width = 0;
  let height = 0;
  const idat = [];
  while (offset < data.length) {
    const length = data.readUInt32BE(offset);
    const type = data.toString('latin1', offset + 4, offset + 8);
    const chunk = data.subarray(offset + 8, offset + 8 + length);
    if (type === 'IHDR') {
      width = chunk.readUInt32BE(0);
      height = chunk.readUInt32BE(4);
      assert.equal(chunk[8], 8, 'bit depth');
      assert.equal(chunk[9], 6, 'RGBA color type');
    }
    if (type === 'IDAT') idat.push(chunk);
    offset += 12 + length;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * 4;
  const rows = [];
  let previous = new Uint8Array(stride);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const row = Uint8Array.from(raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1)));
    for (let i = 0; i < stride; i++) {
      const a = i >= 4 ? row[i - 4] : 0;
      const b = previous[i];
      const c = i >= 4 ? previous[i - 4] : 0;
      const predictor = [0, a, b, (a + b) >> 1,
        (() => { const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
          return pa <= pb && pa <= pc ? a : pb <= pc ? b : c; })()][filter];
      row[i] = (row[i] + predictor) & 255;
    }
    rows.push(row);
    previous = row;
  }
  return { width, height, pixel: (x, y) => [...rows[y].subarray(x * 4, x * 4 + 4)] };
}

function evaluate(source) {
  const bytes = new TextEncoder().encode(source);
  const ptr = instance.exports.servo_js_alloc(bytes.length);
  assert.notEqual(ptr, 0, 'wasm input allocation failed');
  try {
    new Uint8Array(instance.exports.memory.buffer).set(bytes, ptr);
    const encoded = instance.exports.servo_js_evaluate_i32(ptr, bytes.length);
    if (encoded === 0n) return { ok: false };
    return { ok: true, value: Number(BigInt.asIntN(32, encoded)) };
  } finally {
    instance.exports.servo_js_free(ptr, bytes.length);
  }
}

test('module imports exactly the allowed env functions, nothing else', () => {
  // Stronger than "no wasi_* imports": this also catches a regression back
  // to __wbindgen_placeholder__/__wbindgen_externref_xform__ (the
  // wasm-bindgen glue that leaked in via `glow`'s WebGL backend before
  // components/shared/canvas and components/shared/paint's Cargo.toml
  // target-gated it out), an accidental new `env` import nobody adapted
  // worker-adapter.mjs for yet, or any import from a module other than
  // `env` at all -- a raw WebAssembly.instantiate() in a Worker can only
  // ever supply imports under a single, hand-written `env` object.
  const imports = WebAssembly.Module.imports(wasm);
  const modules = new Set(imports.map((i) => i.module));
  assert.deepEqual([...modules], ['env'], 'every import must come from the env module');
  const names = imports.map((i) => i.name).sort();
  assert.deepEqual(names, [...WORKER_ENV_IMPORT_ALLOWLIST].sort(),
    'module must import exactly the Worker-supplied env allowlist, no more and no less');
});

test('production WASM stays within the 64 MiB Worker bundle ceiling', () => {
  assert.ok(wasmBytes.byteLength < 64 * 1024 * 1024,
    `WASM artifact is ${wasmBytes.byteLength} bytes before Worker JavaScript is bundled`);
});

test('SpiderMonkey smoke export runs in wasm', () => {
  assert.equal(instance.exports.servo_js_smoke_test(), 42);
});

test('Worker lifecycle exports are present and initially idle', () => {
  assert.equal(instance.exports.servo_worker_abi_version(), 2);
  assert.equal(typeof instance.exports.servo_worker_reset, 'function');
  assert.equal(typeof instance.exports.servo_worker_pump_status, 'function');
  assert.equal(typeof instance.exports.servo_worker_pending_fetch_count, 'function');
  assert.equal(Number(instance.exports.servo_worker_pending_fetch_count()), 0);
  assert.equal(Number(instance.exports.servo_worker_reset()), 0);
});

test('adapter rejects an artifact without the matching host ABI', async () => {
  const oldModule = new WebAssembly.Module(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]));
  await assert.rejects(createServoWorkerRuntime(oldModule), /ABI mismatch/);
});

test('adapter validates resource budgets before instantiating WASM', async () => {
  for (const maxResponseBytes of [-1, Infinity, 64 * 1024 * 1024 + 1]) {
    await assert.rejects(createServoWorkerRuntime(wasm, { maxResponseBytes }), RangeError);
  }
  for (const maxSubrequests of [0, -1, Infinity, 0.5]) {
    await assert.rejects(createServoWorkerRuntime(wasm, { maxSubrequests }), RangeError);
  }
});

for (const [name, source, expected] of [
  ['arithmetic', '40 + 2', 42],
  ['negative int32 boundary', '-2147483648', -2147483648],
  ['loop and lexical bindings', 'let sum = 0; for (let i = 1; i <= 10; i++) sum += i; sum', 55],
  ['Unicode string semantics', '"💡".length', 2],
  ['regular expression', '/[α-ω]+/u.test("β") ? 1 : 0', 1],
]) {
  test(name, () => assert.deepEqual(evaluate(source), { ok: true, value: expected }));
}

test('Date.now() reflects real wall-clock time, not just a positive number', () => {
  // Deliberately stronger than "> 0": that would also pass if Date.now()
  // were silently wired to the monotonic clock (an arbitrary-origin value
  // that happens to be positive), which is exactly the class of bug this
  // fix was for -- and it's also why this doesn't just evaluate
  // `Date.now()` directly: that returns milliseconds since epoch, a value
  // in the trillions that overflows this harness's int32-only evaluation
  // protocol and would misreport as failure regardless of correctness.
  // getFullYear() stays comfortably inside int32 while still discriminating
  // sharply: if Date.now() were fed a monotonic (process-uptime-scale)
  // value instead of a real epoch, `new Date(...)` would land in early
  // 1970, not the current year.
  const jsYear = evaluate('new Date().getFullYear()');
  const realYear = new Date(Number(unixEpochNsAtStartup / 1_000_000n)).getFullYear();
  assert.deepEqual(jsYear, { ok: true, value: realYear });
});

test('fresh globals prevent state leaking between evaluations', () => {
  assert.deepEqual(evaluate('globalThis.workerOnly = 123; 1'), { ok: true, value: 1 });
  assert.deepEqual(evaluate('typeof workerOnly === "undefined" ? 1 : 0'), { ok: true, value: 1 });
});

test('repeated evaluations remain within a Worker-sized heap', () => {
  const initialBytes = instance.exports.memory.buffer.byteLength;
  for (let i = 0; i < 100; i++) {
    assert.deepEqual(evaluate(`${i} + 1`), { ok: true, value: i + 1 });
  }
  const finalBytes = instance.exports.memory.buffer.byteLength;
  assert.ok(finalBytes <= 64 * 1024 * 1024,
    `wasm heap grew beyond the 64 MiB probe memory budget: ${finalBytes} bytes`);
  assert.ok(finalBytes <= initialBytes * 4,
    `wasm heap grew unexpectedly from ${initialBytes} to ${finalBytes} bytes`);
});

test('JavaScript exception reports failure', () => {
  assert.deepEqual(evaluate('throw new Error("expected")'), { ok: false });
});

test('non-int32 result reports failure', () => {
  assert.deepEqual(evaluate('"not an integer"'), { ok: false });
});

test('Worker adapter fetches a page and evaluates its DOM and inline script', async (t) => {
  const requests = [];
  const fetchErrors = [];
  let slowFetchAborted = false;
  let activeQueuedFetches = 0;
  let peakQueuedFetches = 0;
  let beforeHeadersAborted = false;
  let streamCanceled = false;
  let rejectedBodyCanceled = false;
  let failedStreamController;
  let queuedAborts = 0;
  const runtime = await createServoWorkerRuntime(wasm, {
    url: 'about:blank',
    // This one runtime deliberately stress-loads many pages in a single test.
    // A production Free-tier invocation keeps the adapter's default of 50.
    maxSubrequests: 200,
    log: (message) => {
      fetchErrors.push(message);
      if (/panicked|fatal/i.test(message)) t.diagnostic(message);
    },
    fetchImpl: async (url, init) => {
      requests.push({ url, method: init.method, body: init.body,
        authorization: init.headers.get('authorization'),
        origin: init.headers.get('origin') });
      if (url.endsWith('/abort-before')) {
        return new Promise((_, reject) => {
          init.signal.addEventListener('abort', () => {
            beforeHeadersAborted = true;
            reject(init.signal.reason);
          }, { once: true });
        });
      }
      if (url.includes('/abort-queued-')) {
        return new Promise((_, reject) => {
          init.signal.addEventListener('abort', () => {
            queuedAborts++;
            reject(init.signal.reason);
          }, { once: true });
        });
      }
      if (url.endsWith('/abort-stream')) {
        return new Response(new ReadableStream({
          start(controller) { controller.enqueue(new TextEncoder().encode('partial')); },
          cancel() { streamCanceled = true; },
        }));
      }
      if (url.endsWith('/failed-stream')) {
        return new Response(new ReadableStream({
          start(controller) {
            failedStreamController = controller;
            controller.enqueue(new TextEncoder().encode('incomplete'));
          },
        }));
      }
      if (url.endsWith('/rejected-stream')) {
        return new Response(new ReadableStream({
          cancel() { rejectedBodyCanceled = true; },
        }), { headers: { 'content-length': '9000000' } });
      }
      if (url.includes('/queued-')) {
        activeQueuedFetches++;
        peakQueuedFetches = Math.max(peakQueuedFetches, activeQueuedFetches);
        await new Promise((resolve) => setTimeout(resolve, 15));
        activeQueuedFetches--;
        return new Response('queued-ok');
      }
      if (url === 'https://cors.example/ok') {
        return new Response('cors-ok', {
          status: 200,
          headers: {
            'access-control-allow-origin': 'https://example.test',
            'access-control-expose-headers': 'x-public',
            'x-public': 'shown',
            'x-secret': 'hidden',
          },
        });
      }
      if (url === 'https://cors.example/wildcard') {
        return new Response('wildcard-ok', {
          status: 200,
          headers: { 'access-control-allow-origin': '*', 'x-private': 'hidden' },
        });
      }
      if (url === 'https://cors.example/denied') {
        return new Response('cors-denied', { status: 200 });
      }
      if (url.endsWith('/slow')) {
        return new Promise((_, reject) => {
          init.signal.addEventListener('abort', () => {
            slowFetchAborted = true;
            reject(new DOMException('aborted by page reset', 'AbortError'));
          }, { once: true });
        });
      }
      if (url.endsWith('/data.json')) {
        return new Response('{"value":"fetch-ok"}', {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      if (url.endsWith('/headers')) {
        return new Response('header-ok', {
          status: 201,
          headers: { 'content-type': 'text/plain', 'x-fixture': 'preserved',
            'set-cookie': 'session=private' },
        });
      }
      if (url.endsWith('/big')) {
        return new Response('x'.repeat(1_050_000), {
          status: 200,
          headers: { 'content-type': 'text/plain' },
        });
      }
      if (url.endsWith('/oversize')) {
        return new Response('small', {
          status: 200,
          headers: { 'content-length': '9000000' },
        });
      }
      if (url.endsWith('/failure')) throw new Error('fixture network failure');
      if (url.endsWith('/post')) {
        return new Response(new TextDecoder().decode(init.body), {
          status: 200,
          headers: { 'content-type': 'text/plain' },
        });
      }
      if (url.endsWith('/redirect')) {
        return new Response(null, { status: 302, headers: { location: '/final' } });
      }
      if (url.endsWith('/js-redirect')) {
        return new Response(null, { status: 302, headers: { location: '/data.json' } });
      }
      if (url.endsWith('/cross-redirect')) {
        return new Response(null, {
          status: 302,
          headers: { location: 'https://other.example/data.json' },
        });
      }
      if (url.endsWith('/missing')) {
        return new Response('not found', { status: 404 });
      }
      if (url.endsWith('/site.css')) {
        return new Response('#answer { color: blue }', {
          status: 200,
          headers: { 'content-type': 'text/css' },
        });
      }
      if (url === 'https://cdn.example/external.css') {
        return new Response('#external { color: blue }', {
          status: 200,
          headers: { 'content-type': 'text/css' },
        });
      }
      return new Response(
        '<!doctype html><html><head><title>Worker fixture</title>' +
          '<link rel="stylesheet" href="/site.css">' +
          (url === 'https://example.test/'
            ? '<link id="cross-style" rel="stylesheet" href="https://cdn.example/external.css">'
            : '') +
          '<style>#answer { color: red }</style></head><body>' +
          '<main id="answer">hello from fixture</main>' +
          '<div id="external">external style</div>' +
          '<script>document.body.dataset.ready = "yes";' +
          'fetch("/data.json").then(r => r.json()).then(x => ' +
          'document.body.dataset.fetchValue = x.value);' +
          'fetch("/missing").then(r => ' +
          'document.body.dataset.missingStatus = String(r.status));' +
          'fetch("/headers").then(r => {' +
          'document.body.dataset.header = r.headers.get("x-fixture");' +
          'document.body.dataset.cookieHidden = String(r.headers.get("set-cookie") === null);' +
          'document.body.dataset.headerStatus = String(r.status) });' +
          (url === 'https://example.test/'
            ? 'fetch("/big").then(r => r.arrayBuffer()).then(b => ' +
              'document.body.dataset.bigLength = String(b.byteLength)).catch(e => ' +
              'document.body.dataset.bigError = String(e))'
            : '') + '</script>' +
          '</body></html>',
        { status: 200, headers: { 'content-type': 'text/html; charset=utf-8' } },
      );
    },
  });

  const turn = async () => {
    runtime.pump();
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  assert.equal(runtime.bootstrap(1280, 720, 'about:blank'), false,
    'a second bootstrap must fail without replacing the live SpiderMonkey runtime');
  for (let i = 0; i < 3; i++) await turn();

  assert.equal(runtime.loadPage('https://example.test/'), true);
  for (let i = 0; i < 80; i++) await turn();
  assert.deepEqual(requests.map(({ url }) => url).sort(), [
    'https://example.test/',
    'https://example.test/data.json',
    'https://example.test/headers',
    'https://example.test/big',
    'https://example.test/missing',
    'https://example.test/site.css',
    'https://cdn.example/external.css',
  ].sort());
  assert.ok(requests.every(({ method }) => method === 'GET'));

  assert.equal(runtime.evaluatePage(
    'document.title === "Worker fixture" && ' +
      'document.querySelector("#answer")?.textContent === "hello from fixture" && ' +
      'document.body.dataset.ready === "yes" && ' +
      'document.body.dataset.fetchValue === "fetch-ok" && ' +
      'document.body.dataset.missingStatus === "404" && ' +
      'document.body.dataset.header === "preserved" && ' +
      'document.body.dataset.cookieHidden === "true" && ' +
      'document.body.dataset.headerStatus === "201" && ' +
      'document.body.dataset.bigLength === "1050000" && ' +
      'document.querySelector("style").sheet.cssRules.length === 1 && ' +
      'getComputedStyle(document.querySelector("#external")).color === "rgb(0, 0, 255)" && ' +
      '(() => { try { document.querySelector("#cross-style").sheet.cssRules; ' +
      'return false } catch (e) { return e.name === "SecurityError" } })() && ' +
      'getComputedStyle(document.querySelector("#answer")).color === "rgb(255, 0, 0)" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  if (runtime.pageResult()?.Ok?.Number !== 42) {
    runtime.evaluatePage('JSON.stringify({' +
      'title: document.title, ready: document.body.dataset.ready,' +
      'fetchValue: document.body.dataset.fetchValue,' +
      'missingStatus: document.body.dataset.missingStatus,' +
      'header: document.body.dataset.header,' +
      'headerStatus: document.body.dataset.headerStatus,' +
      'bigLength: document.body.dataset.bigLength,' +
      'bigError: document.body.dataset.bigError,' +
      'cssRules: document.querySelector("style").sheet.cssRules.length,' +
      'color: getComputedStyle(document.querySelector("#answer")).color})');
    for (let i = 0; i < 10; i++) await turn();
    assert.fail(JSON.stringify({ result: runtime.pageResult(), fetchErrors,
      pendingFetchCount: runtime.pendingFetchCount() }));
  }
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });

  assert.equal(runtime.evaluatePage(
    'fetch("/failure").catch(() => document.body.dataset.failed = "yes");' +
      'fetch("/oversize").catch(() => document.body.dataset.oversize = "yes");' +
      'fetch("/post", {method: "POST", body: "x"}).then(r => r.text()).then(t => ' +
      'document.body.dataset.postEcho = t);' +
      'fetch("/post", {method: "POST", body: "x".repeat(300000)}).catch(() => ' +
      'document.body.dataset.largePostRejected = "yes");' +
      'fetch("/js-redirect").then(r => r.json().then(body => ({r, body}))).then(({r, body}) => ' +
      'document.body.dataset.redirected = String(r.redirected && ' +
      'r.url.endsWith("/data.json") && body.value === "fetch-ok"));' +
      'fetch("/cross-redirect", {headers: {authorization: "Bearer secret"}})' +
      '.catch(() => document.body.dataset.crossRedirectBlocked = "yes");' +
      'fetch("https://other.example/data.json").catch(() => ' +
      'document.body.dataset.crossOriginBlocked = "yes");' +
      'fetch("https://cors.example/ok").then(r => r.text().then(body => ({r, body})))' +
      '.then(({r, body}) => document.body.dataset.corsOk = String(' +
      'r.type === "cors" && body === "cors-ok" && ' +
      'r.headers.get("x-public") === "shown" && ' +
      'r.headers.get("x-secret") === null && ' +
      'r.headers.get("access-control-allow-origin") === null));' +
      'fetch("https://cors.example/wildcard").then(r => r.text()).then(t => ' +
      'document.body.dataset.corsWildcard = t);' +
      'fetch("https://cors.example/denied").catch(() => ' +
      'document.body.dataset.corsDenied = "yes");' +
      'fetch("https://cors.example/ok", {credentials:"include"}).catch(() => ' +
      'document.body.dataset.corsCredentialsBlocked = "yes");' +
      'fetch("https://cors.example/ok", {headers:{"x-unsafe":"yes"}}).catch(() => ' +
      'document.body.dataset.corsPreflightBlocked = "yes"); 1',
  ), true);
  for (let i = 0; i < 30; i++) await turn();
  assert.equal(runtime.evaluatePage(
    'document.body.dataset.failed === "yes" && ' +
      'document.body.dataset.oversize === "yes" && ' +
      'document.body.dataset.postEcho === "x" && ' +
      'document.body.dataset.largePostRejected === "yes" && ' +
      'document.body.dataset.crossRedirectBlocked === "yes" && ' +
      'document.body.dataset.crossOriginBlocked === "yes" && ' +
      'document.body.dataset.corsOk === "true" && ' +
      'document.body.dataset.corsWildcard === "wildcard-ok" && ' +
      'document.body.dataset.corsDenied === "yes" && ' +
      'document.body.dataset.corsCredentialsBlocked === "yes" && ' +
      'document.body.dataset.corsPreflightBlocked === "yes" && ' +
      'document.body.dataset.redirected === "true" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });
  assert.ok(fetchErrors.some((message) => message.includes('fixture network failure')));
  assert.ok(fetchErrors.some((message) => message.includes('exceeds')));
  assert.ok(fetchErrors.some((message) => message.includes('request-body streaming')));
  const postRequest = requests.find(({ url }) => url.endsWith('/post'));
  assert.equal(postRequest?.method, 'POST');
  assert.equal(new TextDecoder().decode(postRequest.body), 'x');
  assert.equal(requests.some(({ url }) => url === 'https://other.example/data.json'), true,
    'simple cross-origin page requests reach the host, then require CORS permission');
  assert.equal(requests.find(({ url }) => url === 'https://cors.example/ok')?.origin,
    'https://example.test');
  assert.equal(requests.find(({ url }) => url.endsWith('/cross-redirect'))?.authorization,
    'Bearer secret', 'serialized request header values must be decoded from bytes');
  assert.equal(requests.filter(({ url }) => url === 'https://cors.example/ok').length, 1,
    'credentialed and preflighted requests must fail before reaching the host');

  assert.equal(runtime.evaluatePage(
    'Promise.all(Array.from({length: 9}, (_, i) => ' +
    'fetch("/queued-" + i).then(r => r.text()).then(t => {' +
    'document.body.dataset.queueResolved = String(Number(document.body.dataset.queueResolved || 0) + 1);' +
    'return t }))).then(values => ' +
    'document.body.dataset.queueDone = String(values.every(v => v === "queued-ok")))' +
    '.catch(e => document.body.dataset.queueError = String(e)); 1',
  ), true);
  const queueStatus = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
  assert.equal(queueStatus.settled, true);
  assert.equal(runtime.evaluatePage(
    'document.body.dataset.queueDone === "true" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  if (runtime.pageResult()?.Ok?.Number !== 42) {
    runtime.evaluatePage('JSON.stringify({done: document.body.dataset.queueDone,' +
      'resolved: document.body.dataset.queueResolved,error: document.body.dataset.queueError})');
    for (let i = 0; i < 10; i++) await turn();
    assert.fail(JSON.stringify({ queueStatus, result: runtime.pageResult(),
      peakQueuedFetches, fetchErrors, queuedRequests: requests.filter(({ url }) => url.includes('/queued-')) }));
  }
  assert.equal(peakQueuedFetches, 6,
    'the host adapter must queue above the six outgoing-connection limit');

  assert.equal(runtime.evaluatePage(
    'setTimeout(() => { document.body.dataset.timer = "fired" }, 500); 1',
  ), true);
  for (let i = 0; i < 5; i++) await turn();
  assert.notEqual(runtime.nextTimerDelayMs(), null);
  const timerStatus = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
  assert.equal(timerStatus.settled, true, JSON.stringify({
    timerStatus,
    nextTimerDelayMs: runtime.nextTimerDelayMs(),
    pendingFetchCount: runtime.pendingFetchCount(),
  }));
  assert.equal(runtime.evaluatePage(
    'document.body.dataset.timer === "fired" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });

  // A recurring timer never lets a strict settle finish, but with the network
  // idle it counts as settled under networkIdleMs.
  assert.equal(runtime.evaluatePage('window.poll = setInterval(() => {}, 50); 1'), true);
  for (let i = 0; i < 5; i++) await turn();
  assert.equal((await runtime.pumpUntilSettled({ maxDurationMs: 600 })).settled, false);
  const idleStatus = await runtime.pumpUntilSettled({ maxDurationMs: 3_000, networkIdleMs: 200 });
  assert.equal(idleStatus.settled, true, JSON.stringify(idleStatus));
  assert.equal(idleStatus.timersPending, true);
  assert.equal(runtime.evaluatePage('clearInterval(window.poll); 1'), true);
  for (let i = 0; i < 5; i++) await turn();

  const heapBeforeRepeatLoads = runtime.instance.exports.memory.buffer.byteLength;
  for (let page = 0; page < 4; page++) {
    assert.equal(runtime.loadPage(`https://example.test/repeat-${page}`), true);
    for (let i = 0; i < 30; i++) await turn();
  }
  assert.equal(runtime.evaluatePage(
    'document.title === "Worker fixture" && ' +
      'document.querySelector("#answer") !== null ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });
  const heapAfterRepeatLoads = runtime.instance.exports.memory.buffer.byteLength;
  assert.ok(heapAfterRepeatLoads <= 128 * 1024 * 1024,
    `WASM heap exceeded the Worker memory ceiling: ${heapAfterRepeatLoads} bytes`);
  assert.ok(heapAfterRepeatLoads <= heapBeforeRepeatLoads * 4,
    `repeated page loads grew the heap unexpectedly: ${heapBeforeRepeatLoads} -> ${heapAfterRepeatLoads}`);

  assert.equal(runtime.evaluatePage('globalThis.oldPageSentinel = "private"; 42'), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.equal(runtime.loadPage('https://other.example/'), true);
  for (let i = 0; i < 50; i++) await turn();
  assert.equal(runtime.evaluatePage(
    'location.origin === "https://other.example" && ' +
      'typeof oldPageSentinel === "undefined" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });

  assert.equal(runtime.loadPage('https://example.test/redirect'), true);
  for (let i = 0; i < 45; i++) await turn();
  assert.ok(requests.some(({ url }) => url === 'https://example.test/final'));
  assert.equal(runtime.evaluatePage(
    'location.pathname === "/final" && document.title === "Worker fixture" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });

  assert.equal(runtime.evaluatePage('globalThis.oldPageSentinel = "private"; 42'), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.equal(runtime.reset(), true);
  assert.equal(runtime.pendingFetchCount(), 0);
  for (let i = 0; i < 50; i++) await turn();
  assert.equal(runtime.evaluatePage(
    'document.URL === "about:blank" && typeof oldPageSentinel === "undefined" ? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });

  assert.equal(runtime.loadPage('https://example.test/slow'), true);
  for (let i = 0; i < 10; i++) await turn();
  assert.ok(runtime.pendingFetchCount() > 0);
  assert.equal(runtime.reset(), true);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(slowFetchAborted, true);
  assert.equal(runtime.pendingFetchCount(), 0);

  assert.throws(() => runtime.loadHtml('<p>nope</p>', { url: 'data:text/html,nope' }),
    TypeError);
  assert.equal(runtime.loadHtml(
    '<!doctype html><html><head><title>Inline document</title>' +
      '<style>#inline { color: purple }</style></head><body>' +
      '<main id="inline">host bytes</main><script>' +
      'document.body.dataset.executed = "yes";' +
      'fetch("data.json").then(r => r.json()).then(data => ' +
      'document.body.dataset.relativeFetch = data.value)' +
      '</script></body></html>',
    { url: 'https://inline.example/test/' },
  ), true);
  const inlineStatus = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
  assert.equal(inlineStatus.settled, true);
  assert.equal(runtime.evaluatePage(
    'document.title === "Inline document" && ' +
      'document.querySelector("#inline").textContent === "host bytes" && ' +
      'document.body.dataset.executed === "yes" && ' +
      'document.body.dataset.relativeFetch === "fetch-ok" && ' +
      'getComputedStyle(document.querySelector("#inline")).color === "rgb(128, 0, 128)" ' +
      '? 42 : 0',
  ), true);
  for (let i = 0; i < 10; i++) await turn();
  if (runtime.pageResult()?.Ok?.Number !== 42) {
    runtime.evaluatePage('JSON.stringify({' +
      'url: document.URL, title: document.title, text: document.querySelector("#inline")?.textContent,' +
      'executed: document.body?.dataset.executed,' +
      'relativeFetch: document.body?.dataset.relativeFetch,' +
      'color: document.querySelector("#inline") && getComputedStyle(document.querySelector("#inline")).color})');
    for (let i = 0; i < 10; i++) await turn();
    assert.fail(JSON.stringify({ inlineStatus, result: runtime.pageResult(), fetchErrors,
      inlineRequests: requests.filter(({ url }) => url.startsWith('https://inline.example/')) }));
  }
  assert.equal(requests.some(({ url }) => url === 'https://inline.example/test/'), false,
    'host-supplied HTML must not make an outbound navigation subrequest');
  assert.ok(requests.some(({ url }) => url === 'https://inline.example/test/data.json'),
    'relative script fetches must resolve against the supplied page URL');

  const settle = async () => {
    const status = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
    assert.equal(status.settled, true, JSON.stringify({ status, fetchErrors }));
    assert.equal(runtime.pendingFetchCount(), 0);
  };
  const checkPage = async (expression) => {
    const logStart = fetchErrors.length;
    assert.equal(runtime.evaluatePage(`(${expression}) ? 42 : 0`), true);
    await settle();
    const settledResult = runtime.pageResult();
    if (settledResult?.Ok?.Number === 42) return;
    let lateTurns = 0;
    while (!runtime.pageResult() && lateTurns < 50 && !runtime.trapped) {
      runtime.pump();
      lateTurns++;
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    assert.fail(JSON.stringify({ settledResult, lateResult: runtime.pageResult(), lateTurns,
      trapped: String(runtime.trapped ?? ''), log: fetchErrors.slice(logStart) }));
  };

  await t.test('settling budgets and concurrent calls fail explicitly', async () => {
    assert.deepEqual(await runtime.pumpUntilSettled({ maxTurns: 0 }), { settled: false, turns: 0 });
    await assert.rejects(runtime.pumpUntilSettled({ maxDurationMs: Infinity }), RangeError);
    await assert.rejects(runtime.pumpUntilSettled({ maxTurns: -1 }), RangeError);
    await assert.rejects(runtime.pumpUntilSettled({ until: true }), TypeError);
    const pending = runtime.pumpUntilSettled();
    await assert.rejects(runtime.pumpUntilSettled(), /already running/);
    assert.equal((await pending).settled, true);
  });

  await t.test('AbortController cancels a host fetch before headers', async () => {
    runtime.evaluatePage('globalThis.beforeController = new AbortController();' +
      'fetch("/abort-before", {signal: beforeController.signal}).catch(e => ' +
      'document.body.dataset.beforeAbort = e.name); 1');
    for (let i = 0; i < 10; i++) await turn();
    assert.ok(requests.some(({ url }) => url.endsWith('/abort-before')));
    runtime.evaluatePage('beforeController.abort(); 1');
    await settle();
    assert.equal(beforeHeadersAborted, true);
    await checkPage('document.body.dataset.beforeAbort === "AbortError"');
  });

  await t.test('streaming headers wake the page and abort cancels its open body', async () => {
    runtime.evaluatePage('globalThis.streamController = new AbortController();' +
      'fetch("/abort-stream", {signal: streamController.signal}).then(r => {' +
      'document.body.dataset.streamHeaders = String(r.status);' +
      'r.text().then(() => document.body.dataset.streamAbort = "unexpected success",' +
      'e => document.body.dataset.streamAbort = e.name);' +
      'streamController.abort(); }); 1');
    await settle();
    assert.equal(streamCanceled, true, 'an open host body must be canceled');
    await checkPage('document.body.dataset.streamHeaders === "200" && ' +
      'document.body.dataset.streamAbort === "AbortError"');
  });

  await t.test('canceling queued requests never starts extra host connections', async () => {
    const before = requests.length;
    runtime.evaluatePage('globalThis.queueControllers = Array.from({length:9}, () => new AbortController());' +
      'globalThis.queueAbortCount = 0;' +
      'queueControllers.forEach((c,i) => fetch("/abort-queued-" + i, {signal:c.signal})' +
      '.catch(e => { if (e.name === "AbortError") queueAbortCount++ }));' +
      'queueControllers.forEach(c => c.abort()); 1');
    await settle();
    assert.equal(requests.slice(before).filter(({ url }) => url.includes('/abort-queued-')).length, 6);
    assert.equal(queuedAborts, 6);
    await checkPage('queueAbortCount === 9');
  });

  await t.test('failure after response headers rejects the body, not a truncated success', async () => {
    runtime.evaluatePage('fetch("/failed-stream").then(r => {' +
      'document.body.dataset.failedStreamHeaders = "yes";' +
      'return r.text() }).then(() => document.body.dataset.failedStream = "unexpected success",' +
      'e => document.body.dataset.failedStream = e.name); 1');
    for (let i = 0; i < 15; i++) await turn();
    assert.ok(failedStreamController);
    failedStreamController.error(new Error('fixture body interrupted'));
    await settle();
    await checkPage('document.body.dataset.failedStreamHeaders === "yes" && ' +
      'document.body.dataset.failedStream === "TypeError"');
  });

  await t.test('rejecting response metadata releases the unread host body', async () => {
    runtime.evaluatePage('fetch("/rejected-stream").catch(e => ' +
      'document.body.dataset.rejectedStream = e.name); 1');
    await settle();
    assert.equal(rejectedBodyCanceled, true);
    await checkPage('document.body.dataset.rejectedStream === "TypeError"');
  });

  await t.test('response ABI enforces headers, chunks, and exactly one terminal event', async () => {
    const dispatchFetch = runtime.dispatchFetch;
    let requestId;
    runtime.dispatchFetch = (request) => { requestId = request.id; };
    try {
      runtime.evaluatePage('fetch("/protocol").then(r => r.text()).then(t => ' +
        'document.body.dataset.protocol = t); 1');
      for (let i = 0; i < 10 && !requestId; i++) await turn();
      assert.equal(typeof requestId, 'string');
    } finally {
      runtime.dispatchFetch = dispatchFetch;
    }
    const buffers = [requestId, 'https://inline.example/protocol', '[]', 'complete']
      .map((value) => {
        const bytes = new TextEncoder().encode(value);
        const ptr = runtime.instance.exports.servo_js_alloc(bytes.length);
        assert.notEqual(ptr, 0);
        new Uint8Array(runtime.instance.exports.memory.buffer, ptr, bytes.length).set(bytes);
        return [ptr, bytes.length];
      });
    try {
      const [id, url, headers, body] = buffers;
      const exports = runtime.instance.exports;
      assert.equal(exports.servo_worker_deliver_http_chunk(...id, ...body), 0);
      assert.equal(exports.servo_worker_finish_http_response(...id), 0);
      assert.equal(exports.servo_worker_begin_http_response(...id, ...url, 200, ...headers, 0), 1);
      assert.equal(exports.servo_worker_begin_http_response(...id, ...url, 200, ...headers, 0), 0);
      assert.equal(exports.servo_worker_deliver_http_chunk(...id, ...body), 1);
      assert.equal(exports.servo_worker_finish_http_response(...id), 1);
      assert.equal(exports.servo_worker_finish_http_response(...id), 0);
      assert.equal(exports.servo_worker_deliver_http_chunk(...id, ...body), 0);
    } finally {
      for (const buffer of buffers) runtime.instance.exports.servo_js_free(...buffer);
    }
    await settle();
    await checkPage('document.body.dataset.protocol === "complete"');
  });

  for (const { name, source } of webPlatformCases) {
    await t.test(name, async () => checkPage(source));
  }

  await t.test('a frame request makes layout build a display list for the Worker renderer', async () => {
    const exports = runtime.instance.exports;
    assert.equal(exports.servo_worker_request_frame(), 1);
    await settle();
    assert.ok(exports.servo_worker_frame_item_count() > 0, 'no display items were captured');
  });

  await t.test('host fonts register, and invalid font data is rejected', () => {
    const mono = readFileSync(new URL('../fonts/NotoSansMono-Regular.ttf', import.meta.url));
    assert.equal(runtime.registerFont(mono), 1);
    assert.throws(() => runtime.registerFont(new Uint8Array([1, 2, 3, 4])), TypeError);
    assert.throws(() => runtime.registerFont(new Uint8Array()), RangeError);
  });

  await t.test('canvas drawImage decodes an <img> through the deferred Worker pool', async () => {
    assert.equal(runtime.evaluatePage(`(() => {
      const src = document.createElement('canvas'); src.width = 2; src.height = 2;
      const s = src.getContext('2d'); s.fillStyle = 'rgb(0, 128, 0)'; s.fillRect(0, 0, 2, 2);
      const img = new Image();
      img.onload = () => {
        const c = document.createElement('canvas'); c.width = 2; c.height = 2;
        const ctx = c.getContext('2d'); ctx.drawImage(img, 0, 0);
        document.body.dataset.imgPixel = Array.from(ctx.getImageData(1, 1, 1, 1).data).join(',');
      };
      img.onerror = () => { document.body.dataset.imgPixel = 'error'; };
      img.src = src.toDataURL();
      return 1;
    })()`), true);
    await settle();
    await checkPage('document.body.dataset.imgPixel === "0,128,0,255"');
  });

  await t.test('promise microtasks precede timers and canceled timers stay canceled', async () => {
    runtime.evaluatePage('globalThis.taskOrder = ["sync"];' +
      'Promise.resolve().then(() => taskOrder.push("microtask"));' +
      'const canceled = setTimeout(() => taskOrder.push("canceled"), 0); clearTimeout(canceled);' +
      'setTimeout(() => { taskOrder.push("timer");' +
      'Promise.resolve().then(() => taskOrder.push("timer-microtask")); }, 5); 1');
    await settle();
    await checkPage('taskOrder.join(",") === "sync,microtask,timer,timer-microtask"');
  });
});

test('host subrequest budget counts redirects and survives reset', async () => {
  const requests = [];
  let discardedRedirectBody = false;
  const runtime = await createServoWorkerRuntime(wasm, {
    maxSubrequests: 2,
    log: () => {},
    fetchImpl: async (url) => {
      requests.push(url);
      if (url.endsWith('/redirect')) {
        return new Response(new ReadableStream({
          cancel() { discardedRedirectBody = true; },
        }), { status: 302, headers: { location: '/final' } });
      }
      return new Response('final');
    },
  });
  const settle = async () => {
    assert.equal((await runtime.pumpUntilSettled({ maxDurationMs: 3_000 })).settled, true);
    assert.equal(runtime.pendingFetchCount(), 0);
  };
  // Deliberately no bootstrap pumps here: the factory must return a usable
  // initial browsing context, so callers can load immediately after awaiting it.
  runtime.loadHtml('<!doctype html><title>budget</title><body><script>' +
    'fetch("/redirect").then(r => r.text()).then(t => {' +
    'document.body.dataset.first = t; return fetch("/over-budget") })' +
    '.catch(e => document.body.dataset.failure = e.name);</script>',
  { url: 'https://budget.example/' });
  await settle();
  runtime.evaluatePage('document.body.dataset.first === "final" && ' +
    'document.body.dataset.failure === "TypeError" ? 42 : 0');
  await settle();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });
  assert.equal(discardedRedirectBody, true);
  assert.deepEqual(requests, ['https://budget.example/redirect', 'https://budget.example/final']);

  runtime.reset();
  runtime.loadHtml('<!doctype html><body><script>fetch("/after-reset")' +
    '.catch(e => document.body.dataset.failure = e.name);</script>',
  { url: 'https://budget.example/again' });
  await settle();
  runtime.evaluatePage('document.body.dataset.failure === "TypeError" ? 42 : 0');
  await settle();
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 42 } });
  assert.equal(requests.length, 2, 'reset must not replenish the invocation subrequest budget');
});

test('screenshots rasterize backgrounds, borders, text, images and canvas', async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 400,
    height: 300,
    fetchImpl: async (url) => url.endsWith('.svg')
      ? new Response('<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="#f0f"/></svg>',
        { headers: { 'content-type': 'image/svg+xml' } })
      : new Response('{}'),
  });
  runtime.loadHtml(`<!doctype html><body style="margin:0;background:rgb(10, 20, 30)">
    <div style="position:absolute;left:330px;top:20px;width:10px;height:10px;
      background:url(t.svg), linear-gradient(transparent, transparent) no-repeat;background-size:10px"></div>
    <div id="box" style="position:absolute;left:20px;top:20px;width:100px;height:50px;
      background:rgb(0, 200, 0);border:5px solid rgb(0, 0, 255)"></div>
    <canvas id="canvas" width="40" height="40" style="position:absolute;left:200px;top:20px"></canvas>
    <img id="image" width="40" height="40" style="position:absolute;left:260px;top:20px">
    <div style="position:absolute;left:20px;top:150px;font:24px sans-serif;color:white">Hello</div>
    <script>
      const canvas = document.getElementById('canvas').getContext('2d');
      canvas.fillStyle = 'rgb(255, 0, 0)';
      canvas.fillRect(0, 0, 40, 40);
      const source = document.createElement('canvas');
      source.width = 2; source.height = 2;
      const context = source.getContext('2d');
      context.fillStyle = 'rgb(255, 255, 0)';
      context.fillRect(0, 0, 2, 2);
      document.getElementById('image').src = source.toDataURL();
    </script></body>`, { url: 'https://shot.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });

  const png = decodePng(await runtime.screenshot());
  assert.equal(png.width, 400);
  assert.equal(png.height, 300);
  assert.deepEqual(png.pixel(390, 290), [10, 20, 30, 255], 'page background');
  assert.deepEqual(png.pixel(70, 50), [0, 200, 0, 255], 'box background');
  assert.deepEqual(png.pixel(22, 50), [0, 0, 255, 255], 'box border');
  assert.deepEqual(png.pixel(220, 40), [255, 0, 0, 255], 'canvas content');
  assert.deepEqual(png.pixel(280, 40), [255, 255, 0, 255], 'image content');
  assert.deepEqual(png.pixel(335, 25), [255, 0, 255, 255], 'SVG background image');
  let inked = 0;
  for (let y = 150; y < 185; y++) {
    for (let x = 20; x < 90; x++) {
      if (png.pixel(x, y)[0] > 128) inked++;
    }
  }
  assert.ok(inked > 50, `text should draw glyphs (inked ${inked} pixels)`);
});

test('screenshots draw shadows and filters, follow scrolling, and capture full pages', async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 300,
    height: 200,
    fetchImpl: async () => new Response('{}'),
  });
  runtime.loadHtml(`<!doctype html><body style="margin:0;background:white">
    <div style="position:absolute;left:20px;top:20px;width:100px;height:50px;background:white;
      box-shadow:0 0 0 10px rgb(0, 0, 255)"></div>
    <div style="position:absolute;left:160px;top:20px;width:60px;height:50px;
      background:rgb(255, 0, 0);filter:grayscale(1)"></div>
    <div style="position:absolute;left:0;top:1000px;width:300px;height:1000px;background:rgb(0, 128, 0)"></div>
    </body>`, { url: 'https://shot.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });

  const top = decodePng(await runtime.screenshot());
  assert.deepEqual(top.pixel(15, 45), [0, 0, 255, 255], 'box-shadow spread');
  const [red, green, blue] = top.pixel(190, 45);
  assert.ok(red === green && green === blue && red > 0 && red < 255, `grayscale filter: ${top.pixel(190, 45)}`);

  runtime.evaluatePage('window.scrollTo(0, 1000); window.scrollY');
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
  assert.deepEqual(runtime.pageResult(), { Ok: { Number: 1000 } });
  const scrolled = decodePng(await runtime.screenshot());
  assert.deepEqual(scrolled.pixel(150, 100), [0, 128, 0, 255], 'viewport follows the scroll position');

  const full = decodePng(await runtime.screenshot({ fullPage: true }));
  assert.equal(full.width, 300);
  assert.equal(full.height, 2000);
  assert.deepEqual(full.pixel(15, 45), [0, 0, 255, 255], 'full page starts at the top');
  assert.deepEqual(full.pixel(150, 1500), [0, 128, 0, 255], 'full page reaches the bottom');
});

test('screenshots apply mask-image (icons drawn as masked colored boxes)', async () => {
  // Left half opaque, right half transparent.
  const maskSvg = '<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">' +
    '<rect width="10" height="20" fill="#000"/></svg>';
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 200,
    height: 100,
    fetchImpl: async (url) => url.endsWith('.svg')
      ? new Response(maskSvg, { headers: { 'content-type': 'image/svg+xml' } })
      : new Response('{}'),
  });
  runtime.loadHtml(`<!doctype html><body style="margin:0;background:white">
    <div style="position:absolute;left:10px;top:10px;width:40px;height:40px;background:rgb(255, 0, 0);
      mask-image:url(m.svg);mask-size:40px 40px;mask-repeat:no-repeat"></div>
    <div style="position:absolute;left:110px;top:10px;width:40px;height:40px;background:rgb(0, 0, 255);
      -webkit-mask:url(m.svg) no-repeat center / 20px 20px"></div>
    </body>`, { url: 'https://shot.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });

  const png = decodePng(await runtime.screenshot());
  assert.deepEqual(png.pixel(15, 30), [255, 0, 0, 255], 'masked-in half is drawn');
  assert.deepEqual(png.pixel(45, 30), [255, 255, 255, 255], 'masked-out half is not');
  // Centered 20px mask: x 120..130 visible, the rest of the box masked out.
  assert.deepEqual(png.pixel(125, 30), [0, 0, 255, 255], '-webkit-mask shorthand, visible part');
  assert.deepEqual(png.pixel(135, 30), [255, 255, 255, 255], '-webkit-mask shorthand, masked part');
  assert.deepEqual(png.pixel(112, 30), [255, 255, 255, 255], 'outside the mask tile is masked out');
});

test('screenshots tile a repeating mask-image and blend luminance and multiple mask layers', async () => {
  // A 10x10 tile, opaque (black) in its left half only.
  const stripeSvg = '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">' +
    '<rect width="5" height="10" fill="#000"/></svg>';
  // A 20x20 box, opaque (mid-gray, so alpha and luminance masking differ) in
  // its top or bottom half.
  const topSvg = '<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">' +
    '<rect width="20" height="10" fill="#808080"/></svg>';
  const bottomSvg = '<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">' +
    '<rect y="10" width="20" height="10" fill="#808080"/></svg>';
  const svgByPath = { 'stripe.svg': stripeSvg, 'top.svg': topSvg, 'bottom.svg': bottomSvg };
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 200,
    height: 100,
    fetchImpl: async (url) => {
      const name = url.split('/').pop();
      return name in svgByPath
        ? new Response(svgByPath[name], { headers: { 'content-type': 'image/svg+xml' } })
        : new Response('{}');
    },
  });
  runtime.loadHtml(`<!doctype html><body style="margin:0;background:white">
    <div style="position:absolute;left:0;top:0;width:40px;height:20px;background:rgb(255, 0, 0);
      mask-image:url(stripe.svg);mask-size:10px 10px;mask-repeat:repeat"></div>
    <div style="position:absolute;left:60px;top:0;width:20px;height:20px;background:rgb(255, 0, 0);
      mask-image:url(top.svg);mask-mode:alpha"></div>
    <div style="position:absolute;left:100px;top:0;width:20px;height:20px;background:rgb(255, 0, 0);
      mask-image:url(top.svg);mask-mode:luminance"></div>
    <div style="position:absolute;left:140px;top:0;width:20px;height:20px;background:rgb(255, 0, 0);
      mask-image:url(top.svg), url(bottom.svg)"></div>
    </body>`, { url: 'https://shot.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });

  const png = decodePng(await runtime.screenshot());
  // mask-repeat: repeat, three tiles across a 40px-wide box.
  assert.deepEqual(png.pixel(2, 10), [255, 0, 0, 255], 'first tile, visible half');
  assert.deepEqual(png.pixel(7, 10), [255, 255, 255, 255], 'first tile, masked half');
  assert.deepEqual(png.pixel(22, 10), [255, 0, 0, 255], 'third tile, visible half (repeat wrapped)');
  assert.deepEqual(png.pixel(27, 10), [255, 255, 255, 255], 'third tile, masked half (repeat wrapped)');

  // mask-mode: alpha (the default): the gray mask pixel is fully opaque, so
  // it fully unmasks the red box regardless of its mid-gray color.
  assert.deepEqual(png.pixel(70, 5), [255, 0, 0, 255], 'mask-mode: alpha, top half fully visible');
  assert.deepEqual(png.pixel(70, 15), [255, 255, 255, 255], 'mask-mode: alpha, bottom half masked out');

  // mask-mode: luminance: the same gray pixel's luminance (~0.5) partially
  // unmasks the red box, blending it toward the white background -- unlike
  // the fully-opaque result above.
  const [r, g, b, a] = png.pixel(110, 5);
  assert.deepEqual([r, a], [255, 255], 'mask-mode: luminance, red channel unaffected over white');
  assert.ok(g > 90 && g < 160 && g === b, `mask-mode: luminance should partially unmask (got rgba ${r},${g},${b},${a})`);
  assert.deepEqual(png.pixel(110, 15), [255, 255, 255, 255], 'mask-mode: luminance, bottom half still masked out');

  // Two mask-image layers (top.svg, bottom.svg), each opaque in a different
  // half. `mask-composite: add` (the default) unions them, so both halves
  // are visible -- unlike naive intersection, which would mask out the
  // entire box since neither layer alone covers it.
  assert.deepEqual(png.pixel(150, 5), [255, 0, 0, 255], 'unioned mask layers, top half visible');
  assert.deepEqual(png.pixel(150, 15), [255, 0, 0, 255], 'unioned mask layers, bottom half visible');
});

test('screenshots apply gradient mask-image layers and mask-composite', async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 400,
    height: 200,
    fetchImpl: async () => new Response('{}'),
  });
  runtime.loadHtml(`<!doctype html><body style="margin:0;background:white">
    <div style="position:absolute;left:0;top:0;width:100px;height:60px;background:rgb(255, 0, 0);
      mask-image:linear-gradient(to right, black, transparent)"></div>
    <div style="position:absolute;left:110px;top:0;width:60px;height:60px;background:rgb(255, 0, 0);
      padding:15px;box-sizing:border-box;
      mask:linear-gradient(black, black), linear-gradient(black, black) content-box;
      mask-composite:subtract"></div>
    <div style="position:absolute;left:180px;top:0;width:60px;height:60px;background:rgb(255, 0, 0);
      padding:15px;box-sizing:border-box;
      mask:linear-gradient(black, black) content-box, linear-gradient(black, black);
      mask-composite:subtract"></div>
    <div style="position:absolute;left:250px;top:0;width:60px;height:60px;background:rgb(255, 0, 0);
      padding:15px;box-sizing:border-box;
      mask:linear-gradient(black,black) content-box, linear-gradient(black,black);
      mask-composite:exclude"></div>
    <div style="position:absolute;left:320px;top:0;width:60px;height:60px;background:rgb(255, 0, 0);
      mask-image:url(missing.png), linear-gradient(black, black);
      mask-composite:intersect"></div>
    </body>`, { url: 'https://shot.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });

  const png = decodePng(await runtime.screenshot());

  // Linear gradient fade: nearly opaque at the left edge, nearly
  // transparent at the right (a couple of pixels in from each edge, since
  // the interpolated stop value right at x=0/x=100 is only asymptotically
  // exact).
  const opaqueEnd = png.pixel(2, 30);
  assert.ok(opaqueEnd[0] === 255 && opaqueEnd[1] < 15 && opaqueEnd[1] === opaqueEnd[2],
    `gradient mask should be nearly fully visible near the opaque end (got rgba ${opaqueEnd.join(',')})`);
  const transparentEnd = png.pixel(98, 30);
  assert.ok(transparentEnd[0] === 255 && transparentEnd[1] > 240 && transparentEnd[1] === transparentEnd[2],
    `gradient mask should be nearly fully masked near the transparent end (got rgba ${transparentEnd.join(',')})`);
  const mid = png.pixel(50, 30);
  assert.ok(mid[0] === 255 && mid[1] > 20 && mid[1] < 235 && mid[1] === mid[2],
    `gradient mask midpoint should be a red/white blend (got rgba ${mid.join(',')})`);

  // Order-sensitive `subtract`: per
  // <https://drafts.fxtf.org/css-masking-1/#the-mask-composite>, layers
  // combine bottom-up -- each layer is the "source", the composite of the
  // layers *below* it (listed *after* it) is the "destination" it combines
  // into. `subtract` keeps the source only where the destination does not
  // already cover it. Layer 0 (border-box, listed first, so on top) is the
  // source combining onto layer 1 (content-box, listed last, so on the
  // bottom and unaffected by its own composite value): the content box is
  // subtracted out of the border box, leaving a ring.
  assert.deepEqual(png.pixel(112, 2), [255, 0, 0, 255], 'subtract ring: padding area visible');
  assert.deepEqual(png.pixel(140, 30), [255, 255, 255, 255], 'subtract ring: content area masked out');

  // The same two shapes with the mask-image list order swapped: now the
  // content-box layer is on top, subtracting itself out of the border-box
  // layer below it. Since the content box is entirely inside the border
  // box, this removes it completely and leaves nothing.
  assert.deepEqual(png.pixel(182, 2), [255, 255, 255, 255], 'reordered subtract: padding area also masked out');
  assert.deepEqual(png.pixel(210, 30), [255, 255, 255, 255], 'reordered subtract: content area masked out');

  // `mask-composite: exclude` (XOR) of a content-box and a border-box solid
  // mask leaves the same ring shape as the (non-reordered) subtract case --
  // exclude is symmetric, so layer order does not matter for it.
  assert.deepEqual(png.pixel(252, 2), [255, 0, 0, 255], 'exclude ring: padding area visible');
  assert.deepEqual(png.pixel(280, 30), [255, 255, 255, 255], 'exclude ring: content area masked out');

  // `mask-composite: intersect` with a failed (never-loaded) layer: the
  // failed layer contributes as fully transparent, so intersecting it with
  // the other (opaque) layer hides the element entirely, regardless of
  // which of the two is on top.
  assert.deepEqual(png.pixel(350, 30), [255, 255, 255, 255], 'intersect with a failed layer hides the element');
});
