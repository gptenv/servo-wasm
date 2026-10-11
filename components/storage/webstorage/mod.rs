/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod engines;

use std::borrow::ToOwned;
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use log::warn;
use malloc_size_of::MallocSizeOf;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::pub_domains::registered_domain_name;
use profile_traits::mem::{
    ProcessReports, ProfilerChan as MemProfilerChan, Report, ReportKind, perform_memory_report,
};
use profile_traits::path;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use servo_base::id::WebViewId;
use servo_base::threadpool::ThreadPool;
use servo_base::{read_json_from_file, write_json_to_file};
use servo_url::{ImmutableOrigin, ServoUrl};
use storage_traits::webstorage_thread::{OriginDescriptor, WebStorageThreadMsg, WebStorageType};
use uuid::Uuid;

use crate::webstorage::engines::WebStorageEngine;
use crate::webstorage::engines::sqlite::SqliteEngine;

const QUOTA_SIZE_LIMIT: usize = 5 * 1024 * 1024;

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Every Worker WASM web-storage manager, in creation order. Snapshot and
    /// restore visit all of them so no storage group is missed; the Worker
    /// port runs a single browsing context, so exactly one holds live data.
    static WORKER_WEBSTORAGE_MANAGERS: RefCell<Vec<Rc<RefCell<WebStorageManager>>>> =
        const { RefCell::new(Vec::new()) };
}

/// Postcard-encoded by the Worker WASM web-storage snapshot exports: one
/// storage area's complete key/value data, grouped by ascii-serialized origin.
/// Session data is merged across webviews; the Worker port runs a single
/// webview per instance, so the merge is exact in practice.
#[cfg(target_arch = "wasm32")]
#[derive(Deserialize, Serialize)]
struct WorkerWebStorageSnapshot {
    version: u32,
    origins: Vec<(String, Vec<(String, String)>)>,
}

#[cfg(target_arch = "wasm32")]
const WORKER_WEBSTORAGE_SNAPSHOT_VERSION: u32 = 1;

/// Map a snapshot `kind` to its storage area: 0 is localStorage, 1 is
/// sessionStorage. This matches the `kind` parameter of the
/// `servo_worker_storage_state_*` exports.
#[cfg(target_arch = "wasm32")]
fn worker_webstorage_kind(kind: u32) -> Result<WebStorageType, String> {
    match kind {
        0 => Ok(WebStorageType::Local),
        1 => Ok(WebStorageType::Session),
        _ => Err(format!("unknown web-storage snapshot kind {kind}")),
    }
}

#[cfg(target_arch = "wasm32")]
fn parse_snapshot_origin(ascii: &str) -> Result<ImmutableOrigin, String> {
    ServoUrl::parse(ascii)
        .map(|url| url.origin())
        .map_err(|_| format!("invalid origin in web-storage snapshot: {ascii}"))
}

/// Serialize one Worker web-storage area for snapshotting across WASM
/// instance recycles. Queries every registered Worker web-storage manager and
/// merges per origin, first writer wins.
#[cfg(target_arch = "wasm32")]
pub fn serialize_worker_webstorage(kind: u32) -> Result<Vec<u8>, String> {
    let storage_type = worker_webstorage_kind(kind)?;
    let mut merged: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut visited = false;
    WORKER_WEBSTORAGE_MANAGERS.with(|managers| {
        for manager in managers.borrow().iter() {
            let manager = manager
                .try_borrow()
                .map_err(|_| "web-storage manager is busy".to_owned())?;
            visited = true;
            for (origin, pairs) in manager.snapshot_storage(storage_type) {
                merged.entry(origin).or_insert(pairs);
            }
        }
        Ok::<_, String>(())
    })?;
    if !visited {
        return Err("no Worker web-storage manager registered".to_owned());
    }
    let snapshot = WorkerWebStorageSnapshot {
        version: WORKER_WEBSTORAGE_SNAPSHOT_VERSION,
        origins: merged.into_iter().collect(),
    };
    postcard::to_stdvec(&snapshot).map_err(|error| error.to_string())
}

/// Replace one Worker web-storage area from a snapshot previously produced by
/// `serialize_worker_webstorage`. Like the cookie-jar restore, this replaces
/// the complete area. Session restore targets `webview` and requires `Some`;
/// local restore ignores it. The snapshot is applied to every registered
/// Worker web-storage manager.
#[cfg(target_arch = "wasm32")]
pub fn restore_worker_webstorage(
    kind: u32,
    webview: Option<WebViewId>,
    bytes: &[u8],
) -> Result<(), String> {
    let storage_type = worker_webstorage_kind(kind)?;
    let snapshot: WorkerWebStorageSnapshot =
        postcard::from_bytes(bytes).map_err(|error| error.to_string())?;
    if snapshot.version != WORKER_WEBSTORAGE_SNAPSHOT_VERSION {
        return Err(format!(
            "unsupported web-storage snapshot version {}",
            snapshot.version
        ));
    }
    if matches!(storage_type, WebStorageType::Session) && webview.is_none() {
        return Err("session storage restore requires a webview id".to_owned());
    }
    let mut restored = false;
    WORKER_WEBSTORAGE_MANAGERS.with(|managers| {
        for manager in managers.borrow().iter() {
            let mut manager = manager
                .try_borrow_mut()
                .map_err(|_| "web-storage manager is busy".to_owned())?;
            manager.restore_storage(storage_type, webview, &snapshot)?;
            restored = true;
        }
        Ok::<_, String>(())
    })?;
    if !restored {
        return Err("no Worker web-storage manager registered".to_owned());
    }
    Ok(())
}

pub trait WebStorageThreadFactory {
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
    ) -> Self;
}

impl WebStorageThreadFactory for GenericSender<WebStorageThreadMsg> {
    /// Create a storage thread
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
    ) -> GenericSender<WebStorageThreadMsg> {
        let (chan, port) = generic_channel::channel().unwrap();
        let chan2 = chan.clone();
        thread::Builder::new()
            .name("WebStorageManager".to_owned())
            .spawn(move || {
                mem_profiler_chan.run_with_memory_reporting(
                    || WebStorageManager::new(port, config_dir).start(),
                    reporter_name,
                    chan2,
                    WebStorageThreadMsg::CollectMemoryReport,
                );
            })
            .expect("Thread spawning failed");
        chan
    }
}

/// Worker WASM web storage: the same manager, with in-memory SQLite, driven on
/// the script thread through `storage_traits::webstorage_thread` instead of a
/// spawned thread. Data lives only as long as the WASM instance.
#[cfg(target_arch = "wasm32")]
pub fn new_worker_webstorage() -> GenericSender<WebStorageThreadMsg> {
    let (chan, port) = generic_channel::channel().unwrap();
    // Shared with the snapshot/restore exports below: the pump closure owns
    // one clone, the registry owns the other. Everything runs on the single
    // Worker thread, so RefCell aliasing cannot cross threads.
    let manager = Rc::new(RefCell::new(WebStorageManager::new(port, None)));
    WORKER_WEBSTORAGE_MANAGERS.with(|managers| managers.borrow_mut().push(manager.clone()));
    let mut running = true;
    let pump_manager = manager.clone();
    storage_traits::webstorage_thread::register_worker_webstorage_pump(Box::new(move || {
        let Ok(mut manager) = pump_manager.try_borrow_mut() else {
            // A snapshot or restore holds the manager; skip this pump turn.
            return;
        };
        while running {
            let Ok(message) = manager.port.try_recv() else {
                break;
            };
            running = manager.handle_message(message);
        }
    }));
    servo_base::worker_services::register(Box::new(
        storage_traits::webstorage_thread::process_worker_webstorage,
    ));
    chan
}

#[derive(Deserialize, MallocSizeOf, Serialize)]
pub struct StorageOrigins {
    // TODO: Consider grouping by eTLD+1
    // TODO: Consider ImmutableOrigin instead of String for tracking origins
    origin_descriptors: FxHashMap<String, OriginDescriptor>,
}

impl StorageOrigins {
    fn new() -> Self {
        StorageOrigins {
            origin_descriptors: FxHashMap::default(),
        }
    }

    /// Ensures that an origin descriptor exists for the given origin.
    ///
    /// Returns `true` if a new origin descriptor was created, or `false` if
    /// one already existed.
    fn ensure_origin_descriptor(&mut self, origin: &ImmutableOrigin) -> bool {
        let origin = origin.ascii_serialization().into_owned();
        match self.origin_descriptors.entry(origin.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(OriginDescriptor::new(origin));
                true
            },
        }
    }

    fn origin_descriptors(&self) -> Vec<OriginDescriptor> {
        self.origin_descriptors.values().cloned().collect()
    }

    fn take_origins_for_sites(&mut self, sites: &[String]) -> Vec<ImmutableOrigin> {
        // TODO: This can use `extract_if` once MSVR is bumbed (>=1.88)

        let mut result = Vec::new();

        self.origin_descriptors.retain(|_, descriptor| {
            let url =
                ServoUrl::parse(&descriptor.name).expect("Should always be able to parse origins.");

            let Some(domain) = registered_domain_name(&url) else {
                warn!("Failed to get a registered domain name for: {url}");
                return true;
            };
            let domain = domain.to_string();

            if sites.contains(&domain) {
                result.push(url.origin());
                false
            } else {
                true
            }
        });

        result
    }
}

#[derive(Clone, Default, MallocSizeOf)]
pub struct OriginEntry {
    tree: BTreeMap<String, String>,
    size: usize,
}

impl OriginEntry {
    pub fn inner(&self) -> &BTreeMap<String, String> {
        &self.tree
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        let old_value = self.tree.insert(key.clone(), value.clone());
        let size_change = match &old_value {
            Some(old) => value.len() as isize - old.len() as isize,
            None => (key.len() + value.len()) as isize,
        };
        self.size = (self.size as isize + size_change) as usize;
        old_value
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        let old_value = self.tree.remove(key);
        if let Some(old) = &old_value {
            self.size -= key.len() + old.len();
        }
        old_value
    }

    pub fn clear(&mut self) {
        self.tree.clear();
        self.size = 0;
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

struct WebStorageEnvironment<E: WebStorageEngine> {
    engine: E,
    data: OriginEntry,
}

impl<E: WebStorageEngine> MallocSizeOf for WebStorageEnvironment<E> {
    fn size_of(&self, ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        self.data.size_of(ops)
    }
}

impl<E: WebStorageEngine> WebStorageEnvironment<E> {
    fn new(engine: E) -> Self {
        WebStorageEnvironment {
            data: engine.load().unwrap_or_default(),
            engine,
        }
    }

    fn clear(&mut self) {
        self.data.clear();
        let _ = self.engine.clear();
    }

    fn delete(&mut self, key: &str) {
        let _ = self.engine.delete(key);
    }

    fn set(&mut self, key: &str, value: &str) {
        let _ = self.engine.set(key, value);
    }
}

impl<E: WebStorageEngine> Drop for WebStorageEnvironment<E> {
    fn drop(&mut self) {
        self.engine.save(&self.data);
    }
}

struct WebStorageManager {
    port: GenericReceiver<WebStorageThreadMsg>,
    session_storage_origins: StorageOrigins,
    local_storage_origins: StorageOrigins,
    session_data: FxHashMap<WebViewId, FxHashMap<ImmutableOrigin, OriginEntry>>,
    config_dir: Option<PathBuf>,
    thread_pool: Arc<ThreadPool>,
    environments: FxHashMap<ImmutableOrigin, WebStorageEnvironment<SqliteEngine>>,
}

impl WebStorageManager {
    fn new(
        port: GenericReceiver<WebStorageThreadMsg>,
        config_dir: Option<PathBuf>,
    ) -> WebStorageManager {
        let mut local_storage_origins = StorageOrigins::new();
        if let Some(ref config_dir) = config_dir {
            read_json_from_file(&mut local_storage_origins, config_dir, "localstorage.json");
        }
        WebStorageManager {
            port,
            session_storage_origins: StorageOrigins::new(),
            local_storage_origins,
            session_data: FxHashMap::default(),
            config_dir,
            thread_pool: ThreadPool::global(),
            environments: FxHashMap::default(),
        }
    }
}

impl WebStorageManager {
    fn start(&mut self) {
        loop {
            let message = self.port.recv().unwrap();
            if !self.handle_message(message) {
                break;
            }
        }
    }

    /// Handle one message; returns false once the manager should stop.
    fn handle_message(&mut self, message: WebStorageThreadMsg) -> bool {
        {
            match message {
                WebStorageThreadMsg::Length(sender, storage_type, webview_id, url) => {
                    self.length(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::Key(sender, storage_type, webview_id, url, index) => {
                    self.key(sender, storage_type, webview_id, url, index)
                },
                WebStorageThreadMsg::Keys(sender, storage_type, webview_id, url) => {
                    self.keys(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::SetItem(
                    sender,
                    storage_type,
                    webview_id,
                    url,
                    name,
                    value,
                ) => {
                    self.set_item(sender, storage_type, webview_id, url, name, value);
                },
                WebStorageThreadMsg::GetItem(sender, storage_type, webview_id, url, name) => {
                    self.request_item(sender, storage_type, webview_id, url, name)
                },
                WebStorageThreadMsg::RemoveItem(sender, storage_type, webview_id, url, name) => {
                    self.remove_item(sender, storage_type, webview_id, url, name);
                },
                WebStorageThreadMsg::Clear(sender, storage_type, webview_id, url) => {
                    self.clear(sender, storage_type, webview_id, url);
                },
                WebStorageThreadMsg::Clone {
                    sender,
                    src: src_webview_id,
                    dest: dest_webview_id,
                } => {
                    self.clone(src_webview_id, dest_webview_id);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::ListOrigins(sender, storage_type) => {
                    let _ = sender.send(self.origin_descriptors(storage_type));
                },
                WebStorageThreadMsg::ClearDataForSites(sender, storage_type, sites) => {
                    self.clear_data_for_sites(storage_type, &sites);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::CollectMemoryReport(sender) => {
                    let reports = self.collect_memory_reports();
                    sender.send(ProcessReports::new(reports));
                },
                WebStorageThreadMsg::Exit(sender) => {
                    // Nothing to do since we save localstorage set eagerly.
                    let _ = sender.send(());
                    return false;
                },
            }
        }
        true
    }

    fn collect_memory_reports(&self) -> Vec<Report> {
        let mut reports = vec![];
        perform_memory_report(|ops| {
            reports.push(Report {
                path: path!["storage", "local"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.environments.size_of(ops) + self.local_storage_origins.size_of(ops),
            });

            reports.push(Report {
                path: path!["storage", "session"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.session_data.size_of(ops) + self.session_storage_origins.size_of(ops),
            });
        });
        reports
    }

    fn save_local_storage_origins(&self) {
        if let Some(ref config_dir) = self.config_dir {
            write_json_to_file(&self.local_storage_origins, config_dir, "localstorage.json");
        }
    }

    fn get_origin_location(&self, origin: &ImmutableOrigin) -> Option<PathBuf> {
        match &self.config_dir {
            Some(config_dir) => {
                const NAMESPACE_SERVO_WEBSTORAGE: &uuid::Uuid = &Uuid::from_bytes([
                    0x37, 0x9e, 0x56, 0xb0, 0x1a, 0x76, 0x44, 0xc5, 0xa4, 0xdb, 0xe2, 0x18, 0xc5,
                    0xc8, 0xa3, 0x5d,
                ]);
                let origin_uuid = Uuid::new_v5(
                    NAMESPACE_SERVO_WEBSTORAGE,
                    origin.ascii_serialization().as_bytes(),
                );
                Some(config_dir.join("webstorage").join(origin_uuid.to_string()))
            },
            None => None,
        }
    }

    fn add_new_environment(&mut self, origin: &ImmutableOrigin) -> Result<(), rusqlite::Error> {
        let origin_location = self.get_origin_location(origin);

        let engine = SqliteEngine::new(&origin_location, self.thread_pool.clone())?;
        let environment = WebStorageEnvironment::new(engine);
        self.environments.insert(origin.clone(), environment);
        Ok(())
    }

    fn get_environment(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&WebStorageEnvironment<SqliteEngine>, rusqlite::Error> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn get_environment_mut(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&mut WebStorageEnvironment<SqliteEngine>, rusqlite::Error> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get_mut(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get_mut(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn select_data(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&OriginEntry> {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .get(&webview_id)
                .and_then(|origin_map| origin_map.get(&origin)),
            WebStorageType::Local => {
                // FIXME: Selecting data for read only operations should not
                // create a new origin descriptor. However, this currently
                // needs to happen because get_environment always creates an
                // environment, even for read only operations.
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment(&origin) {
                    Ok(env) => Some(&env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn select_data_mut(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&mut OriginEntry> {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .get_mut(&webview_id)
                .and_then(|origin_map| origin_map.get_mut(&origin)),
            WebStorageType::Local => {
                // FIXME: Selecting data for read only operations should not
                // create a new origin descriptor. However, this currently
                // needs to happen because get_environment always creates an
                // environment, even for read only operations.
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment_mut(&origin) {
                    Ok(env) => Some(&mut env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn ensure_data_mut(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&mut OriginEntry> {
        match storage_type {
            WebStorageType::Session => {
                self.session_storage_origins
                    .ensure_origin_descriptor(&origin);
                Some(
                    self.session_data
                        .entry(webview_id)
                        .or_default()
                        .entry(origin)
                        .or_default(),
                )
            },
            WebStorageType::Local => {
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment_mut(&origin) {
                    Ok(env) => Some(&mut env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn length(
        &mut self,
        sender: GenericSender<usize>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        sender
            .send(data.map_or(0, |entry| entry.inner().len()))
            .unwrap();
    }

    fn key(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        index: u32,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        let key = data
            .and_then(|entry| entry.inner().keys().nth(index as usize))
            .cloned();
        sender.send(key).unwrap();
    }

    fn keys(
        &mut self,
        sender: GenericSender<Vec<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        let keys = data.map_or(vec![], |entry| entry.inner().keys().cloned().collect());

        sender.send(keys).unwrap();
    }

    /// Sends Ok(changed, Some(old_value)) in case there was a previous
    /// value with the same key name but with different value name
    /// otherwise sends Err(()) to indicate that the operation would result in
    /// exceeding the quota limit
    fn set_item(
        &mut self,
        sender: GenericSender<Result<(bool, Option<String>), ()>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
        value: String,
    ) {
        let Some(entry) = self.ensure_data_mut(storage_type, webview_id, origin.clone()) else {
            sender.send(Err(())).unwrap();
            return;
        };
        let total_size = entry.size();

        let mut new_total_size = total_size + value.len();
        if let Some(old_value) = entry.inner().get(&name) {
            new_total_size -= old_value.len();
        } else {
            new_total_size += name.len();
        }

        let message = if new_total_size > QUOTA_SIZE_LIMIT {
            Err(())
        } else {
            let result =
                entry
                    .insert(name.clone(), value.clone())
                    .map_or(Ok((true, None)), |old| {
                        if old == value {
                            Ok((false, None))
                        } else {
                            Ok((true, Some(old)))
                        }
                    });
            if storage_type == WebStorageType::Local
                && let Ok(env) = self.get_environment_mut(&origin)
            {
                env.set(&name, &value);
            }
            result
        };
        sender.send(message).unwrap();
    }

    fn request_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        sender
            .send(data.and_then(|entry| entry.inner().get(&name)).cloned())
            .unwrap();
    }

    /// Sends Some(old_value) in case there was a previous value with the key name, otherwise sends None
    fn remove_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        let data = self.select_data_mut(storage_type, webview_id, origin.clone());
        let old_value = data.and_then(|entry| entry.remove(&name));
        sender.send(old_value).unwrap();
        if storage_type == WebStorageType::Local
            && let Ok(env) = self.get_environment_mut(&origin)
        {
            env.delete(&name);
        }
    }

    fn clear(
        &mut self,
        sender: GenericSender<bool>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let data = self.select_data_mut(storage_type, webview_id, origin.clone());
        sender
            .send(data.is_some_and(|entry| {
                if !entry.inner().is_empty() {
                    entry.clear();
                    true
                } else {
                    false
                }
            }))
            .unwrap();
        if storage_type == WebStorageType::Local
            && let Ok(env) = self.get_environment_mut(&origin)
        {
            env.clear();
        }
    }

    fn clone(&mut self, src_webview_id: WebViewId, dest_webview_id: WebViewId) {
        let Some(src_origin_entries) = self.session_data.get(&src_webview_id) else {
            return;
        };

        let dest_origin_entries = src_origin_entries.clone();
        self.session_data
            .insert(dest_webview_id, dest_origin_entries);
    }

    /// Collect one storage area's complete key/value data for a Worker WASM
    /// snapshot, grouped by ascii-serialized origin. Origins without entries
    /// are skipped. Session data is merged across webviews, first writer wins.
    #[cfg(target_arch = "wasm32")]
    fn snapshot_storage(
        &self,
        storage_type: WebStorageType,
    ) -> Vec<(String, Vec<(String, String)>)> {
        fn pairs(entry: &OriginEntry) -> Vec<(String, String)> {
            entry
                .inner()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }

        let mut origins = BTreeMap::new();
        match storage_type {
            WebStorageType::Local => {
                for (origin, env) in &self.environments {
                    if env.data.inner().is_empty() {
                        continue;
                    }
                    origins.insert(origin.ascii_serialization().into_owned(), pairs(&env.data));
                }
            },
            WebStorageType::Session => {
                for origin_map in self.session_data.values() {
                    for (origin, entry) in origin_map {
                        if entry.inner().is_empty() {
                            continue;
                        }
                        origins
                            .entry(origin.ascii_serialization().into_owned())
                            .or_insert_with(|| pairs(entry));
                    }
                }
            },
        }
        origins.into_iter().collect()
    }

    /// Replace one storage area from a Worker WASM snapshot, mirroring the
    /// cookie-jar restore contract. Local restore clears every origin first;
    /// session restore replaces the given webview's data and requires its id.
    #[cfg(target_arch = "wasm32")]
    fn restore_storage(
        &mut self,
        storage_type: WebStorageType,
        webview: Option<WebViewId>,
        snapshot: &WorkerWebStorageSnapshot,
    ) -> Result<(), String> {
        match storage_type {
            WebStorageType::Local => {
                for env in self.environments.values_mut() {
                    env.clear();
                }
                for (origin_ascii, pairs) in &snapshot.origins {
                    let origin = parse_snapshot_origin(origin_ascii)?;
                    self.local_storage_origins.ensure_origin_descriptor(&origin);
                    let env = self
                        .get_environment_mut(&origin)
                        .map_err(|error| error.to_string())?;
                    for (key, value) in pairs {
                        env.data.insert(key.clone(), value.clone());
                        env.set(key, value);
                    }
                }
            },
            WebStorageType::Session => {
                let webview = webview
                    .ok_or_else(|| "session storage restore requires a webview id".to_owned())?;
                let mut origin_map = FxHashMap::default();
                for (origin_ascii, pairs) in &snapshot.origins {
                    let origin = parse_snapshot_origin(origin_ascii)?;
                    self.session_storage_origins
                        .ensure_origin_descriptor(&origin);
                    let mut entry = OriginEntry::default();
                    for (key, value) in pairs {
                        entry.insert(key.clone(), value.clone());
                    }
                    origin_map.insert(origin, entry);
                }
                self.session_data.insert(webview, origin_map);
            },
        }
        Ok(())
    }

    fn origin_descriptors(&mut self, storage_type: WebStorageType) -> Vec<OriginDescriptor> {
        match storage_type {
            WebStorageType::Session => self.session_storage_origins.origin_descriptors(),
            WebStorageType::Local => self.local_storage_origins.origin_descriptors(),
        }
    }

    fn clear_data_for_sites(&mut self, storage_type: WebStorageType, sites: &[String]) {
        match storage_type {
            WebStorageType::Session => {
                let origins = self.session_storage_origins.take_origins_for_sites(sites);

                self.session_data.retain(|_, origins_map| {
                    for origin in &origins {
                        origins_map.remove(origin);
                    }
                    !origins_map.is_empty()
                });
            },
            WebStorageType::Local => {
                let origins = self.local_storage_origins.take_origins_for_sites(sites);

                if self.config_dir.is_some() {
                    for origin in origins {
                        self.environments.remove(&origin);

                        let origin_location = self
                            .get_origin_location(&origin)
                            .expect("Should always be able to get origin location.");

                        if let Err(error) = std::fs::remove_dir_all(&origin_location) {
                            warn!("Failed to delete origin location: {:?}", error);
                            self.local_storage_origins.ensure_origin_descriptor(&origin);
                        }
                    }

                    self.save_local_storage_origins();
                } else {
                    for origin in origins {
                        self.environments.remove(&origin);
                    }
                }
            },
        }
    }
}
