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
use profile_traits::generic_callback::GenericCallback as ProfileGenericCallback;
use servo::{
    Code, DevicePoint, InputEvent, Key, KeyState, KeyboardEvent, Location, Modifiers, MouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, RenderingContext, Servo, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewPoint, WheelDelta, WheelEvent,
    WheelMode, attach_worker_cookies, pump_worker_services,
};
use servo::{WorkerFetchHandler, pump_worker_fetches, set_worker_fetch_handler};
use servo::{
    JavaScriptEvaluationError, WebDriverCommandMsg, WebDriverJSResult, WebDriverScriptCommand,
};
use servo_url::{ImmutableOrigin, ServoUrl};
use std::cell::{Cell, RefCell};
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

use servo_base::generic_channel::{self, GenericCallback, GenericReceiver, TryReceiveError};
use servo_base::id::BrowsingContextId;

thread_local! {
    static FETCH_CALLBACKS: RefCell<HashMap<RequestId, WorkerFetchEntry>> =
        RefCell::new(HashMap::new());
    static WEBSOCKET_CALLBACKS: RefCell<HashMap<RequestId,
        ProfileGenericCallback<net_traits::WebSocketNetworkEvent>>> =
        RefCell::new(HashMap::new());
    static BROWSER: RefCell<Option<WorkerBrowser>> = const { RefCell::new(None) };
    static LAST_PAGE_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// Interpreter operations each pump turn may run; negative is unlimited.
    static SCRIPT_BUDGET: Cell<i64> = const { Cell::new(-1) };
    static PAGE_EVALUATIONS: RefCell<HashMap<u32, GenericReceiver<WebDriverJSResult>>> =
        RefCell::new(HashMap::new());
    static NEXT_PAGE_EVALUATION: Cell<u32> = const { Cell::new(0) };
    static PAGE_EVALUATION_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

const MAX_PENDING_PAGE_EVALUATIONS: usize = 64;

// Defined by the Worker build of mozjs_sys (js/src/vm/WorkerScriptBudget.h).
unsafe extern "C" {
    fn mozjs_worker_set_script_budget(operations: i64);
    fn mozjs_worker_script_budget_terminations() -> u64;
}

/// Grants the configured operation budget to scripts run during one pump
/// turn, and lifts it again so work outside a turn is not charged.
struct ScriptBudgetTurn;

impl ScriptBudgetTurn {
    fn begin() -> Self {
        let budget = SCRIPT_BUDGET.with(Cell::get);
        unsafe { mozjs_worker_set_script_budget(budget) };
        ScriptBudgetTurn
    }
}

impl Drop for ScriptBudgetTurn {
    fn drop(&mut self) {
        unsafe { mozjs_worker_set_script_budget(-1) };
    }
}

struct WorkerFetchEntry {
    callback: GenericCallback<FetchResponseMsg>,
    visibility: WorkerResponseVisibility,
    accepts_cookies: bool,
    cookie_origin: ImmutableOrigin,
    response_started: bool,
}

#[derive(serde::Deserialize)]
struct WorkerRedirectCookies {
    request_id: Uuid,
    response_url: String,
    next_url: String,
    set_cookies: Vec<String>,
}

const WORKER_ABI_VERSION: u32 = 8;

/// The stable host-facing subset of a Servo request. Do not serialize
/// RequestBuilder here: its internal fields are not an ABI contract.
#[derive(serde::Serialize)]
struct WorkerFetchRequest<'a> {
    id: String,
    url: String,
    method: &'a str,
    headers: Vec<(&'a str, &'a [u8])>,
    body: Option<WorkerFetchBody<'a>>,
    destination: &'a Destination,
    redirect_mode: &'a net_traits::request::RedirectMode,
}

#[derive(serde::Serialize)]
struct WorkerFetchBody<'a> {
    worker_bytes: Option<&'a [u8]>,
}

impl<'a> WorkerFetchRequest<'a> {
    fn from_request(request: &'a RequestBuilder) -> Self {
        Self {
            id: request.id.0.to_string(),
            url: request.url.url().as_str().to_owned(),
            method: request.method.as_str(),
            headers: request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes()))
                .collect(),
            body: request.body.as_ref().map(|body| WorkerFetchBody {
                worker_bytes: body.worker_bytes.as_deref(),
            }),
            destination: &request.destination,
            redirect_mode: &request.redirect_mode,
        }
    }
}

#[derive(serde::Serialize)]
struct WorkerHostMessage<'a> {
    version: u32,
    #[serde(flatten)]
    command: WorkerHostCommand<'a>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerHostCommand<'a> {
    Fetch {
        request: WorkerFetchRequest<'a>,
    },
    Cancel {
        request_ids: &'a [RequestId],
    },
    #[serde(rename = "web_socket_connect")]
    WebSocketConnect {
        request_id: String,
        url: String,
        protocols: Vec<String>,
    },
    #[serde(rename = "web_socket_action")]
    WebSocketAction {
        request_id: String,
        action: net_traits::WebSocketDomAction,
    },
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
    let mut preferences = servo::Preferences::default();
    preferences.dom_indexeddb_enabled = true;
    preferences.dom_cache_storage_enabled = true;
    preferences.dom_storage_manager_api_enabled = true;
    let servo = ServoBuilder::default().preferences(preferences).build();
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

/// Ask the page to update its rendering on the next pump, so layout builds a
/// display list for the Worker renderer. Returns 1 if a browser exists.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_request_frame() -> i32 {
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        browser.servo.worker_request_rendering();
        1
    })
}

thread_local! {
    static LAST_FRAME_PNG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Rasterize the latest rendering (after `servo_worker_request_frame` and
/// pumping) to a PNG held in a result buffer. `flags` bit 0 captures the whole
/// page instead of the viewport. Returns the PNG length, or zero on failure
/// (the reason is logged through `worker_log_error`).
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_render_png(flags: u32) -> u32 {
    let full_page = flags & 1 != 0;
    let result = BROWSER.with(|browser| match browser.borrow().as_ref() {
        Some(browser) => browser.servo.worker_render_png(full_page),
        None => Err("Servo has not been bootstrapped".to_owned()),
    });
    match result {
        Ok(png) => LAST_FRAME_PNG.with(|slot| {
            let len = png.len() as u32;
            *slot.borrow_mut() = png;
            len
        }),
        Err(error) => {
            LAST_FRAME_PNG.with(|slot| slot.borrow_mut().clear());
            let message = format!("Worker render failed: {error}");
            unsafe { host_log_error(message.as_ptr(), message.len()) };
            0
        },
    }
}

/// Begin a pull-based PNG capture. Each call leaves only its current chunk in
/// the result buffer; the host must pull every strip before requesting finish.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_stream_png_begin(flags: u32) -> u32 {
    let result = BROWSER.with(|browser| match browser.borrow().as_ref() {
        Some(browser) => browser.servo.worker_stream_png_begin(flags & 1 != 0),
        None => Err("Servo has not been bootstrapped".to_owned()),
    });
    store_png_stream_result(result)
}

/// Pull the next strip's IDAT chunk. Returns zero when all image rows have
/// been emitted or on error; call `servo_worker_stream_png_finish` either way.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_stream_png_next() -> u32 {
    let result = BROWSER.with(|browser| match browser.borrow().as_ref() {
        Some(browser) => browser.servo.worker_stream_png_next(),
        None => Err("Servo has not been bootstrapped".to_owned()),
    });
    match result {
        Ok(Some(bytes)) => store_png_stream_result(Ok(bytes)),
        Ok(None) => {
            LAST_FRAME_PNG.with(|slot| slot.borrow_mut().clear());
            0
        },
        Err(error) => store_png_stream_result(Err(error)),
    }
}

/// Finish the compressed stream and write the PNG IEND chunk.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_stream_png_finish() -> u32 {
    let result = BROWSER.with(|browser| match browser.borrow().as_ref() {
        Some(browser) => browser.servo.worker_stream_png_finish(),
        None => Err("Servo has not been bootstrapped".to_owned()),
    });
    store_png_stream_result(result)
}

fn store_png_stream_result(result: Result<Vec<u8>, String>) -> u32 {
    match result {
        Ok(bytes) => LAST_FRAME_PNG.with(|slot| {
            let len = bytes.len() as u32;
            *slot.borrow_mut() = bytes;
            len
        }),
        Err(error) => {
            LAST_FRAME_PNG.with(|slot| slot.borrow_mut().clear());
            let message = format!("Worker PNG stream failed: {error}");
            unsafe { host_log_error(message.as_ptr(), message.len()) };
            0
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_frame_png_ptr() -> *const u8 {
    LAST_FRAME_PNG.with(|slot| slot.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_frame_png_len() -> usize {
    LAST_FRAME_PNG.with(|slot| slot.borrow().len())
}

/// Changes whenever the Worker renderer receives an image or font; compare it
/// before and after a frame to know whether another frame would show more.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_frame_resource_generation() -> u32 {
    BROWSER.with(|browser| {
        browser
            .borrow()
            .as_ref()
            .map_or(0, |browser| browser.servo.worker_resource_generation())
    })
}

/// Diagnostic: describe the captured spatial trees into the frame result
/// buffer (read with `servo_worker_frame_png_ptr/len`). Returns its length.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_frame_describe() -> u32 {
    let text = BROWSER.with(|browser| {
        browser
            .borrow()
            .as_ref()
            .map(|browser| browser.servo.worker_describe_frame())
            .unwrap_or_default()
    });
    LAST_FRAME_PNG.with(|slot| {
        *slot.borrow_mut() = text.into_bytes();
        slot.borrow().len() as u32
    })
}

/// Number of display items captured for the Worker renderer (diagnostic).
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_frame_item_count() -> u32 {
    BROWSER.with(|browser| {
        browser.borrow().as_ref().map_or(0, |browser| {
            browser.servo.worker_captured_item_count() as u32
        })
    })
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

/// Traverse one entry in the current page's session history.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_go_back() -> i32 {
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        if !browser.webview.can_go_back() {
            return 0;
        }
        browser.webview.go_back(1);
        1
    })
}

/// Traverse forward one entry in the current page's session history.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_go_forward() -> i32 {
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        if !browser.webview.can_go_forward() {
            return 0;
        }
        browser.webview.go_forward(1);
        1
    })
}

/// Reload the current page.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_reload() -> i32 {
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        browser.webview.reload();
        1
    })
}

/// Dispatch a pointer move in device pixels.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pointer_move(x: f32, y: f32) -> i32 {
    if !valid_input_point(x, y) {
        return 0;
    }
    with_worker_webview(|webview| {
        webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
            WebViewPoint::Device(DevicePoint::new(x, y)),
        )));
    })
}

/// Dispatch a mouse button event. `action`: 0 down, 1 up; `button` uses the
/// DOM button numbering (0 primary, 1 auxiliary, 2 secondary).
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_mouse_button(action: u32, button: u32, x: f32, y: f32) -> i32 {
    if action > 1 || button > 4 || !valid_input_point(x, y) {
        return 0;
    }
    with_worker_webview(|webview| {
        let action = if action == 0 {
            MouseButtonAction::Down
        } else {
            MouseButtonAction::Up
        };
        webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            action,
            MouseButton::from(button),
            WebViewPoint::Device(DevicePoint::new(x, y)),
        )));
    })
}

/// Dispatch a pixel-mode wheel event at the given device-pixel point.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_scroll_by(delta_x: f64, delta_y: f64, x: f32, y: f32) -> i32 {
    if !valid_input_point(x, y) || !delta_x.is_finite() || !delta_y.is_finite() {
        return 0;
    }
    with_worker_webview(|webview| {
        webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
            WheelDelta {
                x: delta_x,
                y: delta_y,
                z: 0.0,
                mode: WheelMode::DeltaPixel,
            },
            WebViewPoint::Device(DevicePoint::new(x, y)),
        )));
    })
}

/// Dispatch a keyboard key down/up event. The key is a DOM key value such as
/// `a`, `Enter`, or `ArrowDown`; `state`: 0 down, 1 up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_key(
    key_ptr: *const u8,
    key_len: usize,
    state: u32,
    modifiers: u32,
) -> i32 {
    const ALLOWED_MODIFIERS: u32 = 0x249;
    if key_ptr.is_null()
        || key_len == 0
        || key_len > 64
        || state > 1
        || modifiers & !ALLOWED_MODIFIERS != 0
    {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(key_ptr, key_len) };
    let Ok(key_string) = std::str::from_utf8(bytes) else {
        return 0;
    };
    let Ok(key) = Key::from_str(key_string) else {
        return 0;
    };
    with_worker_webview(|webview| {
        webview.focus();
        let state = if state == 0 {
            KeyState::Down
        } else {
            KeyState::Up
        };
        webview.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new_without_event(
            state,
            key,
            Code::Unidentified,
            Location::Standard,
            Modifiers::from_bits_truncate(modifiers),
            false,
            false,
        )));
    })
}

fn valid_input_point(x: f32, y: f32) -> bool {
    x.is_finite() && y.is_finite() && x.abs() <= 1_000_000.0 && y.abs() <= 1_000_000.0
}

fn with_worker_webview(action: impl FnOnce(&WebView)) -> i32 {
    BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return 0;
        };
        action(&browser.webview);
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

/// Start a correlated evaluation in the current document's main realm.
/// Unlike `servo_worker_evaluate_page`, a returned promise (or thenable) is
/// awaited; its fulfillment value or rejection becomes the result. Returns a
/// nonzero evaluation ID, or zero if the script or browser is invalid or too
/// many evaluations are pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_evaluate_page_async(ptr: *const u8, len: usize) -> u32 {
    if ptr.is_null() || len == 0 || len > 4 * 1024 * 1024 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(script) = std::str::from_utf8(bytes) else {
        return 0;
    };
    if PAGE_EVALUATIONS.with(|evaluations| evaluations.borrow().len()) >=
        MAX_PENDING_PAGE_EVALUATIONS
    {
        return 0;
    }
    let Some((sender, receiver)) = generic_channel::channel() else {
        return 0;
    };
    let sent = BROWSER.with(|browser| {
        let binding = browser.borrow();
        let Some(browser) = binding.as_ref() else {
            return false;
        };
        let browsing_context = BrowsingContextId::from(browser.webview.id());
        browser
            .servo
            .execute_webdriver_command(WebDriverCommandMsg::ScriptCommand(
                browsing_context,
                WebDriverScriptCommand::ExecuteScriptWithCallback(script.to_owned(), sender),
            ));
        true
    });
    if !sent {
        return 0;
    }
    let id = NEXT_PAGE_EVALUATION.with(|next| {
        let id = next.get().wrapping_add(1).max(1);
        next.set(id);
        id
    });
    PAGE_EVALUATIONS.with(|evaluations| evaluations.borrow_mut().insert(id, receiver));
    id
}

/// Check a correlated evaluation. Returns 1 when its JSON result is ready in
/// the evaluation result buffer (the evaluation is then retired), 0 while it is
/// pending, and -1 for an unknown, canceled or already retired ID.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_poll_page_evaluation(id: u32) -> i32 {
    let outcome = PAGE_EVALUATIONS.with(|evaluations| {
        let evaluations = evaluations.borrow();
        let receiver = evaluations.get(&id)?;
        Some(match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryReceiveError::Empty) => None,
            // The script thread dropped the reply without answering, e.g.
            // because the document was replaced before the script ran.
            Err(TryReceiveError::ReceiveError(_)) => {
                Some(Err(JavaScriptEvaluationError::WebViewNotReady))
            },
        })
    });
    match outcome {
        None => -1,
        Some(None) => 0,
        Some(Some(result)) => {
            PAGE_EVALUATIONS.with(|evaluations| evaluations.borrow_mut().remove(&id));
            let json = serde_json::to_vec(&result).unwrap_or_else(|_| {
                br#"{"Err":"InternalError"}"#.to_vec()
            });
            PAGE_EVALUATION_RESULT.with(|slot| *slot.borrow_mut() = json);
            1
        },
    }
}

/// Stop waiting for a correlated evaluation. A late reply is discarded.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_cancel_page_evaluation(id: u32) -> i32 {
    PAGE_EVALUATIONS.with(|evaluations| i32::from(evaluations.borrow_mut().remove(&id).is_some()))
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_page_evaluation_result_ptr() -> *const u8 {
    PAGE_EVALUATION_RESULT.with(|result| result.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_page_evaluation_result_len() -> usize {
    PAGE_EVALUATION_RESULT.with(|result| result.borrow().len())
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

/// Limit the interpreter operations (loop iterations, function calls and
/// regexp backtracks) that scripts may run during each pump turn. Once a turn
/// exhausts it, every script in the rest of that turn is terminated
/// uncatchably. Zero removes the limit.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_set_script_budget(operations: u64) {
    let budget = i64::try_from(operations).unwrap_or(i64::MAX);
    SCRIPT_BUDGET.with(|limit| limit.set(if budget == 0 { -1 } else { budget }));
}

/// Number of scripts terminated so far because a pump turn exhausted its
/// operation budget.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_script_budget_terminations() -> u32 {
    u32::try_from(unsafe { mozjs_worker_script_budget_terminations() }).unwrap_or(u32::MAX)
}

fn pump_worker_once() -> (usize, bool) {
    let _budget = ScriptBudgetTurn::begin();
    // A Servo event-loop turn can enqueue a fetch, so pump once before and
    // once after it. The second pass is what makes a newly scheduled request
    // visible to the host without requiring an extra no-op turn.
    pump_worker_services();
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
    pump_worker_services();
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

unsafe fn parse_worker_request_id(ptr: *const u8, len: usize) -> Option<RequestId> {
    if ptr.is_null() || len != 36 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let value = std::str::from_utf8(bytes).ok()?;
    uuid::Uuid::parse_str(value).ok().map(RequestId)
}

fn send_worker_websocket_event(
    request_id: RequestId,
    event: net_traits::WebSocketNetworkEvent,
    terminal: bool,
) -> i32 {
    let callback = WEBSOCKET_CALLBACKS.with(|callbacks| {
        let mut callbacks = callbacks.borrow_mut();
        let callback = callbacks.get(&request_id).cloned();
        if terminal {
            callbacks.remove(&request_id);
        }
        callback
    });
    callback.map_or(0, |callback| i32::from(callback.send(event).is_ok()))
}

/// Complete the Worker's host-side WebSocket handshake.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_websocket_open(
    id_ptr: *const u8,
    id_len: usize,
    protocol_ptr: *const u8,
    protocol_len: usize,
) -> i32 {
    let Some(request_id) = (unsafe { parse_worker_request_id(id_ptr, id_len) }) else {
        return 0;
    };
    if protocol_ptr.is_null() && protocol_len != 0 {
        return 0;
    }
    let protocol = if protocol_len == 0 {
        None
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(protocol_ptr, protocol_len) };
        match std::str::from_utf8(bytes) {
            Ok(protocol) if !protocol.is_empty() => Some(protocol.to_owned()),
            _ => return 0,
        }
    };
    i32::from(
        send_worker_websocket_event(
            request_id,
            net_traits::WebSocketNetworkEvent::ConnectionEstablished {
                protocol_in_use: protocol,
            },
            false,
        ) != 0,
    )
}

/// Deliver a text or binary message from the Worker WebSocket host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_websocket_message(
    id_ptr: *const u8,
    id_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    is_text: u32,
) -> i32 {
    let Some(request_id) = (unsafe { parse_worker_request_id(id_ptr, id_len) }) else {
        return 0;
    };
    if is_text > 1 || (data_ptr.is_null() && data_len != 0) {
        return 0;
    }
    let bytes = if data_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data_ptr, data_len) }
    };
    let data = if is_text == 1 {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return 0;
        };
        net_traits::MessageData::Text(text.to_owned())
    } else {
        net_traits::MessageData::Binary(bytes.to_vec())
    };
    i32::from(
        send_worker_websocket_event(
            request_id,
            net_traits::WebSocketNetworkEvent::MessageReceived(data),
            false,
        ) != 0,
    )
}

/// Deliver a close or handshake error from the Worker WebSocket host.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_websocket_close(
    id_ptr: *const u8,
    id_len: usize,
    code: u32,
    reason_ptr: *const u8,
    reason_len: usize,
    failed: u32,
) -> i32 {
    let Some(request_id) = (unsafe { parse_worker_request_id(id_ptr, id_len) }) else {
        return 0;
    };
    if (reason_ptr.is_null() && reason_len != 0)
        || failed > 1
        || (code > u16::MAX as u32 && code != u32::MAX)
    {
        return 0;
    }
    let reason = if reason_len == 0 {
        String::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(reason_ptr, reason_len) };
        let Ok(reason) = std::str::from_utf8(bytes) else {
            return 0;
        };
        reason.to_owned()
    };
    let event = if failed == 1 {
        net_traits::WebSocketNetworkEvent::Fail
    } else {
        net_traits::WebSocketNetworkEvent::Close((code != u32::MAX).then_some(code as u16), reason)
    };
    i32::from(send_worker_websocket_event(request_id, event, true) != 0)
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
            let phase = servo_base::worker_trace::get();
            let message = if phase.is_empty() {
                info.to_string()
            } else {
                format!("{info} (during: {phase})")
            };
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
        attach_worker_cookies(&mut request);
        queue_worker_fetch(request, callback);
    }));

    let handler: WorkerFetchHandler = Box::new(|mut request, redirect, channels| {
        apply_worker_redirect(&mut request, redirect);
        match channels {
            net_traits::FetchChannels::ResponseMsg(callback) => {
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
                let payload = encode_host_command(WorkerHostCommand::Fetch {
                    request: WorkerFetchRequest::from_request(&request),
                });
                let accepts_cookies = servo::worker_request_accepts_cookies(&request);
                FETCH_CALLBACKS.with(|callbacks| {
                    callbacks.borrow_mut().insert(
                        request_id,
                        WorkerFetchEntry {
                            callback,
                            visibility,
                            accepts_cookies,
                            cookie_origin: request.url.origin(),
                            response_started: false,
                        },
                    );
                });
                dispatch_worker_fetch(request_id, payload);
            },
            net_traits::FetchChannels::WebSocket {
                event_sender,
                action_receiver,
            } => {
                let request_id = request.id;
                let (url, protocols) = match &request.mode {
                    RequestMode::WebSocket {
                        protocols,
                        original_url,
                    } => (original_url.as_str().to_owned(), protocols.clone()),
                    _ => {
                        let _ = event_sender.send(net_traits::WebSocketNetworkEvent::Fail);
                        return;
                    },
                };
                WEBSOCKET_CALLBACKS.with(|callbacks| {
                    callbacks.borrow_mut().insert(request_id, event_sender);
                });
                action_receiver.set_callback(move |action| {
                    if let Ok(action) = action {
                        let payload = encode_host_command(WorkerHostCommand::WebSocketAction {
                            request_id: request_id.0.to_string(),
                            action,
                        });
                        match payload {
                            Ok(payload) => unsafe {
                                host_fetch_request(payload.as_ptr(), payload.len());
                            },
                            Err(error) => worker_log(&format!(
                                "Worker WebSocket action serialization failed: {error}"
                            )),
                        }
                    }
                });
                let payload = encode_host_command(WorkerHostCommand::WebSocketConnect {
                    request_id: request_id.0.to_string(),
                    url,
                    protocols,
                });
                if let Ok(payload) = payload {
                    unsafe { host_fetch_request(payload.as_ptr(), payload.len()) };
                } else {
                    send_worker_websocket_event(
                        request_id,
                        net_traits::WebSocketNetworkEvent::Fail,
                        true,
                    );
                }
            },
            net_traits::FetchChannels::Prefetch => {},
        }
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
    let payload = encode_host_command(WorkerHostCommand::Fetch {
        request: WorkerFetchRequest::from_request(&request),
    });
    let accepts_cookies = servo::worker_request_accepts_cookies(&request);
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
                accepts_cookies,
                cookie_origin: request.url.origin(),
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
    WEBSOCKET_CALLBACKS.with(|callbacks| callbacks.borrow_mut().clear());
    LAST_PAGE_RESULT.with(|result| result.borrow_mut().clear());
    PAGE_EVALUATIONS.with(|evaluations| evaluations.borrow_mut().clear());
    PAGE_EVALUATION_RESULT.with(|result| result.borrow_mut().clear());
    i32::from(existed)
}

/// Report whether the host still needs to deliver a fetch response.
#[unsafe(no_mangle)]
pub extern "C" fn servo_worker_pending_fetch_count() -> usize {
    FETCH_CALLBACKS.with(|callbacks| callbacks.borrow().len())
}

/// Incorporate redirect cookies before the Worker issues the next hop and
/// return the Cookie header for that hop. The host must pass one bounded JSON
/// object and a writable output buffer; response headers are not page-visible.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn servo_worker_process_redirect_cookies(
    payload_ptr: *const u8,
    payload_len: usize,
    output_ptr: *mut u8,
    output_cap: usize,
) -> i32 {
    if payload_ptr.is_null()
        || payload_len == 0
        || payload_len > 64 * 1024
        || output_ptr.is_null()
        || output_cap == 0
        || output_cap > 64 * 1024
    {
        return -1;
    }
    let payload = unsafe { std::slice::from_raw_parts(payload_ptr, payload_len) };
    let Ok(command): Result<WorkerRedirectCookies, _> = serde_json::from_slice(payload) else {
        return -1;
    };
    let (Ok(response_url), Ok(next_url)) = (
        ServoUrl::parse(&command.response_url),
        ServoUrl::parse(&command.next_url),
    ) else {
        return -1;
    };
    FETCH_CALLBACKS.with(|callbacks| {
        let callbacks = callbacks.borrow();
        let Some(entry) = callbacks.get(&RequestId(command.request_id)) else {
            return -1;
        };
        if entry.response_started {
            return -1;
        }
        if !entry.accepts_cookies
            || response_url.origin() != entry.cookie_origin
            || next_url.origin() != entry.cookie_origin
        {
            return 0;
        }
        for cookie in &command.set_cookies {
            servo::set_worker_cookie_from_header(&response_url, cookie);
        }
        let Some(header) = servo::worker_cookie_header_for_url(&next_url) else {
            return 0;
        };
        let bytes = header.as_bytes();
        if bytes.len() > output_cap {
            return -1;
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), output_ptr, bytes.len()) };
        bytes.len() as i32
    })
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
    let mut set_cookies = Vec::new();
    for (name, value) in pairs {
        if name.eq_ignore_ascii_case("set-cookie") {
            // Set-Cookie is consumed by the browser's cookie store and must
            // not become visible through the page's Response headers.
            set_cookies.push(value);
            continue;
        }
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
    let mut metadata = Metadata::default(url.clone());
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
                if entry.accepts_cookies {
                    for cookie in &set_cookies {
                        servo::set_worker_cookie_from_header(&url, cookie);
                    }
                }
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
