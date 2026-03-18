use std::collections::{HashMap, HashSet};

use extism_pdk::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Plugin contract types (must match scryer-plugins/src/types.rs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PluginDescriptor {
    name: String,
    version: String,
    sdk_version: String,
    plugin_type: String,
    provider_type: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provider_aliases: Vec<String>,
    capabilities: Capabilities,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scoring_policies: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_base_url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allowed_hosts: Vec<String>,
}

#[derive(Serialize)]
struct Capabilities {
    supported_ids: HashMap<String, Vec<String>>,
    #[serde(default)]
    deduplicates_aliases: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    season_param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    episode_param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    query_param: Option<String>,
    // Legacy fields
    search: bool,
    imdb_search: bool,
    tvdb_search: bool,
    #[serde(default)]
    anidb_search: bool,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct SearchRequest {
    query: String,
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    tvdb_id: Option<String>,
    #[serde(default)]
    anidb_id: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    limit: usize,
    #[serde(default)]
    season: Option<u32>,
    #[serde(default)]
    episode: Option<u32>,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct SearchResult {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grabs: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    languages: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// AnimeTosho JSON API response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct AnimetoshoItem {
    #[allow(dead_code)]
    id: Option<i64>,
    title: Option<String>,
    link: Option<String>,
    timestamp: Option<i64>,
    #[allow(dead_code)]
    status: Option<String>,
    torrent_url: Option<String>,
    nzb_url: Option<String>,
    info_hash: Option<String>,
    magnet_uri: Option<String>,
    seeders: Option<i64>,
    leechers: Option<i64>,
    torrent_downloaded_count: Option<i64>,
    total_size: Option<i64>,
    num_files: Option<i64>,
    anidb_aid: Option<i64>,
    anidb_eid: Option<i64>,
    #[allow(dead_code)]
    nyaa_id: Option<i64>,
    #[allow(dead_code)]
    nekobt_id: Option<i64>,
    #[allow(dead_code)]
    anidex_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Plugin exports
// ---------------------------------------------------------------------------

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        name: "AnimeTosho Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "indexer".to_string(),
        provider_type: "animetosho".to_string(),
        provider_aliases: vec![],
        capabilities: Capabilities {
            supported_ids: HashMap::from([
                ("anime".into(), vec!["anidb_id".into()]),
                ("movie".into(), vec!["anidb_id".into()]),
            ]),
            deduplicates_aliases: true,
            season_param: None,
            episode_param: None,
            query_param: Some("q".into()),
            search: true,
            imdb_search: false,
            tvdb_search: false,
            anidb_search: true,
        },
        scoring_policies: vec![],
        default_base_url: Some("https://feed.animetosho.org".to_string()),
        allowed_hosts: vec![],
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;

    let base_url = config::get("base_url")
        .map_err(|e| Error::msg(format!("missing config base_url: {e}")))?
        .unwrap_or_else(|| "https://feed.animetosho.org".to_string());

    let base_url = if base_url.is_empty() {
        "https://feed.animetosho.org".to_string()
    } else {
        base_url
    };

    let endpoint = base_url.trim_end_matches('/');
    let limit = req.limit.clamp(1, 500);
    let query = req.query.trim().to_string();
    let anidb_id = req.anidb_id.as_deref().and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    });
    // Choose endpoint based on what IDs are available.
    // Paginate: 75 results per page, up to ~1000 results (14 pages).
    // Stop when a page returns fewer than PAGE_SIZE results.
    const PAGE_SIZE: usize = 75;
    const MAX_PAGES: usize = 14; // 14 × 75 = 1050
    const MAX_RESULTS: usize = 1000;

    let base_params = if let Some(ref aid) = anidb_id {
        if !query.is_empty() {
            format!("aid={aid}&q={}", url_encode(&query))
        } else {
            format!("aid={aid}")
        }
    } else if !query.is_empty() {
        format!("q={}", url_encode(&query))
    } else {
        return Ok(serde_json::to_string(&SearchResponse { results: vec![] })?);
    };

    let mut all_items: Vec<AnimetoshoItem> = Vec::new();

    for page in 1..=MAX_PAGES {
        let params = format!("{base_params}&page={page}");
        let page_items = match query_json_api(endpoint, &params) {
            Ok(items) => items,
            Err(_) => break,
        };

        let page_count = page_items.len();
        all_items.extend(page_items);

        if page_count < PAGE_SIZE || all_items.len() >= MAX_RESULTS {
            break;
        }
    }

    // Dedup by info_hash (first occurrence wins)
    let deduped = dedup_items(all_items);

    let results = build_results(deduped);
    Ok(serde_json::to_string(&SearchResponse { results })?)
}

// ---------------------------------------------------------------------------
// API queries
// ---------------------------------------------------------------------------

fn query_json_api(endpoint: &str, params: &str) -> Result<Vec<AnimetoshoItem>, Error> {
    let url = format!("{endpoint}/json?{params}");

    let body = http_get_with_retry(&url)?;
    Ok(serde_json::from_str::<Vec<AnimetoshoItem>>(&body).unwrap_or_default())
}

/// HTTP GET with 429 retry handling.
///
/// On a 429 response, retries with escalating backoff: 2s → 5s → 10s.
/// Respects `Retry-After` / `X-Retry-After` headers if present.
/// If the 429 persists after 10s (or Retry-After > 10s), returns an error.
fn http_get_with_retry(url: &str) -> Result<String, Error> {
    const BACKOFF_SECS: &[u64] = &[2, 5, 10];

    let http_req = HttpRequest::new(url)
        .with_header("Accept", "application/json")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    let mut next_delay: u64 = 0;
    for attempt in 0..=BACKOFF_SECS.len() {
        if next_delay > 0 {
            let start = std::time::Instant::now();
            let wait = std::time::Duration::from_secs(next_delay);
            while start.elapsed() < wait {
                std::hint::spin_loop();
            }
        }

        let resp = http::request::<Vec<u8>>(&http_req, None)
            .map_err(|e| Error::msg(format!("HTTP request failed: {e}")))?;

        if resp.status_code() == 429 {
            if attempt >= BACKOFF_SECS.len() {
                return Err(Error::msg("HTTP 429: rate limited after all retries"));
            }

            // Honor Retry-After / X-Retry-After if present, otherwise use backoff table
            let server_delay = resp
                .headers()
                .get("retry-after")
                .or_else(|| resp.headers().get("x-retry-after"))
                .and_then(|v| v.parse::<u64>().ok());

            next_delay = match server_delay {
                Some(secs) if secs > 10 => {
                    return Err(Error::msg(format!(
                        "HTTP 429: Retry-After {secs}s exceeds maximum"
                    )));
                }
                Some(secs) => secs,
                None => BACKOFF_SECS[attempt],
            };
            continue;
        }

        if resp.status_code() >= 400 {
            return Err(Error::msg(format!("HTTP {}", resp.status_code())));
        }

        return Ok(String::from_utf8_lossy(&resp.body()).to_string());
    }

    Err(Error::msg("HTTP request exhausted all retries"))
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

fn dedup_items(items: Vec<AnimetoshoItem>) -> Vec<AnimetoshoItem> {
    let mut seen: HashSet<String> = HashSet::new();
    items.into_iter().filter(|item| {
        match &item.info_hash {
            Some(h) if !h.is_empty() => seen.insert(h.to_ascii_lowercase()),
            _ => true, // keep items without info_hash
        }
    }).collect()
}

// ---------------------------------------------------------------------------
// Result building
// ---------------------------------------------------------------------------

fn build_results(items: Vec<AnimetoshoItem>) -> Vec<SearchResult> {
    let mut results = Vec::with_capacity(items.len() * 2);

    for item in items {
        let title = match item.title {
            Some(ref t) if !t.is_empty() => t.clone(),
            _ => continue,
        };
        let link = item.link.clone();
        let size_bytes = item.total_size;
        let published_at = item.timestamp.map(format_timestamp);

        let source_tracker = detect_source_tracker(&item);

        // Shared extra fields for both results
        let mut common_extra = HashMap::new();
        if let Some(aid) = item.anidb_aid {
            common_extra.insert("anidb_aid".to_string(), serde_json::Value::from(aid));
        }
        if let Some(eid) = item.anidb_eid {
            common_extra.insert("anidb_eid".to_string(), serde_json::Value::from(eid));
        }
        if let Some(nf) = item.num_files {
            common_extra.insert("num_files".to_string(), serde_json::Value::from(nf));
        }
        if let Some(ref tracker) = source_tracker {
            common_extra.insert(
                "source_tracker".to_string(),
                serde_json::Value::from(tracker.as_str()),
            );
        }

        // Torrent result
        if let Some(ref torrent_url) = item.torrent_url {
            let mut extra = common_extra.clone();
            extra.insert("download_type".to_string(), serde_json::Value::from("torrent"));
            if let Some(seeders) = item.seeders {
                extra.insert("seeders".to_string(), serde_json::Value::from(seeders));
            }
            if let Some(leechers) = item.leechers {
                extra.insert("leechers".to_string(), serde_json::Value::from(leechers));
            }
            if let Some(ref hash) = item.info_hash {
                extra.insert("info_hash".to_string(), serde_json::Value::from(hash.as_str()));
            }
            if let Some(ref magnet) = item.magnet_uri {
                extra.insert("magnet_uri".to_string(), serde_json::Value::from(magnet.as_str()));
            }

            results.push(SearchResult {
                title: title.clone(),
                link: link.clone(),
                download_url: Some(torrent_url.clone()),
                size_bytes,
                published_at: published_at.clone(),
                grabs: item.torrent_downloaded_count,
                languages: vec![],
                extra,
            });
        }

        // NZB result
        if let Some(ref nzb_url) = item.nzb_url {
            let mut extra = common_extra;
            extra.insert("download_type".to_string(), serde_json::Value::from("nzb"));

            results.push(SearchResult {
                title,
                link,
                download_url: Some(nzb_url.clone()),
                size_bytes,
                published_at,
                grabs: None,
                languages: vec![],
                extra,
            });
        }
    }

    results
}

fn detect_source_tracker(item: &AnimetoshoItem) -> Option<String> {
    if item.nyaa_id.is_some() {
        Some("nyaa".to_string())
    } else if item.nekobt_id.is_some() {
        Some("nekobt".to_string())
    } else if item.anidex_id.is_some() {
        Some("anidex".to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Convert a unix timestamp to an ISO 8601 string.
fn format_timestamp(ts: i64) -> String {
    const DAYS_IN_MONTH: [[i64; 12]; 2] = [
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    ];

    let secs = ts;
    let sec = ((secs % 60) + 60) % 60;
    let mins_total = (secs - sec) / 60;
    let min = ((mins_total % 60) + 60) % 60;
    let hours_total = (mins_total - min) / 60;
    let hour = ((hours_total % 24) + 24) % 24;
    let mut days = (hours_total - hour) / 24;

    let mut year: i64 = 1970;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if days < year_days { break; }
        days -= year_days;
        year += 1;
    }

    let leap_idx = if is_leap(year) { 1 } else { 0 };
    let mut month: usize = 0;
    while month < 12 && days >= DAYS_IN_MONTH[leap_idx][month] {
        days -= DAYS_IN_MONTH[leap_idx][month];
        month += 1;
    }

    format!(
        "{year:04}-{:02}-{:02}T{hour:02}:{min:02}:{sec:02}Z",
        month + 1,
        days + 1,
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            b' ' => output.push_str("%20"),
            _ => {
                output.push('%');
                output.push_str(&format!("{byte:02X}"));
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_timestamp ─────────────────────────────────────────────────

    #[test]
    fn epoch_zero() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_date() {
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn y2k() {
        assert_eq!(format_timestamp(946_684_800), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn leap_year_feb29() {
        assert_eq!(format_timestamp(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn recent_date() {
        assert_eq!(format_timestamp(1_735_689_600), "2025-01-01T00:00:00Z");
    }

    // ── url_encode ───────────────────────────────────────────────────────

    #[test]
    fn encode_plain() {
        assert_eq!(url_encode("naruto"), "naruto");
    }

    #[test]
    fn encode_special() {
        let encoded = url_encode("[SubGroup] Title (1080p)");
        assert!(encoded.contains("%5B"));
        assert!(encoded.contains("%5D"));
        assert!(encoded.contains("%28"));
        assert!(encoded.contains("%29"));
        assert!(encoded.contains("%20"));
    }

    // ── detect_source_tracker ────────────────────────────────────────────

    fn make_item() -> AnimetoshoItem {
        AnimetoshoItem {
            id: None, title: Some("Test".into()), link: None, timestamp: None,
            status: None, torrent_url: None, nzb_url: None, info_hash: None,
            magnet_uri: None, seeders: None, leechers: None,
            torrent_downloaded_count: None, total_size: None, num_files: None,
            anidb_aid: None, anidb_eid: None, nyaa_id: None, nekobt_id: None,
            anidex_id: None,
        }
    }

    #[test]
    fn nyaa_detected() {
        let mut item = make_item();
        item.nyaa_id = Some(123);
        assert_eq!(detect_source_tracker(&item), Some("nyaa".into()));
    }

    #[test]
    fn no_tracker() {
        assert_eq!(detect_source_tracker(&make_item()), None);
    }

    // ── dedup_items ──────────────────────────────────────────────────────

    #[test]
    fn dedup_by_info_hash() {
        let mut a = make_item();
        a.info_hash = Some("ABC123".into());
        a.torrent_url = Some("https://example.com/a.torrent".into());

        let mut b = make_item();
        b.info_hash = Some("abc123".into()); // same hash, different case
        b.torrent_url = Some("https://example.com/b.torrent".into());

        let mut c = make_item();
        c.info_hash = Some("DEF456".into());
        c.torrent_url = Some("https://example.com/c.torrent".into());

        let deduped = dedup_items(vec![a, b, c]);
        assert_eq!(deduped.len(), 2);
        // First occurrence (a) wins over b
        assert_eq!(deduped[0].torrent_url.as_deref(), Some("https://example.com/a.torrent"));
        assert_eq!(deduped[1].torrent_url.as_deref(), Some("https://example.com/c.torrent"));
    }

    #[test]
    fn dedup_keeps_items_without_hash() {
        let mut a = make_item();
        a.title = Some("A".into());

        let mut b = make_item();
        b.title = Some("B".into());

        let deduped = dedup_items(vec![a, b]);
        assert_eq!(deduped.len(), 2);
    }

    // ── build_results ────────────────────────────────────────────────────

    #[test]
    fn dual_torrent_and_nzb() {
        let mut item = make_item();
        item.torrent_url = Some("https://example.com/file.torrent".into());
        item.nzb_url = Some("https://example.com/file.nzb".into());
        item.total_size = Some(1_000_000);
        item.seeders = Some(10);
        item.torrent_downloaded_count = Some(42);

        let results = build_results(vec![item]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].extra.get("download_type"), Some(&serde_json::Value::from("torrent")));
        assert_eq!(results[0].grabs, Some(42));
        assert_eq!(results[1].extra.get("download_type"), Some(&serde_json::Value::from("nzb")));
    }

    #[test]
    fn empty_title_skipped() {
        let mut item = make_item();
        item.title = Some("".into());
        item.torrent_url = Some("https://example.com/file.torrent".into());
        assert!(build_results(vec![item]).is_empty());
    }

}
