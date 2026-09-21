import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// No WASI import object here, deliberately: this module used to require 15
// wasi_snapshot_preview1 imports (clock_time_get, fd_read/write, path_open,
// environ_get, proc_exit, ...) purely because wasi-sysroot's libc.a bakes
// them into its precompiled objects for functions like clock_gettime/fopen/
// fprintf. Cloudflare Workers load wasm32-unknown-unknown modules via plain
// WebAssembly.instantiate() with no WASI runtime, so as built, this module
// could not actually have been instantiated in a real Worker -- it only
// worked in this test because node:wasi was polyfilling those imports,
// silently masking the problem. mozjs-sys/src/worker_libc_shim.c now
// intercepts each higher-level libc function at the layer where wasi-libc's
// own real-WASI-import object would otherwise get linked in, so the only
// imports this module needs are the three below, all of which a Worker can
// actually supply. Instantiating with `{}` here (no WASI polyfill at all)
// is the actual regression test for that.
const wasmPath = process.env.SERVO_WASM_PATH ??
  new URL('../../../target/wasm32-unknown-unknown/debug/servo_js_wasm.wasm', import.meta.url);
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

test('module instantiates with no WASI import object', () => {
  const imports = WebAssembly.Module.imports(wasm);
  const wasiImports = imports.filter((i) => i.module.startsWith('wasi_'));
  assert.deepEqual(wasiImports, [], 'module must not require any wasi_snapshot_preview1 imports');
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

test('JavaScript exception reports failure', () => {
  assert.deepEqual(evaluate('throw new Error("expected")'), { ok: false });
});

test('non-int32 result reports failure', () => {
  assert.deepEqual(evaluate('"not an integer"'), { ok: false });
});
