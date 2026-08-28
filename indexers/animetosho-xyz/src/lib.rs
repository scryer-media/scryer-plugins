use std::collections::{HashMap, HashSet};

use newznab_common::{
    Capabilities, ConfigFieldDef, ConfigFieldRole, ConfigFieldType, IndexerCategoryModel,
    IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities,
    IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind,
    IndexerTorrentCapabilities, MetadataExtractor, NewznabConfig, NewznabHttpBehavior,
    PluginActionRequest, PluginActionResponse, PluginDescriptor, ProviderDescriptor, SDK_VERSION,
    SearchRequest, SearchResponse, SearchResult, current_sdk_constraint, execute_full_search,
    extract_base_metadata, polite_http_get,
};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::{ConfigFieldOption, ConfigFieldValueSource};

const PROVIDER_ID: &str = "animetosho-xyz";
const DEFAULT_BASE_URL: &str = "https://feed.animetosho.xyz";
const DEFAULT_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const PAGE_SIZE: usize = 200;
const NATIVE_PAGE_SIZE: usize = 75;
const MAX_PAGES: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadMode {
    Nzb,
    Torrent,
}

impl DownloadMode {
    fn from_config() -> Result<Self, Error> {
        match config_string("download_mode")?
            .unwrap_or_else(|| "nzb".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "nzb" | "usenet" => Ok(Self::Nzb),
            "torrent" | "torznab" => Ok(Self::Torrent),
            other => Err(Error::msg(format!(
                "invalid download_mode '{other}', expected 'nzb' or 'torrent'"
            ))),
        }
    }

    fn api_path(self) -> &'static str {
        match self {
            Self::Nzb => "/api/newznab",
            Self::Torrent => "/api/torznab",
        }
    }

    fn source_kind(self) -> IndexerSourceKind {
        match self {
            Self::Nzb => IndexerSourceKind::Usenet,
            Self::Torrent => IndexerSourceKind::Torrent,
        }
    }

    fn protocol(self) -> IndexerProtocol {
        match self {
            Self::Nzb => IndexerProtocol::Usenet,
            Self::Torrent => IndexerProtocol::Torrent,
        }
    }

    fn extractor(self) -> MetadataExtractor {
        match self {
            Self::Nzb => extract_base_metadata,
            Self::Torrent => torrent_metadata_extractor,
        }
    }
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "AnimeTosho.xyz Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_ID.to_string(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            strategy_plan: Some(scryer_plugin_sdk::IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 4,
            }),
            source_kind: IndexerSourceKind::Generic,
            capabilities: Capabilities {
                supported_ids: HashMap::from([("anime".to_string(), vec!["anidb_id".to_string()])]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("ep".into()),
                query_param: Some("q".into()),
                supported_query_facets: vec!["anime".to_string()],
                search: true,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: true,
                rss: true,
                protocols: vec![IndexerProtocol::Usenet, IndexerProtocol::Torrent],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::IdQuery,
                    IndexerSearchInput::AggregateIdQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::AbsoluteEpisode,
                    IndexerSearchInput::Category,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec![
                    "tvdb_id".to_string(),
                    "tmdb_id".to_string(),
                    "anidb_id".to_string(),
                ],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(PAGE_SIZE as u32),
                    max_page_size: Some(PAGE_SIZE as u32),
                    max_pages: Some(MAX_PAGES as u32),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: true,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_leechers: true,
                    reports_info_hash: true,
                    reports_magnet_uri: true,
                    reports_volume_factors: true,
                    supports_private_tracker_flags: false,
                    supports_seed_requirements: false,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    grabs: true,
                    comments: true,
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
    }
}

async fn search(mut req: SearchRequest) -> FnResult<SearchResponse> {
    let mode = DownloadMode::from_config()?;
    let config = animetosho_config(mode)?;
    let mut response = if req.ids.contains_key("anidb_id") {
        execute_anidb_search(&config, &req, mode).await?
    } else {
        normalize_request(&mut req);
        execute_full_search(&config, &req, mode.extractor()).await?
    };
    annotate_response(&mut response, mode);
    Ok(response)
}

async fn action(request: PluginActionRequest) -> FnResult<PluginActionResponse> {
    newznab_common::execute_provider_action(request).await
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "base_url".to_string(),
            label: "Base URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: Some(DEFAULT_BASE_URL.to_string()),
            value_source: ConfigFieldValueSource::User,
            role: Some(ConfigFieldRole::ConnectionUrl),
            host_binding: None,
            options: vec![],
            help_text: Some("AnimeTosho.xyz feed API base URL".to_string()),
        },
        ConfigFieldDef {
            key: "api_key".to_string(),
            label: "API Key".to_string(),
            field_type: ConfigFieldType::Password,
            required: true,
            default_value: None,
            value_source: ConfigFieldValueSource::User,
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("AnimeTosho.xyz API key".to_string()),
        },
        ConfigFieldDef {
            key: "download_mode".to_string(),
            label: "Download Mode".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("nzb".to_string()),
            value_source: ConfigFieldValueSource::User,
            role: None,
            host_binding: None,
            options: vec![
                ConfigFieldOption {
                    value: "nzb".to_string(),
                    label: "NZB".to_string(),
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "torrent".to_string(),
                    label: "Torrent".to_string(),
                    config_overrides: Default::default(),
                },
            ],
            help_text: Some("Use NZB/Newznab results or torrent/Torznab results".to_string()),
        },
        ConfigFieldDef {
            key: "additional_params".to_string(),
            label: "Additional Parameters".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: ConfigFieldValueSource::User,
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("Extra query parameters appended to every request".to_string()),
        },
    ]
}

fn animetosho_config(mode: DownloadMode) -> Result<NewznabConfig, Error> {
    let base_url = config_string("base_url")?.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key =
        config_string("api_key")?.ok_or_else(|| Error::msg("api_key is not configured"))?;
    Ok(NewznabConfig {
        base_url,
        api_key,
        api_path: mode.api_path().to_string(),
        additional_params: config_string("additional_params")?.unwrap_or_default(),
        page_size: PAGE_SIZE,
        http_behavior: NewznabHttpBehavior {
            plugin_id: PROVIDER_ID.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            max_search_pages: MAX_PAGES,
            ..NewznabHttpBehavior::default()
        },
    })
}

fn normalize_request(req: &mut SearchRequest) {
    req.ids.clear();
    req.facet = Some("anime".to_string());
    req.category = Some("anime".to_string());
    req.season = None;
    req.episode = None;
    req.absolute_episode = None;
}

async fn execute_anidb_search(
    config: &NewznabConfig,
    req: &SearchRequest,
    mode: DownloadMode,
) -> Result<SearchResponse, Error> {
    let anidb_id = anidb_id(req)?;
    let query = native_episode_query(req);
    let max_results = if req.limit == 0 {
        NATIVE_PAGE_SIZE * MAX_PAGES
    } else {
        req.limit.min(NATIVE_PAGE_SIZE * MAX_PAGES)
    };
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for page in 1..=MAX_PAGES {
        let url = native_search_url(config, anidb_id, query.as_deref(), page);
        let (_, body) = polite_http_get(&url, "application/json", &config.http_behavior).await?;
        let items = serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .map_err(|error| Error::msg(format!("invalid AnimeTosho JSON response: {error}")))?;
        let page_len = items.len();

        for item in &items {
            let Some(result) = native_search_result(item, mode) else {
                continue;
            };
            let Some(download_url) = result.download_url.as_deref() else {
                continue;
            };
            if seen.insert(download_url.to_ascii_lowercase()) {
                results.push(result);
            }
            if results.len() >= max_results {
                break;
            }
        }

        if results.len() >= max_results || page_len < NATIVE_PAGE_SIZE {
            break;
        }
    }

    Ok(SearchResponse {
        results,
        ..SearchResponse::default()
    })
}

fn anidb_id(req: &SearchRequest) -> Result<u64, Error> {
    req.ids
        .get("anidb_id")
        .ok_or_else(|| Error::msg("anidb_id is required for AniDB search"))?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::msg("anidb_id must be a positive integer"))
}

fn native_episode_query(req: &SearchRequest) -> Option<String> {
    if let Some(absolute_episode) = req.absolute_episode {
        Some(absolute_episode.to_string())
    } else if let (Some(season), Some(episode)) = (req.season, req.episode) {
        Some(format!("S{season:02}E{episode:02}"))
    } else if let Some(episode) = req.episode {
        Some(format!("E{episode:02}"))
    } else if let Some(season) = req.season {
        Some(format!("S{season:02}"))
    } else {
        let query = req.query.trim();
        (!query.is_empty()).then(|| query.to_string())
    }
}

fn native_search_url(
    config: &NewznabConfig,
    anidb_id: u64,
    query: Option<&str>,
    page: usize,
) -> String {
    let mut params = config
        .additional_params
        .trim()
        .trim_start_matches(['?', '&'])
        .split('&')
        .filter(|param| !param.trim().is_empty())
        .filter(|param| {
            let key = param.split_once('=').map_or(*param, |(key, _)| key).trim();
            !matches!(
                key.to_ascii_lowercase().as_str(),
                "apikey" | "api_key" | "t" | "aid" | "aids" | "q" | "season" | "ep" | "page"
            )
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    params.push(format!("apikey={}", url_encode(&config.api_key)));
    params.push(format!("aid={anidb_id}"));
    if let Some(query) = query {
        params.push(format!("q={}", url_encode(query)));
    }
    params.push(format!("page={page}"));
    format!(
        "{}/json?{}",
        config.base_url.trim_end_matches('/'),
        params.join("&")
    )
}

fn native_search_result(item: &serde_json::Value, mode: DownloadMode) -> Option<SearchResult> {
    let title = item.get("title")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    let download_url = item
        .get(match mode {
            DownloadMode::Nzb => "nzb_url",
            DownloadMode::Torrent => "torrent_url",
        })?
        .as_str()?
        .trim();
    if download_url.is_empty() {
        return None;
    }

    let seeders = json_i64(item, "seeders");
    let leechers = json_i64(item, "leechers");
    let mut provider_extra = HashMap::new();
    for key in ["anidb_aid", "anidb_eid", "num_files"] {
        if let Some(value) = item.get(key).and_then(serde_json::Value::as_i64) {
            provider_extra.insert(key.to_string(), serde_json::Value::from(value));
        }
    }
    let external_ids = json_i64(item, "anidb_aid")
        .map(|id| HashMap::from([("anidb_id".to_string(), id.to_string())]))
        .unwrap_or_default();
    let info_hash_v1 = matches!(mode, DownloadMode::Torrent)
        .then(|| item.get("info_hash").and_then(serde_json::Value::as_str))
        .flatten()
        .map(normalize_info_hash)
        .filter(|hash| hash.len() == 40);
    let info_hash_v2 = matches!(mode, DownloadMode::Torrent)
        .then(|| item.get("info_hash_v2").and_then(serde_json::Value::as_str))
        .flatten()
        .map(normalize_info_hash)
        .filter(|hash| hash.len() == 64);
    let link = json_string(item, "link");

    Some(SearchResult {
        title: title.to_string(),
        link: link.clone(),
        download_url: Some(download_url.to_string()),
        size_bytes: json_i64(item, "total_size"),
        published_at: json_i64(item, "timestamp").map(format_timestamp),
        grabs: json_i64(item, "torrent_downloaded_count"),
        provider_extra,
        external_ids,
        magnet_url: matches!(mode, DownloadMode::Torrent)
            .then(|| json_string(item, "magnet_uri"))
            .flatten(),
        info_hash_v1,
        info_hash_v2,
        info_url: link,
        seeders: matches!(mode, DownloadMode::Torrent)
            .then_some(seeders)
            .flatten(),
        leechers: matches!(mode, DownloadMode::Torrent)
            .then_some(leechers)
            .flatten(),
        peers: matches!(mode, DownloadMode::Torrent)
            .then(|| {
                seeders
                    .zip(leechers)
                    .map(|(seeders, leechers)| seeders + leechers)
            })
            .flatten(),
        ..SearchResult::default()
    })
}

fn json_i64(item: &serde_json::Value, key: &str) -> Option<i64> {
    item.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

fn json_string(item: &serde_json::Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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

fn format_timestamp(timestamp: i64) -> String {
    const DAYS_IN_MONTH: [[i64; 12]; 2] = [
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    ];

    let second = timestamp.rem_euclid(60);
    let minutes = (timestamp - second) / 60;
    let minute = minutes.rem_euclid(60);
    let hours = (minutes - minute) / 60;
    let hour = hours.rem_euclid(24);
    let mut days = (hours - hour) / 24;
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap_index = usize::from(is_leap_year(year));
    let mut month = 0;
    while month < 12 && days >= DAYS_IN_MONTH[leap_index][month] {
        days -= DAYS_IN_MONTH[leap_index][month];
        month += 1;
    }

    format!(
        "{year:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}Z",
        month + 1,
        days + 1
    )
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn annotate_response(response: &mut SearchResponse, mode: DownloadMode) {
    for result in &mut response.results {
        result.source_kind = Some(mode.source_kind());
        result.protocol = Some(mode.protocol());
        result.provider_extra.insert(
            "download_mode".to_string(),
            serde_json::Value::from(match mode {
                DownloadMode::Nzb => "nzb",
                DownloadMode::Torrent => "torrent",
            }),
        );
    }
}

fn config_string(key: &str) -> Result<Option<String>, Error> {
    Ok(config::get(key)
        .map_err(|error| Error::msg(format!("failed to read config {key}: {error}")))?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn torrent_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut grabs = None;
    let mut seeders = None;
    let mut leechers = None;
    let mut peers = None;
    let mut downloads = None;
    let mut download_volume_factor = None;
    let mut upload_volume_factor = None;
    let mut info_hash = None;
    let mut magnet_uri = None;
    let mut languages = Vec::new();

    for (name, value) in pairs {
        let normalized = normalize_key(name);
        let trimmed = value.trim();
        match normalized.as_str() {
            "language" => languages.extend(split_multi_value(trimmed)),
            "grabs" => grabs = parse_i64(trimmed),
            "seeders" => seeders = parse_i64(trimmed),
            "leechers" => leechers = parse_i64(trimmed),
            "peers" => peers = parse_i64(trimmed),
            "downloads" => downloads = parse_i64(trimmed),
            "downloadvolumefactor" => download_volume_factor = parse_f64(trimmed),
            "uploadvolumefactor" => upload_volume_factor = parse_f64(trimmed),
            "infohash" => {
                let value = normalize_info_hash(trimmed);
                if !value.is_empty() {
                    info_hash = Some(value);
                }
            }
            "magneturl" if !trimmed.is_empty() => magnet_uri = Some(trimmed.to_string()),
            _ => {}
        }
    }

    let mut extra = HashMap::new();
    if let Some(value) = seeders {
        extra.insert("seeders".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = leechers {
        extra.insert("leechers".to_string(), serde_json::Value::from(value));
    }
    let derived_peers = peers.or_else(|| {
        seeders
            .zip(leechers)
            .map(|(seeders, leechers)| seeders + leechers)
    });
    if let Some(value) = derived_peers {
        extra.insert("peers".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = downloads {
        extra.insert("downloads".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = download_volume_factor {
        extra.insert(
            "downloadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
        if (value - 0.0).abs() < f64::EPSILON {
            extra.insert("freeleech".to_string(), serde_json::Value::from(true));
        }
    }
    if let Some(value) = upload_volume_factor {
        extra.insert(
            "uploadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
    }
    if let Some(ref value) = info_hash {
        extra.insert(
            "info_hash".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    if let Some(value) = magnet_uri {
        extra.insert("magnet_uri".to_string(), serde_json::Value::from(value));
    }

    (dedupe(languages), grabs, extra)
}

fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn split_multi_value(value: &str) -> Vec<String> {
    value
        .split(['/', '|', ','])
        .flat_map(|part| part.split(" - "))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_i64(value: &str) -> Option<i64> {
    value.replace(',', "").parse::<i64>().ok()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.replace(',', "").parse::<f64>().ok()
}

fn normalize_info_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if out
            .iter()
            .all(|existing: &String| !existing.eq_ignore_ascii_case(&value))
        {
            out.push(value);
        }
    }
    out
}

scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
    action = action,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn descriptor_requires_api_key_and_supports_both_protocols() {
        let descriptor = build_descriptor();
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        let api_key = indexer
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api_key field");
        assert!(api_key.required);
        assert_eq!(
            indexer.capabilities.protocols,
            vec![IndexerProtocol::Usenet, IndexerProtocol::Torrent]
        );
        assert_eq!(
            indexer.capabilities.supported_ids.get("anime"),
            Some(&vec!["anidb_id".to_string()])
        );
        assert_eq!(indexer.capabilities.season_param.as_deref(), Some("season"));
        assert_eq!(indexer.capabilities.episode_param.as_deref(), Some("ep"));
        assert!(indexer.capabilities.anidb_search);
        assert!(
            indexer
                .capabilities
                .search_inputs
                .contains(&IndexerSearchInput::AbsoluteEpisode)
        );
        assert_eq!(
            indexer
                .capabilities
                .limits
                .as_ref()
                .and_then(|limits| limits.page_size),
            Some(PAGE_SIZE as u32)
        );
    }

    #[test]
    fn torrent_metadata_extracts_peer_fields() {
        let (_, _, extra) = torrent_metadata_extractor(&pairs(&[
            ("seeders", "3"),
            ("leechers", "28"),
            ("peers", "31"),
            ("infohash", "7E189F4382634CC21D2A31E5106C8CB6894A2C83"),
            ("magneturl", "magnet:?xt=urn:btih:abc"),
            ("downloadvolumefactor", "0"),
        ]));

        assert_eq!(extra.get("seeders"), Some(&serde_json::Value::from(3)));
        assert_eq!(extra.get("leechers"), Some(&serde_json::Value::from(28)));
        assert_eq!(extra.get("peers"), Some(&serde_json::Value::from(31)));
        assert_eq!(
            extra.get("info_hash"),
            Some(&serde_json::Value::from(
                "7e189f4382634cc21d2a31e5106c8cb6894a2c83"
            ))
        );
        assert_eq!(extra.get("freeleech"), Some(&serde_json::Value::from(true)));
    }

    #[test]
    fn normalize_text_request_drops_id_search_shape() {
        let mut request = SearchRequest {
            query: "Example Animation S02E01".to_string(),
            ids: HashMap::from([("tvdb_id".to_string(), "424536".to_string())]),
            season: Some(2),
            episode: Some(1),
            categories: vec!["5070".to_string()],
            ..SearchRequest::default()
        };

        normalize_request(&mut request);

        assert!(request.ids.is_empty());
        assert_eq!(request.facet.as_deref(), Some("anime"));
        assert_eq!(request.category.as_deref(), Some("anime"));
        assert_eq!(request.season, None);
        assert_eq!(request.episode, None);
    }

    #[test]
    fn anidb_season_search_uses_native_id_and_structured_query() {
        let request = SearchRequest {
            query: "Synthetic Animation S02E03".to_string(),
            ids: HashMap::from([
                ("anidb_id".to_string(), "01535".to_string()),
                ("tvdb_id".to_string(), "424536".to_string()),
            ]),
            season: Some(2),
            episode: Some(3),
            ..SearchRequest::default()
        };
        let config = NewznabConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: "test-key".to_string(),
            api_path: "/api/newznab".to_string(),
            additional_params: "foo=bar&q=stale&aids=1&season=9&ep=9&t=tvsearch".to_string(),
            page_size: PAGE_SIZE,
            http_behavior: NewznabHttpBehavior::default(),
        };

        assert_eq!(anidb_id(&request).expect("valid AniDB ID"), 1535);
        assert_eq!(native_episode_query(&request).as_deref(), Some("S02E03"));
        let url = native_search_url(&config, 1535, Some("S02E03"), 1);

        assert_eq!(
            url,
            "https://feed.animetosho.xyz/json?foo=bar&apikey=test-key&aid=1535&q=S02E03&page=1"
        );
        assert!(!url.contains("Synthetic"));
    }

    #[test]
    fn anidb_absolute_search_uses_absolute_number() {
        let request = SearchRequest {
            query: "Synthetic Animation 21".to_string(),
            ids: HashMap::from([("anidb_id".to_string(), "1535".to_string())]),
            season: Some(2),
            episode: Some(3),
            absolute_episode: Some(21),
            ..SearchRequest::default()
        };
        let config = NewznabConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: "test-key".to_string(),
            api_path: "/api/torznab".to_string(),
            additional_params: String::new(),
            page_size: PAGE_SIZE,
            http_behavior: NewznabHttpBehavior::default(),
        };

        assert_eq!(native_episode_query(&request).as_deref(), Some("21"));
        assert_eq!(
            native_search_url(&config, 1535, Some("21"), 1),
            "https://feed.animetosho.xyz/json?apikey=test-key&aid=1535&q=21&page=1"
        );
    }

    #[test]
    fn invalid_anidb_id_is_rejected() {
        let request = SearchRequest {
            query: "Synthetic Animation S02E03".to_string(),
            ids: HashMap::from([("anidb_id".to_string(), "not-an-id".to_string())]),
            season: Some(2),
            episode: Some(3),
            ..SearchRequest::default()
        };
        let error = anidb_id(&request).expect_err("invalid AniDB IDs must not become searches");
        assert_eq!(error.to_string(), "anidb_id must be a positive integer");
    }

    #[test]
    fn native_torrent_result_preserves_identity_and_peer_metadata() {
        let item = serde_json::json!({
            "title": "Synthetic Animation S02E03 1080p WEB-DL",
            "link": "https://example.test/view/42",
            "torrent_url": "https://example.test/download/42/torrent",
            "nzb_url": "https://example.test/nzb/42",
            "timestamp": 1_700_000_000_i64,
            "total_size": 1_234_567_i64,
            "torrent_downloaded_count": 9,
            "anidb_aid": 1535,
            "anidb_eid": 9001,
            "num_files": 1,
            "info_hash": "7E189F4382634CC21D2A31E5106C8CB6894A2C83",
            "magnet_uri": "magnet:?xt=urn:btih:7e189f4382634cc21d2a31e5106c8cb6894a2c83",
            "seeders": 3,
            "leechers": 28
        });

        let result = native_search_result(&item, DownloadMode::Torrent)
            .expect("synthetic native result should map");

        assert_eq!(
            result.download_url.as_deref(),
            Some("https://example.test/download/42/torrent")
        );
        assert_eq!(
            result.external_ids.get("anidb_id").map(String::as_str),
            Some("1535")
        );
        assert_eq!(
            result.info_hash_v1.as_deref(),
            Some("7e189f4382634cc21d2a31e5106c8cb6894a2c83")
        );
        assert_eq!(result.seeders, Some(3));
        assert_eq!(result.leechers, Some(28));
        assert_eq!(result.peers, Some(31));
        assert_eq!(result.published_at.as_deref(), Some("2023-11-14T22:13:20Z"));
    }
}
