use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
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
use serde::Deserialize;
use url::Url;

const DEFAULT_BASE_URL: &str = "https://filelist.io";
const DEFAULT_CATEGORIES: &str = "23,21,27";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "filelist".to_string(),
        name: "FileList Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "filelist".to_string(),
            provider_aliases: vec!["filelist.io".to_string()],
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                supported_ids: HashMap::from([("series".to_string(), vec!["imdb_id".to_string()])]),
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                query_param: Some("q".to_string()),
                supported_query_facets: vec!["series".to_string()],
                search: true,
                imdb_search: true,
                tvdb_search: false,
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
                    IndexerSearchInput::TextQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::AbsoluteEpisode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["imdb_id".to_string()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(200),
                    max_page_size: Some(200),
                    rate_limit_hint_seconds: Some(2),
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_info_hash: false,
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
    let config = FileListConfig::from_extism()?;
    let urls = build_urls(&config, &req);
    let mut results = Vec::new();

    for url in urls {
        let body = get_json(&url, &config.username, &config.passkey)?;
        let mut parsed = parse_torrents(&config, &body)?;
        results.append(&mut parsed);
    }

    let limit = if req.limit == 0 {
        200
    } else {
        req.limit.min(200)
    };
    let results = dedupe_results(results).into_iter().take(limit).collect();
    Ok(serde_json::to_string(&PluginResult::Ok(SearchResponse {
        results,
        ..Default::default()
    }))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            None,
            Some("FileList username"),
        ),
        field(
            "passkey",
            "Passkey",
            ConfigFieldType::Password,
            true,
            None,
            Some("FileList passkey"),
        ),
        connection_field(
            "base_url",
            "API URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("FileList site URL"),
        ),
        field(
            "categories",
            "Categories",
            ConfigFieldType::String,
            true,
            Some(DEFAULT_CATEGORIES),
            Some("Comma-separated FileList category IDs"),
        ),
        field(
            "anime_categories",
            "Anime Categories",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma-separated FileList anime category IDs"),
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

fn build_urls(config: &FileListConfig, req: &SearchRequest) -> Vec<String> {
    let season_episode = match (req.season, req.episode) {
        (Some(season), Some(episode)) => format!("&season={season}&episode={episode}"),
        (Some(season), None) => format!("&season={season}"),
        _ => String::new(),
    };

    let mut urls = Vec::new();
    let categories = if req.absolute_episode.is_some() {
        &config.anime_categories
    } else {
        &config.categories
    };

    if req.query.trim().is_empty()
        && req.ids.is_empty()
        && req.season.is_none()
        && req.episode.is_none()
        && req.absolute_episode.is_none()
    {
        urls.push(request_url(
            &config.base_url,
            "latest-torrents",
            &union_categories(&config.categories, &config.anime_categories),
            "",
        ));
        return urls;
    }

    if categories.is_empty() {
        return urls;
    }

    if let Some(imdb_id) = req.ids.get("imdb_id").filter(|value| !value.is_empty()) {
        if let Some(absolute_episode) = req.absolute_episode {
            urls.push(request_url(
                &config.base_url,
                "search-torrents",
                categories,
                &format!("&type=imdb&query={imdb_id}&season=0&episode={absolute_episode}"),
            ));
        }
        if req.absolute_episode.is_none() || !season_episode.is_empty() {
            urls.push(request_url(
                &config.base_url,
                "search-torrents",
                categories,
                &format!("&type=imdb&query={imdb_id}{season_episode}"),
            ));
        }
    }

    if !req.query.trim().is_empty() {
        let encoded = urlencoding::encode(req.query.trim());
        if let Some(absolute_episode) = req.absolute_episode {
            urls.push(request_url(
                &config.base_url,
                "search-torrents",
                categories,
                &format!("&type=name&query={encoded}&season=0&episode={absolute_episode}"),
            ));
        }
        if req.absolute_episode.is_none() || !season_episode.is_empty() {
            urls.push(request_url(
                &config.base_url,
                "search-torrents",
                categories,
                &format!("&type=name&query={encoded}{season_episode}"),
            ));
        }
    }

    urls
}

fn request_url(base_url: &str, action: &str, categories: &[i64], params: &str) -> String {
    let categories = categories
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}/api.php?action={action}&category={categories}{params}",
        base_url.trim_end_matches('/')
    )
}

fn get_json(url: &str, username: &str, passkey: &str) -> Result<String, Error> {
    let request = HttpRequest::new(url)
        .with_header("Accept", "application/json")
        .with_header("User-Agent", "Scryer FileList Indexer/0.1")
        .with_header(
            "Authorization",
            format!("Basic {}", STANDARD.encode(format!("{username}:{passkey}"))),
        );
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("FileList request failed: {error}")))?;
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).to_string();
    if status != 200 {
        return Err(Error::msg(format!(
            "FileList API returned HTTP {status}: {body}"
        )));
    }
    Ok(body)
}

fn parse_torrents(config: &FileListConfig, body: &str) -> Result<Vec<SearchResult>, Error> {
    let torrents: Vec<FileListTorrent> = serde_json::from_str(body)
        .map_err(|error| Error::msg(format!("FileList JSON parse failed: {error}")))?;
    Ok(torrents
        .into_iter()
        .map(|torrent| {
            let seeders = torrent.seeders as i64;
            let leechers = torrent.leechers as i64;
            let mut external_ids = HashMap::new();
            if let Some(imdb_id) = normalize_imdb(torrent.imdb.as_deref()) {
                external_ids.insert("imdb_id".to_string(), imdb_id);
            }
            let mut indexer_flags = Vec::new();
            if torrent.freeleech {
                indexer_flags.push("freeleech".to_string());
            }
            if torrent.internal {
                indexer_flags.push("internal".to_string());
            }
            let mut provider_extra = HashMap::new();
            provider_extra.insert("files".to_string(), serde_json::Value::from(torrent.files));
            provider_extra.insert(
                "comments".to_string(),
                serde_json::Value::from(torrent.comments),
            );
            provider_extra.insert(
                "times_completed".to_string(),
                serde_json::Value::from(torrent.times_completed),
            );

            SearchResult {
                title: torrent.name,
                download_url: Some(download_url(config, &torrent.id)),
                size_bytes: Some(torrent.size),
                published_at: Some(torrent.upload_date),
                grabs: Some(torrent.times_completed as i64),
                thumbs_up: Some(torrent.seeders),
                thumbs_down: Some(torrent.leechers),
                provider_extra,
                guid: Some(format!("FileList-{}", torrent.id)),
                info_url: Some(info_url(config, &torrent.id)),
                source_kind: Some(IndexerSourceKind::Torrent),
                protocol: Some(IndexerProtocol::Torrent),
                external_ids,
                seeders: Some(seeders),
                peers: Some(seeders + leechers),
                leechers: Some(leechers),
                indexer_flags,
                ..SearchResult::default()
            }
        })
        .collect())
}

fn download_url(config: &FileListConfig, id: &str) -> String {
    let mut url =
        Url::parse(&config.base_url).unwrap_or_else(|_| Url::parse(DEFAULT_BASE_URL).unwrap());
    url.set_path("download.php");
    url.query_pairs_mut()
        .append_pair("id", id)
        .append_pair("passkey", &config.passkey);
    url.to_string()
}

fn info_url(config: &FileListConfig, id: &str) -> String {
    let mut url =
        Url::parse(&config.base_url).unwrap_or_else(|_| Url::parse(DEFAULT_BASE_URL).unwrap());
    url.set_path("details.php");
    url.query_pairs_mut().append_pair("id", id);
    url.to_string()
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

fn union_categories(left: &[i64], right: &[i64]) -> Vec<i64> {
    let mut out = left.to_vec();
    for value in right {
        if !out.contains(value) {
            out.push(*value);
        }
    }
    out
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

struct FileListConfig {
    base_url: String,
    username: String,
    passkey: String,
    categories: Vec<i64>,
    anime_categories: Vec<i64>,
}

impl FileListConfig {
    fn from_extism() -> Result<Self, Error> {
        let categories = config_csv_i64("categories", DEFAULT_CATEGORIES);
        let anime_categories = config_csv_i64("anime_categories", "");
        if categories.is_empty() && anime_categories.is_empty() {
            return Err(Error::msg(
                "FileList requires categories or anime_categories configuration",
            ));
        }
        Ok(Self {
            base_url: config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            username: required_config("username")?,
            passkey: required_config("passkey")?,
            categories,
            anime_categories,
        })
    }
}

#[derive(Deserialize)]
struct FileListTorrent {
    id: String,
    name: String,
    size: i64,
    leechers: i32,
    seeders: i32,
    #[serde(default)]
    times_completed: u32,
    #[serde(default)]
    comments: u32,
    #[serde(default)]
    files: u32,
    #[serde(default, rename = "imdb")]
    imdb: Option<String>,
    #[serde(default)]
    internal: bool,
    #[serde(default, rename = "freeleech")]
    freeleech: bool,
    #[serde(default, rename = "upload_date")]
    upload_date: String,
}
