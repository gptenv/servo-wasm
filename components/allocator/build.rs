use std::env;
use std::path::PathBuf;

fn main() {
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("wasm32") {
        return;
    }
    // Same wasi-sysroot lib directory convention as mozjs-sys's build.rs
    // (WASM_CXX_LIB_DIR env var, defaulting to the same path) -- this
    // crate's wasm32 allocator (see lib.rs) links against wasi-sysroot's
    // libc purely for its malloc/free/posix_memalign symbols, so that
    // Rust's allocations and any linked C++ (e.g. mozjs-wasm's
    // SpiderMonkey) share one real allocator instead of two independent
    // ones both assuming they own memory from the linker's `__heap_base`.
    let wasm_lib_dir = env::var_os("WASM_CXX_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/wasi-sysroot/lib/wasm32-wasi"));
    assert!(
        wasm_lib_dir.join("libc.a").exists(),
        "missing wasm libc archive at {}",
        wasm_lib_dir.display()
    );
    println!("cargo:rustc-link-search=native={}", wasm_lib_dir.display());
    println!("cargo:rustc-link-lib=static=c");
}
