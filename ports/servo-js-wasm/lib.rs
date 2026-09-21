//! Executable SpiderMonkey probe for the Cloudflare Worker wasm target.

use bytes::Bytes;
use dpi::PhysicalSize;
use js::jsapi::{GCReason, OnNewGlobalHookOption};
use js::jsval::UndefinedValue;
use js::rooted;
use js::rust::SIMPLE_GLOBAL_CLASS;
use js::rust::wrappers2::{JS_GC, JS_NewGlobalObject};
use js::rust::{CompileOptionsWrapper, JSEngine, RealmOptions, Runtime, evaluate_script};
use net_traits::http_status::HttpStatus;
use net_traits::request::RequestId;
use net_traits::{
    FetchMetadata, FetchResponseMsg, Metadata, NetworkError, ResourceFetchTiming,
    ResourceTimingType,
};
use servo::{
    RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder,
};
use servo::{WorkerFetchHandler, pump_worker_fetches, set_worker_fetch_handler};
use servo_url::ServoUrl;
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem;
use std::ptr;
use std::rc::Rc;
use std::str::FromStr;
use url::Url;
use uuid::Uuid;

use servo_base::generic_channel::GenericCallback;

thread_local! {
    static FETCH_CALLBACKS: RefCell<HashMap<RequestId, GenericCallback<FetchResponseMsg>>> =
        RefCell::new(HashMap::new());
    static BROWSER: RefCell<Option<WorkerBrowser>> = const { RefCell::new(None) };
    static LAST_PAGE_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct WorkerBrowser {
    servo: Servo,
    webview: WebView,
    _rendering_context: Rc<dyn RenderingContext>,
}

/// Create the single-threaded Servo instance used by a Worker isolate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_bootstrap(
    width: u32,
    height: u32,
    url_ptr: *const u8,
    url_len: usize,
) -> i32 {
    if width == 0 || height == 0 || url_ptr.is_null() || url_len == 0 || url_len > 16 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let Ok(url_string) = std::str::from_utf8(bytes) else {
        return 0;
    };
    let Ok(url) = Url::parse(url_string) else {
        return 0;
    };
    let rendering_context = match SoftwareRenderingContext::new(PhysicalSize { width, height }) {
        Ok(context) => Rc::new(context) as Rc<dyn RenderingContext>,
        Err(_) => return 0,
    };
    let servo = ServoBuilder::default().build();
    let webview = WebViewBuilder::new(&servo, rendering_context.clone())
        .url(url)
        .build();
    BROWSER.with(|browser| {
        *browser.borrow_mut() = Some(WorkerBrowser {
            servo,
            webview,
            _rendering_context: rendering_context,
        });
    });
    1
}

/// Load another page in the existing Worker webview.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_load_page(url_ptr: *const u8, url_len: usize) -> i32 {
    if url_ptr.is_null() || url_len == 0 || url_len > 16 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let Ok(url_string) = std::str::from_utf8(bytes) else {
        return 0;
    };
    let Ok(url) = Url::parse(url_string) else {
        return 0;
    };
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        browser.webview.load(url);
        1
    })
}

/// Queue JavaScript for evaluation in the current Servo document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_evaluate_page(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 || len > 4 * 1024 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(script) = std::str::from_utf8(bytes) else {
        return 0;
    };
    LAST_PAGE_RESULT.with(|slot| slot.borrow_mut().clear());
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        browser.webview.evaluate_javascript(script, |result| {
            if let Ok(value) = serde_json::to_vec(&result) {
                LAST_PAGE_RESULT.with(|slot| *slot.borrow_mut() = value);
            }
        });
        1
    })
}

/// Advance Servo and the Worker fetch queue. Returns the number of network
/// messages handed to the host during this turn.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pump() -> usize {
    // A Servo event-loop turn can enqueue a fetch, so pump once before and
    // once after it. The second pass is what makes a newly scheduled request
    // visible to the host without requiring an extra no-op turn.
    let mut requests = pump_worker_fetches();
    BROWSER.with(|browser| {
        if let Some(browser) = browser.borrow().as_ref() {
            browser.servo.spin_event_loop();
        }
    });
    requests += pump_worker_fetches();
    requests
}

/// Return the pending page-evaluation result as a borrowed JSON buffer.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_page_result_ptr() -> *const u8 {
    LAST_PAGE_RESULT.with(|result| result.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_page_result_len() -> usize {
    LAST_PAGE_RESULT.with(|result| result.borrow().len())
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// Host entry point receiving a JSON-encoded Servo RequestBuilder.
    #[link_name = "worker_fetch_request"]
    fn host_fetch_request(ptr: *const u8, len: usize);
}

fn install_fetch_adapter() {
    let handler: WorkerFetchHandler = Box::new(|request, _redirect, channels| {
        let net_traits::FetchChannels::ResponseMsg(callback) = channels else {
            return;
        };
        let request_id = request.id;
        let Ok(payload) = serde_json::to_vec(&request) else {
            return;
        };
        FETCH_CALLBACKS.with(|callbacks| {
            callbacks.borrow_mut().insert(request_id, callback);
        });
        unsafe { host_fetch_request(payload.as_ptr(), payload.len()) };
    });
    set_worker_fetch_handler(handler);
}

/// Install the callback bridge used by the Cloudflare Worker host.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_install_fetch_adapter() {
    install_fetch_adapter();
}

/// Pump queued Servo fetch requests into the host callback.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pump_fetches() -> usize {
    pump_worker_fetches()
}

/// Deliver one JSON-encoded [`FetchResponseMsg`] from the host to Servo.
/// Returns 1 when a matching pending request was found, otherwise 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_deliver_fetch_response(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 || len > 16 * 1024 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(message) = serde_json::from_slice::<FetchResponseMsg>(bytes) else {
        return 0;
    };
    let request_id = message.request_id();
    let is_finished = matches!(message, FetchResponseMsg::ProcessResponseEOF(..));
    let delivered = FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let Some(callback) = callbacks.get_mut(&request_id) else {
            return false;
        };
        callback.send(message).is_ok()
    });
    if is_finished {
        FETCH_CALLBACKS.with(|callbacks| {
            callbacks.borrow_mut().remove(&request_id);
        });
    }
    i32::from(delivered)
}

/// Deliver a successful HTTP response through a compact host-facing ABI.
///
/// The Worker host should call this once for headers/body completion. Keeping
/// the conversion here avoids making JavaScript know about Servo's private
/// URL, header-map, and timing serialization formats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_deliver_http_response(
    request_id_ptr: *const u8,
    request_id_len: usize,
    url_ptr: *const u8,
    url_len: usize,
    status: u16,
    content_type_ptr: *const u8,
    content_type_len: usize,
    body_ptr: *const u8,
    body_len: usize,
) -> i32 {
    if request_id_ptr.is_null()
        || url_ptr.is_null()
        || content_type_ptr.is_null() && content_type_len != 0
        || body_ptr.is_null() && body_len != 0
        || request_id_len == 0
        || url_len == 0
        || body_len > 64 * 1024 * 1024
    {
        return 0;
    }
    let request_id_bytes = unsafe { std::slice::from_raw_parts(request_id_ptr, request_id_len) };
    let url_bytes = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let content_type_bytes = if content_type_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(content_type_ptr, content_type_len) }
    };
    let Ok(request_id_text) = std::str::from_utf8(request_id_bytes) else {
        return 0;
    };
    let Ok(request_uuid) = Uuid::parse_str(request_id_text) else {
        return 0;
    };
    let Ok(url_text) = std::str::from_utf8(url_bytes) else {
        return 0;
    };
    let Ok(url) = ServoUrl::parse(url_text) else {
        return 0;
    };
    let content_type = std::str::from_utf8(content_type_bytes)
        .ok()
        .and_then(|value| mime::Mime::from_str(value).ok());
    if !(100..=599).contains(&status) {
        return 0;
    }

    let request_id = RequestId(request_uuid);
    let metadata = FetchMetadata::Unfiltered({
        let mut metadata = Metadata::default(url.clone());
        metadata.status = HttpStatus::new_raw(status, Vec::new());
        metadata.set_content_type(content_type.as_ref());
        metadata
    });
    let body = if body_len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(body_ptr, body_len) })
    };
    let response = [
        FetchResponseMsg::ProcessResponse(request_id, Ok(metadata)),
        FetchResponseMsg::ProcessResponseChunk(request_id, body),
        FetchResponseMsg::ProcessResponseEOF(
            request_id,
            Ok(()),
            ResourceFetchTiming::new(ResourceTimingType::Resource),
        ),
    ];
    let delivered = FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let Some(callback) = callbacks.get_mut(&request_id) else {
            return false;
        };
        for message in response {
            if callback.send(message).is_err() {
                return false;
            }
        }
        true
    });
    FETCH_CALLBACKS.with(|callbacks| {
        callbacks.borrow_mut().remove(&request_id);
    });
    i32::from(delivered)
}

/// Complete a failed host fetch so Servo does not leave a request pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_deliver_http_error(
    request_id_ptr: *const u8,
    request_id_len: usize,
    message_ptr: *const u8,
    message_len: usize,
) -> i32 {
    if request_id_ptr.is_null() || message_ptr.is_null() || request_id_len == 0 {
        return 0;
    }
    let request_id_bytes = unsafe { std::slice::from_raw_parts(request_id_ptr, request_id_len) };
    let message_bytes = unsafe { std::slice::from_raw_parts(message_ptr, message_len) };
    let Ok(request_id_text) = std::str::from_utf8(request_id_bytes) else {
        return 0;
    };
    let Ok(request_uuid) = Uuid::parse_str(request_id_text) else {
        return 0;
    };
    let error =
        NetworkError::ResourceLoadError(String::from_utf8_lossy(message_bytes).into_owned());
    let request_id = RequestId(request_uuid);
    let delivered = FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let Some(callback) = callbacks.get_mut(&request_id) else {
            return false;
        };
        let response_ok = callback
            .send(FetchResponseMsg::ProcessResponse(
                request_id,
                Err(error.clone()),
            ))
            .is_ok();
        let eof_ok = callback
            .send(FetchResponseMsg::ProcessResponseEOF(
                request_id,
                Err(error),
                ResourceFetchTiming::new(ResourceTimingType::Resource),
            ))
            .is_ok();
        response_ok && eof_ok
    });
    FETCH_CALLBACKS.with(|callbacks| {
        callbacks.borrow_mut().remove(&request_id);
    });
    i32::from(delivered)
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

struct WorkerJsState {
    // Keep both objects alive for the lifetime of the Worker isolate. The
    // runtime owns the JS context; the engine handle keeps SpiderMonkey's
    // process-wide engine initialized while that runtime exists.
    _engine: JSEngine,
    runtime: Runtime,
}

thread_local! {
    // A Worker isolate executes its wasm exports on one thread. Keeping one
    // runtime here avoids paying SpiderMonkey runtime/self-hosted-code setup
    // costs for every request while each evaluation still gets a fresh global
    // object below, so script state cannot leak between requests.
    static STATE: RefCell<Option<WorkerJsState>> = const { RefCell::new(None) };
}

/// Execute JavaScript and return its signed 32-bit result.
pub fn evaluate_i32(source: &str) -> Result<i32, &'static str> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.is_none() {
            let engine = JSEngine::init().map_err(|_| "SpiderMonkey initialization failed")?;
            let runtime = Runtime::new(engine.handle());
            *state = Some(WorkerJsState {
                _engine: engine,
                runtime,
            });
        }
        let runtime = &mut state.as_mut().expect("state initialized").runtime;
        let result = (|| {
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
        })();

        // Global objects are deliberately short-lived to isolate requests.
        // Collect them after their rooted handles leave scope; otherwise a
        // long-lived Worker runtime retains every request's realm until its
        // heap threshold is reached, which is far beyond Worker memory.
        unsafe { JS_GC(runtime.cx(), GCReason::API) };
        result
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
