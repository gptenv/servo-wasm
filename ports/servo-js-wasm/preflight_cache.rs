/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Engine-side CORS-preflight cache for the Worker fetch bridge.
//!
//! The bridge bypasses Servo's native HTTP fetch algorithm, and with it the
//! `CorsCache` in `components/net/fetch/cors_cache.rs`, so the WASM port
//! keeps its own preflight cache. It follows the Fetch standard's [CORS-preflight
//! cache](https://fetch.spec.whatwg.org/#cors-preflight-cache): entries are
//! keyed by origin and URL, each holds one method or one header name plus the
//! credentials flag the preflight was granted under, and each expires after
//! the `Access-Control-Max-Age` its preflight response carried (5 seconds
//! when the header is absent or unparsable).
//!
//! A cache hit lets a preflighted request skip the OPTIONS round-trip: the
//! engine simply omits `cors_preflight` from the host command and the adapter
//! sends the actual request. The actual response is still CORS-checked by
//! `filter_worker_metadata`, so a hit can never widen what a page may read.

/// Upper bound on retained entries, so a hostile page spraying distinct
/// preflighted URLs cannot grow the cache without limit.
const MAX_ENTRIES: usize = 1024;

/// Seconds cached when the preflight response carries no usable
/// `Access-Control-Max-Age`, per the Fetch standard.
const DEFAULT_MAX_AGE_SECS: u64 = 5;

/// Nanoseconds per second, for converting `Access-Control-Max-Age`
/// delta-seconds to the monotonic clock the host provides.
const NS_PER_SEC: u64 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum MethodOrHeader {
    Method(String),
    Header(String),
}

#[derive(Clone, Debug)]
struct PreflightCacheEntry {
    origin: String,
    url: String,
    credentials: bool,
    method_or_header: MethodOrHeader,
    /// Monotonic nanoseconds after which this entry must not be used.
    expires_ns: u64,
}

#[derive(Clone, Debug, Default)]
pub struct PreflightCache {
    entries: Vec<PreflightCacheEntry>,
}

impl PreflightCache {
    pub fn new() -> Self {
        PreflightCache {
            entries: Vec::new(),
        }
    }

    fn entry_matches(
        entry: &PreflightCacheEntry,
        origin: &str,
        url: &str,
        credentials: bool,
        now_ns: u64,
    ) -> bool {
        // Per the standard, an entry granted with credentials matches any
        // request, while an entry granted without credentials never satisfies
        // a credentialed request.
        entry.expires_ns > now_ns
            && entry.origin == origin
            && entry.url == url
            && (entry.credentials || !credentials)
    }

    fn prune(&mut self, now_ns: u64) {
        self.entries.retain(|entry| entry.expires_ns > now_ns);
    }

    /// True when the preflight for this method and every CORS-unsafe header
    /// can be skipped: each has an unexpired cache entry. Expired entries are
    /// pruned as a side effect.
    pub fn allows(
        &mut self,
        origin: &str,
        url: &str,
        credentials: bool,
        method: &str,
        headers: &[String],
        now_ns: u64,
    ) -> bool {
        self.prune(now_ns);
        let method_hit = self.entries.iter().any(|entry| {
            Self::entry_matches(entry, origin, url, credentials, now_ns)
                && entry.method_or_header == MethodOrHeader::Method(method.to_owned())
        });
        if !method_hit {
            return false;
        }
        headers.iter().all(|header| {
            let header = header.to_ascii_lowercase();
            self.entries.iter().any(|entry| {
                Self::entry_matches(entry, origin, url, credentials, now_ns)
                    && entry.method_or_header == MethodOrHeader::Header(header.clone())
            })
        })
    }

    /// Record a successful preflight for `method` and `headers`. Entries with
    /// a zero max-age are not stored: they would already be expired.
    pub fn store(
        &mut self,
        origin: String,
        url: String,
        credentials: bool,
        method: String,
        headers: &[String],
        max_age_secs: u64,
        now_ns: u64,
    ) {
        if max_age_secs == 0 {
            return;
        }
        self.prune(now_ns);
        let expires_ns = now_ns.saturating_add(max_age_secs.saturating_mul(NS_PER_SEC));
        let mut keys = Vec::with_capacity(headers.len() + 1);
        keys.push(MethodOrHeader::Method(method));
        keys.extend(
            headers
                .iter()
                .map(|header| MethodOrHeader::Header(header.to_ascii_lowercase())),
        );
        // Refresh rather than duplicate: drop live entries this store supersedes.
        self.entries.retain(|entry| {
            !(entry.origin == origin
                && entry.url == url
                && entry.credentials == credentials
                && keys.contains(&entry.method_or_header))
        });
        self.entries.extend(keys.into_iter().map(|method_or_header| {
            PreflightCacheEntry {
                origin: origin.clone(),
                url: url.clone(),
                credentials,
                method_or_header,
                expires_ns,
            }
        }));
        // Bound memory: evict the earliest-expiring entries first.
        while self.entries.len() > MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.expires_ns)
                .map(|(index, _)| index);
            match oldest {
                Some(index) => {
                    self.entries.remove(index);
                },
                None => break,
            }
        }
    }
}

/// Parse `Access-Control-Max-Age` delta-seconds from a preflight response.
/// The Fetch standard caches for 5 seconds when the header is absent or
/// unparsable.
pub fn preflight_max_age_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_AGE_SECS)
}
