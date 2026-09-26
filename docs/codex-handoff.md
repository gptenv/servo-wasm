# Handoff: servo-wasm Worker port (2026-09-26)

This is the current handoff. `claude-handoff.md` is historical. The sources of
truth for status are [production-readiness.md](production-readiness.md)
(review findings R1-R11), [WORKER-ABI.md](../ports/servo-js-wasm/WORKER-ABI.md)
(the host contract) and
[worker-compatibility-matrix.md](worker-compatibility-matrix.md). The original
review is [expert-recommendations-2026-09-25.md](expert-recommendations-2026-09-25.md).

The project is **not** nearly finished. It is a working controlled-evaluation
browser engine with several P0/P1 items still open (listed below). Do not flip
checkboxes in the trackers without test evidence.

## State at handoff

- Branch `main` of `gptenv/servo-wasm`, remote HEAD `cd29215de5e`. Local changes
  after that revision add basic Web Crypto and repair its no-feature module
  gating; the CI workflow update is also local. Do not push without asking.
- `gptenv/mozjs-wasm` `main` is at `f942001b4` (the script work budget),
  pushed, and locked in `Cargo.lock`.
- The host ABI is **version 8**. The adapter (`worker-adapter.mjs`) and the WASM
  artifact must come from the same build.
- Local verification of revision `d765febd97a`: the production-stripped build
  succeeds; `npm test` passes 125/125; local workerd root, 15 fixtures,
  screenshot and `/runaway` routes pass. The artifact has exactly five `env`
  imports; Wrangler 4.136.3 bundles 61,471.79 KiB.
- **CI is still red at remote HEAD `cd29215de5e`.** The public Actions API
  identifies the failing step as "Build the production Worker artifact
  incrementally"; downloading its detailed log returns 403 without GitHub
  authentication. The local workflow now uses the documented `./mach build`
  command and checks both `<cstdio>` and `<cstring>` with the pinned target
  wrapper. It still needs a remote run. Do not sign in on the user's behalf.
- `servo-mcp` (separate repo, separate owner) pins servo-wasm as a submodule at
  an ABI 4-era revision. Don't edit it. Its owner must take the adapter and WASM
  as a pair, size `scriptBudget`, and apply their egress policy to every method
  (see below).

## Build and test (exact commands)

Never run a bare `cargo build` for Servo. From `/mnt/claudevm/servo-wasm`:

```sh
./mach build --target wasm32-unknown-unknown --no-default-features --jobs 3 \
  --profile production-stripped --manifest-path ports/servo-js-wasm/Cargo.toml -- --locked
cd ports/servo-js-wasm && npm test          # about 3 minutes, needs --stack-size (the script sets it)
npm run workerd                              # local workerd on :8799; routes /, /cases, /screenshot, /runaway
```

- Builds are incremental and usually take seconds to a few minutes, including
  script-crate changes. Don't do clean builds.
- The machine has 15 GB of memory and no swap, so keep `--jobs` at 3 or below.
- A build that exits 0 can still produce a bad module. Always run the tests.
  Check imports with the Node snippet in `claude-handoff.md` (expect `{ env: 5 }`).
- **Testing a mozjs change locally before pushing the fork:** append
  `-- --config 'paths=["/mnt/claudevm/mozjs-wasm/mozjs-sys"]'` to the mach
  command. This doesn't touch Cargo.lock. Then push the fork, run
  `cargo update -p mozjs_sys -p mozjs`, and rebuild with `--locked`.
- **Interpreter hot-path changes need an A/B CPU benchmark.** One macro change
  (`ADVANCE` in `PortableBaselineInterpret.cpp`) once cost 50% on JS calls
  without failing any test.
- **Killing processes:** `pkill -f wrangler` from a Bash tool call matches the
  tool's own shell command line and kills it. Stop workerd by PID or with
  `pkill -x workerd`.

## What changed in this session (newest last)

1. **Script work budget (R1, ABI 7).**
   - Why a budget: Workers have one thread and the clock doesn't advance during
     synchronous execution, so a deadline can never fire.
   - Engine side: the mozjs fork (`js/src/vm/WorkerScriptBudget.h`, under
     `SERVO_WORKER_WASM`) charges work at interrupt checks, backward jumps
     (weighted by bytes) and calls. On exhaustion it terminates the script
     uncatchably, bypassing Servo's interrupt callback, which would otherwise
     shut the script thread down.
   - Port side: `pump_worker_once` grants the budget per turn.
   - Adapter: `scriptBudget` defaults to 20M units, which is about 5-11 s of
     Node CPU when exhausted. It reports `scriptsTerminated`.
   - The calibration table and sizing formula are in WORKER-ABI.md.
2. **`evaluate()` (R7, ABI 8).**
   - Correlated, promise-awaiting evaluation through Servo's WebDriver
     `ExecuteScriptWithCallback` path.
   - A page that overrides `Promise.prototype.then` cannot intercept the result.
   - Errors come back as `Timeout`, `Canceled` or a structured error. The legacy
     `evaluatePage()` single-result slot is unchanged.
3. **History.**
   - The Worker resource handler (`components/net/lib.rs`) dropped
     `GetHistoryState`, so going back to a `pushState` entry trapped the instance.
   - It now stores history states in memory. History traversal is tested and
     reported as supported.
4. **Blob/File** (`components/net/worker_blob_store.rs`).
   - This is an in-memory port of `FileManagerStore`. It fixed traps in
     `URL.revokeObjectURL` and `new File()`/`FormData` (the latter is the
     `SystemTime::now` panic on wasm32; `File` now uses the host clock).
   - It fixed hangs in `Blob.text()` and blob `structuredClone`.
   - `blob:` URLs resolve in-process for `fetch()` and `<img>`, with origin and
     validity checks.
5. **CORS preflight (R2 batch A).**
   - The port decides whether a request needs a preflight and whether the
     preflight response allows it. `servo_worker_check_cors_preflight` is ported
     from native `cors_preflight_fetch`/`cors_check`, except that `*` never
     covers `Authorization`.
   - The adapter only sends the `OPTIONS` request (no body, no cookies, manual
     redirects).
   - Simple cross-origin POST now works.
   - Credentialed cross-origin requests still fail closed.
6. **Explicit failures instead of hangs and traps:**
   - Synchronous http(s) XHR throws `NetworkError`, because the host can't
     answer until the script returns.
   - `new Worker()`, `new AudioContext()` and `new OfflineAudioContext()` throw
     `NotSupportedError`.
   - The regression test runs in a child process with a deadline, because a
     native hang can't be interrupted from JS.

**Egress contract change:** since ABI 8 the host `fetchImpl` receives `OPTIONS`
preflights, cross-origin POST bodies and, once a preflight passes, any method.
The host's destination/SSRF policy must cover every method
(`worker-operations.md`).

7. **Basic Web Crypto.** The no-default-features build now exposes
   `crypto.getRandomValues()` and `crypto.randomUUID()` while keeping
   `SubtleCrypto` gated. The first attempted module split also exposed
   `cryptokey` and the full `subtlecrypto` Rust modules without their optional
   dependencies; those submodules are now feature-gated. The local production
   build and artifact suite pass after that fix.

## Open work, in suggested order

1. **R1 remaining: aggregate memory limits.**
   - Covers blobs, the history-state map, evaluation buffers, decoded
     images/fonts, DOM growth and storage.
   - Calibrate the 20M budget on real Cloudflare. This needs the user's
     authorization to deploy or run remote tests.
2. **R2 batch B: credentialed CORS.**
   - Blocked until the Worker cookie jar honours the SameSite
     site-for-cookies context on cross-site requests (`attach_worker_cookies`
     and `set_worker_cookie_from_header` in `components/net/lib.rs`).
   - Without that, enabling it would leak Lax/Strict cookies.
   - Also still open: redirects after a preflight, and a preflight cache.
3. **R5 Cache API.** `Cache.match/put/add/addAll/delete` and
   `CacheStorage.match` aren't implemented. This spans WebIDL, script and the
   storage backend. Also needed: broader IndexedDB tests (abort, indexes,
   cursors, upgrades).
4. **R4 persistence.** A versioned host storage bridge, designed with the
   `servo-mcp` owner, and real quota enforcement (`client_storage.rs` only
   estimates usage today).
5. **R3 lifecycle.** Close/erase semantics and replacing a trapped instance
   from committed state. Remember that reset is not destruction. Use a fresh
   instance for isolation.
6. **R8 gaps found by the probe:**
   - `FontFace`, `document.fonts.load`, `IntersectionObserver`, `window.print`
     and `execCommand` are absent. They are probably gated by Servo
     preferences; check them and enable with tests where they work.
   - `video.play()` never settles. It should reject.
   - Workers and service workers are unsupported.
7. **Keep hunting traps.** The Worker resource handler still drops unknown
   `CoreResourceMsg` variants, and `script` sometimes calls `recv().unwrap()`
   on the reply, which traps. The API probe style that worked: run each API in
   its own `node` process under an OS `timeout` (scratch scripts weren't kept;
   the regression tests in `tests/wasm.test.mjs` show the pattern).

## Conventions

- Record status only with evidence: a named artifact, a revision and a passing
  result. Update the ABI doc, readiness tracker, compatibility matrix and port
  plan in the same commit as the code.
- Bump `WORKER_ABI_VERSION` (in `lib.rs` and the adapter) when exports or the
  host message schema change. Add new exports to the adapter's
  `REQUIRED_EXPORTS`.
- Every Worker-only change is behind `cfg(target_arch = "wasm32")` (or
  `SERVO_WORKER_WASM` in C++). Native builds must be unaffected.
- Commit messages end with a `Co-Authored-By` line for the agent. Ask the user
  before any push to either repository.
