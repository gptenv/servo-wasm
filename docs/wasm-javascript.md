# JavaScript execution on the Worker target

## Decision

Use SpiderMonkey's **Portable Baseline Interpreter (PBL)** rather than
inventing a new JavaScript VM or relying only on the generic C++ interpreter.

PBL is an existing SpiderMonkey execution tier that interprets JavaScript
CacheIR without generating native machine code. It sits between the generic
C++ interpreter and the native Baseline/Ion JIT tiers. SpiderMonkey documents
it specifically for environments where runtime code generation is unavailable,
including WebAssembly modules.

## Why this is the right fit

- It preserves SpiderMonkey's JavaScript semantics and Servo's existing
  SpiderMonkey bindings.
- It can execute inline-cache paths without native JIT code generation.
- It has a dedicated auxiliary JavaScript stack suitable for a WASM runtime.
- Unsupported or incomplete CacheIR paths can fall back to the generic
  interpreter, preserving correctness at a performance cost.
- SpiderMonkey already ships a `wasi-pbl` build configuration, which gives us
  a concrete reference for the required feature set.

## Required port

The current `mozjs_sys` build wrapper recognizes WASI-oriented targets but
rejects `wasm32-unknown-unknown`. The Worker port will need a local mozjs
variant that:

1. Adds a `wasm32-unknown-unknown` target configuration.
2. Enables `--enable-portable-baseline-interp` and initially forces PBL.
3. Disables native JIT tiers, shared memory, tests, and shell tooling.
4. Uses Rust zlib and no WASI-only imports.
5. Supplies Worker-safe implementations for time, randomness, interrupts,
   stack limits, and other host callbacks.
6. Builds the Rust bindings against the resulting SpiderMonkey symbols.

The first milestone is therefore not “JavaScript with no optimization.” It is
“full SpiderMonkey semantics with a portable optimized interpreter.” We will
benchmark PBL against the generic interpreter after the first successful
runtime test.

## Known caveats

PBL is an execution tier, not a guarantee that every SpiderMonkey subsystem is
WASM-ready. Runtime initialization, garbage collection, atomics, stack limits,
interrupt handling, and Servo's DOM bindings still need validation. PBL also
has a small number of unimplemented fast paths that fall back to the generic
interpreter.

## References

- SpiderMonkey Portable Baseline Interpreter: <https://searchfox.org/mozilla-central/source/js/src/vm/PortableBaselineInterpret.h>
- SpiderMonkey JIT options: <https://searchfox.org/mozilla-central/source/js/src/jit/JitOptions.cpp>
- SpiderMonkey WASI PBL build profile: `js/src/devtools/automation/variants/wasi-pbl`
