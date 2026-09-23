//! Executable SpiderMonkey probe for the Cloudflare Worker wasm target.

use bytes::Bytes;
use dpi::PhysicalSize;
use http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use hyper_serde::Serde;
use js::jsapi::{GCReason, OnNewGlobalHookOption};
use js::jsval::UndefinedValue;
use js::rooted;
use js::rust::SIMPLE_GLOBAL_CLASS;
use js::rust::wrappers2::{JS_GC, JS_NewGlobalObject};
use js::rust::{CompileOptionsWrapper, JSEngine, RealmOptions, Runtime, evaluate_script};
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::http_status::HttpStatus;
use net_traits::request::{
    CredentialsMode, Destination, Origin, RequestBuilder, RequestId, RequestMode,
    get_cors_unsafe_header_names,
};
use net_traits::response::ResponseInit;
use net_traits::{
    FetchMetadata, FetchResponseMsg, FilteredMetadata, Metadata, NetworkError, ResourceFetchTiming,
    ResourceTimingType,
};
use net_traits::{set_worker_fetch_cancel_handler, set_worker_fetch_request_handler};
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
use std::sync::Once;
use url::Url;
use uuid::Uuid;

// Keep the inventory resource reader linked into the raw wasm module.
use servo_default_resources as _;

use servo_base::generic_channel::GenericCallback;

thread_local! {
    static FETCH_CALLBACKS: RefCell<HashMap<RequestId, WorkerFetchEntry>> =
        RefCell::new(HashMap::new());
    static BROWSER: RefCell<Option<WorkerBrowser>> = const { RefCell::new(None) };
    static LAST_PAGE_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct WorkerFetchEntry {
    callback: GenericCallback<FetchResponseMsg>,
    visibility: WorkerResponseVisibility,
    response_started: bool,
}

const WORKER_ABI_VERSION: u32 = 2;

#[derive(serde::Serialize)]
struct WorkerHostMessage<'a> {
    version: u32,
    #[serde(flatten)]
    command: WorkerHostCommand<'a>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerHostCommand<'a> {
    Fetch { request: &'a RequestBuilder },
    Cancel { request_ids: &'a [RequestId] },
}

fn encode_host_command(command: WorkerHostCommand<'_>) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&WorkerHostMessage {
        version: WORKER_ABI_VERSION,
        command,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_abi_version() -> u32 {
    WORKER_ABI_VERSION
}

enum WorkerResponseVisibility {
    Unfiltered,
    Basic,
    Cors { origin: String },
    Opaque,
}

struct WorkerBrowser {
    servo: Servo,
    webview: WebView,
    pending_navigation: Option<Url>,
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
    install_worker_panic_hook();
    if BROWSER.with(|browser| browser.borrow().is_some()) {
        return 0;
    }
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
    register_bundled_fonts();
    let servo = ServoBuilder::default().build();
    let builder = WebViewBuilder::new(&servo, rendering_context.clone());
    let builder = builder.url(url);
    let webview = builder.build();
    BROWSER.with(|browser| {
        *browser.borrow_mut() = Some(WorkerBrowser {
            servo,
            webview,
            pending_navigation: None,
            _rendering_context: rendering_context,
        });
    });
    1
}

/// Fonts compiled into the module so text always has a font; see fonts/README.md.
const BUNDLED_FONTS: [&[u8]; 4] = [
    include_bytes!("fonts/NotoSans-Regular.ttf"),
    include_bytes!("fonts/NotoSans-Bold.ttf"),
    include_bytes!("fonts/NotoSerif-Regular.ttf"),
    include_bytes!("fonts/NotoSansMono-Regular.ttf"),
];

/// Largest font file a host may register (large enough for a full CJK font).
const MAX_HOST_FONT_BYTES: usize = 32 * 1024 * 1024;

fn register_bundled_fonts() {
    for font in BUNDLED_FONTS {
        if let Err(error) = fonts_traits::worker_fonts::register(font.to_vec()) {
            let message = format!("Bundled font failed to register: {error}");
            unsafe { host_log_error(message.as_ptr(), message.len()) };
        }
    }
}

/// Register a host-supplied font file (TTF/OTF, or a TTC/OTC collection) for
/// all pages in this instance. Returns the number of faces added, or zero if
/// the bytes are not a font. Fonts registered after a page has laid out text
/// apply to later font lookups.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_register_font(ptr: *const u8, len: usize) -> u32 {
    if ptr.is_null() || len == 0 || len > MAX_HOST_FONT_BYTES {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    fonts_traits::worker_fonts::register(bytes).unwrap_or(0)
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
        let mut binding = browser.borrow_mut();
        let Some(browser) = binding.as_mut() else {
            return 0;
        };
        // Servo does not override a pending top-level navigation. Coalesce
        // host requests made between pumps so reset followed by load does
        // not discard the requested page in favor of about:blank.
        browser.pending_navigation = Some(url);
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
    pump_worker_once().0
}

/// Pump once and report both fetch dispatches and browser event progress.
/// Bit zero indicates progress; the remaining bits count host fetches.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pump_status() -> u32 {
    let (requests, progressed) = pump_worker_once();
    ((requests.min(u32::MAX as usize >> 1) as u32) << 1) | u32::from(progressed)
}

fn pump_worker_once() -> (usize, bool) {
    // A Servo event-loop turn can enqueue a fetch, so pump once before and
    // once after it. The second pass is what makes a newly scheduled request
    // visible to the host without requiring an extra no-op turn.
    let mut requests = pump_worker_fetches();
    let mut progressed = requests != 0;
    progressed |= BROWSER.with(|browser| {
        if let Some(browser) = browser.borrow_mut().as_mut() {
            let navigation = browser.pending_navigation.take();
            let navigated = navigation.is_some();
            if let Some(url) = navigation {
                browser.webview.load(url);
            }
            browser.servo.worker_spin_event_loop() || navigated
        } else {
            false
        }
    });
    requests += pump_worker_fetches();
    (requests, progressed || requests != 0)
}

/// Return the next scheduled timer deadline in monotonic nanoseconds, or zero
/// when no browser timer is pending. The Worker host should wait until this
/// deadline and call `servo_worker_pump` again.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_next_timer_deadline_ns() -> u64 {
    BROWSER.with(|browser| {
        browser
            .borrow()
            .as_ref()
            .and_then(|browser| browser.servo.worker_next_timer_deadline_ns())
            .unwrap_or(0)
    })
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
    /// Host entry point receiving a versioned fetch or cancellation command.
    #[link_name = "worker_fetch_request"]
    fn host_fetch_request(ptr: *const u8, len: usize);
}

fn install_worker_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let message = info.to_string();
            unsafe { host_log_error(message.as_ptr(), message.len()) };
        }));
    });
}

fn install_fetch_adapter() {
    set_worker_fetch_cancel_handler(Box::new(|request_ids| {
        for request_id in &request_ids {
            complete_worker_fetch_error(*request_id, NetworkError::LoadCancelled);
        }
        if let Ok(payload) = encode_host_command(WorkerHostCommand::Cancel {
            request_ids: &request_ids,
        }) {
            unsafe { host_fetch_request(payload.as_ptr(), payload.len()) };
        }
    }));
    set_worker_fetch_request_handler(Box::new(|mut request, redirect, callback| {
        apply_worker_redirect(&mut request, redirect);
        queue_worker_fetch(request, callback);
    }));

    let handler: WorkerFetchHandler = Box::new(|mut request, redirect, channels| {
        apply_worker_redirect(&mut request, redirect);
        let net_traits::FetchChannels::ResponseMsg(callback) = channels else {
            return;
        };
        let request_id = request.id;
        let visibility = match worker_response_visibility(&mut request) {
            Ok(visibility) => visibility,
            Err(error) => {
                let _ = callback.send(FetchResponseMsg::ProcessResponse(
                    request_id,
                    Err(error.clone()),
                ));
                let _ = callback.send(FetchResponseMsg::ProcessResponseEOF(
                    request_id,
                    Err(error),
                    ResourceFetchTiming::new(ResourceTimingType::Resource),
                ));
                return;
            },
        };
        let payload = encode_host_command(WorkerHostCommand::Fetch { request: &request });
        FETCH_CALLBACKS.with(|callbacks| {
            callbacks.borrow_mut().insert(
                request_id,
                WorkerFetchEntry {
                    callback,
                    visibility,
                    response_started: false,
                },
            );
        });
        dispatch_worker_fetch(request_id, payload);
    });
    set_worker_fetch_handler(handler);
}

fn apply_worker_redirect(request: &mut RequestBuilder, redirect: Option<ResponseInit>) {
    if let Some(Some(Ok(url))) = redirect.map(|response| response.location_url) {
        request.url = UrlWithBlobClaim::from_url_without_having_claimed_blob(url);
    }
}

fn worker_response_visibility(
    request: &mut RequestBuilder,
) -> Result<WorkerResponseVisibility, NetworkError> {
    // This bridge bypasses Servo's native HTTP fetch algorithm. For now only
    // simple, non-credentialed cross-origin CORS requests are supported; a
    // preflight or a credentialed fetch must never be sent without its checks.
    let script_fetch = request.destination == Destination::None;
    let origin = match &request.origin {
        Origin::Origin(origin) => Some(origin),
        Origin::Client => request
            .client
            .as_ref()
            .and_then(|client| match &client.origin {
                Origin::Origin(origin) => Some(origin),
                Origin::Client => None,
            }),
    };
    let same_origin = origin.is_some_and(|origin| *origin == request.url.origin());
    if same_origin {
        return Ok(if script_fetch {
            WorkerResponseVisibility::Basic
        } else {
            WorkerResponseVisibility::Unfiltered
        });
    }
    if request.mode == RequestMode::SameOrigin
        || request.mode == RequestMode::NoCors && script_fetch
    {
        return Err(NetworkError::CorsGeneral);
    }
    if request.mode == RequestMode::NoCors {
        // Parser-owned subresources can consume the internal response, but
        // CSSOM and classic-script error reporting must see an opaque one.
        return Ok(WorkerResponseVisibility::Opaque);
    }
    if request.mode != RequestMode::CorsMode {
        return Ok(WorkerResponseVisibility::Unfiltered);
    }
    let Some(origin) = origin else {
        return Err(NetworkError::CorsGeneral);
    };
    if request.method != Method::GET && request.method != Method::HEAD
        || request.body.is_some()
        || request.use_cors_preflight
        || request.use_url_credentials
        || request.credentials_mode == CredentialsMode::Include
        || !get_cors_unsafe_header_names(&request.headers).is_empty()
    {
        return Err(NetworkError::CorsGeneral);
    }
    let origin = origin.ascii_serialization().into_owned();
    if origin == "null" {
        return Err(NetworkError::CorsGeneral);
    }
    let value = HeaderValue::from_str(&origin).map_err(|_| NetworkError::CorsGeneral)?;
    request.headers.insert(header::ORIGIN, value);
    Ok(WorkerResponseVisibility::Cors { origin })
}

#[cfg(target_arch = "wasm32")]
fn queue_worker_fetch(mut request: RequestBuilder, mut callback: net_traits::BoxedFetchCallback) {
    let request_id = request.id;
    if request.url.url().scheme() == "data" {
        fetch_worker_data_url(request, callback);
        return;
    }
    let visibility = match worker_response_visibility(&mut request) {
        Ok(visibility) => visibility,
        Err(error) => {
            worker_log(&format!(
                "Worker fetch of {} ({:?}, {:?}) rejected before host dispatch: {error:?}",
                request.url.url(),
                request.destination,
                request.mode,
            ));
            callback(FetchResponseMsg::ProcessResponse(
                request_id,
                Err(error.clone()),
            ));
            callback(FetchResponseMsg::ProcessResponseEOF(
                request_id,
                Err(error),
                ResourceFetchTiming::new(ResourceTimingType::Resource),
            ));
            return;
        },
    };
    let payload = encode_host_command(WorkerHostCommand::Fetch { request: &request });
    let callback = GenericCallback::new(move |message| {
        if let Ok(message) = message {
            callback(message);
        }
    })
    .expect("create Worker fetch callback");
    FETCH_CALLBACKS.with(|callbacks| {
        callbacks.borrow_mut().insert(
            request_id,
            WorkerFetchEntry {
                callback,
                visibility,
                response_started: false,
            },
        );
    });
    dispatch_worker_fetch(request_id, payload);
}

fn worker_log(message: &str) {
    unsafe { host_log_error(message.as_ptr(), message.len()) };
}

/// Resolve a `data:` URL inside the Worker, like Fetch's scheme fetch does:
/// it needs no network access, so it must not become a host subrequest (which
/// would also subject it to the cross-origin checks meant for real origins).
fn fetch_worker_data_url(request: RequestBuilder, mut callback: net_traits::BoxedFetchCallback) {
    let request_id = request.id;
    let url = request.url.url();
    let decoded = data_url::DataUrl::process(url.as_str())
        .ok()
        .and_then(|data| {
            let mime = data.mime_type().to_string();
            data.decode_to_vec().ok().map(|(body, _)| (mime, body))
        });
    let Some((mime_type, body)) = decoded else {
        let error = NetworkError::ResourceLoadError("Invalid data: URL".into());
        callback(FetchResponseMsg::ProcessResponse(request_id, Err(error.clone())));
        callback(FetchResponseMsg::ProcessResponseEOF(
            request_id,
            Err(error),
            ResourceFetchTiming::new(ResourceTimingType::Resource),
        ));
        return;
    };
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&mime_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let mut metadata = Metadata::default(url);
    metadata.set_content_type(mime::Mime::from_str(&mime_type).ok().as_ref());
    metadata.headers = Some(Serde(headers));
    // data: responses are basic (same-origin) for every request mode.
    let visibility = if request.destination == Destination::None {
        WorkerResponseVisibility::Basic
    } else {
        WorkerResponseVisibility::Unfiltered
    };
    let metadata = match filter_worker_metadata(metadata, &visibility) {
        Ok(metadata) => metadata,
        Err(error) => {
            callback(FetchResponseMsg::ProcessResponse(request_id, Err(error.clone())));
            callback(FetchResponseMsg::ProcessResponseEOF(
                request_id,
                Err(error),
                ResourceFetchTiming::new(ResourceTimingType::Resource),
            ));
            return;
        },
    };
    callback(FetchResponseMsg::ProcessResponse(request_id, Ok(metadata)));
    if !body.is_empty() {
        callback(FetchResponseMsg::ProcessResponseChunk(
            request_id,
            Bytes::from(body),
        ));
    }
    callback(FetchResponseMsg::ProcessResponseEOF(
        request_id,
        Ok(()),
        ResourceFetchTiming::new(ResourceTimingType::Resource),
    ));
}

fn dispatch_worker_fetch(request_id: RequestId, payload: serde_json::Result<Vec<u8>>) {
    match payload {
        Ok(payload) => unsafe { host_fetch_request(payload.as_ptr(), payload.len()) },
        Err(error) => {
            let message = format!("Worker request serialization failed: {error}");
            worker_log(&message);
            complete_worker_fetch_error(request_id, NetworkError::ResourceLoadError(message));
        },
    }
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

/// Clear the current page by navigating its webview to `about:blank` and
/// discard host-side fetch callbacks. The SpiderMonkey runtime and browser
/// services remain alive because Servo's native shutdown path blocks on OS
/// threads and is not safe to invoke from a Worker.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_reset() -> i32 {
    let existed = BROWSER.with(|browser| {
        let mut binding = browser.borrow_mut();
        let Some(browser) = binding.as_mut() else {
            return false;
        };
        if let Ok(url) = Url::parse("about:blank") {
            browser.pending_navigation = Some(url);
            true
        } else {
            false
        }
    });
    let canceled =
        FETCH_CALLBACKS.with(|callbacks| callbacks.borrow().keys().copied().collect::<Vec<_>>());
    for request_id in canceled {
        complete_worker_fetch_error(request_id, NetworkError::LoadCancelled);
    }
    LAST_PAGE_RESULT.with(|result| result.borrow_mut().clear());
    i32::from(existed)
}

/// Report whether the host still needs to deliver a fetch response.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pending_fetch_count() -> usize {
    FETCH_CALLBACKS.with(|callbacks| callbacks.borrow().len())
}

/// Start a bounded, chunked Worker response. Headers are a JSON array of
/// `[name, value]` pairs so duplicate fields survive the JS/WASM boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_begin_http_response(
    request_id_ptr: *const u8,
    request_id_len: usize,
    url_ptr: *const u8,
    url_len: usize,
    status: u16,
    headers_ptr: *const u8,
    headers_len: usize,
    redirected: i32,
) -> i32 {
    if request_id_ptr.is_null()
        || url_ptr.is_null()
        || (headers_ptr.is_null() && headers_len != 0)
        || request_id_len == 0
        || request_id_len > 64
        || url_len == 0
        || url_len > 16 * 1024
        || headers_len > 64 * 1024
        || !(100..=599).contains(&status)
    {
        return 0;
    }
    let request_id_bytes = unsafe { std::slice::from_raw_parts(request_id_ptr, request_id_len) };
    let url_bytes = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let headers_bytes = if headers_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(headers_ptr, headers_len) }
    };
    let Ok(request_id) = std::str::from_utf8(request_id_bytes)
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(())
    else {
        return 0;
    };
    let Ok(url) = std::str::from_utf8(url_bytes)
        .ok()
        .and_then(|text| ServoUrl::parse(text).ok())
        .ok_or(())
    else {
        return 0;
    };
    let pairs: Vec<(String, String)> = if headers_len == 0 {
        Vec::new()
    } else {
        let Ok(pairs) = serde_json::from_slice(headers_bytes) else {
            return 0;
        };
        pairs
    };
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) else {
            return 0;
        };
        headers.append(name, value);
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| mime::Mime::from_str(value).ok());
    let location = (300..400)
        .contains(&status)
        .then(|| {
            headers
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(|value| url.join(value).map_err(|error| error.to_string()))
        })
        .flatten();
    let mut metadata = Metadata::default(url);
    metadata.status = HttpStatus::new_raw(status, Vec::new());
    metadata.headers = Some(Serde(headers));
    metadata.set_content_type(content_type.as_ref());
    metadata.location_url = location;
    metadata.redirected = redirected != 0;

    FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let request_id = RequestId(request_id);
        let Some(entry) = callbacks.get_mut(&request_id) else {
            return 0;
        };
        if entry.response_started {
            return 0;
        }
        match filter_worker_metadata(metadata, &entry.visibility) {
            Ok(metadata) => {
                entry.response_started = true;
                i32::from(
                    entry
                        .callback
                        .send(FetchResponseMsg::ProcessResponse(request_id, Ok(metadata)))
                        .is_ok(),
                )
            },
            Err(error) => {
                let _ = entry.callback.send(FetchResponseMsg::ProcessResponse(
                    request_id,
                    Err(error.clone()),
                ));
                let _ = entry.callback.send(FetchResponseMsg::ProcessResponseEOF(
                    request_id,
                    Err(error),
                    ResourceFetchTiming::new(ResourceTimingType::Resource),
                ));
                callbacks.remove(&request_id);
                // The host must stop reading this response and must not send
                // chunks. -1 means a terminal CORS denial, not a bad ABI.
                -1
            },
        }
    })
}

fn filter_worker_metadata(
    metadata: Metadata,
    visibility: &WorkerResponseVisibility,
) -> Result<FetchMetadata, NetworkError> {
    if matches!(visibility, WorkerResponseVisibility::Opaque) {
        return Ok(FetchMetadata::Filtered {
            filtered: FilteredMetadata::Opaque,
            unsafe_: metadata,
        });
    }
    let (WorkerResponseVisibility::Basic | WorkerResponseVisibility::Cors { .. }) = visibility
    else {
        return Ok(FetchMetadata::Unfiltered(metadata));
    };
    let mut filtered = metadata.clone();
    let Some(Serde(headers)) = &metadata.headers else {
        return Err(NetworkError::CorsGeneral);
    };
    let exposed = match visibility {
        WorkerResponseVisibility::Cors { origin } => {
            let Some(allowed) = headers
                .get_all(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .iter()
                .map(|value| value.to_str().ok())
                .collect::<Option<Vec<_>>>()
            else {
                return Err(NetworkError::CorsGeneral);
            };
            if allowed.len() != 1 || allowed[0] != "*" && allowed[0] != origin {
                return Err(NetworkError::CorsGeneral);
            }
            headers
                .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(',').map(str::trim).collect::<Vec<_>>())
                .unwrap_or_default()
        },
        _ => Vec::new(),
    };
    let all_exposed = exposed.contains(&"*");
    let mut visible_headers = HeaderMap::new();
    for (name, value) in headers {
        let name_text = name.as_str();
        if name == header::SET_COOKIE || name_text == "set-cookie2" {
            continue;
        }
        if matches!(visibility, WorkerResponseVisibility::Basic)
            || all_exposed
            || matches!(
                name_text,
                "cache-control"
                    | "content-language"
                    | "content-length"
                    | "content-type"
                    | "expires"
                    | "last-modified"
                    | "pragma"
            )
            || exposed
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name_text))
        {
            visible_headers.append(name.clone(), value.clone());
        }
    }
    filtered.headers = Some(Serde(visible_headers));
    Ok(FetchMetadata::Filtered {
        filtered: match visibility {
            WorkerResponseVisibility::Basic => FilteredMetadata::Basic(filtered),
            WorkerResponseVisibility::Cors { .. } => FilteredMetadata::Cors(filtered),
            WorkerResponseVisibility::Unfiltered | WorkerResponseVisibility::Opaque => {
                unreachable!()
            },
        },
        unsafe_: metadata,
    })
}

/// Send at most 256 KiB of response data per call. The host owns the buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_deliver_http_chunk(
    request_id_ptr: *const u8,
    request_id_len: usize,
    body_ptr: *const u8,
    body_len: usize,
) -> i32 {
    if request_id_ptr.is_null()
        || request_id_len == 0
        || request_id_len > 64
        || (body_ptr.is_null() && body_len != 0)
        || body_len > 256 * 1024
    {
        return 0;
    }
    let request_id_bytes = unsafe { std::slice::from_raw_parts(request_id_ptr, request_id_len) };
    let Ok(request_id) = std::str::from_utf8(request_id_bytes)
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(())
    else {
        return 0;
    };
    let body = if body_len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(body_ptr, body_len) })
    };
    let delivered = FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let Some(callback) = callbacks.get_mut(&RequestId(request_id)) else {
            return false;
        };
        if !callback.response_started {
            return false;
        }
        callback
            .callback
            .send(FetchResponseMsg::ProcessResponseChunk(
                RequestId(request_id),
                body,
            ))
            .is_ok()
    });
    i32::from(delivered)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_finish_http_response(
    request_id_ptr: *const u8,
    request_id_len: usize,
) -> i32 {
    finish_http_response(request_id_ptr, request_id_len, None)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_finish_http_error(
    request_id_ptr: *const u8,
    request_id_len: usize,
    message_ptr: *const u8,
    message_len: usize,
) -> i32 {
    if message_ptr.is_null() || message_len > 4096 {
        return 0;
    }
    let message = unsafe { std::slice::from_raw_parts(message_ptr, message_len) };
    finish_http_response(
        request_id_ptr,
        request_id_len,
        Some(NetworkError::ResourceLoadError(
            String::from_utf8_lossy(message).into_owned(),
        )),
    )
}

fn finish_http_response(
    request_id_ptr: *const u8,
    request_id_len: usize,
    error: Option<NetworkError>,
) -> i32 {
    if request_id_ptr.is_null() || request_id_len == 0 || request_id_len > 64 {
        return 0;
    }
    let request_id_bytes = unsafe { std::slice::from_raw_parts(request_id_ptr, request_id_len) };
    let Ok(request_id) = std::str::from_utf8(request_id_bytes)
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(())
    else {
        return 0;
    };
    let request_id = RequestId(request_id);
    if let Some(error) = error {
        return i32::from(complete_worker_fetch_error(request_id, error));
    }
    let callback = FETCH_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        if callbacks
            .get(&request_id)
            .is_some_and(|entry| entry.response_started)
        {
            callbacks.remove(&request_id)
        } else {
            None
        }
    });
    let delivered = callback.is_some_and(|callback| {
        callback
            .callback
            .send(FetchResponseMsg::ProcessResponseEOF(
                request_id,
                Ok(()),
                ResourceFetchTiming::new(ResourceTimingType::Resource),
            ))
            .is_ok()
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
    if request_id_ptr.is_null()
        || message_ptr.is_null()
        || request_id_len == 0
        || request_id_len > 64
        || message_len > 4096
    {
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
    i32::from(complete_worker_fetch_error(RequestId(request_uuid), error))
}

fn complete_worker_fetch_error(request_id: RequestId, error: NetworkError) -> bool {
    let entry = FETCH_CALLBACKS.with(|callbacks| callbacks.borrow_mut().remove(&request_id));
    let Some(entry) = entry else {
        return false;
    };
    // Release the callback-map borrow before notifying listeners. Cancellation
    // can be triggered by a listener's cleanup on this same Worker thread.
    let metadata_ok = entry.response_started
        || entry
            .callback
            .send(FetchResponseMsg::ProcessResponse(
                request_id,
                Err(error.clone()),
            ))
            .is_ok();
    let eof_ok = entry
        .callback
        .send(FetchResponseMsg::ProcessResponseEOF(
            request_id,
            Err(error),
            ResourceFetchTiming::new(ResourceTimingType::Resource),
        ))
        .is_ok();
    metadata_ok && eof_ok
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
