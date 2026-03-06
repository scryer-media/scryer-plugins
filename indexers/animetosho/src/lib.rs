use std::collections::HashMap;

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
}

#[derive(Serialize)]
struct Capabilities {
    search: bool,
    imdb_search: bool,
    tvdb_search: bool,
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
    category: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    limit: usize,
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

#[derive(Deserialize)]
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
    nyaa_id: Option<i64>,
    nekobt_id: Option<i64>,
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
            search: true,
            imdb_search: false,
            tvdb_search: false,
        },
        scoring_policies: vec![],
        default_base_url: Some("https://feed.animetosho.org".to_string()),
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

    let query = req.query.trim().to_string();
    if query.is_empty() {
        let response = SearchResponse { results: vec![] };
        return Ok(serde_json::to_string(&response)?);
    }

    let limit = req.limit.clamp(1, 500);
    let endpoint = base_url.trim_end_matches('/');
    let url = format!("{endpoint}/json?q={}&num={limit}", url_encode(&query));

    let http_req = HttpRequest::new(&url)
        .with_header("Accept", "application/json")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
        );

    let resp = http::request::<Vec<u8>>(&http_req, None)
        .map_err(|e| Error::msg(format!("HTTP request failed: {e}")))?;

    let status = resp.status_code();
    if status >= 400 {
        return Err(WithReturnCode::new(
            Error::msg(format!("AnimeTosho API returned HTTP {status}")),
            1,
        ));
    }

    let body = String::from_utf8_lossy(&resp.body()).to_string();
    let items: Vec<AnimetoshoItem> = serde_json::from_str(&body).unwrap_or_default();

    let results = build_results(items);
    let response = SearchResponse { results };
    Ok(serde_json::to_string(&response)?)
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
            extra.insert(
                "download_type".to_string(),
                serde_json::Value::from("torrent"),
            );
            if let Some(seeders) = item.seeders {
                extra.insert("seeders".to_string(), serde_json::Value::from(seeders));
            }
            if let Some(leechers) = item.leechers {
                extra.insert("leechers".to_string(), serde_json::Value::from(leechers));
            }
            if let Some(ref hash) = item.info_hash {
                extra.insert(
                    "info_hash".to_string(),
                    serde_json::Value::from(hash.as_str()),
                );
            }
            if let Some(ref magnet) = item.magnet_uri {
                extra.insert(
                    "magnet_uri".to_string(),
                    serde_json::Value::from(magnet.as_str()),
                );
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
            extra.insert(
                "download_type".to_string(),
                serde_json::Value::from("nzb"),
            );

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

/// Convert a unix timestamp to an ISO 8601 string.
/// We avoid pulling in chrono/time by doing the conversion manually.
fn format_timestamp(ts: i64) -> String {
    // Days in each month for non-leap and leap years
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

    // Shift from 1970 epoch
    let mut year: i64 = 1970;
    loop {
        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if is_leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let leap_idx = if is_leap { 1 } else { 0 };
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

fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => output.push(byte as char),
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
        // 2024-02-29 00:00:00 UTC
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
        assert!(encoded.contains("%5B")); // [
        assert!(encoded.contains("%5D")); // ]
        assert!(encoded.contains("%28")); // (
        assert!(encoded.contains("%29")); // )
        assert!(encoded.contains("%20")); // space
    }

    // ── detect_source_tracker ────────────────────────────────────────────

    fn make_item() -> AnimetoshoItem {
        AnimetoshoItem {
            id: None,
            title: Some("Test".into()),
            link: None,
            timestamp: None,
            status: None,
            torrent_url: None,
            nzb_url: None,
            info_hash: None,
            magnet_uri: None,
            seeders: None,
            leechers: None,
            torrent_downloaded_count: None,
            total_size: None,
            num_files: None,
            anidb_aid: None,
            anidb_eid: None,
            nyaa_id: None,
            nekobt_id: None,
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
    fn nekobt_detected() {
        let mut item = make_item();
        item.nekobt_id = Some(456);
        assert_eq!(detect_source_tracker(&item), Some("nekobt".into()));
    }

    #[test]
    fn anidex_detected() {
        let mut item = make_item();
        item.anidex_id = Some(789);
        assert_eq!(detect_source_tracker(&item), Some("anidex".into()));
    }

    #[test]
    fn no_tracker() {
        let item = make_item();
        assert_eq!(detect_source_tracker(&item), None);
    }

    // ── build_results ────────────────────────────────────────────────────

    #[test]
    fn dual_torrent_and_nzb() {
        let mut item = make_item();
        item.torrent_url = Some("https://example.com/file.torrent".into());
        item.nzb_url = Some("https://example.com/file.nzb".into());
        item.total_size = Some(1_000_000);
        item.seeders = Some(10);
        item.leechers = Some(5);
        item.torrent_downloaded_count = Some(42);

        let results = build_results(vec![item]);
        assert_eq!(results.len(), 2);

        // Torrent result first
        assert_eq!(
            results[0].extra.get("download_type"),
            Some(&serde_json::Value::from("torrent"))
        );
        assert_eq!(results[0].grabs, Some(42));
        assert_eq!(
            results[0].extra.get("seeders"),
            Some(&serde_json::Value::from(10))
        );

        // NZB result second
        assert_eq!(
            results[1].extra.get("download_type"),
            Some(&serde_json::Value::from("nzb"))
        );
        assert_eq!(results[1].grabs, None);
    }

    #[test]
    fn torrent_only() {
        let mut item = make_item();
        item.torrent_url = Some("https://example.com/file.torrent".into());

        let results = build_results(vec![item]);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].extra.get("download_type"),
            Some(&serde_json::Value::from("torrent"))
        );
    }

    #[test]
    fn empty_title_skipped() {
        let mut item = make_item();
        item.title = Some("".into());
        item.torrent_url = Some("https://example.com/file.torrent".into());

        let results = build_results(vec![item]);
        assert!(results.is_empty());
    }
}
