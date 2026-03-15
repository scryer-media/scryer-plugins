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
            search: true,
            imdb_search: false,
            tvdb_search: true,
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
    let tvdb_id = req.tvdb_id.as_deref().and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    });

    // Collect items from multiple endpoints, then dedup.
    let mut all_items: Vec<AnimetoshoItem> = Vec::new();

    // Priority 1: AniDB ID → JSON API (richest metadata)
    if let Some(ref aid) = anidb_id {
        if let Ok(items) = query_json_api(endpoint, &format!("aid={aid}&num={limit}")) {
            all_items.extend(items);
        }
    }

    // Priority 2: TVDB ID → Torznab API
    if let Some(ref tvdb) = tvdb_id {
        let mut torznab_params = format!("t=tvsearch&tvdbid={tvdb}&limit={limit}");
        if let Some(s) = req.season {
            torznab_params.push_str(&format!("&season={s}"));
        }
        if let Some(e) = req.episode {
            torznab_params.push_str(&format!("&ep={e}"));
        }
        if let Ok(items) = query_torznab_api(endpoint, &torznab_params) {
            all_items.extend(items);
        }
    }

    // Priority 3: Freetext → JSON API
    if !query.is_empty() {
        if let Ok(items) = query_json_api(endpoint, &format!("q={}&num={limit}", url_encode(&query))) {
            all_items.extend(items);
        }
    }

    // If no endpoints were queried, return empty
    if all_items.is_empty() && anidb_id.is_none() && tvdb_id.is_none() && query.is_empty() {
        return Ok(serde_json::to_string(&SearchResponse { results: vec![] })?);
    }

    // Dedup by info_hash (first occurrence wins = higher priority endpoint)
    let deduped = dedup_items(all_items);

    let results = build_results(deduped);
    Ok(serde_json::to_string(&SearchResponse { results })?)
}

// ---------------------------------------------------------------------------
// API queries
// ---------------------------------------------------------------------------

fn query_json_api(endpoint: &str, params: &str) -> Result<Vec<AnimetoshoItem>, Error> {
    let url = format!("{endpoint}/json?{params}");
    let http_req = HttpRequest::new(&url)
        .with_header("Accept", "application/json")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    let resp = http::request::<Vec<u8>>(&http_req, None)
        .map_err(|e| Error::msg(format!("JSON API request failed: {e}")))?;

    if resp.status_code() >= 400 {
        return Err(Error::msg(format!("JSON API returned HTTP {}", resp.status_code())));
    }

    let body = String::from_utf8_lossy(&resp.body()).to_string();
    Ok(serde_json::from_str::<Vec<AnimetoshoItem>>(&body).unwrap_or_default())
}

fn query_torznab_api(endpoint: &str, params: &str) -> Result<Vec<AnimetoshoItem>, Error> {
    let url = format!("{endpoint}/nabapi?{params}");
    let http_req = HttpRequest::new(&url)
        .with_header("Accept", "application/xml, */*")
        .with_header("Accept-Language", "en-US,en;q=0.9")
        .with_header("User-Agent", USER_AGENT);

    let resp = http::request::<Vec<u8>>(&http_req, None)
        .map_err(|e| Error::msg(format!("Torznab API request failed: {e}")))?;

    if resp.status_code() >= 400 {
        return Err(Error::msg(format!("Torznab API returned HTTP {}", resp.status_code())));
    }

    let body = String::from_utf8_lossy(&resp.body()).to_string();
    Ok(parse_torznab_items(&body))
}

// ---------------------------------------------------------------------------
// Torznab XML parsing
// ---------------------------------------------------------------------------

fn parse_torznab_items(xml: &str) -> Vec<AnimetoshoItem> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut items = Vec::new();
    let mut in_item = false;
    let mut current_title = None;
    let mut current_link = None;
    let mut current_pub_date = None;
    let mut current_size: Option<i64> = None;
    let mut current_info_hash = None;
    let mut current_magnet_uri = None;
    let mut current_seeders: Option<i64> = None;
    let mut current_leechers: Option<i64> = None;
    let mut current_enclosure_url = None;
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" {
                    in_item = true;
                    current_title = None;
                    current_link = None;
                    current_pub_date = None;
                    current_size = None;
                    current_info_hash = None;
                    current_magnet_uri = None;
                    current_seeders = None;
                    current_leechers = None;
                    current_enclosure_url = None;
                }
                if in_item {
                    current_tag = tag;
                }
            }
            Ok(Event::Empty(ref e)) => {
                if !in_item {
                    buf.clear();
                    continue;
                }
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "enclosure" {
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        match key.as_str() {
                            "url" if current_enclosure_url.is_none() => {
                                current_enclosure_url = Some(val);
                            }
                            "length" => {
                                if let Ok(s) = val.parse::<i64>() {
                                    if s > 0 {
                                        current_size = Some(s);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                } else if tag.contains("attr") {
                    let mut attr_name = String::new();
                    let mut attr_value = String::new();
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        match key.as_str() {
                            "name" => attr_name = val,
                            "value" => attr_value = val,
                            _ => {}
                        }
                    }
                    match attr_name.as_str() {
                        "size" => { current_size = attr_value.parse().ok(); }
                        "infohash" => { current_info_hash = Some(attr_value); }
                        "magneturl" => { current_magnet_uri = Some(attr_value); }
                        "seeders" => { current_seeders = attr_value.parse().ok(); }
                        "peers" => { current_leechers = attr_value.parse::<i64>().ok().map(|p| p.saturating_sub(current_seeders.unwrap_or(0))); }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                if !in_item {
                    buf.clear();
                    continue;
                }
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "title" => { current_title = Some(text); }
                    "link" => { current_link = Some(text); }
                    "pubDate" => { current_pub_date = Some(text); }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" && in_item {
                    in_item = false;
                    if let Some(ref title) = current_title {
                        if !title.is_empty() {
                            // Determine torrent vs NZB from enclosure URL
                            let is_torrent = current_enclosure_url.as_ref()
                                .is_some_and(|u| u.contains(".torrent"));
                            let is_nzb = current_enclosure_url.as_ref()
                                .is_some_and(|u| u.contains(".nzb"));

                            let torrent_url = if is_torrent { current_enclosure_url.clone() } else { None };
                            let nzb_url = if is_nzb { current_enclosure_url.clone() } else { None };

                            let timestamp = current_pub_date.as_ref().and_then(|d| parse_rfc2822_to_epoch(d));

                            items.push(AnimetoshoItem {
                                id: None,
                                title: current_title.clone(),
                                link: current_link.clone(),
                                timestamp,
                                status: None,
                                torrent_url,
                                nzb_url,
                                info_hash: current_info_hash.clone(),
                                magnet_uri: current_magnet_uri.clone(),
                                seeders: current_seeders,
                                leechers: current_leechers,
                                torrent_downloaded_count: None,
                                total_size: current_size,
                                num_files: None,
                                anidb_aid: None,
                                anidb_eid: None,
                                nyaa_id: None,
                                nekobt_id: None,
                                anidex_id: None,
                            });
                        }
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    items
}

/// Best-effort RFC 2822 date to Unix epoch. Falls back to None on failure.
fn parse_rfc2822_to_epoch(date: &str) -> Option<i64> {
    // Format: "Tue, 02 Aug 2016 13:48:04 +0000"
    let parts: Vec<&str> = date.split_whitespace().collect();
    if parts.len() < 5 { return None; }

    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2].to_ascii_lowercase().as_str() {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() < 3 { return None; }
    let hour: i64 = time_parts[0].parse().ok()?;
    let min: i64 = time_parts[1].parse().ok()?;
    let sec: i64 = time_parts[2].parse().ok()?;

    // Simplified days-from-epoch calculation
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) { days += 1; }
    }
    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
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

    // ── parse_rfc2822_to_epoch ───────────────────────────────────────────

    #[test]
    fn parse_rfc2822() {
        let epoch = parse_rfc2822_to_epoch("Tue, 02 Aug 2016 13:48:04 +0000");
        assert!(epoch.is_some());
        assert_eq!(format_timestamp(epoch.unwrap()), "2016-08-02T13:48:04Z");
    }
}
