import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

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

console.log(JSON.stringify({
  schema: 1,
  gitCommit: command('git', ['rev-parse', 'HEAD']),
  gitDirty: command('git', ['status', '--porcelain']).length > 0,
  workerAbiVersion: version,
  wasmBytes: wasm.length,
  wasmSha256: sha256(wasm),
  adapterSha256: sha256(adapter),
  cargoLockSha256: sha256(lock),
  gitSources,
  rustc: command('rustc', ['--version']),
  cargo: command('cargo', ['--version']),
  node: process.version,
  wrangler: '4.136.3',
}, null, 2));
