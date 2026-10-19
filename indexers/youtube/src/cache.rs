//! A short-lived cache of answered search pages, held in this configured
//! instance's plugin state.
//!
//! Repeating an interactive search (reopening the dialog, the host retrying a
//! page) would otherwise spend another call from the small `search.list`
//! bucket for the same public results. Entries live for ten minutes on the
//! host's monotonic clock, at most four are kept, and the key is a digest of
//! the request and the non-secret settings; the API key is not part of it and
//! is never stored.

use scryer_plugin_pdk::component::{monotonic_now_ms, state_cas, state_get};
use scryer_plugin_sdk::{
    PluginSearchRequest as SearchRequest, PluginSearchResponse as SearchResponse,
};
use serde::{Deserialize, Serialize};

use crate::api::YoutubeConfig;

const STATE_KEY: &str = "youtube-search-cache-v1";
const TTL_MS: u64 = 10 * 60 * 1_000;
const MAX_ENTRIES: usize = 4;
const MAX_STATE_BYTES: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Entry {
    key: String,
    stored_ms: u64,
    response: SearchResponse,
}

/// A digest of everything that shapes the answer, except the key.
pub fn request_key(config: &YoutubeConfig, request: &SearchRequest) -> String {
    let canonical = format!(
        "q={}\u{1f}s={:?}\u{1f}e={:?}\u{1f}l={}\u{1f}c={:?}\u{1f}ch={}\u{1f}r={:?}\u{1f}lang={:?}\u{1f}safe={}",
        request.query.trim(),
        request.season,
        request.episode,
        request.limit,
        request.page_cursor,
        config.channel_ids.join(","),
        config.region_code,
        config.relevance_language,
        config.safe_search.as_str(),
    );
    format!("{:016x}", fnv1a(canonical.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn read_entries() -> (Option<Vec<u8>>, Vec<Entry>) {
    let raw = state_get(STATE_KEY);
    let entries = raw
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Vec<Entry>>(bytes).ok())
        .unwrap_or_default();
    (raw, entries)
}

pub fn lookup(key: &str) -> Option<SearchResponse> {
    let now = monotonic_now_ms();
    if now == 0 {
        return None;
    }
    let (_, entries) = read_entries();
    entries
        .into_iter()
        .find(|entry| {
            entry.key == key
                && now.saturating_sub(entry.stored_ms) < TTL_MS
                && entry.stored_ms <= now
        })
        .map(|entry| entry.response)
}

/// Best effort: a lost race or an oversized page simply is not cached.
pub fn store(key: &str, response: &SearchResponse) {
    let now = monotonic_now_ms();
    if now == 0 {
        return;
    }
    let (raw, mut entries) = read_entries();
    entries.retain(|entry| entry.key != key && now.saturating_sub(entry.stored_ms) < TTL_MS);
    entries.push(Entry {
        key: key.to_string(),
        stored_ms: now,
        response: response.clone(),
    });
    while entries.len() > MAX_ENTRIES {
        entries.remove(0);
    }
    let Ok(mut encoded) = serde_json::to_vec(&entries) else {
        return;
    };
    while encoded.len() > MAX_STATE_BYTES && entries.len() > 1 {
        entries.remove(0);
        let Ok(smaller) = serde_json::to_vec(&entries) else {
            return;
        };
        encoded = smaller;
    }
    if encoded.len() > MAX_STATE_BYTES {
        return;
    }
    let _ = state_cas(STATE_KEY, raw, Some(encoded));
}
