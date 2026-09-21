# Cloudflare Worker WASM port handoff

## Scope and non-negotiable build rule

This fork is being ported to a raw `wasm32-unknown-unknown` module that can
be instantiated directly by a Cloudflare Worker. The eventual MCP server is a
**separate future repository**; do not start it here.

**Never use `cargo build` for Servo.** The user has explicitly warned that it
clobbers Servo's executable outputs and causes an expensive rebuild. Use this
exact command from `/mnt/claudevm/servo-wasm`:

```sh
./mach build --target wasm32-unknown-unknown --no-default-features --jobs 4 \
  --profile production-stripped --manifest-path ports/servo-js-wasm/Cargo.toml
```

The stripped profile has previously built at 49,783,950 bytes (47.48 MiB),
well below the 64 MiB bundle requirement. Re-measure after current changes;
the build was deliberately stopped while compiling the final dependency chain.

## Current test-port objective

The user asked to:

1. eliminate all unintended WASI and wasm-bindgen imports;
2. make the static raw-WASM import contract a regression test;
3. add adapter/DOM/CSS/fetch integration coverage;
4. add lifetime/memory/stress coverage;
5. run a portable WPT-style subset through that harness; and
6. add a minimal Cloudflare Worker demo, tested locally before any deployment.

Only item 1 is currently in progress. Do not claim the module is Worker-ready
until a release artifact instantiates with only the allowed `env` imports.

## Known release-artifact failure

The existing Node smoke test was run against the release artifact and failed
at instantiation because the module required generated wasm-bindgen imports.
Before the current cleanup, its imports were:

```text
env: 4
__wbindgen_placeholder__: 19
__wbindgen_externref_xform__: 2
wasi_snapshot_preview1: 15
```

After porting SQLite and UUID but before the current interrupted Rustls build,
the list was:

```text
env: 5
__wbindgen_placeholder__: 3
__wbindgen_externref_xform__: 2
wasi_snapshot_preview1: 15
```

The desired final allowlist is exactly these five `env` imports:

```text
worker_fetch_request
worker_getrandom
worker_log_error
worker_monotonic_now_ns
worker_unix_time_now_ns
```

Use this read-only inspection after every production build:

```sh
node --input-type=module -e '
import { readFileSync } from "node:fs";
const module = new WebAssembly.Module(readFileSync(process.argv[1]));
for (const entry of WebAssembly.Module.imports(module)) {
  console.log(`${entry.module}.${entry.name}`);
}' target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm
```

## Changes already made (uncommitted)

### Servo workspace: `/mnt/claudevm/servo-wasm`

- `Cargo.toml`
  - `chrono` no longer enables its default `wasmbind` feature. This was a
    sensible cleanup, but it was not the remaining direct source of linked
    glue imports.
  - `uuid` is patched to `/mnt/claudevm/uuid-wasm` with the local
    `worker-rng` feature instead of upstream's `js` feature.
  - `rustls-pki-types` is patched to
    `/mnt/claudevm/rustls-pki-types-wasm` with `worker-time` instead of the
    upstream `web` feature.
  - Existing patches to mozjs, WebRender, and sqlite remain in place; preserve
    all unrelated dirty changes.

- `components/shared/net/lib.rs`
  - The `worker_getrandom` import now returns `i32` status.
  - `__getrandom_v03_custom` returns `getrandom::Error::new_custom(1)` on a
    non-zero host status. This is fail-closed; no weak random fallback.

- `ports/servo-js-wasm/worker-adapter.mjs`
  - `worker_getrandom` still chunks calls at 65,536 bytes, the Web Crypto
    quota.
  - It now returns `0` on success and `1` after logging a Web Crypto failure.

### SQLite Worker platform fork: `/mnt/claudevm/sqlite-wasm-rs`

- The vague `src/shim.rs` was renamed to `src/worker_platform.rs`, and
  `src/lib.rs` now declares `mod worker_platform`.
- This is not a SQLite-engine rewrite. It replaces this wrapper crate's
  wasm-bindgen/browser glue with raw `env` imports:
  - `worker_getrandom(ptr, len) -> i32`
  - `worker_unix_time_now_ns() -> u64`
- `WasmOsCallback::random` traps if entropy fails; there is no `Math.random`
  fallback.
- SQLite `getentropy` returns an explicit error (WASI `NOTCAPABLE`) when the
  host reports failure.
- `localtime` now calculates the proleptic Gregorian UTC fields in Rust;
  Cloudflare Workers use the UTC runtime clock, so no JavaScript `Date` bridge
  is needed.
- `Cargo.toml` no longer depends on `wasm-bindgen` or `js-sys`.
- The local SQLite source had a pre-existing `__floatscan`/`putchar_` collision
  workaround; keep it intact.

### UUID Worker RNG fork: `/mnt/claudevm/uuid-wasm`

- Cloned upstream tag `v1.26.1`, branch `codex/worker-host-rng`.
- Added the `worker-rng` feature.
- On `wasm32-unknown-unknown`, this feature calls the raw checked
  `env::worker_getrandom` import instead of wasm-bindgen Web Crypto bindings.
- Feature selection deliberately excludes `js`, `rng-rand`, and
  `rng-getrandom` for that implementation. Do not re-enable UUID's `js`
  feature in Servo.

### Rustls clock fork: `/mnt/claudevm/rustls-pki-types-wasm`

- Cloned upstream tag `v/1.15.1`, branch `codex/worker-clock`.
- Added `worker-time`.
- `UnixTime::now()` on this target uses raw
  `env::worker_unix_time_now_ns()` and `Duration::from_nanos`, avoiding
  `web-time` and wasm-bindgen.
- The build was stopped after this fork and its Rustls dependants had compiled
  successfully, but before a final module link/import inspection. Resume with
  the required `./mach build` command above.

## Remaining import work

1. Finish the interrupted build, inspect imports, and run the existing Node
   smoke test. It must have no `wasi_*`, `__wbindgen_*`, or other modules.
2. The `wasi_snapshot_preview1` imports are **not** visible in Cargo's target
   dependency graph (`cargo tree --target wasm32-unknown-unknown -p
   servo-js-wasm -i wasi` prints nothing). They come from a native archive
   pulled in through the link step, likely a wasi-sysroot object. Trace linker
   archive selection rather than adding a WASI polyfill. The existing
   SpiderMonkey `worker_libc_shim.c` was specifically intended to prevent
   this class of leak, so verify link order/strong-vs-weak overrides first.
3. Any remaining wasm-bindgen imports after the Rustls patch are expected to
   come from `glow`'s wasm/WebGL backend. Cloudflare Workers have no browser
   `WebGLRenderingContext`; target-gate or stub the Worker WebGL path rather
   than supplying unstable wasm-bindgen placeholder imports. JavaScript, DOM,
   CSS, and CPU-side font/layout work are the current priorities; WebGL is
   not required for them.

## Existing tests and gaps

- [ports/servo-js-wasm/tests/wasm.test.mjs](../ports/servo-js-wasm/tests/wasm.test.mjs)
  currently covers raw module construction, SpiderMonkey evaluation, dates,
  fresh globals, heap growth, and exception handling.
- It is stale: it describes three allowed imports and supplies only three;
  the real ABI now has the five `env` imports listed above. Update it to:
  - assert the exact module/name allowlist, not merely absence of WASI;
  - supply a no-op `worker_fetch_request` and a checked Node CSPRNG-backed
    `worker_getrandom` that returns `0`;
  - use `SERVO_WASM_PATH` pointing at the production-stripped artifact.
- Upstream Servo has `./mach test-unit` and native WPT suites, but `./mach
  test-wpt` explicitly refuses cross targets. Do not describe upstream WPT as
  coverage for this Worker port.
- No Worker adapter integration fixture, DOM/CSS test corpus, stress suite,
  portable WPT-style corpus, or Cloudflare demo Worker exists yet.

## Recommended next implementation order

1. Make the release import allowlist test pass.
2. Extend the Node test to invoke `createServoWorkerRuntime` from
   `worker-adapter.mjs`, bootstrap, load deterministic fixture pages, pump the
   event loop, and evaluate scripts.
3. Add local fixtures for HTML parsing, DOM mutation/selectors/events, CSS
   cascade/layout/custom properties, JavaScript async behavior, fetch success,
   redirects, MIME handling, and failed fetches.
4. Add repeated-load/evaluation, delayed-response, failed-response, and heap
   growth stress tests. Keep Worker state local to a runtime/isolate.
5. Port a focused, deterministic WPT-style subset into those fixtures; do not
   attempt the full upstream WPT runner on the cross target.
6. Add a small, non-MCP Worker demo only after raw instantiation works. Use
   `wrangler.jsonc`, a current compatibility date, no secrets, explicit error
   handling, bounded response bodies, and local workerd/Wrangler validation.
   Do not deploy without user direction.

## Cloudflare notes already verified

- Cloudflare documents `crypto.getRandomValues()` as cryptographically sound.
- The Web Crypto API limits each call to 65,536 bytes; the adapter chunks
  larger requests.
- A Worker CSPRNG failure must fail closed. Never restore `Math.random()`.
- Current Cloudflare Worker guidance was retrieved during this task; use the
  local Cloudflare skills for Wrangler and Workers best practices before
  creating the demo Worker.

