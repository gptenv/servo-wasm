import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const file = (path) => readFileSync(new URL(path, import.meta.url));
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const wasm = file('../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm');
const adapter = file('./worker-adapter.mjs');
const lock = file('../../Cargo.lock');
const module = new WebAssembly.Module(wasm);
const version = new WebAssembly.Instance(module, {
  env: {
    worker_fetch_request() {},
    worker_getrandom() { return 1; },
    worker_log_error() {},
    worker_monotonic_now_ns() { return 0n; },
    worker_unix_time_now_ns() { return 0n; },
  },
}).exports.servo_worker_abi_version();
const source = lock.toString();
const gitSources = [...new Set(source.match(/git\+[^"\n]+#[0-9a-f]{40}/g) ?? [])].sort();
const command = (program, args) => execFileSync(program, args, { encoding: 'utf8' }).trim();
const bundleFiles = process.env.SERVO_WORKER_BUNDLE_DIR
  ? readdirSync(process.env.SERVO_WORKER_BUNDLE_DIR)
      .filter((name) => name.endsWith('.wasm') || name.endsWith('.js'))
      .sort()
      .map((name) => {
        const bytes = readFileSync(join(process.env.SERVO_WORKER_BUNDLE_DIR, name));
        return { name, bytes: bytes.length, sha256: sha256(bytes) };
      })
  : null;
const bundleBytes = bundleFiles?.reduce((sum, file) => sum + file.bytes, 0) ?? null;
if (bundleBytes !== null && bundleBytes >= 64 * 1024 * 1024) {
  throw new Error(`Worker upload exceeds 64 MiB (${bundleBytes} bytes)`);
}

console.log(JSON.stringify({
  schema: 1,
  gitCommit: command('git', ['rev-parse', 'HEAD']),
  gitDirty: command('git', ['status', '--porcelain']).length > 0,
  workerAbiVersion: version,
  wasmBytes: wasm.length,
  wasmSha256: sha256(wasm),
  bundleBytes,
  bundleFiles,
  adapterSha256: sha256(adapter),
  cargoLockSha256: sha256(lock),
  gitSources,
  rustc: command('rustc', ['--version']),
  cargo: command('cargo', ['--version']),
  node: process.version,
  wrangler: '4.136.3',
}, null, 2));
