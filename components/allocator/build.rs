use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    let build_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // This crate only wants malloc/free/posix_memalign from libc.a. Linking
    // wasi-sysroot's *unmodified* `libc.a` -- even with the `-bundle`
    // modifier below disabled -- still leaves it as an ordinary archive
    // input to the final servo-js-wasm link, where cargo's own reordering
    // of `cargo:rustc-link-lib` directives across the whole crate graph can
    // place it ahead of mozjs-sys/build.rs's `worker_libc_shim`. If that
    // happens, the exact same `.o` members `worker_libc_shim.c` exists to
    // override (posix.o, getenv.o, exit.o, ...) get pulled in from *this*
    // crate's reference to libc.a instead, bringing their real WASI imports
    // back regardless of anything mozjs-sys's build.rs does. Confirmed
    // empirically while chasing down the last `wasi_snapshot_preview1`
    // imports in the Worker build.
    //
    // Link a copy of libc.a with those same members physically removed
    // instead, under a name that can never collide with a plain `-lc`
    // reference anywhere else in the graph. `worker_libc_trimmed_objects()`
    // must stay in sync with mozjs-sys/build.rs's own (larger) removal
    // list -- see that file's `worker_libc_trimmed_objects_to_remove` for
    // how each entry there was found needed; this crate does not call any
    // of them itself, so it just needs the same objects kept out of reach.
    build_trimmed_wasi_libc(&wasm_lib_dir, &build_dir);
    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static:-bundle=worker_wasi_libc_trimmed");
}

fn worker_libc_trimmed_objects() -> &'static [&'static str] {
    &[
        "clock_gettime.o",
        "time.o",
        "write.o",
        "writev.o",
        "readv.o",
        "lseek.o",
        "isatty.o",
        "fcntl.o",
        "ioctl.o",
        "read.o",
        "close.o",
        "posix.o",
        "fstat.o",
        "getenv.o",
        "setenv.o",
        "unsetenv.o",
        "exit.o",
        "_Exit.o",
        "chdir.o",
    ]
}

fn build_trimmed_wasi_libc(wasm_lib_dir: &Path, build_dir: &Path) {
    let src = wasm_lib_dir.join("libc.a");
    let dst = build_dir.join("libworker_wasi_libc_trimmed.a");
    println!("cargo:rerun-if-changed={}", src.display());
    std::fs::copy(&src, &dst).expect("copy wasi-sysroot libc.a for trimming");
    let ar = env::var_os("AR").unwrap_or_else(|| OsString::from("ar"));
    let status = Command::new(&ar)
        .arg("d")
        .arg(&dst)
        .args(worker_libc_trimmed_objects())
        .status()
        .expect("run ar to trim wasi-sysroot libc.a");
    assert!(
        status.success(),
        "ar d failed while trimming wasi-sysroot libc.a"
    );
}
