use std::collections::HashMap;

use extism_pdk::*;
use newznab_common::{
    Capabilities, ConfigFieldDef, ConfigFieldRole, ConfigFieldType, IndexerCategoryModel,
    IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities,
    IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind,
    IndexerTorrentCapabilities, MetadataExtractor, NewznabConfig, NewznabHttpBehavior,
    PluginDescriptor, PluginResult, ProviderDescriptor, SDK_VERSION, SearchRequest, SearchResponse,
    current_sdk_constraint, execute_raw_search, extract_base_metadata,
};
use scryer_plugin_sdk::{ConfigFieldOption, ConfigFieldValueSource};

const PROVIDER_ID: &str = "animetosho-xyz";
const DEFAULT_BASE_URL: &str = "https://feed.animetosho.xyz";
const DEFAULT_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const PAGE_SIZE: usize = 200;
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

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
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
            source_kind: IndexerSourceKind::Generic,
            capabilities: Capabilities {
                supported_ids: HashMap::new(),
                deduplicates_aliases: false,
                season_param: None,
                episode_param: None,
                query_param: Some("q".into()),
                supported_query_facets: vec!["anime".to_string()],
                search: true,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: false,
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

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let mut req: SearchRequest = serde_json::from_str(&input)?;
    normalize_request(&mut req);

    let mode = DownloadMode::from_config()?;
    let config = animetosho_config(mode)?;
    let mut response = execute_raw_search(&config, &req, mode.extractor())?;
    annotate_response(&mut response, mode);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_indexer_action(input: String) -> FnResult<String> {
    Ok(newznab_common::execute_provider_action(&input)?)
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
                },
                ConfigFieldOption {
                    value: "torrent".to_string(),
                    label: "Torrent".to_string(),
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
        assert!(indexer.capabilities.supported_ids.is_empty());
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
    fn normalize_request_drops_id_search_shape() {
        let mut request = SearchRequest {
            query: "Frieren S02E01".to_string(),
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
}
