use std::collections::HashMap;

use newznab_common::{
    Capabilities, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchInput, IndexerSourceKind, IndexerTorrentCapabilities, NewznabConfig,
    PluginActionRequest, PluginActionResponse, PluginDescriptor, ProviderDescriptor, SDK_VERSION,
    SearchRequest, SearchResponse, current_sdk_constraint, execute_full_search,
    standard_config_fields,
};
use scryer_plugin_pdk::*;

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "torznab".to_string(),
        name: "Torznab Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "torznab".to_string(),
            provider_aliases: vec!["jackett".to_string()],
            search_semantics_version: Some(1),
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                supported_ids: HashMap::from([
                    ("movie".into(), vec!["imdb_id".into()]),
                    ("series".into(), vec!["tvdb_id".into()]),
                    ("anime".into(), vec!["tvdb_id".into()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".into()),
                episode_param: Some("ep".into()),
                query_param: Some("q".into()),
                supported_query_facets: vec![],
                search: true,
                imdb_search: true,
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
                    "imdb_id".into(),
                    "tvdb_id".into(),
                    "tmdb_id".into(),
                    "tvmaze_id".into(),
                    "tvrage_id".into(),
                    "anidb_id".into(),
                ],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    ..IndexerCategoryModel::default()
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(100),
                    max_page_size: Some(100),
                    max_pages: Some(30),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: true,
                    grab_quota_supported: true,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_leechers: true,
                    reports_info_hash: true,
                    reports_magnet_uri: true,
                    reports_volume_factors: true,
                    supports_private_tracker_flags: true,
                    supports_seed_requirements: true,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    subtitles: true,
                    grabs: true,
                    votes: true,
                    comments: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    protection_hint: true,
                    ..IndexerResponseFeatures::default()
                }),
            },
            scoring_policies: vec![],
            config_fields: standard_config_fields(None),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    }
}

async fn search(req: SearchRequest) -> Result<SearchResponse, Error> {
    let config = NewznabConfig::from_host()?;
    let mut response = execute_full_search(&config, &req, torznab_metadata_extractor).await?;
    apply_magnet_fallback(&mut response);
    Ok(response)
}

/// Provider-extra key the extractor uses for a magnet it *synthesized* from
/// `infohash`. Never leaves this plugin: `apply_magnet_fallback` either promotes
/// it to `magnet_uri` or drops it.
const MAGNET_URI_FALLBACK_KEY: &str = "magnet_uri_fallback";

/// A magnet the feed itself supplies (`magneturl`) is an indexer artifact and is
/// reported as such. A magnet synthesized from `infohash` is not: it names only
/// public trackers and carries no private flag, so next to a real torrent link
/// it would outrank the file the tracker actually serves — Sonarr never
/// synthesizes one — and on a private tracker it can never fetch metadata at
/// all. Keep it strictly as the last resort for an item that offers no other
/// way to download.
fn apply_magnet_fallback(response: &mut SearchResponse) {
    for result in &mut response.results {
        let fallback = result
            .provider_extra
            .remove(MAGNET_URI_FALLBACK_KEY)
            .and_then(|value| value.as_str().map(str::to_string));
        if result.magnet_url.is_some() || result.download_url.is_some() {
            continue;
        }
        if let Some(magnet_uri) = fallback {
            result.provider_extra.insert(
                "magnet_uri".to_string(),
                serde_json::Value::from(magnet_uri.clone()),
            );
            result.magnet_url = Some(magnet_uri);
        }
    }
}

async fn action(request: PluginActionRequest) -> Result<PluginActionResponse, Error> {
    newznab_common::execute_provider_action(request).await
}

fn torznab_metadata_extractor(
    pairs: &[(String, String)],
) -> (Vec<String>, Option<i64>, HashMap<String, serde_json::Value>) {
    let mut grabs: Option<i64> = None;
    let mut seeders: Option<i64> = None;
    let mut leechers: Option<i64> = None;
    let mut peers: Option<i64> = None;
    let mut downloads: Option<i64> = None;
    let mut downloadvolumefactor: Option<f64> = None;
    let mut uploadvolumefactor: Option<f64> = None;
    let mut minimumratio: Option<f64> = None;
    let mut minimumseedtime: Option<i64> = None;
    let mut info_hash: Option<String> = None;
    let mut magnet_uri: Option<String> = None;
    let mut genres: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let mut languages: Vec<String> = Vec::new();

    for (name, value) in pairs {
        let normalized = name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let trimmed = value.trim();

        match normalized.as_str() {
            "language" => {
                languages.extend(split_multi_value(trimmed));
            }
            "grabs" => {
                grabs = parse_i64(trimmed);
            }
            "seeders" => {
                seeders = parse_i64(trimmed);
            }
            "peers" => {
                peers = parse_i64(trimmed);
            }
            "leechers" => {
                leechers = parse_i64(trimmed);
            }
            "downloads" => {
                downloads = parse_i64(trimmed);
            }
            "downloadvolumefactor" => {
                downloadvolumefactor = parse_f64(trimmed);
            }
            "uploadvolumefactor" => {
                uploadvolumefactor = parse_f64(trimmed);
            }
            "minimumratio" => {
                minimumratio = parse_f64(trimmed);
            }
            "minimumseedtime" => {
                minimumseedtime = parse_i64(trimmed);
            }
            "infohash" => {
                let normalized_hash = normalize_info_hash(trimmed);
                if !normalized_hash.is_empty() {
                    info_hash = Some(normalized_hash);
                }
            }
            "magneturl" if !trimmed.is_empty() => {
                magnet_uri = Some(trimmed.to_string());
            }
            "genre" => {
                genres.extend(split_multi_value(trimmed));
            }
            "tag" => {
                tags.extend(split_multi_value(trimmed));
            }
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
    if let Some(value) = downloadvolumefactor {
        extra.insert(
            "downloadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
        if (value - 0.0).abs() < f64::EPSILON {
            extra.insert("freeleech".to_string(), serde_json::Value::from(true));
        }
    }
    if let Some(value) = uploadvolumefactor {
        extra.insert(
            "uploadvolumefactor".to_string(),
            serde_json::Value::from(value),
        );
    }
    if let Some(value) = minimumratio {
        extra.insert("minimumratio".to_string(), serde_json::Value::from(value));
    }
    if let Some(value) = minimumseedtime {
        extra.insert(
            "minimumseedtime".to_string(),
            serde_json::Value::from(value),
        );
    }
    if let Some(ref value) = info_hash {
        extra.insert(
            "info_hash".to_string(),
            serde_json::Value::from(value.as_str()),
        );
        // A magnet synthesized from the info hash is only a fallback for an item
        // with no other download path; `apply_magnet_fallback` decides that once
        // the item's enclosure is known. It must not masquerade as `magnet_uri`.
        if magnet_uri.is_none() {
            extra.insert(
                MAGNET_URI_FALLBACK_KEY.to_string(),
                serde_json::Value::from(build_magnet_uri(value)),
            );
        }
    }
    if let Some(value) = magnet_uri {
        extra.insert("magnet_uri".to_string(), serde_json::Value::from(value));
    }
    if !genres.is_empty() {
        extra.insert(
            "genres".to_string(),
            serde_json::to_value(dedupe(genres)).unwrap_or_default(),
        );
    }
    if !tags.is_empty() {
        extra.insert(
            "tags".to_string(),
            serde_json::to_value(dedupe(tags)).unwrap_or_default(),
        );
    }

    (dedupe(languages), grabs, extra)
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
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn build_magnet_uri(info_hash: &str) -> String {
    const TRACKERS: &[&str] = &[
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://tracker.bittor.pw:1337/announce",
        "udp://public.popcorn-tracker.org:6969/announce",
        "udp://tracker.dler.org:6969/announce",
        "udp://exodus.desync.com:6969",
        "udp://open.demonii.com:1337/announce",
    ];

    let mut uri = format!("magnet:?xt=urn:btih:{info_hash}");
    for tracker in TRACKERS {
        uri.push_str("&tr=");
        uri.push_str(tracker);
    }
    uri
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

scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = build_descriptor,
    search = search,
    action = action,
);

#[cfg(test)]
mod tests {
    use super::*;
    use newznab_common::SearchResult;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn descriptor_is_torznab() {
        let json = serde_json::to_string(&build_descriptor()).unwrap();
        assert!(json.contains("torznab"));
    }

    #[test]
    fn extracts_torrent_metadata() {
        let p = pairs(&[
            ("seeders", "42"),
            ("peers", "9"),
            ("infohash", "ABCDEF1234567890ABCDEF1234567890ABCDEF12"),
            ("magneturl", "magnet:?xt=urn:btih:abcdef"),
            ("downloadvolumefactor", "0"),
        ]);
        let (_, _, extra) = torznab_metadata_extractor(&p);
        assert_eq!(extra.get("seeders"), Some(&serde_json::Value::from(42)));
        assert_eq!(extra.get("peers"), Some(&serde_json::Value::from(9)));
        assert_eq!(
            extra.get("info_hash"),
            Some(&serde_json::Value::from(
                "abcdef1234567890abcdef1234567890abcdef12"
            ))
        );
        assert_eq!(
            extra.get("magnet_uri"),
            Some(&serde_json::Value::from("magnet:?xt=urn:btih:abcdef"))
        );
        assert_eq!(extra.get("freeleech"), Some(&serde_json::Value::from(true)));
    }

    #[test]
    fn a_synthesized_magnet_is_kept_out_of_magnet_uri_by_the_extractor() {
        let p = pairs(&[("infohash", "ABCDEF1234567890ABCDEF1234567890ABCDEF12")]);
        let (_, _, extra) = torznab_metadata_extractor(&p);
        assert_eq!(extra.get("magnet_uri"), None);
        let fallback = extra
            .get(MAGNET_URI_FALLBACK_KEY)
            .and_then(|value| value.as_str())
            .expect("synthesized magnet lands under the fallback key");
        assert!(
            fallback.starts_with("magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abcdef12")
        );
    }

    fn result_with_fallback(download_url: Option<&str>, magnet_url: Option<&str>) -> SearchResult {
        let mut provider_extra = HashMap::new();
        provider_extra.insert(
            "info_hash".to_string(),
            serde_json::Value::from("abcdef1234567890abcdef1234567890abcdef12"),
        );
        provider_extra.insert(
            MAGNET_URI_FALLBACK_KEY.to_string(),
            serde_json::Value::from(build_magnet_uri("abcdef1234567890abcdef1234567890abcdef12")),
        );
        SearchResult {
            title: "Release".to_string(),
            download_url: download_url.map(str::to_string),
            magnet_url: magnet_url.map(str::to_string),
            provider_extra,
            ..Default::default()
        }
    }

    fn response_of(results: Vec<SearchResult>) -> SearchResponse {
        SearchResponse {
            results,
            ..Default::default()
        }
    }

    #[test]
    fn a_synthesized_magnet_never_competes_with_a_torrent_link() {
        let mut response = response_of(vec![result_with_fallback(
            Some("https://tracker.example/download/1.torrent"),
            None,
        )]);
        apply_magnet_fallback(&mut response);
        let result = &response.results[0];
        assert_eq!(result.magnet_url, None);
        assert_eq!(result.provider_extra.get("magnet_uri"), None);
        assert_eq!(result.provider_extra.get(MAGNET_URI_FALLBACK_KEY), None);
        assert_eq!(
            result.download_url.as_deref(),
            Some("https://tracker.example/download/1.torrent")
        );
        assert!(result.provider_extra.contains_key("info_hash"));
    }

    #[test]
    fn a_synthesized_magnet_is_the_last_resort_for_an_item_with_no_download_path() {
        let mut response = response_of(vec![result_with_fallback(None, None)]);
        apply_magnet_fallback(&mut response);
        let result = &response.results[0];
        let magnet = result.magnet_url.as_deref().expect("fallback promoted");
        assert!(magnet.starts_with("magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abcdef12"));
        assert_eq!(
            result
                .provider_extra
                .get("magnet_uri")
                .and_then(|value| value.as_str()),
            Some(magnet)
        );
        assert_eq!(result.provider_extra.get(MAGNET_URI_FALLBACK_KEY), None);
    }

    #[test]
    fn an_indexer_provided_magnet_is_left_alone() {
        let mut response = response_of(vec![result_with_fallback(
            Some("https://tracker.example/download/1.torrent"),
            Some("magnet:?xt=urn:btih:abcdef"),
        )]);
        apply_magnet_fallback(&mut response);
        let result = &response.results[0];
        assert_eq!(
            result.magnet_url.as_deref(),
            Some("magnet:?xt=urn:btih:abcdef")
        );
        assert_eq!(result.provider_extra.get(MAGNET_URI_FALLBACK_KEY), None);
    }

    #[test]
    fn derives_peers_from_seeders_and_leechers_when_peers_missing() {
        let p = pairs(&[("seeders", "42"), ("leechers", "9")]);
        let (_, _, extra) = torznab_metadata_extractor(&p);

        assert_eq!(extra.get("seeders"), Some(&serde_json::Value::from(42)));
        assert_eq!(extra.get("leechers"), Some(&serde_json::Value::from(9)));
        assert_eq!(extra.get("peers"), Some(&serde_json::Value::from(51)));
    }

    #[test]
    fn extracts_languages_genres_and_tags() {
        let p = pairs(&[
            ("language", "English - Japanese"),
            ("genre", "Action / Sci-Fi"),
            ("tag", "remux, internal"),
        ]);
        let (languages, _, extra) = torznab_metadata_extractor(&p);
        assert_eq!(languages, vec!["English", "Japanese"]);
        assert_eq!(
            serde_json::from_value::<Vec<String>>(extra.get("genres").unwrap().clone()).unwrap(),
            vec!["Action", "Sci-Fi"]
        );
        assert_eq!(
            serde_json::from_value::<Vec<String>>(extra.get("tags").unwrap().clone()).unwrap(),
            vec!["remux", "internal"]
        );
    }

    #[test]
    fn extracts_grabs_and_ratio_rules() {
        let p = pairs(&[
            ("grabs", "1,234"),
            ("minimumratio", "1.5"),
            ("minimumseedtime", "7200"),
        ]);
        let (_, grabs, extra) = torznab_metadata_extractor(&p);
        assert_eq!(grabs, Some(1234));
        assert_eq!(
            extra.get("minimumratio"),
            Some(&serde_json::Value::from(1.5))
        );
        assert_eq!(
            extra.get("minimumseedtime"),
            Some(&serde_json::Value::from(7200))
        );
    }
}
