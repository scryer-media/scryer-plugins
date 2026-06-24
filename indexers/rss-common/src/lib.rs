use std::collections::HashMap;
use std::sync::OnceLock;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use regex::Regex;
pub use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    IndexerCapabilities as Capabilities, IndexerCategoryModel, IndexerCategoryValueKind,
    IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, IndexerTorrentCapabilities,
    PluginDescriptor, PluginResult, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult, ProviderDescriptor,
    SDK_VERSION, current_sdk_constraint,
};
use url::Url;

#[derive(Default)]
struct ParsedItem {
    title: Option<String>,
    link: Option<String>,
    guid: Option<String>,
    description: Option<String>,
    published_at: Option<String>,
    enclosure_url: Option<String>,
    enclosure_type: Option<String>,
    enclosure_length: Option<i64>,
    categories: Vec<String>,
    attrs: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadPreference {
    Auto,
    Enclosure,
    Magnet,
    Link,
    Guid,
}

#[derive(Clone, Copy, Debug)]
pub struct RssParseOptions {
    pub provider_tag: &'static str,
    pub source_kind: IndexerSourceKind,
    pub protocol: IndexerProtocol,
    pub download_preference: DownloadPreference,
    pub use_guid_info_url: bool,
    pub use_enclosure_url: bool,
    pub use_enclosure_length: bool,
    pub size_element_name: Option<&'static str>,
    pub info_hash_element_name: Option<&'static str>,
    pub peers_element_name: Option<&'static str>,
    pub seeds_element_name: Option<&'static str>,
    pub leechers_element_name: Option<&'static str>,
    pub magnet_element_name: Option<&'static str>,
    pub calculate_peers_as_sum: bool,
    pub parse_size_in_description: bool,
    pub parse_seeders_in_description: bool,
    pub page_size: usize,
}

impl RssParseOptions {
    pub fn torrent(provider_tag: &'static str) -> Self {
        Self {
            provider_tag,
            source_kind: IndexerSourceKind::Torrent,
            protocol: IndexerProtocol::Torrent,
            download_preference: DownloadPreference::Auto,
            use_guid_info_url: false,
            use_enclosure_url: false,
            use_enclosure_length: false,
            size_element_name: None,
            info_hash_element_name: None,
            peers_element_name: None,
            seeds_element_name: None,
            leechers_element_name: None,
            magnet_element_name: None,
            calculate_peers_as_sum: false,
            parse_size_in_description: false,
            parse_seeders_in_description: false,
            page_size: 200,
        }
    }

    pub fn usenet(provider_tag: &'static str) -> Self {
        Self {
            provider_tag,
            source_kind: IndexerSourceKind::Usenet,
            protocol: IndexerProtocol::Usenet,
            download_preference: DownloadPreference::Auto,
            use_guid_info_url: false,
            use_enclosure_url: false,
            use_enclosure_length: false,
            size_element_name: None,
            info_hash_element_name: None,
            peers_element_name: None,
            seeds_element_name: None,
            leechers_element_name: None,
            magnet_element_name: None,
            calculate_peers_as_sum: false,
            parse_size_in_description: false,
            parse_seeders_in_description: false,
            page_size: 200,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RssHttpConfig {
    pub user_agent: String,
    pub cookie: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub additional_headers: String,
}

impl RssHttpConfig {
    pub fn from_extism(default_user_agent: &str) -> Self {
        Self {
            user_agent: config_value("user_agent")
                .unwrap_or_else(|| default_user_agent.to_string()),
            cookie: config_value("cookie"),
            username: config_value("username"),
            password: config_value("password"),
            additional_headers: config_value("additional_headers").unwrap_or_default(),
        }
    }
}

pub struct DescriptorSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub provider_type: &'static str,
    pub provider_aliases: Vec<String>,
    pub source_kind: IndexerSourceKind,
    pub protocols: Vec<IndexerProtocol>,
    pub search: bool,
    pub rss: bool,
    pub query_only: bool,
    pub feed_modes: Vec<IndexerFeedMode>,
    pub search_inputs: Vec<IndexerSearchInput>,
    pub config_fields: Vec<ConfigFieldDef>,
    pub rate_limit_seconds: Option<u32>,
    pub page_size: Option<u32>,
    pub torrent: Option<IndexerTorrentCapabilities>,
}

pub fn build_indexer_descriptor(spec: DescriptorSpec) -> PluginDescriptor {
    let max_page_size = spec.page_size;
    let (supported_ids, supported_external_ids) = if spec.query_only {
        (HashMap::new(), Vec::new())
    } else {
        (
            HashMap::from([(
                "anime".to_string(),
                vec!["tvdb_id".to_string(), "anidb_id".to_string()],
            )]),
            vec!["tvdb_id".to_string(), "anidb_id".to_string()],
        )
    };
    PluginDescriptor {
        id: spec.id.to_string(),
        name: spec.name.to_string(),
        version: spec.version.to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: spec.provider_type.to_string(),
            provider_aliases: spec.provider_aliases,
            source_kind: spec.source_kind,
            capabilities: Capabilities {
                supported_ids,
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                query_param: Some("q".to_string()),
                search: spec.search,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: false,
                rss: spec.rss,
                protocols: spec.protocols,
                feed_modes: spec.feed_modes,
                search_inputs: spec.search_inputs,
                supported_external_ids,
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::String],
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: spec.page_size,
                    max_page_size,
                    rate_limit_hint_seconds: spec.rate_limit_seconds,
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: spec.torrent,
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    grabs: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: spec.config_fields,
            allowed_hosts: vec![],
            rate_limit_seconds: spec.rate_limit_seconds.map(i64::from),
        }),
    }
}

pub fn field(
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

pub fn connection_field(
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

pub fn select_field(
    key: &str,
    label: &str,
    default_value: Option<&str>,
    options: &[(&str, &str)],
) -> ConfigFieldDef {
    ConfigFieldDef {
        options: options
            .iter()
            .map(|(value, label)| ConfigFieldOption {
                value: (*value).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
        ..field(
            key,
            label,
            ConfigFieldType::Select,
            false,
            default_value,
            None,
        )
    }
}

pub fn http_config_fields(default_user_agent: &str) -> Vec<ConfigFieldDef> {
    vec![
        field(
            "user_agent",
            "User Agent",
            ConfigFieldType::String,
            false,
            Some(default_user_agent),
            Some("Optional custom User-Agent header"),
        ),
        field(
            "cookie",
            "Cookie Header",
            ConfigFieldType::Password,
            false,
            None,
            Some("Optional raw Cookie header for private feeds"),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some("Optional username for HTTP basic auth"),
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("Optional password for HTTP basic auth"),
        ),
        field(
            "additional_headers",
            "Additional Headers",
            ConfigFieldType::Multiline,
            false,
            None,
            Some("Optional extra headers, one per line, formatted as Header-Name: value"),
        ),
    ]
}

pub fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn required_config(key: &str) -> Result<String, Error> {
    config_value(key).ok_or_else(|| Error::msg(format!("{key} is not configured")))
}

pub fn config_bool(key: &str) -> bool {
    config_value(key)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub fn execute_rss_urls(
    provider_id: &str,
    urls: &[String],
    http_config: &RssHttpConfig,
    req: &SearchRequest,
    options: RssParseOptions,
) -> Result<SearchResponse, Error> {
    let limit = if req.limit == 0 {
        options.page_size
    } else {
        req.limit.min(options.page_size)
    };
    let mut results = Vec::new();

    for url in urls {
        let body = fetch_feed(provider_id, url, http_config)?;
        results.extend(parse_rss_feed(&body, url, options));
        if results.len() >= limit {
            break;
        }
    }

    let results = dedupe_results(filter_results(results, req))
        .into_iter()
        .take(limit)
        .collect();

    Ok(SearchResponse {
        results,
        ..Default::default()
    })
}

pub fn fetch_feed(
    provider_id: &str,
    feed_url: &str,
    config: &RssHttpConfig,
) -> Result<String, Error> {
    let logged_url = redact_url_for_log(feed_url);
    let mut request = HttpRequest::new(feed_url)
        .with_header(
            "Accept",
            "application/rss+xml, application/xml, text/xml;q=0.9, */*;q=0.8",
        )
        .with_header("User-Agent", config.user_agent.as_str())
        .with_header("Accept-Language", "en-US,en;q=0.9");

    if let Some(cookie) = config.cookie.as_deref() {
        request = request.with_header("Cookie", cookie);
    }

    if let Some(username) = config.username.as_deref() {
        let password = config.password.as_deref().unwrap_or_default();
        let encoded = STANDARD.encode(format!("{username}:{password}"));
        request = request.with_header("Authorization", format!("Basic {encoded}"));
    }

    for line in config.additional_headers.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let header_name = name.trim();
            let header_value = value.trim();
            if !header_name.is_empty() && !header_value.is_empty() {
                request = request.with_header(header_name, header_value);
            }
        }
    }

    log!(
        LogLevel::Debug,
        "http_trace plugin={} method=GET attempt=1 url={}",
        provider_id,
        logged_url
    );

    let response = http::request::<Vec<u8>>(&request, None).map_err(|e| {
        log!(
            LogLevel::Debug,
            "http_trace_error plugin={} method=GET attempt=1 url={} error={}",
            provider_id,
            logged_url,
            e
        );
        Error::msg(format!("HTTP request failed: {e}"))
    })?;
    let status = response.status_code();
    log!(
        LogLevel::Debug,
        "http_trace_response plugin={} method=GET attempt=1 status={} url={}",
        provider_id,
        status,
        logged_url
    );
    if status >= 400 {
        return Err(Error::msg(format!("RSS feed returned HTTP {status}")));
    }

    Ok(String::from_utf8_lossy(&response.body()).to_string())
}

pub fn redact_url_for_log(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };

    let redacted_query = query
        .split('&')
        .map(|pair| {
            let Some((key, value)) = pair.split_once('=') else {
                return pair.to_string();
            };

            if matches!(
                key.trim().to_ascii_lowercase().as_str(),
                "apikey" | "api_key" | "token" | "key" | "password" | "pass" | "passkey"
            ) {
                format!("{key}=REDACTED")
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    format!("{base}?{redacted_query}")
}

pub fn parse_rss_feed(body: &str, feed_url: &str, options: RssParseOptions) -> Vec<SearchResult> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut results = Vec::new();
    let mut item = ParsedItem::default();
    let mut in_item = false;
    let mut current_tag: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref event)) => {
                let name = tag_name(event);
                let local = local_name(&name);
                if local == "item" {
                    in_item = true;
                    item = ParsedItem::default();
                    current_tag = None;
                } else if in_item {
                    match local.as_str() {
                        "title" | "link" | "guid" | "description" | "pubDate" | "category" => {
                            current_tag = Some(local);
                        }
                        "enclosure" => {
                            parse_enclosure(event, &mut item);
                            current_tag = None;
                        }
                        _ if is_option_element(&local, options) => {
                            current_tag = Some(local);
                        }
                        _ => current_tag = None,
                    }
                }
            }
            Ok(Event::Empty(ref event)) if in_item => {
                let name = tag_name(event);
                let local = local_name(&name);
                if local == "enclosure" {
                    parse_enclosure(event, &mut item);
                } else if local == "attr"
                    && let Some(pair) = parse_attr_pair(event)
                {
                    item.attrs.push(pair);
                }
            }
            Ok(Event::Text(text)) if in_item => {
                apply_text(
                    &mut item,
                    current_tag.as_deref(),
                    decode_text(text.as_ref()),
                    options,
                );
            }
            Ok(Event::CData(text)) if in_item => {
                apply_text(
                    &mut item,
                    current_tag.as_deref(),
                    decode_text(text.as_ref()),
                    options,
                );
            }
            Ok(Event::End(ref event)) => {
                let name = String::from_utf8_lossy(event.name().as_ref()).to_string();
                let local = local_name(&name);
                if local == "item" {
                    in_item = false;
                    current_tag = None;
                    if let Some(result) = build_result(item, feed_url, options) {
                        results.push(result);
                    }
                    item = ParsedItem::default();
                } else if current_tag.as_deref() == Some(local.as_str()) {
                    current_tag = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                log!(
                    LogLevel::Debug,
                    "rss_parse_error plugin={} error={}",
                    options.provider_tag,
                    error
                );
                break;
            }
            _ => {}
        }

        buf.clear();
    }

    results
}

fn tag_name(event: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(event.name().as_ref()).to_string()
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

fn is_option_element(local: &str, options: RssParseOptions) -> bool {
    [
        options.size_element_name,
        options.info_hash_element_name,
        options.peers_element_name,
        options.seeds_element_name,
        options.leechers_element_name,
        options.magnet_element_name,
    ]
    .into_iter()
    .flatten()
    .any(|name| name.eq_ignore_ascii_case(local))
}

fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn apply_text(
    item: &mut ParsedItem,
    current_tag: Option<&str>,
    value: String,
    options: RssParseOptions,
) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }

    match current_tag.unwrap_or_default() {
        "title" => merge_text(&mut item.title, trimmed),
        "link" => merge_text(&mut item.link, trimmed),
        "guid" => merge_text(&mut item.guid, trimmed),
        "description" => merge_text(&mut item.description, trimmed),
        "pubDate" => merge_text(&mut item.published_at, trimmed),
        "category" => item.categories.push(trimmed.to_string()),
        tag if is_option_element(tag, options) => {
            item.attrs.push((tag.to_string(), trimmed.to_string()));
        }
        _ => {}
    }
}

fn merge_text(slot: &mut Option<String>, value: &str) {
    match slot {
        Some(existing) => existing.push_str(value),
        None => *slot = Some(value.to_string()),
    }
}

fn parse_enclosure(event: &BytesStart<'_>, item: &mut ParsedItem) {
    for attr in event.attributes().flatten() {
        let key = attr.key.as_ref();
        let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();
        match key {
            b"url" => item.enclosure_url = Some(value),
            b"length" => item.enclosure_length = value.replace(',', "").parse::<i64>().ok(),
            b"type" => item.enclosure_type = Some(value),
            _ => {}
        }
    }
}

fn parse_attr_pair(event: &BytesStart<'_>) -> Option<(String, String)> {
    let mut name = None;
    let mut value = None;
    for attr in event.attributes().flatten() {
        match attr.key.as_ref() {
            b"name" => name = Some(String::from_utf8_lossy(attr.value.as_ref()).to_string()),
            b"value" => value = Some(String::from_utf8_lossy(attr.value.as_ref()).to_string()),
            _ => {}
        }
    }
    Some((name?, value?))
}

fn build_result(
    item: ParsedItem,
    feed_url: &str,
    options: RssParseOptions,
) -> Option<SearchResult> {
    let title = item.title.clone()?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let mut provider_extra = HashMap::new();
    let mut external_ids = HashMap::new();
    let mut languages = Vec::new();
    let mut grabs = None;
    let mut seeders = None;
    let mut peers = None;
    let mut leechers = None;
    let mut info_hash_v1 = None;
    let mut magnet_url = find_first_magnet_in_sources(&[
        item.enclosure_url.as_deref(),
        item.link.as_deref(),
        item.guid.as_deref(),
        item.description.as_deref(),
    ]);
    let mut size_bytes = if options.use_enclosure_length {
        item.enclosure_length
    } else {
        None
    };
    let mut download_volume_factor = None;
    let mut upload_volume_factor = None;
    let mut minimum_seed_ratio = None;
    let mut minimum_seed_time_minutes = None;
    let mut indexer_flags = Vec::new();

    for (name, value) in &item.attrs {
        let normalized = normalize_key(name);
        let trimmed = value.trim();
        provider_extra.insert(
            format!("raw_{normalized}"),
            serde_json::Value::from(trimmed.to_string()),
        );
        match normalized.as_str() {
            "language" => languages.extend(split_multi_value(trimmed)),
            "grabs" | "downloads" => {
                grabs = parse_i64(trimmed).or(grabs);
                if let Some(value) = parse_i64(trimmed) {
                    provider_extra.insert("downloads".to_string(), serde_json::Value::from(value));
                }
            }
            "seeders" | "seeds" => {
                seeders = parse_i64(trimmed).or(seeders);
            }
            "peers" => {
                peers = parse_i64(trimmed).or(peers);
            }
            "leechers" => {
                leechers = parse_i64(trimmed).or(leechers);
                if options.peers_element_name == Some(name.as_str()) {
                    peers = parse_i64(trimmed).or(peers);
                }
            }
            "downloadvolumefactor" => {
                if let Some(value) = parse_f64(trimmed) {
                    download_volume_factor = Some(value);
                    if (value - 0.0).abs() < f64::EPSILON {
                        indexer_flags.push("freeleech".to_string());
                        provider_extra
                            .insert("freeleech".to_string(), serde_json::Value::from(true));
                    }
                }
            }
            "uploadvolumefactor" => {
                upload_volume_factor = parse_f64(trimmed).or(upload_volume_factor)
            }
            "minimumratio" => minimum_seed_ratio = parse_f64(trimmed).or(minimum_seed_ratio),
            "minimumseedtime" => {
                minimum_seed_time_minutes = parse_i64(trimmed).or(minimum_seed_time_minutes);
            }
            "infohash" | "infohashv1" => {
                let normalized_hash = normalize_info_hash(trimmed);
                if !normalized_hash.is_empty() {
                    info_hash_v1 = Some(normalized_hash);
                }
            }
            "magnet" | "magneturl" if trimmed.starts_with("magnet:") => {
                magnet_url = Some(trimmed.to_string());
            }
            "imdb" | "imdbid" => {
                let normalized_id = normalize_imdb(trimmed);
                if !normalized_id.is_empty() {
                    external_ids.insert("imdb_id".to_string(), normalized_id);
                }
            }
            "tvdbid" if !trimmed.is_empty() && trimmed != "0" => {
                external_ids.insert("tvdb_id".to_string(), trimmed.to_string());
            }
            "anidbid" if !trimmed.is_empty() && trimmed != "0" => {
                external_ids.insert("anidb_id".to_string(), trimmed.to_string());
            }
            "size" if size_bytes.is_none() => {
                size_bytes = parse_size(trimmed).or_else(|| parse_i64(trimmed));
            }
            _ => {}
        }
    }

    if options.parse_size_in_description && size_bytes.is_none() {
        size_bytes = item.description.as_deref().and_then(parse_size);
    }

    if options.parse_seeders_in_description
        && let Some(description) = item.description.as_deref()
    {
        let desc_seeders = parse_seeders(description);
        let desc_leechers = parse_leechers(description);
        let desc_peers = parse_peers(description);

        seeders = desc_seeders
            .or_else(|| {
                desc_peers
                    .zip(desc_leechers)
                    .map(|(peers, leechers)| peers - leechers)
            })
            .or(seeders);

        peers = desc_peers
            .or_else(|| {
                desc_seeders
                    .zip(desc_leechers)
                    .map(|(seeders, leechers)| seeders + leechers)
            })
            .or(peers);
        leechers = desc_leechers.or(leechers);
    }

    if options.calculate_peers_as_sum
        && let (Some(seeders), Some(peer_value)) = (seeders, peers)
    {
        peers = Some(seeders + peer_value);
    }

    if info_hash_v1.is_none() {
        info_hash_v1 = magnet_url
            .as_deref()
            .and_then(extract_info_hash_from_magnet)
            .or_else(|| item.guid.as_deref().and_then(find_hex_info_hash));
    }

    if let Some(ref value) = info_hash_v1 {
        provider_extra.insert(
            "info_hash".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    if let Some(ref value) = magnet_url {
        provider_extra.insert(
            "magnet_uri".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    if let Some(value) = seeders {
        provider_extra.insert("seeders".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = peers {
        provider_extra.insert("peers".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = leechers {
        provider_extra.insert("leechers".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = size_bytes {
        provider_extra.insert("reported_size".to_string(), serde_json::Value::from(value));
    }

    let provider_categories = dedupe(item.categories.clone());
    if !provider_categories.is_empty() {
        provider_extra.insert(
            "categories".to_string(),
            serde_json::to_value(&provider_categories).unwrap_or_default(),
        );
    }
    if let Some(ref value) = item.enclosure_type {
        provider_extra.insert(
            "enclosure_type".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    provider_extra.insert(
        "feed_source".to_string(),
        serde_json::Value::from(options.provider_tag),
    );

    let download_url = select_download_url(&item, magnet_url.as_deref(), options)
        .and_then(|value| resolve_url(feed_url, &value));
    let link = item
        .link
        .as_deref()
        .and_then(|value| resolve_url(feed_url, value));
    let guid_url = item
        .guid
        .as_deref()
        .and_then(|value| resolve_url(feed_url, value));
    let info_url = if options.use_guid_info_url {
        guid_url.clone()
    } else {
        link.clone()
            .filter(|value| Some(value.as_str()) != download_url.as_deref())
    };

    Some(SearchResult {
        title,
        link,
        download_url,
        size_bytes,
        published_at: item.published_at,
        grabs,
        languages: dedupe(languages),
        thumbs_up: seeders.and_then(|value| i32::try_from(value).ok()),
        thumbs_down: leechers
            .or(peers)
            .and_then(|value| i32::try_from(value).ok()),
        provider_extra,
        guid: item.guid,
        info_url,
        source_kind: Some(options.source_kind),
        protocol: Some(options.protocol),
        external_ids,
        categories: provider_categories.clone(),
        provider_categories,
        magnet_url,
        info_hash_v1,
        seeders,
        peers,
        leechers,
        download_volume_factor,
        upload_volume_factor,
        indexer_flags: dedupe(indexer_flags),
        minimum_seed_ratio,
        minimum_seed_time_minutes,
        ..SearchResult::default()
    })
}

fn select_download_url(
    item: &ParsedItem,
    magnet_uri: Option<&str>,
    options: RssParseOptions,
) -> Option<String> {
    let enclosure = item.enclosure_url.as_deref();
    let link = item
        .link
        .as_deref()
        .filter(|value| looks_like_download_candidate(value));
    let guid = item
        .guid
        .as_deref()
        .filter(|value| looks_like_download_candidate(value));

    match options.download_preference {
        DownloadPreference::Enclosure => enclosure.map(ToString::to_string),
        DownloadPreference::Magnet => magnet_uri.map(ToString::to_string),
        DownloadPreference::Link => link.map(ToString::to_string),
        DownloadPreference::Guid => guid.map(ToString::to_string),
        DownloadPreference::Auto => {
            if options.use_enclosure_url {
                enclosure
                    .or(magnet_uri)
                    .or(link)
                    .or(guid)
                    .map(ToString::to_string)
            } else {
                magnet_uri
                    .or_else(|| enclosure.filter(|value| value.starts_with("magnet:?")))
                    .or(link)
                    .or(enclosure)
                    .or(guid)
                    .map(ToString::to_string)
            }
        }
    }
}

fn looks_like_download_candidate(value: &str) -> bool {
    value.starts_with("magnet:?")
        || value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with('/')
        || value.ends_with(".torrent")
        || value.ends_with(".nzb")
}

fn resolve_url(feed_url: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("magnet:?")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return Some(trimmed.to_string());
    }

    Url::parse(feed_url)
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|url| url.to_string())
}

fn filter_results(results: Vec<SearchResult>, req: &SearchRequest) -> Vec<SearchResult> {
    let title_terms = requested_title_terms(req);
    let imdb_id = req.ids.get("imdb_id").map(|value| normalize_imdb(value));
    let tvdb_id = req.ids.get("tvdb_id").map(|value| value.trim().to_string());
    let anidb_id = req
        .ids
        .get("anidb_id")
        .map(|value| value.trim().to_string());
    let season_episode = build_episode_token(req.season, req.episode);
    let absolute_episode = req
        .absolute_episode
        .map(|episode| dedupe(vec![episode.to_string(), format!("{episode:02}")]))
        .unwrap_or_default();
    let category_terms = requested_category_terms(req);

    results
        .into_iter()
        .filter(|result| {
            title_matches(result, &title_terms)
                && external_id_matches(result, "imdb_id", imdb_id.as_deref())
                && external_id_matches(result, "tvdb_id", tvdb_id.as_deref())
                && external_id_matches(result, "anidb_id", anidb_id.as_deref())
                && episode_matches(result, season_episode.as_deref(), &absolute_episode)
                && category_matches(result, &category_terms)
        })
        .collect()
}

fn title_matches(result: &SearchResult, title_terms: &[String]) -> bool {
    if title_terms.is_empty() {
        return true;
    }

    let title = normalize_for_match(&result.title);
    title_terms.iter().any(|term| {
        title.contains(term)
            || term
                .split_whitespace()
                .all(|token| !token.is_empty() && title.contains(token))
    })
}

fn external_id_matches(result: &SearchResult, key: &str, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    match result.external_ids.get(key) {
        Some(found) => found.eq_ignore_ascii_case(requested),
        None => true,
    }
}

fn episode_matches(
    result: &SearchResult,
    season_episode: Option<&str>,
    absolute_episode: &[String],
) -> bool {
    let title = normalize_for_match(&result.title);
    if let Some(token) = season_episode
        && title.contains(token)
    {
        return true;
    }
    if !absolute_episode.is_empty()
        && absolute_episode
            .iter()
            .any(|token| title.split_whitespace().any(|part| part == token))
    {
        return true;
    }
    season_episode.is_none() && absolute_episode.is_empty()
}

fn build_episode_token(season: Option<u32>, episode: Option<u32>) -> Option<String> {
    match (season, episode) {
        (Some(season), Some(episode)) => Some(format!("s{season:02}e{episode:02}")),
        _ => None,
    }
}

fn requested_title_terms(req: &SearchRequest) -> Vec<String> {
    let mut terms = Vec::new();
    let query = normalize_for_match(&req.query);
    if !query.is_empty() {
        terms.push(query);
    }
    for alias in &req.tagged_aliases {
        let normalized = normalize_for_match(&alias.name);
        if !normalized.is_empty() {
            terms.push(normalized);
        }
    }
    dedupe(terms)
}

fn requested_category_terms(req: &SearchRequest) -> Vec<String> {
    let mut terms = Vec::new();
    if let Some(facet) = req.facet.as_deref() {
        let normalized = normalize_for_match(facet);
        if !normalized.is_empty() {
            terms.push(normalized);
        }
    }
    if let Some(category) = req.category.as_deref() {
        let normalized = normalize_for_match(category);
        if !normalized.is_empty() {
            terms.push(normalized);
        }
    }
    for category in &req.categories {
        let normalized = normalize_for_match(category);
        if !normalized.is_empty() {
            terms.push(normalized);
        }
    }
    dedupe(terms)
}

fn category_matches(result: &SearchResult, requested: &[String]) -> bool {
    if requested.is_empty() {
        return true;
    }

    let available = result
        .provider_categories
        .iter()
        .chain(result.categories.iter())
        .map(|value| normalize_for_match(value))
        .collect::<Vec<_>>();

    available.is_empty()
        || requested
            .iter()
            .any(|wanted| available.iter().any(|have| have.contains(wanted)))
}

fn dedupe_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut out = Vec::new();
    for result in results {
        let key = result
            .guid
            .clone()
            .or_else(|| result.download_url.clone())
            .or_else(|| result.magnet_url.clone())
            .unwrap_or_else(|| result.title.clone());
        if out.iter().all(|existing: &SearchResult| {
            let existing_key = existing
                .guid
                .clone()
                .or_else(|| existing.download_url.clone())
                .or_else(|| existing.magnet_url.clone())
                .unwrap_or_else(|| existing.title.clone());
            !existing_key.eq_ignore_ascii_case(&key)
        }) {
            out.push(result);
        }
    }
    out
}

fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn normalize_for_match(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_multi_value(value: &str) -> Vec<String> {
    value
        .split(['/', '|'])
        .flat_map(|part| part.split(" - "))
        .flat_map(|part| part.split(','))
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

fn parse_size(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return trimmed.parse::<i64>().ok();
    }

    let regex = size_regex();
    let captures = regex.captures(trimmed)?;
    let value = captures
        .name("value")?
        .as_str()
        .replace(',', "")
        .parse::<f64>()
        .ok()?;
    let unit = captures.name("unit")?.as_str().to_ascii_lowercase();
    let power = match unit.as_str() {
        "kb" | "kib" => 1,
        "mb" | "mib" => 2,
        "gb" | "gib" => 3,
        _ => 0,
    };
    let prefix = if unit.contains('i') {
        1024_f64
    } else {
        1024_f64
    };
    Some((value * prefix.powi(power)).round() as i64)
}

fn size_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?P<value>(?:\d+,)*\d+(?:\.\d{1,3})?)\W?(?P<unit>[KMG]i?B)\b")
            .expect("valid size regex")
    })
}

fn normalize_info_hash(value: &str) -> String {
    let normalized = value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if normalized.len() == 40 {
        normalized
    } else {
        String::new()
    }
}

fn normalize_imdb(value: &str) -> String {
    let digits = value
        .trim()
        .trim_start_matches("tt")
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        String::new()
    } else {
        format!("tt{digits}")
    }
}

fn find_first_magnet(value: Option<&str>) -> Option<String> {
    let text = value?.replace("&amp;", "&");
    let start = text.find("magnet:?")?;
    let rest = &text[start..];
    let end = rest
        .find(['"', '\'', '<', '>', ' ', '\n', '\r', '\t'])
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn find_first_magnet_in_sources(values: &[Option<&str>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .find_map(|value| find_first_magnet(Some(value)))
}

fn extract_info_hash_from_magnet(magnet: &str) -> Option<String> {
    let marker = "xt=urn:btih:";
    let index = magnet.find(marker)?;
    let tail = &magnet[index + marker.len()..];
    let value = tail.split('&').next()?.trim();
    let normalized = normalize_info_hash(value);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn find_hex_info_hash(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 40 {
        return None;
    }

    for window in bytes.windows(40) {
        if window.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(String::from_utf8_lossy(window).to_ascii_lowercase());
        }
    }

    None
}

fn parse_seeders(value: &str) -> Option<i64> {
    parse_count(value, seeders_regex())
}

fn parse_leechers(value: &str) -> Option<i64> {
    parse_count(value, leechers_regex())
}

fn parse_peers(value: &str) -> Option<i64> {
    parse_count(value, peers_regex())
}

fn parse_count(value: &str, regex: &Regex) -> Option<i64> {
    let captures = regex.captures(value)?;
    captures
        .name("value")
        .or_else(|| captures.name("value_prefix"))
        .and_then(|value| value.as_str().parse::<i64>().ok())
}

fn seeders_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?:seeders?:\s*(?P<value>\d+)|(?P<value_prefix>\d+)\s*seeders?)")
            .expect("valid seeders regex")
    })
}

fn leechers_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?:leechers?:\s*(?P<value>\d+)|(?P<value_prefix>\d+)\s*leechers?)")
            .expect("valid leechers regex")
    })
}

fn peers_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(?:peers?:\s*(?P<value>\d+)|(?P<value_prefix>\d+)\s*peers?)")
            .expect("valid peers regex")
    })
}

pub fn dedupe(values: Vec<String>) -> Vec<String> {
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
