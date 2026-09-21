# Cloudflare Worker WASM port handoff

*Last updated by Claude Sonnet 5, end of session 2026-09-21. Supersedes the
prior handoff of the same name (that one's still in git history if you want
the original framing). Item 1 of the original objective list is now DONE and
verified. This doc tells you exactly what's proven vs. assumed, and where to
start on items 2-6.*

## Scope and non-negotiable build rule

This fork is being ported to a raw `wasm32-unknown-unknown` module that can
be instantiated directly by a Cloudflare Worker. The eventual MCP server is a
**separate future repository**; do not start it here.

**Never use `cargo build` for Servo.** It clobbers Servo's executable outputs
and causes an expensive rebuild. Use this exact command from
`/mnt/claudevm/servo-wasm`:

```sh
./mach build --target wasm32-unknown-unknown --no-default-features --jobs 4 \
  --profile production-stripped --manifest-path ports/servo-js-wasm/Cargo.toml
```

Current verified size: **48,978,802 bytes (~46.7 MiB)**, well below the 64 MiB
bundle requirement. A from-scratch build (clean `target/wasm32-unknown-unknown`)
takes ~5 minutes with warm registry/git caches; expect longer on a truly cold
machine. Incremental rebuilds after touching one file are usually seconds.

## Dependency patches are now git-based, not local paths

`servo-wasm/Cargo.toml`'s `[patch.crates-io]` section points at `gptenv/*` git
forks on specific branches, **not** `/mnt/claudevm/...` absolute paths anymore.
This matters a lot for your workflow:

- **Editing a forked repo locally does nothing for the servo-wasm build until
  you push it.** If you change `/mnt/claudevm/mozjs-wasm/...` and run
  `./mach build`, Cargo fetches whatever is currently on `gptenv/mozjs-wasm`'s
  `main` branch on GitHub — your uncommitted local edit is invisible. Commit
  and push to the fork before rebuilding servo-wasm to see the effect.
- Current patch table:

  | Crate(s) | Repo | Branch |
  |---|---|---|
  | `mozjs`, `mozjs_sys` | `gptenv/mozjs-wasm` | `main` |
  | `webrender`, `webrender_api`, `wr_glyph_rasterizer`, `wr_malloc_size_of` | `gptenv/webrender-wasm` | `main` (mirrors the old `0.70` branch — `main` there does **not** track upstream webrender's own default branch, see that repo's history) |
  | `sqlite-wasm-rs` | `gptenv/sqlite-wasm-rs` | `master` |
  | `rusqlite` | `gptenv/rusqlite-wasm` | `master` |
  | `uuid` | `gptenv/uuid-wasm` | `main` |
  | `rustls-pki-types` | `gptenv/pki-types-wasm` | `main` |

- **Why git patches instead of paths**: local path patches bypass Cargo's
  semver checking entirely, which was silently letting broken/incompatible
  fork versions slide through undetected. Git patches enforce real semver
  unification across the whole dependency graph — which is *more work* to
  satisfy, but it's what actually caught two real bugs this session (see
  "What broke overnight" below). Don't revert to path patches to make your
  life easier; fix the version mismatch properly instead, the way the
  `rusqlite-wasm`/`sqlite-wasm-rs` fix below did.
- If you fork a *new* crate, remember: GitHub won't auto-create the repo on
  `git push`. Someone with GitHub access (the user) has to create
  `gptenv/<name>` first, or give you `gh` CLI access if you have it.

## What's DONE (objective item 1, fully verified)

**Eliminated all unintended WASI and wasm-bindgen imports.** The
production-stripped artifact now imports exactly these 5 `env` functions and
nothing else — verified via a from-scratch (`rm -rf target/wasm32-unknown-unknown`)
build, not an incremental one that could be hiding stale artifacts:

```
worker_fetch_request
worker_getrandom
worker_log_error
worker_monotonic_now_ns
worker_unix_time_now_ns
```

Verify this yourself with:

```sh
node --input-type=module -e '
import { readFileSync } from "node:fs";
const m = new WebAssembly.Module(readFileSync("target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm"));
const byModule = {};
for (const e of WebAssembly.Module.imports(m)) byModule[e.module] = (byModule[e.module]||0)+1;
console.log(byModule);
'
```

Expect `{ env: 5 }`. If you see `wasi_snapshot_preview1` or
`__wbindgen_placeholder__`/`__wbindgen_externref_xform__` again, something
regressed — see "Known fragility" below before you start debugging from
scratch, several of these exact failure modes already have root causes on
file.

### The regression test (objective item 2 — also done, for the import contract specifically)

`ports/servo-js-wasm/tests/wasm.test.mjs` now asserts the exact 5-import
allowlist (not just "no wasi_* imports"), points at the production-stripped
artifact by default, and supplies real `worker_fetch_request` (no-op) and
`worker_getrandom` (Node's `crypto.randomFillSync`, fail-closed) implementations
so the whole thing actually instantiates and runs 12 passing tests: the
allowlist check, a SpiderMonkey smoke test, arithmetic/loop/unicode/regex JS
evaluation, `Date.now()` wall-clock correctness, global-state isolation
between evaluations, heap-growth bounds over 100 evaluations, and exception
handling. Run it:

```sh
cd ports/servo-js-wasm && node --test tests/wasm.test.mjs
```

This only covers the *import contract* regression, though — it does not
exercise DOM/CSS/fetch or run a stress/WPT suite. That's items 3-5, still open
(see below).

## What broke overnight, and the actual root causes (read before you touch build.rs files)

If you're debugging a `wasi_snapshot_preview1` or `__wbindgen_*` import
reappearing, or a linker warning, or (worst case) a wasm module that "builds
successfully" but traps on instantiation with something like
`CompileError: not enough arguments on the stack for call`, one of these five
already-diagnosed issues is very likely why. Don't re-derive these from
scratch; read the comments at the cited locations first.

1. **wasi-sysroot's `libc.a` bakes real WASI imports into precompiled `.o`
   files** for higher-level POSIX functions (`clock_gettime`, `getenv`,
   `open`, `exit`, ...). `mozjs-wasm/mozjs-sys/src/worker_libc_shim.c`
   overrides these with Worker-native implementations, but simply linking
   `worker_libc_shim.c` *alongside* the real `libc.a` doesn't work reliably —
   neither link ordering nor the `+whole-archive` linker modifier stops the
   real, WASI-import-bearing objects from also getting linked (you get either
   a silent WASI-import leak or a hard duplicate-symbol error, depending on
   which). The actual fix, in `mozjs-sys/build.rs`'s
   `build_trimmed_wasi_libc`: physically copy `libc.a` and `ar d` out every
   object the shim overrides, before it's ever linked. `components/allocator/build.rs`
   needed the identical fix independently (it references the same `libc.a` on
   its own, for `malloc`/`free`).

2. **Rust's default `+bundle` modifier** on `cargo:rustc-link-lib=static=NAME`
   pre-embeds whichever archive members satisfy a crate's own undefined
   symbols directly into *that crate's own rlib*, at the point the rlib is
   built — bypassing the final link's lazy archive search entirely. This is
   what let `chdir.o` (via a bindgen-generated declaration nobody actually
   calls) sneak into `mozjs_sys`'s rlib with its own real WASI import. Every
   `cargo:rustc-link-lib=static=...` referencing `libc.a`/`libc++.a` in this
   codebase now explicitly uses the `:-bundle` modifier to suppress this.

3. **`internal/floatscan.c` in sqlite-wasm-rs's vendored musl subset** doesn't
   reliably pick up the `-include shim/wasm-shim.h` command-line flag that's
   supposed to rename its `__floatscan` export to `rust_sqlite_wasm_floatscan`
   (avoiding a collision with wasi-sysroot's own, differently-ABI'd
   `__floatscan`). Confirmed reproducible across multiple from-scratch
   rebuilds; root mechanism not fully isolated (best guess: clang's implicit
   PCH handling for `-include`, racing under parallel `--jobs` builds). Fixed
   with a belt-and-suspenders `-D__floatscan=rust_sqlite_wasm_floatscan` flag
   in `sqlite-wasm-rs/build.rs`, which doesn't depend on file-inclusion at
   all. **If another renamed symbol in that same header ever shows this
   symptom, add the same kind of explicit `-D` flag rather than trusting
   `-include` alone.**

4. **`glow` (WebGL bindings) unconditionally pulls in
   `wasm-bindgen`/`web_sys`/`js-sys` for *any* `wasm32` target**, regardless
   of which of its own Cargo features are enabled — there's no glow feature
   to opt out of this. Cloudflare Workers never expose a WebGL context, so
   every `glow` dependency in this codebase needs to be target-gated
   (`[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`), not
   feature-gated. Fixed so far in: `components/shared/canvas/Cargo.toml`,
   `components/shared/paint/Cargo.toml`, and (this session, found the hard
   way — it was a *separate*, entirely unused dependency, not reached via
   canvas/paint at all) `components/script/Cargo.toml`. **If `wasm-bindgen`
   reappears in `cargo tree --target wasm32-unknown-unknown -p servo-js-wasm
   -i wasm-bindgen`, check for another stray `glow` dependency before
   assuming it's a sqlite/rusqlite regression** — script's was invisible
   until a completely unrelated fix (switching to git patches) made Cargo
   enforce semver strictly enough to surface it.

5. **`rusqlite`'s own wasm32 backend delegates to `sqlite-wasm-rs`** as its
   FFI provider (see `rusqlite/Cargo.toml`'s
   `[target.'cfg(all(target_family = "wasm", target_os = "unknown"))'.dependencies]`
   block), but upstream pins an old version and hardcodes wasm-bindgen. This
   is what `gptenv/rusqlite-wasm` (forked from the real `v0.38.0` tag, *not*
   HEAD — HEAD is `0.40.1` and `sea-query-rusqlite` pins `rusqlite = "^0.38"`,
   so using HEAD as the fork base breaks that constraint one level up) fixes:
   it changes rusqlite's hardcoded `sqlite-wasm-rs` feature from
   `["wasm-bindgen"]` to `["worker"]` and bumps the version pin to `0.6.1` to
   match our `sqlite-wasm-rs` fork. **When you cherry-pick or rebase this fork
   against a newer rusqlite release, double-check `optional = true` isn't
   accidentally introduced on the `sqlite-wasm-rs` line** — at `v0.38.0` that
   dependency is unconditional with no corresponding enabling feature; adding
   `optional = true` without also wiring up a feature to enable it means
   nothing turns it on and `sqlite-wasm-rs` silently drops out of the
   dependency graph entirely (this exact mistake happened once already during
   the cherry-pick and cost real debugging time — see
   `gptenv/rusqlite-wasm`'s commit history for the fix).

## sqlite-wasm-rs fork architecture (read before touching it again)

Upstream `sqlite-wasm-rs` independently rebuilt its own adapter-agnostic
architecture (v0.5.5 → v0.6.1) that happens to solve almost exactly the same
problem this fork's earlier `worker_platform.rs` solved, more generally. The
fork has been rebased onto that, so:

- `src/host/mod.rs` documents a generic contract: five
  `rust_sqlite_wasm_host_*` C-ABI hooks (`sleep`, `random`,
  `epoch_timestamp_in_ms`, `fill_entropy`, `localtime`) that *any* adapter can
  implement. `src/shim.rs` (upstream's, renamed from the old `shim.rs`/
  `worker_platform.rs` split) delegates to whichever adapter defines them.
- `src/host/wasm_bindgen.rs` is upstream's own browser/wasm-bindgen adapter
  (feature `wasm-bindgen`).
- `src/host/worker.rs` is this fork's addition (feature `worker`): the same
  five hooks via raw `env` imports (`worker_getrandom`,
  `worker_unix_time_now_ns`), fail-closed on entropy failure, UTC-only
  `localtime` (Workers run UTC, no JS `Date` bridge needed).
- **`wasm-bindgen` and `worker` are mutually exclusive** — both define the
  same five C-ABI symbols; enabling both is a linker error, not a silent
  problem, so you'll find out immediately if it happens.
- The `__floatscan`/`putchar_` collision-avoidance workaround in
  `shim/wasm-shim.h` and `shim/worker_putchar.c` is **this fork's own
  addition, not upstream's** — needed only because this crate is linked
  alongside mozjs-wasm's own wasi-libc-derived C runtime in the Servo build.
  If you ever rebase against a newer upstream commit, you'll need to
  re-apply this by hand (it won't survive a `git checkout upstream/master --
  shim/`).
- `extensions/sqlite-vec` and `crates/rsqlite-vfs` had drifted independently
  upstream (real VFS/SAH-pool feature work, unrelated to any of this fork's
  changes) — already reconciled, just don't be surprised by the size of that
  part of the diff if you look at it.

## Recommended next steps, in the original handoff's priority order

Only item 1 is done. From the original 6-item objective:

2. ~~Make the static raw-WASM import contract a regression test~~ — **done**,
   see above, but *only* for the import contract. Consider whether you want
   additional regression coverage for "no `+bundle` regressions" specifically
   (e.g. a CI step that greps for `cargo:rustc-link-lib=static=` without a
   `:-bundle`/`:-whole-archive` modifier touching a `libc`-family archive) —
   given how much debugging time issue #1/#2 above cost, that class of bug is
   worth guarding against mechanically, not just by comment.
3. **Add adapter/DOM/CSS/fetch integration coverage.** Nothing exists yet.
   `worker-adapter.mjs` (`ports/servo-js-wasm/worker-adapter.mjs`) is the real
   adapter surface to test against — bootstrap it, load deterministic
   fixture HTML/CSS, pump the event loop, assert on rendered/evaluated
   output. No fixture corpus exists yet either; you're starting from zero
   here.
4. **Lifetime/memory/stress coverage** beyond the existing "100 evaluations,
   heap doesn't blow the 64 MiB budget" check in `wasm.test.mjs`. Consider
   repeated full page loads (not just JS evaluations), delayed/failed fetch
   responses, and heap behavior under those.
5. **Portable WPT-style subset.** Upstream Servo's `./mach test-wpt`
   explicitly refuses cross targets — don't try to make that work. Port a
   small, deterministic, hand-picked subset into your own fixtures instead.
6. **Minimal Cloudflare Worker demo**, tested locally (workerd/Wrangler) only
   after 3-5 give you confidence raw instantiation actually works for real
   page content, not just the smoke-test JS evaluation. Do **not** deploy
   without the user's explicit direction. Note: `servo-fetch`'s repo already
   has a `worker/` scaffold (npm + wasm-pack build outputs) that's
   deliberately *not* committed to that repo (outside its sparse-checkout
   scope, excluded from its cargo workspace) — it looks like early WIP
   exploration for exactly this kind of demo. Worth a look before starting
   from scratch, but verify what state it's actually in; it wasn't audited
   this session beyond confirming it shouldn't be committed as-is.

## Practical build/debug tips learned this session

- **A build that exits 0 is not proof the artifact is valid.** Twice this
  session, `./mach build` reported success while producing a wasm module that
  failed to instantiate (a real ABI/signature mismatch that the linker only
  *warns* about, doesn't error on). Always follow a build with the Node
  import-check snippet above, and ideally the full test suite.
- **Incremental rebuilds can hide staleness across dependency-resolution
  changes.** If you change how a dependency is *sourced* (local path → git
  patch, or similar), don't trust an incremental rebuild — `rm -rf
  target/wasm32-unknown-unknown` and rebuild clean before believing the
  result. This cost real time twice this session chasing what turned out to
  be stale-cache ghosts before landing on genuine bugs.
- **`cargo tree --target wasm32-unknown-unknown -p servo-js-wasm --manifest-path
  ports/servo-js-wasm/Cargo.toml -i <crate>`** (inverted dependency view) is
  the fastest way to find *why* something unwanted is in the graph. Much
  faster than a full build when you just need to check a dependency edge —
  though note it still needs to fetch/resolve the whole graph, so it's not
  instant on a cold git-fetch cache either.
- No `wasm-objdump`/`wasm2wat`/`wasm-dis` installed in this environment, but
  `llvm-objdump -d --triple=wasm32 <file>.wasm` works for disassembly, and
  `llvm-nm` for extracted `.o` symbol inspection — useful for exactly the
  kind of ABI-mismatch debugging in issues #3/#5 above.
- `ar`, `llvm-ar`, `llvm-nm`, `llvm-objdump` are all present; `wasm-pack` is
  installed and works for running sqlite-wasm-rs's own test suite
  (`wasm-pack test --node -- --features wasm-bindgen` from that repo's root —
  note it needs an explicit feature, `wasm-pack test --node` with zero
  features fails to *link* even on clean upstream master, that's not a
  regression, the crate has no default host adapter).
