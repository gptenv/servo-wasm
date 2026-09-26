/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

pub mod async_runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod connector;
pub mod cookie;
pub mod cookie_storage;
#[cfg(not(target_arch = "wasm32"))]
mod decoder;
#[cfg(not(target_arch = "wasm32"))]
mod devtools;
#[cfg(not(target_arch = "wasm32"))]
mod disk_cache;
pub mod embedder;
#[cfg(not(target_arch = "wasm32"))]
pub mod filemanager_thread;
#[cfg(not(target_arch = "wasm32"))]
mod hosts;
pub mod hsts;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_cache;
#[cfg(not(target_arch = "wasm32"))]
pub mod http_loader;
pub mod image_cache;
#[cfg(not(target_arch = "wasm32"))]
pub mod local_directory_listing;
#[cfg(not(target_arch = "wasm32"))]
pub mod protocols;
#[cfg(target_arch = "wasm32")]
pub mod protocols {
    use servo_url::ServoUrl;

    /// Worker-side protocol registry. Network loading is supplied by the
    /// host's `fetch()` adapter; custom native protocol handlers are not
    /// available in the wasm build yet.
    #[derive(Default)]
    pub struct ProtocolRegistry;

    impl ProtocolRegistry {
        pub fn with_internal_protocols() -> Self {
            Self
        }

        pub fn merge(&mut self, _other: Self) {}

        pub fn privileged_urls(&self) -> Vec<ServoUrl> {
            Vec::new()
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub mod request_interceptor;
#[cfg(not(target_arch = "wasm32"))]
pub mod resource_thread;
#[cfg(target_arch = "wasm32")]
pub mod resource_thread {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crossbeam_channel::Sender;
    use http::{HeaderValue, header};
    use net_traits::request::{CredentialsMode, Origin, RequestBuilder};
    use net_traits::response::ResponseInit;
    use net_traits::{AsyncRuntime, CoreResourceMsg, FetchChannels, ResourceThreads};
    use profile_traits::mem::ProfilerChan;
    use profile_traits::time::ProfilerChan as TimeProfilerChan;
    use servo_base::generic_channel::{GenericReceiver, channel};

    /// Host callback used by the wasm build to implement network loading.
    ///
    /// The callback receives Servo's normal fetch request and response-channel
    /// contract. A Worker adapter can translate the request to `fetch()` and
    /// send `FetchResponseMsg` values through the supplied channels.
    pub type WorkerFetchHandler =
        Box<dyn FnMut(RequestBuilder, Option<ResponseInit>, FetchChannels)>;

    thread_local! {
        static RESOURCE_RECEIVER: RefCell<Option<GenericReceiver<CoreResourceMsg>>> =
            const { RefCell::new(None) };
        static FETCH_HANDLER: RefCell<Option<WorkerFetchHandler>> =
            const { RefCell::new(None) };
        /// The Worker's cookie jar: in memory, for the life of the instance.
        static COOKIES: RefCell<crate::cookie_storage::CookieStorage> =
            RefCell::new(crate::cookie_storage::CookieStorage::new(150));
        /// Serialized `history.pushState()` data, as the native resource
        /// thread keeps it, for the life of the instance.
        static HISTORY_STATES: RefCell<HashMap<servo_base::id::HistoryStateId, Vec<u8>>> =
            RefCell::new(HashMap::new());
    }

    fn set_cookie(
        url: &servo_url::ServoUrl,
        cookie: cookie::Cookie<'static>,
        source: net_traits::CookieSource,
    ) {
        if let Some(cookie) = crate::cookie::ServoCookie::new_wrapped(cookie, url, source) {
            COOKIES.with(|jar| jar.borrow_mut().push(cookie, url, source));
        }
    }

    /// Store a `Set-Cookie` value received by the Worker fetch adapter.
    pub fn set_worker_cookie_from_header(url: &servo_url::ServoUrl, value: &str) {
        if let Ok(cookie) = cookie::Cookie::parse(value.to_owned()) {
            set_cookie(url, cookie.into_owned(), net_traits::CookieSource::HTTP);
        }
    }

    pub fn worker_cookie_header_for_url(url: &servo_url::ServoUrl) -> Option<String> {
        COOKIES.with(|jar| {
            let mut jar = jar.borrow_mut();
            jar.remove_expired_cookies_for_url(url);
            jar.cookies_for_url(url, net_traits::CookieSource::HTTP)
        })
    }

    pub fn worker_request_accepts_cookies(request: &RequestBuilder) -> bool {
        let origin = match &request.origin {
            Origin::Origin(origin) => Some(origin),
            Origin::Client => request.client.as_ref().and_then(|client| match &client.origin {
                Origin::Origin(origin) => Some(origin),
                Origin::Client => None,
            }),
        };
        match request.credentials_mode {
            CredentialsMode::Omit => false,
            CredentialsMode::CredentialsSameOrigin => {
                origin.is_some_and(|origin| *origin == request.url.origin())
            },
            CredentialsMode::Include => true,
        }
    }

    pub fn attach_worker_cookies(request: &mut RequestBuilder) {
        // Never pass a preexisting Cookie header to the privileged host fetch.
        // The request's credentials mode and resolved client origin control it.
        request.headers.remove(header::COOKIE);
        if !worker_request_accepts_cookies(request) {
            return;
        }

        let url = request.url.url();
        let cookie_header = worker_cookie_header_for_url(&url);
        if let Some(cookie_header) = cookie_header
            && let Ok(cookie_header) = HeaderValue::from_bytes(cookie_header.as_bytes())
        {
            request.headers.insert(header::COOKIE, cookie_header);
        }
    }

    /// Install the host-side implementation of network fetching.
    pub fn set_worker_fetch_handler(handler: WorkerFetchHandler) {
        FETCH_HANDLER.with(|slot| *slot.borrow_mut() = Some(handler));
    }

    /// Pump queued resource messages from Servo into the installed Worker
    /// fetch handler. The Worker integration should call this between event
    /// turns; it never blocks.
    pub fn pump_worker_fetches() -> usize {
        let mut processed = 0;
        loop {
            let message = RESOURCE_RECEIVER.with(|receiver| {
                receiver
                    .borrow()
                    .as_ref()
                    .and_then(|receiver| receiver.try_recv().ok())
            });
            let Some(message) = message else { break };

            match message {
                CoreResourceMsg::Fetch(mut request, channels) => {
                    attach_worker_cookies(&mut request);
                    FETCH_HANDLER.with(|handler| {
                        if let Some(handler) = handler.borrow_mut().as_mut() {
                            handler(request, None, channels);
                        }
                    });
                },
                CoreResourceMsg::FetchRedirect(mut request, response, callback) => {
                    attach_worker_cookies(&mut request);
                    FETCH_HANDLER.with(|handler| {
                        if let Some(handler) = handler.borrow_mut().as_mut() {
                            handler(
                                request,
                                Some(response),
                                FetchChannels::ResponseMsg(callback),
                            );
                        }
                    });
                },
                CoreResourceMsg::Cancel(_) => {},
                CoreResourceMsg::SetCookieForUrl(url, cookie, source, sender) => {
                    set_cookie(&url, cookie.into_inner().to_owned(), source);
                    if let Some(sender) = sender {
                        let _ = sender.send(());
                    }
                },
                CoreResourceMsg::SetCookiesForUrl(url, cookies, source) => {
                    for cookie in cookies {
                        set_cookie(&url, cookie.into_inner(), source);
                    }
                },
                CoreResourceMsg::GetCookieStringForUrl(url, sender, source) => {
                    let cookies = COOKIES.with(|jar| {
                        let mut jar = jar.borrow_mut();
                        jar.remove_expired_cookies_for_url(&url);
                        jar.cookies_for_url(&url, source)
                    });
                    let _ = sender.send(cookies);
                },
                CoreResourceMsg::GetCookiesForUrl(url, sender, source) => {
                    let cookies = COOKIES.with(|jar| {
                        let mut jar = jar.borrow_mut();
                        jar.remove_expired_cookies_for_url(&url);
                        jar.cookies_data_for_url(&url, source)
                            .map(hyper_serde::Serde)
                            .collect()
                    });
                    let _ = sender.send(cookies);
                },
                CoreResourceMsg::DeleteCookies(url, sender) => {
                    COOKIES.with(|jar| jar.borrow_mut().clear_storage(url.as_ref()));
                    if let Some(sender) = sender {
                        let _ = sender.send(());
                    }
                },
                CoreResourceMsg::DeleteCookie(url, name) => {
                    COOKIES.with(|jar| jar.borrow_mut().delete_cookie_with_name(&url, name));
                },
                // Keepalive requests are sent as ordinary host fetches, which
                // the Worker does not keep past the invocation.
                CoreResourceMsg::TotalSizeOfInFlightKeepAliveRecords(_, sender) => {
                    let _ = sender.send(0);
                },
                CoreResourceMsg::DeleteSessionCookies(sender) => {
                    COOKIES.with(|jar| jar.borrow_mut().clear_session_cookies());
                    let _ = sender.send(());
                },
                CoreResourceMsg::GetHistoryState(history_state_id, sender) => {
                    let state = HISTORY_STATES
                        .with(|states| states.borrow().get(&history_state_id).cloned());
                    let _ = sender.send(state);
                },
                CoreResourceMsg::SetHistoryState(history_state_id, structured_data) => {
                    HISTORY_STATES.with(|states| {
                        states.borrow_mut().insert(history_state_id, structured_data)
                    });
                },
                CoreResourceMsg::RemoveHistoryStates(states_to_remove) => {
                    HISTORY_STATES.with(|states| {
                        let mut states = states.borrow_mut();
                        for history_state_id in states_to_remove {
                            states.remove(&history_state_id);
                        }
                    });
                },
                // Anything else is unsupported on the Worker. Dropping the
                // message drops any reply sender, so a caller waiting on it
                // gets an error instead of waiting forever.
                other => log::debug!("Unsupported Worker resource message: {other:?}"),
            }
            processed += 1;
        }
        processed
    }

    /// Placeholder until the Worker `fetch()` adapter owns resource loading.
    pub fn new_resource_threads(
        _devtools_sender: Option<Sender<devtools_traits::DevtoolsControlMsg>>,
        _time_profiler_chan: TimeProfilerChan,
        _mem_profiler_chan: ProfilerChan,
        _net_embedder_proxy: impl Send + 'static,
        _config_dir: Option<std::path::PathBuf>,
        _certificate_path: Option<String>,
        _ignore_certificate_errors: bool,
        _protocols: Arc<super::protocols::ProtocolRegistry>,
    ) -> (ResourceThreads, ResourceThreads, Box<dyn AsyncRuntime>) {
        let (sender, receiver) = channel().expect("create Worker resource channel");
        RESOURCE_RECEIVER.with(|slot| *slot.borrow_mut() = Some(receiver));
        net_traits::set_worker_resource_pump(Box::new(|| {
            pump_worker_fetches();
        }));
        servo_base::worker_services::register(Box::new(|| {
            pump_worker_fetches();
        }));
        let public = ResourceThreads::new(sender.clone());
        let private = ResourceThreads::new(sender);
        (public, private, super::async_runtime::init_async_runtime())
    }
}
pub mod subresource_integrity;
#[cfg(feature = "test-util")]
pub mod test_util;
#[cfg(not(target_arch = "wasm32"))]
mod websocket_loader;

/// An implementation of the [Fetch specification](https://fetch.spec.whatwg.org/)
#[cfg(not(target_arch = "wasm32"))]
pub mod fetch {
    pub mod cors_cache;
    pub mod fetch_params;
    pub mod headers;
    pub mod methods;
}

/// A module for re-exports of items used in unit tests.
#[cfg(not(target_arch = "wasm32"))]
pub mod test {
    pub use crate::decoder::{BodyStreamError, DECODER_BUFFER_SIZE, map_decode_error};
    pub use crate::hosts::parse_hostsfile;
    pub use crate::http_loader::HttpState;
}
