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
    use std::sync::Arc;

    use crossbeam_channel::Sender;
    use net_traits::request::RequestBuilder;
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
                CoreResourceMsg::Fetch(request, channels) => {
                    FETCH_HANDLER.with(|handler| {
                        if let Some(handler) = handler.borrow_mut().as_mut() {
                            handler(request, None, channels);
                        }
                    });
                },
                CoreResourceMsg::FetchRedirect(request, response, callback) => {
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
                _ => {},
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
