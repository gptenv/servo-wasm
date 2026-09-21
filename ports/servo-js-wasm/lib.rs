//! Executable SpiderMonkey probe for the Cloudflare Worker wasm target.

use js::jsapi::OnNewGlobalHookOption;
use js::jsval::UndefinedValue;
use js::rooted;
use js::rust::wrappers2::JS_NewGlobalObject;
use js::rust::{evaluate_script, CompileOptionsWrapper, JSEngine, RealmOptions, Runtime};
use js::rust::SIMPLE_GLOBAL_CLASS;
use std::cell::RefCell;
use std::alloc::{GlobalAlloc, Layout};
use std::ffi::c_void;
use std::mem;
use std::ptr;

// SpiderMonkey and libc++ allocate through the linked C runtime. Rust must
// use that same allocator; dlmalloc and C malloc cannot independently manage
// one wasm linear memory without eventually overlapping allocations.
struct WorkerAllocator;

#[global_allocator]
static ALLOCATOR: WorkerAllocator = WorkerAllocator;

unsafe extern "C" {
    fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32;
    fn free(ptr: *mut c_void);
}

unsafe impl GlobalAlloc for WorkerAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut ptr = ptr::null_mut();
        let align = layout.align().max(mem::size_of::<usize>());
        if unsafe { posix_memalign(&mut ptr, align, layout.size().max(1)) } == 0 {
            ptr.cast()
        } else {
            ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { free(ptr.cast()) };
    }
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    #[link_name = "worker_monotonic_now_ns"]
    fn host_monotonic_now_ns() -> u64;
    #[link_name = "worker_log_error"]
    fn host_log_error(ptr: *const u8, len: usize);
}

/// Called by SpiderMonkey's Worker-specific timestamp implementation.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_monotonic_now_ns() -> u64 {
    unsafe { host_monotonic_now_ns() }
}

/// WASI libc's local-time implementation is UTC in this embedding.
#[unsafe(no_mangle)]
pub extern "C" fn tzset() {}

/// Diplomat's panic hook reports to the Worker host, then traps.
#[unsafe(no_mangle)]
pub extern "C" fn diplomat_throw_error_js(ptr: *const u8, len: usize) {
    unsafe { host_log_error(ptr, len) };
    std::process::abort();
}

thread_local! {
    // SpiderMonkey initialization is process-wide and may only happen once.
    // Keep the engine alive across Worker requests in the same isolate.
    static ENGINE: RefCell<Option<JSEngine>> = const { RefCell::new(None) };
}

/// Execute JavaScript and return its signed 32-bit result.
pub fn evaluate_i32(source: &str) -> Result<i32, &'static str> {
    ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        if engine.is_none() {
            *engine = Some(JSEngine::init().map_err(|_| "SpiderMonkey initialization failed")?);
        }
        let mut runtime = Runtime::new(engine.as_ref().expect("engine initialized").handle());
        let options = RealmOptions::default();
        let cx = runtime.cx();
        rooted!(&in(cx) let global = unsafe {
            JS_NewGlobalObject(cx, &SIMPLE_GLOBAL_CLASS, ptr::null_mut(),
                               OnNewGlobalHookOption::FireOnNewGlobalHook, &*options)
        });
        if global.handle().is_null() {
            return Err("global object creation failed");
        }
        rooted!(&in(cx) let mut result = UndefinedValue());
        let options = CompileOptionsWrapper::new(cx, c"worker-eval.js".to_owned(), 1);
        evaluate_script(cx, global.handle(), source, result.handle_mut(), options)
            .map_err(|_| "JavaScript evaluation failed")?;
        if !result.get().is_int32() {
            return Err("JavaScript result is not an int32");
        }
        Ok(result.get().to_int32())
    })
}

/// Stable export for a minimal Worker-side execution smoke test.
#[unsafe(no_mangle)]
pub extern "C" fn servo_js_smoke_test() -> i32 {
    evaluate_i32("40 + 2").unwrap_or(i32::MIN)
}

/// Allocate input bytes in wasm linear memory for the host to fill.
#[unsafe(no_mangle)]
pub extern "C" fn servo_js_alloc(len: usize) -> *mut u8 {
    if len == 0 || len > 1024 * 1024 {
        return ptr::null_mut();
    }
    let mut bytes = vec![0; len].into_boxed_slice();
    let ptr = bytes.as_mut_ptr();
    mem::forget(bytes);
    ptr
}

/// Release an allocation returned by `servo_js_alloc`.
///
/// # Safety
/// `ptr` and `len` must match a live allocation returned by `servo_js_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_js_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len != 0 {
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

/// Evaluate a UTF-8 script. Bit 32 signals success; low 32 bits hold the i32.
/// Zero means evaluation failed or the result was not an i32.
///
/// # Safety
/// `ptr` must point to `len` readable bytes in wasm memory. The host owns the
/// buffer and must keep it alive until this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_js_evaluate_i32(ptr: *const u8, len: usize) -> u64 {
    if ptr.is_null() || len == 0 || len > 1024 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return 0;
    };
    match evaluate_i32(source) {
        Ok(value) => (1_u64 << 32) | u64::from(value as u32),
        Err(_) => 0,
    }
}
