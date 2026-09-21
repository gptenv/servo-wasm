import assert from 'node:assert/strict';
import { randomFillSync } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';

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
const wasm = new WebAssembly.Module(readFileSync(wasmPath));
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
    // No-op: nothing in this test suite drives Servo's fetch pipeline far
    // enough to issue a real outbound request. Present only so the module
    // (which declares this as a required `env` import) can instantiate.
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

test('SpiderMonkey smoke export runs in wasm', () => {
  assert.equal(instance.exports.servo_js_smoke_test(), 42);
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
    `wasm heap grew beyond the Worker bundle memory budget: ${finalBytes} bytes`);
  assert.ok(finalBytes <= initialBytes * 4,
    `wasm heap grew unexpectedly from ${initialBytes} to ${finalBytes} bytes`);
});

test('JavaScript exception reports failure', () => {
  assert.deepEqual(evaluate('throw new Error("expected")'), { ok: false });
});

test('non-int32 result reports failure', () => {
  assert.deepEqual(evaluate('"not an integer"'), { ok: false });
});
