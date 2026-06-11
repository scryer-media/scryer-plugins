use std::collections::HashMap;

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
use url::Url;

const DEFAULT_BASE_URL: &str = "https://hdbits.org";
const DEFAULT_CATEGORIES: &str = "2,3";
const PAGE_SIZE: usize = 100;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "hdbits".to_string(),
        name: "HDBits Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "hdbits".to_string(),
            provider_aliases: vec!["hdbits.org".to_string()],
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                supported_ids: HashMap::from([("series".to_string(), vec!["tvdb_id".to_string()])]),
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
                supported_external_ids: vec!["tvdb_id".to_string()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(PAGE_SIZE as u32),
                    max_page_size: Some(PAGE_SIZE as u32),
                    rate_limit_hint_seconds: Some(2),
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
                    comments: true,
                    grabs: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    };

    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let config = HdbitsConfig::from_extism()?;
    let Some(query) = build_query(&config, &req) else {
        return Ok(serde_json::to_string(&PluginResult::Ok(SearchResponse {
            results: Vec::new(),
            ..Default::default()
        }))?);
    };
    let body = post_query(&config, &query)?;
    let mut results = parse_response(&config, &body)?;
    let limit = if req.limit == 0 {
        PAGE_SIZE
    } else {
        req.limit.min(PAGE_SIZE)
    };
    results.truncate(limit);

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
            Some("HDBits site URL"),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            None,
            Some("HDBits username"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("HDBits API key/passkey"),
        ),
        field(
            "categories",
            "Categories",
            ConfigFieldType::String,
            true,
            Some(DEFAULT_CATEGORIES),
            Some("Comma-separated HDBits category IDs"),
        ),
        field(
            "codecs",
            "Codecs",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma-separated HDBits codec IDs"),
        ),
        field(
            "mediums",
            "Mediums",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma-separated HDBits medium IDs"),
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

fn build_query(config: &HdbitsConfig, req: &SearchRequest) -> Option<TorrentQuery> {
    let mut query = TorrentQuery {
        username: config.username.clone(),
        passkey: config.api_key.clone(),
        category: config.categories.clone(),
        codec: config.codecs.clone(),
        medium: config.mediums.clone(),
        limit: Some(PAGE_SIZE as i64),
        ..TorrentQuery::default()
    };

    let has_search_criteria = !req.query.trim().is_empty()
        || !req.ids.is_empty()
        || req.season.is_some()
        || req.episode.is_some()
        || req.absolute_episode.is_some();

    if !has_search_criteria {
        return Some(query);
    }

    if let Some(tvdb_id) = req
        .ids
        .get("tvdb_id")
        .and_then(|value| value.parse::<i64>().ok())
    {
        query.tvdb = Some(TvdbInfo {
            id: Some(tvdb_id),
            season: req.season.map(i64::from),
            episode: req.episode.map(i64::from),
        });

        Some(query)
    } else {
        None
    }
}

fn post_query(config: &HdbitsConfig, query: &TorrentQuery) -> Result<String, Error> {
    let url = format!("{}/api/torrents", config.base_url.trim_end_matches('/'));
    let request = HttpRequest::new(&url)
        .with_method("POST")
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "Scryer HDBits Indexer/0.1");
    let body = serde_json::to_vec(query)?;
    let response = http::request::<Vec<u8>>(&request, Some(body))
        .map_err(|error| Error::msg(format!("HDBits request failed: {error}")))?;
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).to_string();
    if status != 200 {
        return Err(Error::msg(format!(
            "HDBits API returned HTTP {status}: {body}"
        )));
    }
    Ok(body)
}

fn parse_response(config: &HdbitsConfig, body: &str) -> Result<Vec<SearchResult>, Error> {
    let response: HdbitsResponse = serde_json::from_str(body)
        .map_err(|error| Error::msg(format!("HDBits JSON parse failed: {error}")))?;
    if response.status != 0 {
        return Err(Error::msg(format!(
            "HDBits API returned status {}: {}",
            response.status,
            response.message.unwrap_or_default()
        )));
    }
    let torrents: Vec<TorrentResponse> = serde_json::from_value(response.data)
        .map_err(|error| Error::msg(format!("HDBits data parse failed: {error}")))?;

    Ok(torrents
        .into_iter()
        .map(|torrent| {
            let seeders = torrent.seeders as i64;
            let leechers = torrent.leechers as i64;
            let mut external_ids = HashMap::new();
            if let Some(tvdb_id) = torrent.tvdb.as_ref().and_then(|tvdb| tvdb.id) {
                external_ids.insert("tvdb_id".to_string(), tvdb_id.to_string());
            }
            if let Some(imdb_id) = torrent.imdb.as_ref().and_then(|imdb| imdb.id) {
                external_ids.insert("imdb_id".to_string(), format!("tt{imdb_id:07}"));
            }

            let mut indexer_flags = Vec::new();
            if torrent.freeleech.as_deref() == Some("yes") {
                indexer_flags.push("freeleech".to_string());
            }
            if torrent.type_origin == 1 {
                indexer_flags.push("internal".to_string());
            }

            let mut provider_extra = HashMap::new();
            provider_extra.insert(
                "comments".to_string(),
                serde_json::Value::from(torrent.comments),
            );
            provider_extra.insert(
                "numfiles".to_string(),
                serde_json::Value::from(torrent.numfiles),
            );
            provider_extra.insert(
                "times_completed".to_string(),
                serde_json::Value::from(torrent.times_completed),
            );
            provider_extra.insert(
                "type_category".to_string(),
                serde_json::Value::from(torrent.type_category),
            );
            provider_extra.insert(
                "type_codec".to_string(),
                serde_json::Value::from(torrent.type_codec),
            );
            provider_extra.insert(
                "type_medium".to_string(),
                serde_json::Value::from(torrent.type_medium),
            );

            SearchResult {
                title: torrent.name,
                download_url: Some(download_url(config, &torrent.id)),
                size_bytes: Some(torrent.size),
                published_at: Some(torrent.added),
                grabs: Some(torrent.times_completed as i64),
                provider_extra,
                guid: Some(format!("HDBits-{}", torrent.id)),
                info_url: Some(info_url(config, &torrent.id)),
                source_kind: Some(IndexerSourceKind::Torrent),
                protocol: Some(IndexerProtocol::Torrent),
                external_ids,
                provider_categories: vec![torrent.type_category.to_string()],
                info_hash_v1: Some(torrent.hash),
                seeders: Some(seeders),
                peers: Some(seeders + leechers),
                leechers: Some(leechers),
                indexer_flags,
                ..SearchResult::default()
            }
        })
        .collect())
}

fn download_url(config: &HdbitsConfig, id: &str) -> String {
    let mut url =
        Url::parse(&config.base_url).unwrap_or_else(|_| Url::parse(DEFAULT_BASE_URL).unwrap());
    url.set_path("download.php");
    url.query_pairs_mut()
        .append_pair("id", id)
        .append_pair("passkey", &config.api_key);
    url.to_string()
}

fn info_url(config: &HdbitsConfig, id: &str) -> String {
    let mut url =
        Url::parse(&config.base_url).unwrap_or_else(|_| Url::parse(DEFAULT_BASE_URL).unwrap());
    url.set_path("details.php");
    url.query_pairs_mut().append_pair("id", id);
    url.to_string()
}

fn config_csv_i64(key: &str, default_value: &str) -> Vec<i64> {
    config_value(key)
        .unwrap_or_else(|| default_value.to_string())
        .split([',', ';', '\n'])
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
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

struct HdbitsConfig {
    base_url: String,
    username: String,
    api_key: String,
    categories: Vec<i64>,
    codecs: Vec<i64>,
    mediums: Vec<i64>,
}

impl HdbitsConfig {
    fn from_extism() -> Result<Self, Error> {
        Ok(Self {
            base_url: config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            username: required_config("username")?,
            api_key: required_config("api_key")?,
            categories: config_csv_i64("categories", DEFAULT_CATEGORIES),
            codecs: config_csv_i64("codecs", ""),
            mediums: config_csv_i64("mediums", ""),
        })
    }
}

#[derive(Default, Serialize)]
struct TorrentQuery {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Passkey")]
    passkey: String,
    #[serde(rename = "Hash", skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(rename = "Search", skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(rename = "Category", skip_serializing_if = "Vec::is_empty")]
    category: Vec<i64>,
    #[serde(rename = "Codec", skip_serializing_if = "Vec::is_empty")]
    codec: Vec<i64>,
    #[serde(rename = "Medium", skip_serializing_if = "Vec::is_empty")]
    medium: Vec<i64>,
    #[serde(rename = "Origin", skip_serializing_if = "Option::is_none")]
    origin: Option<i64>,
    #[serde(rename = "tvdb", skip_serializing_if = "Option::is_none")]
    tvdb: Option<TvdbInfo>,
    #[serde(rename = "Limit", skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
    #[serde(rename = "Page", skip_serializing_if = "Option::is_none")]
    page: Option<i64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct TvdbInfo {
    #[serde(rename = "Id", alias = "id")]
    id: Option<i64>,
    #[serde(
        rename = "Season",
        alias = "season",
        skip_serializing_if = "Option::is_none"
    )]
    season: Option<i64>,
    #[serde(
        rename = "Episode",
        alias = "episode",
        skip_serializing_if = "Option::is_none"
    )]
    episode: Option<i64>,
}

#[derive(Deserialize)]
struct HdbitsResponse {
    #[serde(alias = "Status")]
    status: i64,
    #[serde(default, alias = "Message")]
    message: Option<String>,
    #[serde(alias = "Data")]
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct TorrentResponse {
    id: String,
    hash: String,
    leechers: i32,
    seeders: i32,
    name: String,
    #[serde(default)]
    times_completed: u32,
    size: i64,
    #[serde(default)]
    added: String,
    #[serde(default)]
    comments: u32,
    #[serde(default)]
    numfiles: u32,
    #[serde(default)]
    freeleech: Option<String>,
    #[serde(default)]
    type_category: i64,
    #[serde(default)]
    type_codec: i64,
    #[serde(default)]
    type_medium: i64,
    #[serde(default)]
    type_origin: i64,
    #[serde(default)]
    imdb: Option<ImdbInfo>,
    #[serde(default)]
    tvdb: Option<TvdbInfo>,
}

#[derive(Deserialize)]
struct ImdbInfo {
    #[serde(alias = "Id")]
    id: Option<i64>,
}
