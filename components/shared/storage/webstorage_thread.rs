/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use malloc_size_of_derive::MallocSizeOf;
use profile_traits::mem::ReportsChan;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericSender;
use servo_base::id::WebViewId;
use servo_url::ImmutableOrigin;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, MallocSizeOf, Serialize)]
pub enum WebStorageType {
    Session,
    Local,
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct OriginDescriptor {
    pub name: String,
}

impl OriginDescriptor {
    pub fn new(name: String) -> Self {
        OriginDescriptor { name }
    }
}

/// Request operations on the storage data associated with a particular url
#[derive(Debug, Deserialize, Serialize)]
pub enum WebStorageThreadMsg {
    /// gets the number of key/value pairs present in the associated storage data
    Length(
        GenericSender<usize>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the name of the key at the specified index in the associated storage data
    Key(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        u32,
    ),

    /// Gets the available keys in the associated storage data
    Keys(
        GenericSender<Vec<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the value associated with the given key in the associated storage data
    GetItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// sets the value of the given key in the associated storage data
    SetItem(
        GenericSender<Result<(bool, Option<String>), ()>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
        String,
    ),

    /// removes the key/value pair for the given key in the associated storage data
    RemoveItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// clears the associated storage data by removing all the key/value pairs
    Clear(
        GenericSender<bool>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// clones all storage data of the given top-level browsing context for a new browsing context.
    /// should only be used for sessionStorage.
    Clone {
        sender: GenericSender<()>,
        src: WebViewId,
        dest: WebViewId,
    },

    /// gets the list of origin descriptors for given storage type
    ///
    /// TODO: Consider returning `Vec<SiteDescriptor>`
    ListOrigins(GenericSender<Vec<OriginDescriptor>>, WebStorageType),

    /// clears storage data for given storage type and sites, affecting all matching origins
    ClearDataForSites(GenericSender<()>, WebStorageType, Vec<String>),

    /// send a reply when done cleaning up thread resources and then shut it down
    Exit(GenericSender<()>),

    /// Measure memory used by this thread and send the report over the provided channel.
    CollectMemoryReport(ReportsChan),
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static WORKER_WEBSTORAGE_PUMPS: std::cell::RefCell<Vec<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Register a Worker WASM web-storage service that runs in-process instead of
/// on its own thread. It must handle every message already queued when called.
#[cfg(target_arch = "wasm32")]
pub fn register_worker_webstorage_pump(pump: Box<dyn FnMut()>) {
    WORKER_WEBSTORAGE_PUMPS.with(|pumps| pumps.borrow_mut().push(pump));
}

/// Run queued web-storage messages so their replies exist. Script calls this
/// after sending, before blocking on a reply; on native targets the storage
/// thread handles messages concurrently and this does nothing.
pub fn process_worker_webstorage() {
    #[cfg(target_arch = "wasm32")]
    WORKER_WEBSTORAGE_PUMPS.with(|pumps| {
        for pump in pumps.borrow_mut().iter_mut() {
            pump();
        }
    });
}
