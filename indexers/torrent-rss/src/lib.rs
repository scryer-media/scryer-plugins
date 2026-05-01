use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use extism_pdk::*;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, IndexerCapabilities as Capabilities,
    IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode,
    IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures, IndexerSearchInput,
    IndexerSourceKind, IndexerTorrentCapabilities, PluginDescriptor, PluginResult,
    PluginSearchRequest as SearchRequest, PluginSearchResponse as SearchResponse,
    PluginSearchResult as SearchResult, ProviderDescriptor, SDK_VERSION,
};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DownloadPreference {
    Auto,
    Enclosure,
    Magnet,
    Link,
    Guid,
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(build_descriptor_json()?)
}

fn build_descriptor_json() -> Result<String, Error> {
    let descriptor = PluginDescriptor {
        id: "torrent-rss".to_string(),
        name: "Torrent RSS Feed Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "torrent_rss".to_string(),
            provider_aliases: vec!["rss".to_string()],
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                supported_ids: HashMap::new(),
                deduplicates_aliases: false,
                season_param: None,
                episode_param: None,
                query_param: None,
                search: false,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: false,
                rss: true,
                protocols: vec![IndexerProtocol::Torrent],
                feed_modes: vec![IndexerFeedMode::Recent, IndexerFeedMode::Rss],
                search_inputs: vec![
                    IndexerSearchInput::TextQuery,
                    IndexerSearchInput::Category,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["imdb_id".into(), "tvdb_id".into()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::String],
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
                    reports_info_hash: true,
                    reports_magnet_uri: true,
                    reports_volume_factors: true,
                    supports_private_tracker_flags: true,
                    supports_seed_requirements: true,
                    ..IndexerTorrentCapabilities::default()
                }),
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
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let req: SearchRequest = serde_json::from_str(&input)?;
    let feed_url = read_config("feed_url")?;
    if feed_url.trim().is_empty() {
        return Err(Error::msg("Torrent RSS feed indexer requires feed_url configuration").into());
    }

    let cookie = optional_config("cookie");
    let username = optional_config("username");
    let password = optional_config("password");
    let user_agent = optional_config("user_agent")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Scryer Torrent RSS Indexer/0.1".to_string());
    let additional_headers = optional_config("additional_headers").unwrap_or_default();
    let preference = download_preference(optional_config("download_preference"));

    let body = fetch_feed(
        feed_url.trim(),
        &user_agent,
        cookie.as_deref(),
        username.as_deref(),
        password.as_deref(),
        &additional_headers,
    )?;

    let limit = req.limit.clamp(1, 200);
    let mut results = parse_rss_feed(&body, preference);
    results = filter_results(results, &req, limit);

    Ok(serde_json::to_string(&PluginResult::Ok(SearchResponse {
        results,
        ..Default::default()
    }))?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "feed_url".to_string(),
            label: "Feed URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Direct RSS feed URL for the torrent tracker or aggregator".to_string(),
            ),
        },
        ConfigFieldDef {
            key: "download_preference".to_string(),
            label: "Download Preference".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("auto".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![
                ConfigFieldOption {
                    value: "auto".to_string(),
                    label: "Auto".to_string(),
                },
                ConfigFieldOption {
                    value: "magnet".to_string(),
                    label: "Magnet".to_string(),
                },
                ConfigFieldOption {
                    value: "enclosure".to_string(),
                    label: "Enclosure".to_string(),
                },
                ConfigFieldOption {
                    value: "link".to_string(),
                    label: "Link".to_string(),
                },
                ConfigFieldOption {
                    value: "guid".to_string(),
                    label: "GUID".to_string(),
                },
            ],
            help_text: Some(
                "Which RSS field should be used as the download URL when multiple candidates exist"
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "username".to_string(),
            label: "Username".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Optional username for HTTP basic auth".to_string()),
        },
        ConfigFieldDef {
            key: "password".to_string(),
            label: "Password".to_string(),
            field_type: ConfigFieldType::Password,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Optional password for HTTP basic auth".to_string()),
        },
        ConfigFieldDef {
            key: "cookie".to_string(),
            label: "Cookie Header".to_string(),
            field_type: ConfigFieldType::Password,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Optional raw Cookie header for private trackers that gate RSS with session cookies"
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "user_agent".to_string(),
            label: "User Agent".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: Some("Scryer Torrent RSS Indexer/0.1".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Optional custom User-Agent header".to_string()),
        },
        ConfigFieldDef {
            key: "additional_headers".to_string(),
            label: "Additional Headers".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Optional extra headers, one per line, formatted as Header-Name: value".to_string(),
            ),
        },
    ]
}

fn read_config(key: &str) -> Result<String, Error> {
    config::get(key)
        .map_err(|e| Error::msg(format!("missing config {key}: {e}")))?
        .ok_or_else(|| Error::msg(format!("missing config {key}")))
}

fn optional_config(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
}

fn download_preference(value: Option<String>) -> DownloadPreference {
    match value
        .unwrap_or_else(|| "auto".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "enclosure" => DownloadPreference::Enclosure,
        "magnet" => DownloadPreference::Magnet,
        "link" => DownloadPreference::Link,
        "guid" => DownloadPreference::Guid,
        _ => DownloadPreference::Auto,
    }
}

fn fetch_feed(
    feed_url: &str,
    user_agent: &str,
    cookie: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
    additional_headers: &str,
) -> Result<String, Error> {
    let logged_url = redact_url_for_log(feed_url);

    let mut request = HttpRequest::new(feed_url)
        .with_header(
            "Accept",
            "application/rss+xml, application/xml, text/xml;q=0.9, */*;q=0.8",
        )
        .with_header("User-Agent", user_agent)
        .with_header("Accept-Language", "en-US,en;q=0.9");

    if let Some(cookie) = cookie {
        request = request.with_header("Cookie", cookie);
    }

    if let Some(username) = username {
        let password = password.unwrap_or_default();
        let encoded = STANDARD.encode(format!("{username}:{password}"));
        request = request.with_header("Authorization", format!("Basic {encoded}"));
    }

    for line in additional_headers.lines() {
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
        "http_trace plugin=torrent_rss method=GET attempt=1 url={}",
        logged_url
    );

    let response = http::request::<Vec<u8>>(&request, None).map_err(|e| {
        log!(
            LogLevel::Debug,
            "http_trace_error plugin=torrent_rss method=GET attempt=1 url={} error={}",
            logged_url,
            e
        );
        Error::msg(format!("HTTP request failed: {e}"))
    })?;
    let status = response.status_code();
    log!(
        LogLevel::Debug,
        "http_trace_response plugin=torrent_rss method=GET attempt=1 status={} url={}",
        status,
        logged_url
    );
    if status >= 400 {
        return Err(Error::msg(format!(
            "Torrent RSS feed returned HTTP {status}"
        )));
    }

    Ok(String::from_utf8_lossy(&response.body()).to_string())
}

fn redact_url_for_log(url: &str) -> String {
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
                "apikey" | "api_key" | "token" | "key" | "password" | "pass"
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

fn parse_rss_feed(body: &str, preference: DownloadPreference) -> Vec<SearchResult> {
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
                if name == "item" {
                    in_item = true;
                    item = ParsedItem::default();
                    current_tag = None;
                } else if in_item {
                    match name.as_str() {
                        "title" | "link" | "guid" | "description" | "pubDate" | "category" => {
                            current_tag = Some(name);
                        }
                        "enclosure" => {
                            parse_enclosure(event, &mut item);
                            current_tag = None;
                        }
                        _ => current_tag = None,
                    }
                }
            }
            Ok(Event::Empty(ref event)) => {
                if in_item {
                    let name = tag_name(event);
                    if name == "enclosure" {
                        parse_enclosure(event, &mut item);
                    } else if name == "attr" || name.ends_with(":attr") {
                        if let Some(pair) = parse_attr_pair(event) {
                            item.attrs.push(pair);
                        }
                    }
                }
            }
            Ok(Event::Text(text)) => {
                if in_item {
                    apply_text(
                        &mut item,
                        current_tag.as_deref(),
                        decode_text(text.as_ref()),
                    );
                }
            }
            Ok(Event::CData(text)) => {
                if in_item {
                    apply_text(
                        &mut item,
                        current_tag.as_deref(),
                        decode_text(text.as_ref()),
                    );
                }
            }
            Ok(Event::End(ref event)) => {
                let name = String::from_utf8_lossy(event.name().as_ref()).to_string();
                if name == "item" {
                    in_item = false;
                    current_tag = None;
                    if let Some(result) = build_result(item, preference) {
                        results.push(result);
                    }
                    item = ParsedItem::default();
                } else if current_tag.as_deref() == Some(name.as_str()) {
                    current_tag = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }

        buf.clear();
    }

    results
}

fn tag_name(event: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(event.name().as_ref()).to_string()
}

fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn apply_text(item: &mut ParsedItem, current_tag: Option<&str>, value: String) {
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

fn build_result(item: ParsedItem, preference: DownloadPreference) -> Option<SearchResult> {
    let title = item.title.clone()?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let mut extra = HashMap::new();
    let mut languages = Vec::new();
    let mut grabs = None;
    let mut info_hash = None;
    let mut magnet_uri = find_first_magnet_in_sources(&[
        item.enclosure_url.as_deref(),
        item.link.as_deref(),
        item.guid.as_deref(),
        item.description.as_deref(),
    ]);

    for (name, value) in &item.attrs {
        let normalized = normalize_key(name);
        let trimmed = value.trim();
        match normalized.as_str() {
            "language" => languages.extend(split_multi_value(trimmed)),
            "grabs" | "downloads" => {
                grabs = parse_i64(trimmed).or(grabs);
                if let Some(value) = parse_i64(trimmed) {
                    extra.insert("downloads".to_string(), serde_json::Value::from(value));
                }
            }
            "seeders" => {
                if let Some(value) = parse_i64(trimmed) {
                    extra.insert("seeders".to_string(), serde_json::Value::from(value));
                }
            }
            "peers" | "leechers" => {
                if let Some(value) = parse_i64(trimmed) {
                    extra.insert("peers".to_string(), serde_json::Value::from(value));
                }
            }
            "downloadvolumefactor" => {
                if let Some(value) = parse_f64(trimmed) {
                    extra.insert(
                        "downloadvolumefactor".to_string(),
                        serde_json::Value::from(value),
                    );
                    if (value - 0.0).abs() < f64::EPSILON {
                        extra.insert("freeleech".to_string(), serde_json::Value::from(true));
                    }
                }
            }
            "uploadvolumefactor" => {
                if let Some(value) = parse_f64(trimmed) {
                    extra.insert(
                        "uploadvolumefactor".to_string(),
                        serde_json::Value::from(value),
                    );
                }
            }
            "minimumratio" => {
                if let Some(value) = parse_f64(trimmed) {
                    extra.insert("minimumratio".to_string(), serde_json::Value::from(value));
                }
            }
            "minimumseedtime" => {
                if let Some(value) = parse_i64(trimmed) {
                    extra.insert(
                        "minimumseedtime".to_string(),
                        serde_json::Value::from(value),
                    );
                }
            }
            "infohash" => {
                let normalized_hash = normalize_info_hash(trimmed);
                if !normalized_hash.is_empty() {
                    info_hash = Some(normalized_hash.clone());
                    extra.insert(
                        "info_hash".to_string(),
                        serde_json::Value::from(normalized_hash),
                    );
                }
            }
            "magneturl" => {
                if !trimmed.is_empty() {
                    magnet_uri = Some(trimmed.to_string());
                }
            }
            "imdb" | "imdbid" => {
                let normalized_id = normalize_imdb(trimmed);
                if !normalized_id.is_empty() {
                    extra.insert(
                        "response_imdbid".to_string(),
                        serde_json::Value::from(normalized_id),
                    );
                }
            }
            "tvdbid" => {
                if !trimmed.is_empty() && trimmed != "0" {
                    extra.insert(
                        "response_tvdbid".to_string(),
                        serde_json::Value::from(trimmed),
                    );
                }
            }
            "size" => {
                if item.enclosure_length.is_none() {
                    if let Some(value) = parse_i64(trimmed) {
                        extra.insert("reported_size".to_string(), serde_json::Value::from(value));
                    }
                }
            }
            _ => {}
        }
    }

    if info_hash.is_none() {
        info_hash = magnet_uri
            .as_deref()
            .and_then(extract_info_hash_from_magnet)
            .or_else(|| item.guid.as_deref().and_then(find_hex_info_hash));
    }

    if let Some(ref value) = info_hash {
        extra.insert(
            "info_hash".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    if let Some(ref value) = magnet_uri {
        extra.insert(
            "magnet_uri".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    if !item.categories.is_empty() {
        extra.insert(
            "categories".to_string(),
            serde_json::to_value(dedupe(item.categories.clone())).unwrap_or_default(),
        );
    }
    if let Some(ref value) = item.enclosure_type {
        extra.insert(
            "enclosure_type".to_string(),
            serde_json::Value::from(value.as_str()),
        );
    }
    extra.insert("feed_source".to_string(), serde_json::Value::from("rss"));

    let download_url = select_download_url(&item, magnet_uri.as_deref(), preference);
    let size_bytes = item
        .enclosure_length
        .or_else(|| extra.get("reported_size").and_then(|value| value.as_i64()));

    let link = item.link.filter(|value| !value.trim().is_empty());
    let info_url = link
        .clone()
        .filter(|value| Some(value.as_str()) != download_url.as_deref());

    Some(SearchResult {
        title,
        link,
        download_url,
        size_bytes,
        published_at: item.published_at,
        grabs,
        languages: dedupe(languages),
        thumbs_up: extra
            .get("seeders")
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok()),
        thumbs_down: extra
            .get("peers")
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok()),
        subtitles: vec![],
        password_hint: None,
        protected: None,
        provider_extra: extra,
        guid: item.guid,
        info_url,
        ..SearchResult::default()
    })
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

fn select_download_url(
    item: &ParsedItem,
    magnet_uri: Option<&str>,
    preference: DownloadPreference,
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

    match preference {
        DownloadPreference::Enclosure => enclosure.map(ToString::to_string),
        DownloadPreference::Magnet => magnet_uri.map(ToString::to_string),
        DownloadPreference::Link => link.map(ToString::to_string),
        DownloadPreference::Guid => guid.map(ToString::to_string),
        DownloadPreference::Auto => magnet_uri
            .or_else(|| enclosure.filter(|value| value.starts_with("magnet:?")))
            .or(enclosure)
            .or(link)
            .or(guid)
            .map(ToString::to_string),
    }
}

fn looks_like_download_candidate(value: &str) -> bool {
    value.starts_with("magnet:?") || value.starts_with("http://") || value.starts_with("https://")
}

fn filter_results(
    results: Vec<SearchResult>,
    req: &SearchRequest,
    limit: usize,
) -> Vec<SearchResult> {
    let normalized_query = normalize_for_match(&req.query);
    let imdb_id = req.ids.get("imdb_id").map(|value| normalize_imdb(value));
    let tvdb_id = req.ids.get("tvdb_id").map(|value| value.trim().to_string());
    let season_episode = build_episode_tokens(req.season, req.episode);
    let category_terms = requested_category_terms(req);

    results
        .into_iter()
        .filter(|result| {
            title_matches(result, &normalized_query)
                && imdb_matches(result, imdb_id.as_deref())
                && tvdb_matches(result, tvdb_id.as_deref())
                && episode_matches(result, season_episode.as_deref())
                && category_matches(result, &category_terms)
        })
        .take(limit)
        .collect()
}

fn title_matches(result: &SearchResult, normalized_query: &str) -> bool {
    if normalized_query.is_empty() {
        return true;
    }

    let title = normalize_for_match(&result.title);
    if title.contains(normalized_query) {
        return true;
    }

    normalized_query
        .split_whitespace()
        .all(|token| !token.is_empty() && title.contains(token))
}

fn imdb_matches(result: &SearchResult, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    match result
        .provider_extra
        .get("response_imdbid")
        .and_then(|value| value.as_str())
    {
        Some(found) => found.eq_ignore_ascii_case(requested),
        None => true,
    }
}

fn tvdb_matches(result: &SearchResult, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    match result
        .provider_extra
        .get("response_tvdbid")
        .and_then(|value| value.as_str())
    {
        Some(found) => found == requested,
        None => true,
    }
}

fn episode_matches(result: &SearchResult, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    normalize_for_match(&result.title).contains(token)
}

fn build_episode_tokens(season: Option<u32>, episode: Option<u32>) -> Option<String> {
    match (season, episode) {
        (Some(season), Some(episode)) => Some(format!("s{season:02}e{episode:02}")),
        _ => None,
    }
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
        .provider_extra
        .get("categories")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(normalize_for_match)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    available.is_empty()
        || requested
            .iter()
            .any(|wanted| available.iter().any(|have| have.contains(wanted)))
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

    fn parse(body: &str, preference: DownloadPreference) -> Vec<SearchResult> {
        parse_rss_feed(body, preference)
    }

    #[test]
    fn descriptor_is_torrent_rss() {
        let json = build_descriptor_json().unwrap();
        assert!(json.contains("\"provider_type\":\"torrent_rss\""));
        assert!(json.contains("\"source_kind\":\"torrent\""));
    }

    #[test]
    fn parses_enclosure_and_torrent_attrs() {
        let body = r#"
<rss>
  <channel>
    <item>
      <title>Dune.Part.Two.2024.2160p.WEB-DL</title>
      <link>https://tracker.example/torrents/123</link>
      <guid>https://tracker.example/download/123.torrent</guid>
      <pubDate>Tue, 10 Mar 2026 12:00:00 GMT</pubDate>
      <category>Movies</category>
      <enclosure url="https://tracker.example/download/123.torrent" length="123456" type="application/x-bittorrent" />
      <torznab:attr name="seeders" value="55" />
      <torznab:attr name="peers" value="12" />
      <torznab:attr name="infohash" value="ABCDEF1234567890ABCDEF1234567890ABCDEF12" />
      <torznab:attr name="imdbid" value="tt15239678" />
    </item>
  </channel>
</rss>
"#;

        let results = parse(body, DownloadPreference::Auto);
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(
            result.download_url.as_deref(),
            Some("https://tracker.example/download/123.torrent")
        );
        assert_eq!(result.size_bytes, Some(123456));
        assert_eq!(
            result.provider_extra.get("seeders"),
            Some(&serde_json::Value::from(55))
        );
        assert_eq!(
            result.provider_extra.get("peers"),
            Some(&serde_json::Value::from(12))
        );
        assert_eq!(
            result.provider_extra.get("info_hash"),
            Some(&serde_json::Value::from(
                "abcdef1234567890abcdef1234567890abcdef12"
            ))
        );
        assert_eq!(
            result.provider_extra.get("response_imdbid"),
            Some(&serde_json::Value::from("tt15239678"))
        );
    }

    #[test]
    fn auto_prefers_magnet_when_present() {
        let body = r#"
<rss>
  <channel>
    <item>
      <title>Show.S01E02.1080p.WEB</title>
      <link>https://tracker.example/torrents/123</link>
      <description><![CDATA[<a href="magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abcdef12&dn=Show">Magnet</a>]]></description>
      <enclosure url="https://tracker.example/download/123.torrent" length="999" type="application/x-bittorrent" />
    </item>
  </channel>
</rss>
"#;

        let results = parse(body, DownloadPreference::Auto);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abcdef12&dn=Show")
        );
        assert_eq!(
            results[0].provider_extra.get("info_hash"),
            Some(&serde_json::Value::from(
                "abcdef1234567890abcdef1234567890abcdef12"
            ))
        );
    }

    #[test]
    fn explicit_enclosure_preference_uses_torrent_file() {
        let body = r#"
<rss>
  <channel>
    <item>
      <title>Movie.2026.1080p.BluRay</title>
      <link>magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abcdef12</link>
      <enclosure url="https://tracker.example/download/123.torrent" length="42" type="application/x-bittorrent" />
    </item>
  </channel>
</rss>
"#;

        let results = parse(body, DownloadPreference::Enclosure);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("https://tracker.example/download/123.torrent")
        );
    }

    #[test]
    fn filter_results_matches_query_and_episode_token() {
        let results = vec![
            SearchResult {
                title: "Show.S01E02.1080p.WEB".to_string(),
                link: None,
                download_url: None,
                size_bytes: None,
                published_at: None,
                grabs: None,
                languages: vec![],
                thumbs_up: None,
                thumbs_down: None,
                subtitles: vec![],
                password_hint: None,
                protected: None,
                provider_extra: HashMap::new(),
                guid: None,
                info_url: None,
                ..SearchResult::default()
            },
            SearchResult {
                title: "Other.S01E02.1080p.WEB".to_string(),
                link: None,
                download_url: None,
                size_bytes: None,
                published_at: None,
                grabs: None,
                languages: vec![],
                thumbs_up: None,
                thumbs_down: None,
                subtitles: vec![],
                password_hint: None,
                protected: None,
                provider_extra: HashMap::new(),
                guid: None,
                info_url: None,
                ..SearchResult::default()
            },
        ];

        let filtered = filter_results(
            results,
            &SearchRequest {
                query: "Show".to_string(),
                ids: HashMap::new(),
                facet: Some("series".to_string()),
                category: Some("tv".to_string()),
                categories: vec![],
                limit: 10,
                season: Some(1),
                episode: Some(2),
                absolute_episode: None,
                tagged_aliases: vec![],
                ..SearchRequest::default()
            },
            10,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Show.S01E02.1080p.WEB");
    }
}
