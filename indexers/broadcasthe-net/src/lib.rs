use std::collections::HashMap;

use chrono::DateTime;
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, IndexerCapabilities as Capabilities,
    IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode,
    IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput,
    IndexerSourceKind, IndexerTorrentCapabilities, PluginDescriptor, PluginResult,
    PluginSearchRequest as SearchRequest, PluginSearchResponse as SearchResponse,
    PluginSearchResult as SearchResult, ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.broadcasthe.net/";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "broadcasthe-net".to_string(),
        name: "BroadcasTheNet Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "broadcasthe_net".to_string(),
            provider_aliases: vec!["btn".to_string(), "broadcasthe.net".to_string()],
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                supported_ids: HashMap::from([(
                    "series".to_string(),
                    vec!["tvdb_id".to_string(), "tvrage_id".to_string()],
                )]),
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                query_param: None,
                search: true,
                imdb_search: false,
                tvdb_search: true,
                anidb_search: false,
                rss: true,
                protocols: vec![IndexerProtocol::Torrent],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["tvdb_id".to_string(), "tvrage_id".to_string()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::String],
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(PAGE_SIZE as u32),
                    max_page_size: Some(PAGE_SIZE as u32),
                    rate_limit_hint_seconds: Some(5),
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_info_hash: true,
                    reports_magnet_uri: false,
                    supports_private_tracker_flags: true,
                    supports_seed_requirements: true,
                    ..IndexerTorrentCapabilities::default()
                }),
                response_features: Some(IndexerResponseFeatures {
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(5),
        }),
    };

    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let base_url = config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key = required_config("api_key")?;
    let limit = request_limit(&req);
    let queries = build_queries(&req);
    let mut results = Vec::new();

    for query in queries {
        for page in 0..MAX_PAGES {
            let offset = page * PAGE_SIZE;
            let response = execute_query(&base_url, &api_key, &query, offset)?;
            let mut page_results = parse_response(&base_url, response)?;
            let empty = page_results.is_empty();
            results.append(&mut page_results);
            if empty || results.len() >= limit {
                break;
            }
        }
        if results.len() >= limit {
            break;
        }
    }

    let results = dedupe_results(results).into_iter().take(limit).collect();
    Ok(serde_json::to_string(&PluginResult::Ok(SearchResponse {
        results,
        ..Default::default()
    }))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "base_url",
            "API URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("BroadcasTheNet JSON-RPC API URL"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("BroadcasTheNet API key"),
        ),
        field(
            "minimum_seeders",
            "Minimum Seeders",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some("Minimum seeders preference for host-side release decisions"),
        ),
    ]
}

fn build_queries(req: &SearchRequest) -> Vec<BtnQuery> {
    let mut queries = Vec::new();
    let tvdb = req.ids.get("tvdb_id").filter(|value| !value.is_empty());
    let tvrage = req.ids.get("tvrage_id").filter(|value| !value.is_empty());

    if tvdb.is_none() && tvrage.is_none() {
        if req.query.trim().is_empty() && req.ids.is_empty() {
            queries.push(BtnQuery {
                age: Some("<=86400".to_string()),
                ..BtnQuery::default()
            });
        }
        return queries;
    }

    let mut base = BtnQuery::default();
    if let Some(tvdb) = tvdb {
        base.tvdb = Some(tvdb.to_string());
    } else if let Some(tvrage) = tvrage {
        base.tvrage = Some(tvrage.to_string());
    }

    match (req.season, req.episode) {
        (Some(season), Some(episode)) => {
            queries.push(BtnQuery {
                category: Some("Episode".to_string()),
                name: Some(format!("S{season:02}%E{episode:02}%")),
                ..base
            });
        }
        (Some(season), None) => {
            queries.push(BtnQuery {
                category: Some("Season".to_string()),
                name: Some(format!("Season {season}%")),
                ..base.clone()
            });
            queries.push(BtnQuery {
                category: Some("Episode".to_string()),
                name: Some(format!("S{season:02}E%")),
                ..base
            });
        }
        _ => queries.push(base),
    }

    queries
}

fn execute_query(
    base_url: &str,
    api_key: &str,
    query: &BtnQuery,
    offset: usize,
) -> Result<BtnTorrents, Error> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "getTorrents",
        "params": [api_key, query, PAGE_SIZE, offset],
        "id": "scryer"
    });
    let request = HttpRequest::new(base_url)
        .with_method("POST")
        .with_header("Accept", "application/json-rpc, application/json")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "Scryer BroadcasTheNet Indexer/0.1");

    let response = http::request::<Vec<u8>>(&request, Some(serde_json::to_vec(&body)?))
        .map_err(|error| Error::msg(format!("BTN request failed: {error}")))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    match status {
        200 => {}
        401 => return Err(Error::msg("API Key invalid or not authorized")),
        404 => {
            return Err(Error::msg(
                "BTN API returned NotFound; the API may have changed",
            ));
        }
        503 => {
            return Err(Error::msg(
                "Cannot do more than 150 BTN API requests per hour",
            ));
        }
        _ => {
            return Err(Error::msg(format!(
                "BTN API returned HTTP {status}: {body_text}"
            )));
        }
    }
    if body_text.contains("Call Limit Exceeded") {
        return Err(Error::msg(
            "Cannot do more than 150 BTN API requests per hour",
        ));
    }
    if body_text == "Query execution was interrupted" {
        return Err(Error::msg("BTN API returned an internal server error"));
    }

    let rpc: JsonRpcResponse<BtnTorrents> = serde_json::from_str(&body_text)
        .map_err(|error| Error::msg(format!("BTN JSON parse failed: {error}")))?;
    if let Some(error) = rpc.error {
        return Err(Error::msg(format!("BTN API returned an error: {error}")));
    }
    rpc.result
        .ok_or_else(|| Error::msg("BTN API response missing result"))
}

fn parse_response(base_url: &str, response: BtnTorrents) -> Result<Vec<SearchResult>, Error> {
    if response.results == 0 {
        return Ok(Vec::new());
    }
    let protocol = if base_url.starts_with("http://") {
        "http:"
    } else {
        "https:"
    };

    let Some(torrents) = response.torrents else {
        return Ok(Vec::new());
    };
    Ok(torrents
        .into_values()
        .map(|torrent| {
            let seeders = torrent.seeders.unwrap_or_default() as i64;
            let leechers = torrent.leechers.unwrap_or_default() as i64;
            let mut external_ids = HashMap::new();
            if let Some(tvdb_id) = torrent.tvdb_id.filter(|value| *value > 0) {
                external_ids.insert("tvdb_id".to_string(), tvdb_id.to_string());
            }
            if let Some(tvrage_id) = torrent.tvrage_id.filter(|value| *value > 0) {
                external_ids.insert("tvrage_id".to_string(), tvrage_id.to_string());
            }
            if let Some(imdb_id) = normalize_imdb(torrent.imdb_id.as_deref()) {
                external_ids.insert("imdb_id".to_string(), imdb_id);
            }

            let mut indexer_flags = vec!["freeleech".to_string()];
            match torrent
                .origin
                .as_deref()
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str()
            {
                "INTERNAL" => indexer_flags.push("internal".to_string()),
                "SCENE" => indexer_flags.push("scene".to_string()),
                _ => {}
            }
            if torrent
                .tags
                .as_ref()
                .is_some_and(|tags| tags.iter().any(|tag| tag == "Subtitles"))
            {
                indexer_flags.push("subtitles".to_string());
            }

            let mut provider_extra = HashMap::new();
            provider_extra.insert(
                "group_id".to_string(),
                serde_json::Value::from(torrent.group_id),
            );
            provider_extra.insert(
                "torrent_id".to_string(),
                serde_json::Value::from(torrent.torrent_id),
            );
            provider_extra.insert(
                "snatched".to_string(),
                serde_json::Value::from(torrent.snatched.unwrap_or_default()),
            );
            if let Some(tags) = torrent.tags.clone() {
                provider_extra.insert(
                    "tags".to_string(),
                    serde_json::to_value(tags).unwrap_or_default(),
                );
            }

            SearchResult {
                title: torrent.release_name.replace('\\', ""),
                link: None,
                download_url: Some(replace_protocol(&torrent.download_url, protocol)),
                size_bytes: Some(torrent.size),
                published_at: DateTime::from_timestamp(torrent.time, 0).map(|dt| dt.to_rfc3339()),
                provider_extra,
                guid: Some(format!("BTN-{}", torrent.torrent_id)),
                info_url: Some(format!(
                    "{protocol}//broadcasthe.net/torrents.php?id={}&torrentid={}",
                    torrent.group_id, torrent.torrent_id
                )),
                source_kind: Some(IndexerSourceKind::Torrent),
                protocol: Some(IndexerProtocol::Torrent),
                external_ids,
                provider_categories: vec![torrent.category],
                info_hash_v1: Some(torrent.info_hash),
                seeders: Some(seeders),
                peers: Some(seeders + leechers),
                leechers: Some(leechers),
                download_volume_factor: Some(0.0),
                origin: torrent.origin,
                source: torrent.source,
                container: torrent.container,
                codec: torrent.codec,
                resolution: torrent.resolution,
                indexer_flags,
                ..SearchResult::default()
            }
        })
        .collect())
}

fn replace_protocol(value: &str, protocol: &str) -> String {
    if let Some(tail) = value.strip_prefix("http:") {
        format!("{protocol}{tail}")
    } else if let Some(tail) = value.strip_prefix("https:") {
        format!("{protocol}{tail}")
    } else {
        value.to_string()
    }
}

fn request_limit(req: &SearchRequest) -> usize {
    if req.limit == 0 {
        PAGE_SIZE
    } else {
        req.limit.min(PAGE_SIZE * MAX_PAGES)
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_config(key: &str) -> Result<String, Error> {
    config_value(key).ok_or_else(|| Error::msg(format!("{key} is not configured")))
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}

fn connection_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        role: Some(ConfigFieldRole::ConnectionUrl),
        ..field(
            key,
            label,
            ConfigFieldType::String,
            required,
            default_value,
            help_text,
        )
    }
}

fn normalize_imdb(value: Option<&str>) -> Option<String> {
    let digits = value?
        .trim()
        .trim_start_matches("tt")
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        Some(format!("tt{:0>7}", digits))
    }
}

fn dedupe_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for result in results {
        let key = result.guid.clone().unwrap_or_else(|| result.title.clone());
        if out.iter().all(|existing: &SearchResult| {
            existing.guid.as_ref().unwrap_or(&existing.title).ne(&key)
        }) {
            out.push(result);
        }
    }
    out
}

#[derive(Clone, Default, Serialize)]
struct BtnQuery {
    #[serde(rename = "Id", skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "Category", skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(rename = "Search", skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(rename = "Tvdb", skip_serializing_if = "Option::is_none")]
    tvdb: Option<String>,
    #[serde(rename = "Tvrage", skip_serializing_if = "Option::is_none")]
    tvrage: Option<String>,
    #[serde(rename = "Age", skip_serializing_if = "Option::is_none")]
    age: Option<String>,
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct BtnTorrents {
    #[serde(rename = "Torrents")]
    torrents: Option<HashMap<String, BtnTorrent>>,
    #[serde(rename = "Results")]
    results: u32,
}

#[derive(Deserialize)]
struct BtnTorrent {
    #[serde(rename = "GroupID")]
    group_id: i64,
    #[serde(rename = "TorrentID")]
    torrent_id: i64,
    #[serde(rename = "Category")]
    category: String,
    #[serde(rename = "Snatched")]
    snatched: Option<i64>,
    #[serde(rename = "Seeders")]
    seeders: Option<i64>,
    #[serde(rename = "Leechers")]
    leechers: Option<i64>,
    #[serde(rename = "Source")]
    source: Option<String>,
    #[serde(rename = "Container")]
    container: Option<String>,
    #[serde(rename = "Codec")]
    codec: Option<String>,
    #[serde(rename = "Resolution")]
    resolution: Option<String>,
    #[serde(rename = "Origin")]
    origin: Option<String>,
    #[serde(rename = "ReleaseName")]
    release_name: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "Time")]
    time: i64,
    #[serde(rename = "TvdbID")]
    tvdb_id: Option<i64>,
    #[serde(rename = "TvrageID")]
    tvrage_id: Option<i64>,
    #[serde(rename = "ImdbID")]
    imdb_id: Option<String>,
    #[serde(rename = "InfoHash")]
    info_hash: String,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "DownloadURL")]
    download_url: String,
}
