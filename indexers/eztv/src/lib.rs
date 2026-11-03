use std::collections::{BTreeMap, HashMap, HashSet};

use scryer_plugin_pdk::runtime::PluginHttpRequest;
use scryer_plugin_pdk::{
    Error,
    error::{codes, plugin_error},
    runtime,
};
use scryer_plugin_sdk::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "eztv".into(),
        name: "EZTV (Beta)".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        sdk_version: SDK_VERSION.into(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        settings: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "eztv".into(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: Some(3),
            strategy_plan: None,
            source_kind: IndexerSourceKind::Torrent,
            capabilities: IndexerCapabilities {
                raw_nab_search: false,
                resolve_network_endpoints: false,
                http: None,
                dynamic_config_fields: None,
                rss: true,
                search: true,
                imdb_search: true,
                tvdb_search: false,
                anidb_search: false,
                supported_ids: HashMap::from([("series".into(), vec!["imdb_id".into()])]),
                deduplicates_aliases: false,
                season_param: None,
                episode_param: None,
                query_param: None,
                supported_query_facets: vec![],
                protocols: vec![IndexerProtocol::Torrent],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![IndexerSearchInput::IdQuery, IndexerSearchInput::Limit],
                supported_external_ids: vec!["imdb_id".into()],
                category_model: None,
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(100),
                    max_page_size: Some(100),
                    max_pages: Some(100),
                    paged_search: true,
                    ..Default::default()
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_info_hash: true,
                    reports_magnet_uri: true,
                    ..Default::default()
                }),
                response_features: Some(IndexerResponseFeatures {
                    guid: true,
                    raw_provider_metadata: true,
                    ..Default::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: vec![ConfigFieldDef {
                key: "base_url".into(),
                label: "Base URL".into(),
                field_type: ConfigFieldType::String,
                required: true,
                default_value: Some("https://eztvx.to".into()),
                role: Some(ConfigFieldRole::ConnectionUrl),
                ..Default::default()
            }],
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    }
}

fn unsupported() -> Error {
    plugin_error(
        PluginErrorCode::Unsupported,
        "EZTV supports recent releases and IMDb television searches only",
    )
    .into_error()
}

fn normalize_imdb(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let digits = raw.strip_prefix("tt").unwrap_or(raw);
    if digits.is_empty() || digits.len() > 12 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    (!digits.is_empty()).then(|| digits.to_owned())
}

fn route(req: &PluginSearchRequest) -> Result<Option<String>, Error> {
    if req
        .facet
        .as_deref()
        .is_some_and(|f| !matches!(f, "series" | "tv"))
        || req.nab.is_some()
        || req.absolute_episode.is_some()
        || req.context.as_ref().is_some_and(|c| {
            c.air_date.is_some()
                || matches!(
                    c.subject_kind,
                    PluginSearchSubjectKind::Movie
                        | PluginSearchSubjectKind::AnimeEpisode
                        | PluginSearchSubjectKind::Special
                )
        })
    {
        return Err(unsupported());
    }
    let recent = req
        .context
        .as_ref()
        .is_some_and(|c| c.request_kind == PluginSearchRequestKind::Recent);
    if recent {
        if !req.ids.is_empty()
            || req.season.is_some()
            || req.episode.is_some()
            || !req.query.trim().is_empty()
        {
            return Err(unsupported());
        }
        return Ok(None);
    }
    if req.rss_catch_up.is_some()
        || req.recent_paging
        || req
            .facet
            .as_deref()
            .is_some_and(|f| !matches!(f, "series" | "tv"))
    {
        return Err(unsupported());
    }
    req.ids
        .iter()
        .find_map(|(key, value)| {
            (normalize_external_id_key(key) == "imdb_id").then(|| normalize_imdb(value))
        })
        .flatten()
        .map(Some)
        .ok_or_else(unsupported)
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
fn integer(row: &Value, key: &str) -> Option<i64> {
    i64::try_from(number(row.get(key)?)?).ok()
}

// Gregorian civil-date conversion; the API gives UTC Unix seconds, not a local date.
fn published(seconds: u64) -> Option<String> {
    if seconds > 253402300799 {
        return None;
    }
    let z = i64::try_from(seconds / 86400).ok()? + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        seconds % 86400 / 3600,
        seconds % 3600 / 60,
        seconds % 60
    ))
}

fn unix_date(year: i64, month: i64, day: i64) -> Option<i64> {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if !(0..=9999).contains(&year) || !(1..=max).contains(&day) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    Some((era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468) * 86400)
}

// Compare instants, including offsets and fractional seconds, rather than their spellings.
fn marker_unix(raw: &str) -> Option<i128> {
    if !raw.is_ascii()
        || raw.len() < 20
        || raw.get(4..5) != Some("-")
        || raw.get(7..8) != Some("-")
        || !matches!(raw.get(10..11), Some("T" | "t"))
        || raw.get(13..14) != Some(":")
        || raw.get(16..17) != Some(":")
    {
        return None;
    }
    let part = |a, b| {
        let value = raw.get(a..b)?;
        value
            .bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| value.parse::<i64>().ok())
            .flatten()
    };
    let seconds = unix_date(part(0, 4)?, part(5, 7)?, part(8, 10)?)?;
    let (hour, minute, second) = (part(11, 13)?, part(14, 16)?, part(17, 19)?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut suffix = raw.get(19..)?;
    let mut nanos = 0i128;
    if let Some(fraction) = suffix.strip_prefix('.') {
        let length = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            return None;
        }
        for (index, byte) in fraction.bytes().take(length.min(9)).enumerate() {
            nanos += i128::from(byte - b'0') * 10i128.pow(8 - index as u32);
        }
        // Subnanosecond precision cannot affect a comparison with an integer provider timestamp.
        if nanos == 0 && fraction.bytes().take(length).any(|c| c != b'0') {
            nanos = 1;
        }
        suffix = &fraction[length..];
    }
    let offset = if matches!(suffix, "Z" | "z") {
        0
    } else {
        if suffix.len() != 6
            || !matches!(suffix.get(0..1), Some("+" | "-"))
            || suffix.get(3..4) != Some(":")
        {
            return None;
        }
        if !suffix[1..3]
            .bytes()
            .chain(suffix[4..6].bytes())
            .all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let h = suffix.get(1..3)?.parse::<i64>().ok()?;
        let m = suffix.get(4..6)?.parse::<i64>().ok()?;
        if !(0..24).contains(&h) || !(0..60).contains(&m) {
            return None;
        }
        (h * 3600 + m * 60) * if suffix.starts_with('-') { -1 } else { 1 }
    };
    Some(i128::from(seconds + hour * 3600 + minute * 60 + second - offset) * 1_000_000_000 + nanos)
}

fn retry_after(raw: &str) -> Option<i64> {
    if let Ok(seconds) = raw.trim().parse::<u64>() {
        return i64::try_from(seconds).ok();
    }
    // IMF-fixdate, the HTTP-date form emitted by current HTTP servers.
    let fields: Vec<_> = raw.split_ascii_whitespace().collect();
    if fields.len() != 6 || fields[5] != "GMT" || !fields[0].ends_with(',') {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == fields[2])? as i64
        + 1;
    let date = unix_date(fields[3].parse().ok()?, month, fields[1].parse().ok()?)?;
    let time: Vec<_> = fields[4]
        .split(':')
        .map(str::parse::<i64>)
        .collect::<Result<_, _>>()
        .ok()?;
    if time.len() != 3
        || !(0..24).contains(&time[0])
        || !(0..60).contains(&time[1])
        || !(0..60).contains(&time[2])
    {
        return None;
    }
    let until = date + time[0] * 3600 + time[1] * 60 + time[2];
    Some(
        until
            .saturating_sub(i64::try_from(runtime::wall_now_ms() / 1000).ok()?)
            .max(0),
    )
}

fn hash(raw: &str) -> Option<String> {
    (raw.len() == 40 && raw.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

fn map_row(row: &Value) -> Option<PluginSearchResult> {
    let id = number(row.get("id")?)?;
    if id == 0 {
        return None;
    }
    let title = row
        .get("filename")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            row.get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })?;
    let supplied_hash = row
        .get("hash")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let mut info_hash = supplied_hash.map(hash).transpose_option()?;
    let supplied_magnet = row
        .get("magnet_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(raw) = supplied_magnet {
        let uri = url::Url::parse(raw).ok()?;
        if uri.scheme() != "magnet" {
            return None;
        }
        let mut hashes = uri
            .query_pairs()
            .filter(|(k, _)| k == "xt")
            .map(|(_, v)| v.strip_prefix("urn:btih:").and_then(hash))
            .collect::<Vec<_>>();
        if hashes.len() != 1 {
            return None;
        }
        let magnet_hash = hashes.pop()??;
        if info_hash.as_ref().is_some_and(|h| h != &magnet_hash) {
            return None;
        }
        info_hash = Some(magnet_hash);
    }
    let info_hash = info_hash?;
    let magnet = supplied_magnet
        .map(str::to_owned)
        .unwrap_or_else(|| format!("magnet:?xt=urn:btih:{info_hash}"));
    let mut result = PluginSearchResult {
        title: title.into(),
        guid: Some(format!("eztv:{id}")),
        download_url: Some(magnet.clone()),
        magnet_url: Some(magnet),
        info_hash_v1: Some(info_hash),
        source_kind: Some(IndexerSourceKind::Torrent),
        protocol: Some(IndexerProtocol::Torrent),
        size_bytes: integer(row, "size_bytes"),
        seeders: integer(row, "seeds"),
        published_at: row
            .get("date_released_unix")
            .and_then(number)
            .and_then(published),
        provider_extra: row
            .as_object()?
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ..PluginSearchResult::default()
    };
    if let Some(imdb) = row
        .get("imdb_id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| number(v).map(|n| n.to_string()))
        })
        .and_then(|v| normalize_imdb(&v))
    {
        result
            .external_ids
            .insert("imdb_id".into(), format!("tt{imdb:0>7}"));
    }
    Some(result)
}

trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(Some(v)) => Some(Some(v)),
            Some(None) => None,
        }
    }
}

struct Page {
    rows: Vec<Value>,
    total: usize,
    page: usize,
    limit: usize,
}
fn invalid(kind: IndexerSearchInvalidResponseKind) -> Error {
    plugin_error(
        codes::INVALID_RESPONSE,
        "EZTV returned an invalid API response",
    )
    .details(PluginErrorDetails::IndexerSearch(
        IndexerSearchPluginError::InvalidResponse { kind },
    ))
    .into_error()
}
fn parse_page(body: &[u8]) -> Result<Page, Error> {
    if body.len() > 8 * 1024 * 1024 {
        return Err(invalid(IndexerSearchInvalidResponseKind::MalformedBody));
    }
    let root: Value = serde_json::from_slice(body)
        .map_err(|_| invalid(IndexerSearchInvalidResponseKind::MalformedBody))?;
    let total = root
        .get("torrents_count")
        .and_then(number)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| invalid(IndexerSearchInvalidResponseKind::InvalidRoot))?;
    let rows = match root.get("torrents") {
        None if total == 0 => Vec::new(),
        Some(Value::Array(rows)) if rows.len() <= 100 => rows.clone(),
        _ => return Err(invalid(IndexerSearchInvalidResponseKind::InvalidRoot)),
    };
    let echoed_number = |key| {
        root.get(key)
            .and_then(number)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid(IndexerSearchInvalidResponseKind::InvalidRoot))
    };
    Ok(Page {
        rows,
        total,
        page: echoed_number("page")?,
        limit: echoed_number("limit")?,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    page: usize,
    binding: String,
}
fn binding(req: &PluginSearchRequest) -> String {
    let mut request = req.clone();
    request.page_cursor = None;
    // Sorted IDs make the binding independent of randomized map iteration.
    let mut value = serde_json::to_value(request).expect("serializable request");
    let ids: BTreeMap<_, _> = req.ids.iter().collect();
    value["ids"] = serde_json::to_value(ids).expect("serializable IDs");
    value.to_string()
}

fn partial(
    response: PluginSearchResponse,
    reason: IndexerSearchIncompleteReason,
    retry: Option<i64>,
) -> Error {
    plugin_error(
        PluginErrorCode::UpstreamUnavailable,
        "EZTV search could not cover all requested releases",
    )
    .details(PluginErrorDetails::IndexerSearch(
        IndexerSearchPluginError::PartialResults {
            response: Box::new(response),
            reason,
            retry_after_seconds: retry,
        },
    ))
    .into_error()
}

fn base_url() -> Result<url::Url, Error> {
    let raw = runtime::config_get("base_url").unwrap_or_else(|| "https://eztvx.to".into());
    let bad = || {
        plugin_error(
            PluginErrorCode::InvalidConfig,
            "EZTV base URL must be an HTTP or HTTPS origin without credentials, query or fragment",
        )
        .into_error()
    };
    let mut url = url::Url::parse(raw.trim()).map_err(|_| bad())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(bad());
    }
    url.set_path("/");
    Ok(url)
}

async fn fetch(
    base: &url::Url,
    page: usize,
    limit: usize,
    imdb: Option<&str>,
) -> Result<Page, Error> {
    let cooldown = runtime::CooldownGate::new("eztv.cooldown");
    cooldown
        .wait()
        .await
        .map_err(runtime::deadline_deferred_error)?;
    runtime::StartRateGate::new("eztv.request-start", 1, 2000)
        .acquire()
        .await
        .map_err(runtime::deadline_deferred_error)?;
    if runtime::monotonic_now_ms() >= runtime::operation_deadline_monotonic_ms() {
        return Err(runtime::deadline_deferred_error(
            runtime::DeadlineExceeded { retry_after_ms: 1 },
        ));
    }
    let mut url = base.clone();
    url.set_path("/api/get-torrents");
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string())
        .append_pair("page", &page.to_string());
    if let Some(id) = imdb {
        url.query_pairs_mut().append_pair("imdb_id", id);
    }
    let response = runtime::http(PluginHttpRequest {
        url: url.into(),
        method: Some("GET".into()),
        headers: BTreeMap::from([("Accept".into(), "application/json".into())]),
        body: vec![],
    })
    .await?;
    if response.status == 429 {
        let retry = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, v)| retry_after(v));
        let mut error = plugin_error(
            PluginErrorCode::RateLimited,
            "EZTV request rate limit reached",
        );
        if let Some(seconds) = retry {
            cooldown.defer_for((seconds as u64).saturating_mul(1000));
            error = error.retry_after_seconds(seconds);
        }
        return Err(error.into_error());
    }
    if response.status >= 500 {
        return Err(plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "EZTV is temporarily unavailable",
        )
        .into_error());
    }
    if !(200..300).contains(&response.status) {
        return Err(
            plugin_error(codes::INVALID_RESPONSE, "EZTV rejected the API request").into_error(),
        );
    }
    if response.headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && !v.to_ascii_lowercase().contains("json")
    }) {
        return Err(invalid(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
        ));
    }
    let data = parse_page(&response.body)?;
    if data.rows.len() > limit || data.page != page || data.limit != limit {
        return Err(invalid(IndexerSearchInvalidResponseKind::InvalidRoot));
    }
    Ok(data)
}

async fn execute(req: &PluginSearchRequest) -> Result<PluginSearchResponse, Error> {
    let imdb = route(req)?;
    if imdb.is_none() && req.page_cursor.is_some() && !req.recent_paging {
        return Err(unsupported());
    }
    let base = base_url()?;
    if req.rss_catch_up.is_some() && (req.page_cursor.is_some() || req.recent_paging) {
        return Err(unsupported());
    }
    let bound = format!("{}|{}", base, binding(req));
    let mut page = if let Some(cursor) = &req.page_cursor {
        if cursor.len() > 32768 {
            return Err(unsupported());
        }
        let cursor: Cursor = serde_json::from_str(cursor).map_err(|_| unsupported())?;
        if cursor.version != 1 || cursor.binding != bound || !(2..=100).contains(&cursor.page) {
            return Err(unsupported());
        }
        cursor.page
    } else {
        1
    };
    let limit = if req.limit == 0 {
        100
    } else {
        req.limit.min(100)
    };
    let catchup = req.rss_catch_up.as_ref();
    let marker_time = catchup
        .map(|marker| marker_unix(&marker.last_seen_published_at).ok_or_else(unsupported))
        .transpose()?;
    let mut response = PluginSearchResponse::default();
    let mut seen = HashSet::new();
    loop {
        let data = match fetch(&base, page, limit, imdb.as_deref()).await {
            Ok(data) => data,
            Err(error) => {
                if response.results.is_empty() {
                    return Err(error);
                }
                let error = scryer_plugin_pdk::error::plugin_error_from(&error);
                let reason = if error.code == PluginErrorCode::RateLimited {
                    IndexerSearchIncompleteReason::RateLimited
                } else if error.code == PluginErrorCode::from(codes::INVALID_RESPONSE) {
                    IndexerSearchIncompleteReason::MalformedContent
                } else {
                    IndexerSearchIncompleteReason::UpstreamFailure
                };
                return Err(partial(response, reason, error.retry_after_seconds));
            }
        };
        let exhausted = page.saturating_mul(limit) >= data.total;
        let mut reached = false;
        let mut malformed = false;
        for row in data.rows {
            let Some(result) = map_row(&row) else {
                malformed = true;
                continue;
            };
            if let Some(marker) = catchup {
                reached |= marker.names(&result)
                    || row
                        .get("date_released_unix")
                        .and_then(number)
                        .zip(marker_time)
                        .is_some_and(|(seconds, marker)| {
                            i128::from(seconds) * 1_000_000_000 < marker
                        });
            }
            let identity = result.info_hash_v1.clone().expect("mapped hash");
            let guid = result.guid.clone().expect("mapped ID");
            let fresh_hash = seen.insert(identity);
            let fresh_id = seen.insert(guid);
            if fresh_hash && fresh_id {
                response.results.push(result);
            }
        }
        if malformed {
            return Err(partial(
                response,
                IndexerSearchIncompleteReason::MalformedContent,
                None,
            ));
        }
        if exhausted || reached {
            return Ok(response);
        }
        if catchup.is_some() {
            if page >= 10 {
                return Err(partial(
                    response,
                    IndexerSearchIncompleteReason::PageCeilingReached,
                    None,
                ));
            }
            page += 1;
            continue;
        }
        if imdb.is_none() && !req.recent_paging {
            return Ok(response);
        }
        if page >= 100 {
            return Err(partial(
                response,
                IndexerSearchIncompleteReason::PageCeilingReached,
                None,
            ));
        }
        response.next_cursor = Some(
            serde_json::to_string(&Cursor {
                version: 1,
                page: page + 1,
                binding: bound,
            })
            .expect("serializable cursor"),
        );
        return Ok(response);
    }
}

async fn search(req: PluginSearchRequest) -> Result<PluginSearchResponse, Error> {
    execute(&req).await
}
async fn action(
    _: scryer_plugin_sdk::command::PluginActionRequest,
) -> Result<scryer_plugin_sdk::command::PluginActionResponse, Error> {
    Err(unsupported())
}
scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
    action = action,
);

#[cfg(test)]
mod tests;
