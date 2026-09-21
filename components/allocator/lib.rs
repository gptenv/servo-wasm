/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Selecting the default global allocator for Servo, and exposing common
//! allocator introspection APIs for memory profiling.

use std::os::raw::c_void;

#[cfg(not(feature = "allocation-tracking"))]
#[global_allocator]
static ALLOC: Allocator = Allocator;

#[cfg(feature = "allocation-tracking")]
#[global_allocator]
static ALLOC: crate::tracking::AccountingAlloc<Allocator> =
    crate::tracking::AccountingAlloc::with_allocator(Allocator);

#[cfg(feature = "allocation-tracking")]
mod tracking;

pub fn is_tracking_unmeasured() -> bool {
    cfg!(feature = "allocation-tracking")
}

pub fn dump_unmeasured(_writer: impl std::io::Write) {
    #[cfg(feature = "allocation-tracking")]
    ALLOC.dump_unmeasured_allocations(_writer);
}

pub fn disable_unmeasured_tracking() {
    #[cfg(feature = "allocation-tracking")]
    ALLOC.disable();
}

pub struct HeapReport {
    pub path: &'static str,
    pub size: Option<usize>,
}

pub use crate::platform::*;

type EnclosingSizeFn = unsafe extern "C" fn(*const c_void) -> usize;

/// # Safety
/// No restrictions. The passed pointer is never dereferenced.
/// This function is only marked unsafe because the MallocSizeOfOps APIs
/// requires an unsafe function pointer.
#[cfg(feature = "allocation-tracking")]
unsafe extern "C" fn enclosing_size_impl(ptr: *const c_void) -> usize {
    let (adjusted, size) = crate::ALLOC.enclosing_size(ptr);
    if size != 0 {
        crate::ALLOC.note_allocation(adjusted, size);
    }
    size
}

#[expect(non_upper_case_globals)]
#[cfg(feature = "allocation-tracking")]
pub static enclosing_size: Option<EnclosingSizeFn> = Some(crate::enclosing_size_impl);

#[expect(non_upper_case_globals)]
#[cfg(not(feature = "allocation-tracking"))]
pub static enclosing_size: Option<EnclosingSizeFn> = None;

#[cfg(all(
    feature = "use-jemalloc",
    not(any(windows, target_env = "ohos", target_arch = "wasm32"))
))]
mod platform {
    use std::ffi::CStr;
    use std::mem::size_of_val;
    use std::os::raw::c_void;
    use std::ptr;

    use tikv_jemalloc_sys::mallctl;
    pub use tikv_jemallocator::Jemalloc as Allocator;

    pub fn heap_reports() -> Vec<crate::HeapReport> {
        vec![
            crate::HeapReport {
                path: "jemalloc-heap-allocated",
                size: jemalloc_stat(c"stats.allocated"),
            },
            crate::HeapReport {
                path: "jemalloc-heap-active",
                size: jemalloc_stat(c"stats.active"),
            },
            crate::HeapReport {
                path: "jemalloc-heap-mapped",
                size: jemalloc_stat(c"stats.mapped"),
            },
        ]
    }

    fn jemalloc_stat(value_name: &CStr) -> Option<usize> {
        // Before we request the measurement of interest, we first send an "epoch"
        // request. Without that jemalloc gives cached statistics(!) which can be
        // highly inaccurate.
        let epoch_c_name = c"epoch";
        let mut epoch: u64 = 0;
        let epoch_ptr = &raw mut epoch;
        let mut epoch_len = size_of_val(&epoch);

        let mut value: usize = 0;
        let value_ptr = &raw mut value;
        let mut value_len = size_of_val(&value);

        // Using the same values for the `old` and `new` parameters is enough
        // to get the statistics updated.
        let rv = unsafe {
            mallctl(
                epoch_c_name.as_ptr(),
                epoch_ptr.cast(),
                &mut epoch_len,
                epoch_ptr.cast(),
                epoch_len,
            )
        };
        if rv != 0 {
            return None;
        }

        let rv = unsafe {
            mallctl(
                value_name.as_ptr(),
                value_ptr.cast(),
                &mut value_len,
                ptr::null_mut(),
                0,
            )
        };
        if rv != 0 {
            return None;
        }

        Some(value)
    }

    /// Get the size of a heap block.
    ///
    /// # Safety
    ///
    /// Passing a non-heap allocated pointer to this function results in undefined behavior.
    pub unsafe extern "C" fn usable_size(ptr: *const c_void) -> usize {
        let size = unsafe { tikv_jemallocator::usable_size(ptr) };
        #[cfg(feature = "allocation-tracking")]
        crate::ALLOC.note_allocation(ptr, size);
        size
    }

    /// Memory allocation APIs compatible with libc
    pub mod libc_compat {
        pub use tikv_jemalloc_sys::{free, malloc, realloc};
    }
}

#[cfg(all(
    not(windows),
    not(target_arch = "wasm32"),
    any(target_env = "ohos", not(feature = "use-jemalloc"))
))]
mod platform {
    pub use std::alloc::System as Allocator;
    use std::os::raw::c_void;

    /// Get the size of a heap block.
    ///
    /// # Safety
    ///
    /// Passing a non-heap allocated pointer to this function results in undefined behavior.
    pub unsafe extern "C" fn usable_size(ptr: *const c_void) -> usize {
        #[cfg(target_vendor = "apple")]
        unsafe {
            let size = libc::malloc_size(ptr);
            #[cfg(feature = "allocation-tracking")]
            crate::ALLOC.note_allocation(ptr, size);
            size
        }

        #[cfg(not(target_vendor = "apple"))]
        unsafe {
            let size = libc::malloc_usable_size(ptr as *mut _);
            #[cfg(feature = "allocation-tracking")]
            crate::ALLOC.note_allocation(ptr, size);
            size
        }
    }

    pub mod libc_compat {
        pub use libc::{free, malloc, realloc};
    }

    pub fn heap_reports() -> Vec<crate::HeapReport> {
        Vec::new()
    }
}

#[cfg(windows)]
mod platform {
    pub use std::alloc::System as Allocator;
    use std::os::raw::c_void;

    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapSize, HeapValidate};

    /// Get the size of a heap block.
    ///
    /// # Safety
    ///
    /// Passing a non-heap allocated pointer to this function results in undefined behavior.
    pub unsafe extern "C" fn usable_size(mut ptr: *const c_void) -> usize {
        unsafe {
            let heap = GetProcessHeap();

            if HeapValidate(heap, 0, ptr) == FALSE {
                ptr = *(ptr as *const *const c_void).offset(-1)
            }

            let size = HeapSize(heap, 0, ptr) as usize;
            #[cfg(feature = "allocation-tracking")]
            crate::ALLOC.note_allocation(ptr, size);
            size
        }
    }

    pub fn heap_reports() -> Vec<crate::HeapReport> {
        Vec::new()
    }
}

#[cfg(target_arch = "wasm32")]
mod platform {
    use std::alloc::{GlobalAlloc, Layout};
    use std::ffi::c_void;
    use std::os::raw::c_void as raw_c_void;
    use std::{mem, ptr};

    // wasm32-unknown-unknown has no libc at all: not even the stub that
    // other targets get from the `libc` crate. Rust's own built-in
    // `std::alloc::System` allocator for this target *would* work on its
    // own, but this crate's whole job is being the *one* global allocator
    // shared by everything linked into the final wasm module -- including
    // mozjs-wasm's C++ (SpiderMonkey calls libc malloc/free directly, not
    // through Rust's allocator hooks). If Rust's built-in allocator and a
    // separate C allocator each independently assumed they owned memory
    // starting at the linker's `__heap_base`, they would hand out
    // overlapping addresses the first time both were used. So instead this
    // links against wasi-sysroot's libc (see build.rs) purely for its
    // malloc implementation, and routes *all* allocation -- Rust's and
    // C++'s -- through those same symbols. This mirrors
    // ports/servo-js-wasm/lib.rs's WorkerAllocator; once this crate and
    // mozjs-wasm are actually linked into one binary, that copy becomes
    // redundant and should be removed in favor of this one.
    unsafe extern "C" {
        fn posix_memalign(out: *mut *mut raw_c_void, align: usize, size: usize) -> i32;
        fn free(ptr: *mut raw_c_void);
    }

    pub struct Allocator;

    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let mut out = ptr::null_mut();
            let align = layout.align().max(mem::size_of::<usize>());
            if unsafe { posix_memalign(&mut out, align, layout.size().max(1)) } == 0 {
                out.cast()
            } else {
                ptr::null_mut()
            }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            unsafe { free(ptr.cast()) };
        }
    }

    /// There is no OS API on this target to query a heap block's usable
    /// size the way `malloc_usable_size`/`HeapSize` do elsewhere, so this
    /// can't report anything meaningful. Desktop platforms already have
    /// precedent for this: `heap_reports` below returns an empty `Vec` on
    /// Windows too, when no introspection API is readily available there.
    ///
    /// # Safety
    /// No restrictions; the pointer is never dereferenced.
    pub unsafe extern "C" fn usable_size(_ptr: *const c_void) -> usize {
        0
    }

    pub fn heap_reports() -> Vec<crate::HeapReport> {
        Vec::new()
    }
}
