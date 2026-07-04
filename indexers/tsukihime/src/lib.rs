use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource, IndexerCapabilities,
    IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol,
    IndexerResponseFeatures, IndexerSearchInput, IndexerSourceKind, IndexerTorrentCapabilities,
    PluginDescriptor, PluginResult, PluginSearchRequest as SearchRequest,
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult, ProviderDescriptor,
    SDK_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PROVIDER_ID: &str = "tsukihime-indexer";
const PROVIDER_TYPE: &str = "tsukihime";
const DEFAULT_BASE_URL: &str = "https://api.tsukihime.org/v1";
const DEFAULT_USER_AGENT: &str = "Scryer Tsukihime Indexer/0.1";
const DEFAULT_MAX_RESULTS: usize = 50;
const API_MAX_RESULTS: usize = 100;
const RATE_LIMIT_HINT_SECONDS: u32 = 2;
const API_RATE_LIMIT_PER_MINUTE: u32 = 60;
const SEARCH_RATE_LIMIT_PER_MINUTE: u32 = 25;
const RATE_LIMIT_WINDOW_SECONDS: u64 = 60;
const API_RATE_LIMIT_VAR_KEY: &str = "tsukihime-indexer-api-rate-limit-v1";
const SEARCH_RATE_LIMIT_VAR_KEY: &str = "tsukihime-indexer-search-rate-limit-v1";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

#[plugin_fn]
pub fn scryer_indexer_search(input: String) -> FnResult<String> {
    let request: SearchRequest = serde_json::from_str(&input)?;
    let response = match search_impl(&request) {
        Ok(response) => response,
        Err(TsukihimeError::RateLimited(_)) => SearchResponse {
            results: Vec::new(),
            ..SearchResponse::default()
        },
        Err(error) => return Err(Error::msg(error.to_string()).into()),
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "Tsukihime Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["tsukihime.org".to_string()],
            source_kind: IndexerSourceKind::Generic,
            capabilities: IndexerCapabilities {
                supported_ids: HashMap::from([(
                    "anime".to_string(),
                    vec![
                        "anidb_id".to_string(),
                        "anilist_id".to_string(),
                        "mal_id".to_string(),
                    ],
                )]),
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                query_param: Some("q".to_string()),
                supported_query_facets: vec!["anime".to_string()],
                search: true,
                anidb_search: true,
                rss: true,
                protocols: vec![
                    IndexerProtocol::Mixed,
                    IndexerProtocol::Torrent,
                    IndexerProtocol::Usenet,
                ],
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
                supported_external_ids: vec![
                    "anidb_id".to_string(),
                    "anilist_id".to_string(),
                    "mal_id".to_string(),
                ],
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(DEFAULT_MAX_RESULTS as u32),
                    max_page_size: Some(API_MAX_RESULTS as u32),
                    rate_limit_hint_seconds: Some(RATE_LIMIT_HINT_SECONDS),
                    ..IndexerLimitCapabilities::default()
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_info_hash: true,
                    reports_magnet_uri: true,
                    ..IndexerTorrentCapabilities::default()
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: true,
                    subtitles: true,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    ..IndexerResponseFeatures::default()
                }),
                ..IndexerCapabilities::default()
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec!["api.tsukihime.org".to_string()],
            rate_limit_seconds: Some(RATE_LIMIT_HINT_SECONDS as i64),
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "base_url",
            "API URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("Tsukihime API URL"),
        ),
        field(
            "max_results",
            "Max Results",
            ConfigFieldType::Number,
            false,
            Some(DEFAULT_MAX_RESULTS.to_string()),
            Some("Default result count; hard-capped at 100 to match Tsukihime's public API"),
        ),
        field(
            "include_adult",
            "Include Adult",
            ConfigFieldType::Bool,
            false,
            Some("false".to_string()),
            Some("Include adult releases from Tsukihime"),
        ),
    ]
}

fn search_impl(request: &SearchRequest) -> Result<SearchResponse, TsukihimeError> {
    let config = TsukihimeConfig::from_extism();
    let limit = config.limit_for_request(request.limit);
    let results = if let Some(anime_id) = resolve_anime_id(&config, request)? {
        anime_results(&config, anime_id, request, limit)?
    } else {
        text_or_recent_results(&config, request, limit)?
    };

    Ok(SearchResponse {
        results: torrents_to_search_results(results, config.include_adult, limit),
        ..SearchResponse::default()
    })
}

fn resolve_anime_id(
    config: &TsukihimeConfig,
    request: &SearchRequest,
) -> Result<Option<i64>, TsukihimeError> {
    for (key, endpoint) in [
        (["anidb_id", "anidb"].as_slice(), "anidb"),
        (["anilist_id", "anilist"].as_slice(), "anilist"),
        (["mal_id", "mal"].as_slice(), "mal"),
    ] {
        let Some(id) = first_request_id(request, key) else {
            continue;
        };
        let path = format!("animes/{endpoint}/{}", url_encode(&id));
        match get_json::<Anime>(config, &path) {
            Ok(anime) => return Ok(Some(anime.id)),
            Err(TsukihimeError::NotFound) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn first_request_id(request: &SearchRequest, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        request
            .ids
            .get(*key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn anime_results(
    config: &TsukihimeConfig,
    anime_id: i64,
    request: &SearchRequest,
    limit: usize,
) -> Result<Vec<Torrent>, TsukihimeError> {
    let episode = request.episode.or(request.absolute_episode);
    let page = if let Some(episode) = episode.filter(|episode| *episode > 0) {
        get_json::<TorrentPage>(config, &format!("animes/{anime_id}/episodes/{episode}"))?
    } else {
        get_json::<TorrentPage>(config, &format!("animes/{anime_id}?limit={limit}&offset=0"))?
    };

    let parent_anime = page.anime;
    Ok(page
        .results
        .into_iter()
        .map(|mut torrent| {
            if torrent.anime.is_none() {
                torrent.anime = parent_anime.clone();
            }
            torrent
        })
        .collect())
}

fn text_or_recent_results(
    config: &TsukihimeConfig,
    request: &SearchRequest,
    limit: usize,
) -> Result<Vec<Torrent>, TsukihimeError> {
    let query = search_query(request);
    let path = if query.trim().is_empty() {
        format!("torrents?limit={limit}&offset=0&sort_by=source_date&order=desc")
    } else if query.chars().count() < 2 {
        return Ok(Vec::new());
    } else {
        format!(
            "search/torrents?q={}&limit={limit}&offset=0",
            url_encode(&query)
        )
    };

    Ok(get_json::<TorrentPage>(config, &path)?.results)
}

fn search_query(request: &SearchRequest) -> String {
    request.query.trim().to_string().or_else_empty(|| {
        request
            .tagged_aliases
            .iter()
            .find_map(|alias| {
                let name = alias.name.trim();
                (!name.is_empty()).then(|| name.to_string())
            })
            .unwrap_or_default()
    })
}

trait OrElseEmpty {
    fn or_else_empty<F: FnOnce() -> String>(self, fallback: F) -> String;
}

impl OrElseEmpty for String {
    fn or_else_empty<F: FnOnce() -> String>(self, fallback: F) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}

fn include_torrent(torrent: &Torrent, include_adult: bool) -> bool {
    let completed = torrent.state.as_deref().unwrap_or("completed") == "completed";
    let adult = torrent.is_adult.unwrap_or(0) != 0;
    completed && (include_adult || !adult)
}

fn torrents_to_search_results(
    torrents: Vec<Torrent>,
    include_adult: bool,
    limit: usize,
) -> Vec<SearchResult> {
    torrents
        .into_iter()
        .filter(|torrent| include_torrent(torrent, include_adult))
        .take(limit)
        .flat_map(|torrent| torrent_to_search_results(&torrent, None))
        .collect()
}

fn torrent_to_search_results(torrent: &Torrent, parent_anime: Option<&Anime>) -> Vec<SearchResult> {
    let mut results = vec![torrent_to_search_result(torrent, parent_anime)];
    if has_nzb(torrent) {
        results.push(nzb_to_search_result(torrent, parent_anime));
    }
    results
}

fn torrent_to_search_result(torrent: &Torrent, parent_anime: Option<&Anime>) -> SearchResult {
    let anime = torrent.anime.as_ref().or(parent_anime);
    let mut provider_extra = torrent_provider_extra(torrent, anime);
    insert_json(&mut provider_extra, "download_kind", "torrent");
    let external_ids = anime.map(anime_external_ids).unwrap_or_default();
    let info_url = torrent_info_url(torrent);
    let download_url = torrent_download_url(torrent);
    let magnet_url = torrent
        .btih
        .as_deref()
        .filter(|btih| btih.len() == 40)
        .map(|btih| magnet_uri(btih, &torrent.name));
    let published_at = torrent
        .source_date
        .or(torrent.added_date)
        .map(format_unix_timestamp);

    SearchResult {
        title: torrent.name.clone(),
        link: info_url.clone(),
        download_url,
        size_bytes: torrent.totalsize,
        published_at,
        languages: torrent.audiolangs.clone(),
        subtitles: torrent.sublangs.clone(),
        provider_extra,
        guid: Some(format!("tsukihime-{}", torrent.id)),
        info_url,
        source_kind: Some(IndexerSourceKind::Torrent),
        protocol: Some(IndexerProtocol::Torrent),
        external_ids,
        categories: vec!["anime".to_string()],
        magnet_url,
        info_hash_v1: torrent.btih.clone(),
        ..SearchResult::default()
    }
}

fn nzb_to_search_result(torrent: &Torrent, parent_anime: Option<&Anime>) -> SearchResult {
    let anime = torrent.anime.as_ref().or(parent_anime);
    let mut provider_extra = torrent_provider_extra(torrent, anime);
    insert_json(&mut provider_extra, "download_kind", "nzb");
    insert_json(
        &mut provider_extra,
        "mirrored_info_hash_v1",
        torrent.btih.as_deref(),
    );
    let external_ids = anime.map(anime_external_ids).unwrap_or_default();
    let info_url = torrent_info_url(torrent);
    let published_at = torrent
        .source_date
        .or(torrent.added_date)
        .map(format_unix_timestamp);

    SearchResult {
        title: torrent.name.clone(),
        link: info_url.clone(),
        download_url: Some(nzb_download_url(torrent)),
        size_bytes: torrent.totalsize,
        published_at,
        languages: torrent.audiolangs.clone(),
        subtitles: torrent.sublangs.clone(),
        provider_extra,
        guid: Some(format!("tsukihime-{}-nzb", torrent.id)),
        info_url,
        source_kind: Some(IndexerSourceKind::Usenet),
        protocol: Some(IndexerProtocol::Usenet),
        external_ids,
        categories: vec!["anime".to_string()],
        ..SearchResult::default()
    }
}

fn torrent_provider_extra(torrent: &Torrent, anime: Option<&Anime>) -> HashMap<String, Value> {
    let mut provider_extra = HashMap::new();
    insert_json(&mut provider_extra, "tsukihime_id", torrent.id);
    insert_json(&mut provider_extra, "state", torrent.state.as_deref());
    insert_json(&mut provider_extra, "has_nzb", torrent.has_nzb);
    insert_json(&mut provider_extra, "main_source", torrent.main_source);
    insert_json(
        &mut provider_extra,
        "nyaa_id",
        positive_i64(torrent.nyaa_id),
    );
    insert_json(
        &mut provider_extra,
        "sukebei_id",
        positive_i64(torrent.sukebei_id),
    );
    insert_json(
        &mut provider_extra,
        "nekobt_id",
        positive_i64(torrent.nekobt_id),
    );
    insert_json(&mut provider_extra, "tt_id", positive_i64(torrent.tt_id));
    insert_json(&mut provider_extra, "filecount", torrent.filecount);
    insert_json(&mut provider_extra, "episode_no", torrent.episode_no);
    if let Some(group) = &torrent.group {
        insert_json(&mut provider_extra, "group_id", group.id);
        insert_json(&mut provider_extra, "group", group.name.as_str());
        insert_json(&mut provider_extra, "group_is_fansub", group.is_fansub);
    }
    if let Some(anime) = anime {
        insert_json(&mut provider_extra, "anime_id", anime.id);
        insert_json(&mut provider_extra, "anime_title", anime.title.as_str());
        insert_json(
            &mut provider_extra,
            "anime_english_title",
            anime.english_title.as_deref(),
        );
    }

    provider_extra
}

fn torrent_info_url(torrent: &Torrent) -> Option<String> {
    Some(format!(
        "{}/torrents/{}",
        DEFAULT_BASE_URL.trim_end_matches('/'),
        torrent.id
    ))
}

fn anime_external_ids(anime: &Anime) -> HashMap<String, String> {
    let mut external_ids = HashMap::new();
    if let Some(id) = anime.anidb {
        external_ids.insert("anidb_id".to_string(), id.to_string());
    }
    if let Some(id) = anime.anilist {
        external_ids.insert("anilist_id".to_string(), id.to_string());
    }
    if let Some(id) = anime.mal {
        external_ids.insert("mal_id".to_string(), id.to_string());
    }
    external_ids
}

fn torrent_download_url(torrent: &Torrent) -> Option<String> {
    positive_i64(torrent.nyaa_id)
        .map(|id| format!("https://nyaa.si/download/{id}.torrent"))
        .or_else(|| {
            positive_i64(torrent.sukebei_id)
                .map(|id| format!("https://sukebei.nyaa.si/download/{id}.torrent"))
        })
}

fn nzb_download_url(torrent: &Torrent) -> String {
    format!(
        "https://storage.tsukihime.org/nzbs/{}/{}.nzb.gz",
        torrent.id,
        url_encode(&torrent.name)
    )
}

fn has_nzb(torrent: &Torrent) -> bool {
    positive_i64(torrent.has_nzb).is_some()
}

fn positive_i64(value: Option<i64>) -> Option<i64> {
    value.filter(|value| *value > 0)
}

fn magnet_uri(btih: &str, title: &str) -> String {
    format!("magnet:?xt=urn:btih:{btih}&dn={}", url_encode(title))
}

fn insert_json<T: serde::Serialize>(target: &mut HashMap<String, Value>, key: &str, value: T) {
    if let Ok(value) = serde_json::to_value(value)
        && !value.is_null()
    {
        target.insert(key.to_string(), value);
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(
    config: &TsukihimeConfig,
    path: &str,
) -> Result<T, TsukihimeError> {
    reserve_api_request(path)?;
    let url = format!(
        "{}/{}",
        config.base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let response = http_get(&url)?;
    let _ = sync_rate_limit_from_headers(path, &response.headers);
    match response.status {
        200..=299 => serde_json::from_slice(&response.body).map_err(|error| {
            TsukihimeError::Message(format!("Tsukihime JSON parse error: {error}"))
        }),
        404 => Err(TsukihimeError::NotFound),
        429 => {
            let retry_after = retry_after_seconds(&response.headers);
            let _ = remember_api_rate_limit(path, retry_after);
            Err(TsukihimeError::RateLimited(retry_after))
        }
        status => Err(TsukihimeError::Message(format!(
            "Tsukihime API returned HTTP {status}: {}",
            compact_error_body(&response.body)
        ))),
    }
}

fn http_get(url: &str) -> Result<TsukihimeHttpResponse, TsukihimeError> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", DEFAULT_USER_AGENT);
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| TsukihimeError::Message(format!("Tsukihime request failed: {error}")))?;
    Ok(TsukihimeHttpResponse {
        status: response.status_code(),
        headers: response.headers().clone(),
        body: response.body(),
    })
}

fn reserve_api_request(path: &str) -> Result<(), TsukihimeError> {
    let now = current_epoch_seconds();
    let mut api_state = load_rate_limit_state(API_RATE_LIMIT_VAR_KEY)?;
    normalize_rate_limit_state(&mut api_state, now);
    if let Some(seconds) = rate_limit_retry_after(&api_state, API_RATE_LIMIT_PER_MINUTE, now) {
        return Err(TsukihimeError::RateLimited(Some(seconds)));
    }

    let search_path = is_search_torrents_path(path);
    let mut search_state = if search_path {
        let mut state = load_rate_limit_state(SEARCH_RATE_LIMIT_VAR_KEY)?;
        normalize_rate_limit_state(&mut state, now);
        if let Some(seconds) = rate_limit_retry_after(&state, SEARCH_RATE_LIMIT_PER_MINUTE, now) {
            return Err(TsukihimeError::RateLimited(Some(seconds)));
        }
        Some(state)
    } else {
        None
    };

    api_state.count = api_state.count.saturating_add(1);
    save_rate_limit_state(API_RATE_LIMIT_VAR_KEY, &api_state)?;
    if let Some(state) = search_state.as_mut() {
        state.count = state.count.saturating_add(1);
        save_rate_limit_state(SEARCH_RATE_LIMIT_VAR_KEY, state)?;
    }
    Ok(())
}

fn sync_rate_limit_from_headers(
    path: &str,
    headers: &HashMap<String, String>,
) -> Result<(), TsukihimeError> {
    let Some(retry_after) = retry_after_from_remaining_headers(headers, current_epoch_seconds())
    else {
        return Ok(());
    };
    remember_api_rate_limit(path, Some(retry_after))
}

fn remember_api_rate_limit(path: &str, retry_after: Option<i64>) -> Result<(), TsukihimeError> {
    let Some(seconds) = retry_after.filter(|seconds| *seconds > 0) else {
        return Ok(());
    };
    let now = current_epoch_seconds();
    let blocked_until = now.saturating_add(seconds as u64);
    block_rate_limit_key(API_RATE_LIMIT_VAR_KEY, blocked_until, now)?;
    if is_search_torrents_path(path) {
        block_rate_limit_key(SEARCH_RATE_LIMIT_VAR_KEY, blocked_until, now)?;
    }
    Ok(())
}

fn block_rate_limit_key(key: &str, blocked_until: u64, now: u64) -> Result<(), TsukihimeError> {
    let mut state = load_rate_limit_state(key)?;
    normalize_rate_limit_state(&mut state, now);
    state.blocked_until = state.blocked_until.max(blocked_until);
    save_rate_limit_state(key, &state)
}

fn load_rate_limit_state(key: &str) -> Result<RateLimitState, TsukihimeError> {
    let raw = var::get::<String>(key).map_err(|error| {
        TsukihimeError::Message(format!("failed to read rate limit state: {error}"))
    })?;
    Ok(raw
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default())
}

fn save_rate_limit_state(key: &str, state: &RateLimitState) -> Result<(), TsukihimeError> {
    let rendered = serde_json::to_string(state).map_err(|error| {
        TsukihimeError::Message(format!("failed to encode rate limit state: {error}"))
    })?;
    var::set(key, rendered).map_err(|error| {
        TsukihimeError::Message(format!("failed to store rate limit state: {error}"))
    })
}

fn normalize_rate_limit_state(state: &mut RateLimitState, now: u64) {
    let window_id = now / RATE_LIMIT_WINDOW_SECONDS;
    if state.window_id != window_id {
        state.window_id = window_id;
        state.count = 0;
    }
    if state.blocked_until <= now {
        state.blocked_until = 0;
    }
}

fn rate_limit_retry_after(state: &RateLimitState, limit: u32, now: u64) -> Option<i64> {
    if state.blocked_until > now {
        return Some((state.blocked_until - now).min(i64::MAX as u64) as i64);
    }
    if state.count < limit {
        return None;
    }
    let next_window = state
        .window_id
        .saturating_add(1)
        .saturating_mul(RATE_LIMIT_WINDOW_SECONDS);
    Some(next_window.saturating_sub(now).max(1) as i64)
}

fn is_search_torrents_path(path: &str) -> bool {
    path.trim_start_matches('/')
        .split('?')
        .next()
        .is_some_and(|endpoint| endpoint == "search/torrents")
}

fn retry_after_seconds(headers: &HashMap<String, String>) -> Option<i64> {
    header_value(headers, "Retry-After").and_then(|value| value.trim().parse::<i64>().ok())
}

fn retry_after_from_remaining_headers(headers: &HashMap<String, String>, now: u64) -> Option<i64> {
    let remaining = header_value(headers, "X-RateLimit-Remaining")?
        .trim()
        .parse::<i64>()
        .ok()?;
    if remaining > 0 {
        return None;
    }
    let reset = header_value(headers, "X-RateLimit-Reset")?
        .trim()
        .parse::<u64>()
        .ok()?;
    let seconds = if reset > now {
        reset.saturating_sub(now)
    } else {
        reset
    };
    (seconds > 0).then_some(seconds.min(RATE_LIMIT_WINDOW_SECONDS) as i64)
}

fn header_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn compact_error_body(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let trimmed = body.trim();
    const MAX_ERROR_BODY_CHARS: usize = 240;
    if trimmed.chars().count() > MAX_ERROR_BODY_CHARS {
        format!(
            "{}...",
            trimmed
                .chars()
                .take(MAX_ERROR_BODY_CHARS)
                .collect::<String>()
        )
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug)]
enum TsukihimeError {
    Message(String),
    NotFound,
    RateLimited(Option<i64>),
}

impl fmt::Display for TsukihimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::NotFound => formatter.write_str("Tsukihime API resource was not found"),
            Self::RateLimited(Some(seconds)) => {
                write!(
                    formatter,
                    "Tsukihime API rate limited; retry after {seconds}s"
                )
            }
            Self::RateLimited(None) => formatter.write_str("Tsukihime API rate limited"),
        }
    }
}

struct TsukihimeHttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Default, Serialize, Deserialize)]
struct RateLimitState {
    window_id: u64,
    count: u32,
    blocked_until: u64,
}

#[derive(Clone)]
struct TsukihimeConfig {
    base_url: String,
    max_results: usize,
    include_adult: bool,
}

impl TsukihimeConfig {
    fn from_extism() -> Self {
        Self {
            base_url: config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_results: config_usize("max_results", DEFAULT_MAX_RESULTS),
            include_adult: config_bool("include_adult", false),
        }
    }

    fn limit_for_request(&self, request_limit: usize) -> usize {
        let requested = if request_limit == 0 {
            self.max_results
        } else {
            request_limit.min(self.max_results)
        };
        requested.clamp(1, API_MAX_RESULTS)
    }
}

#[derive(Debug, Deserialize)]
struct TorrentPage {
    #[serde(default)]
    results: Vec<Torrent>,
    #[serde(default)]
    anime: Option<Anime>,
}

#[derive(Debug, Deserialize)]
struct Torrent {
    id: i64,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    has_nzb: Option<i64>,
    #[serde(default)]
    main_source: Option<i64>,
    #[serde(default)]
    nyaa_id: Option<i64>,
    #[serde(default)]
    sukebei_id: Option<i64>,
    #[serde(default)]
    nekobt_id: Option<i64>,
    #[serde(default)]
    tt_id: Option<i64>,
    name: String,
    #[serde(default)]
    btih: Option<String>,
    #[serde(default)]
    is_adult: Option<i64>,
    #[serde(default)]
    totalsize: Option<i64>,
    #[serde(default)]
    filecount: Option<i64>,
    #[serde(default)]
    audiolangs: Vec<String>,
    #[serde(default)]
    sublangs: Vec<String>,
    #[serde(default)]
    episode_no: Option<i64>,
    #[serde(default)]
    source_date: Option<i64>,
    #[serde(default)]
    added_date: Option<i64>,
    #[serde(default)]
    anime: Option<Anime>,
    #[serde(default)]
    group: Option<Group>,
}

#[derive(Clone, Debug, Deserialize)]
struct Anime {
    id: i64,
    title: String,
    #[serde(default)]
    english_title: Option<String>,
    #[serde(default)]
    anilist: Option<i64>,
    #[serde(default)]
    mal: Option<i64>,
    #[serde(default)]
    anidb: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Group {
    id: i64,
    name: String,
    #[serde(default)]
    is_fansub: Option<i64>,
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<String>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value,
        value_source: ConfigFieldValueSource::User,
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
            default_value.map(str::to_string),
            help_text,
        )
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_bool(key: &str, default: bool) -> bool {
    config_value(key)
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn config_usize(key: &str, default: usize) -> usize {
    config_value(key)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn url_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

fn format_unix_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_exposes_kind_api_limits_and_anime_ids() {
        let descriptor = build_descriptor();
        assert_eq!(descriptor.id, "tsukihime-indexer");
        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert_eq!(indexer.provider_type, "tsukihime");
        assert_eq!(indexer.source_kind, IndexerSourceKind::Generic);
        assert_eq!(indexer.rate_limit_seconds, Some(2));
        assert_eq!(
            indexer.capabilities.protocols,
            vec![
                IndexerProtocol::Mixed,
                IndexerProtocol::Torrent,
                IndexerProtocol::Usenet
            ]
        );
        assert_eq!(
            indexer.capabilities.supported_ids.get("anime"),
            Some(&vec![
                "anidb_id".to_string(),
                "anilist_id".to_string(),
                "mal_id".to_string()
            ])
        );
        assert!(
            indexer
                .capabilities
                .search_inputs
                .contains(&IndexerSearchInput::TextQuery)
        );
    }

    #[test]
    fn torrent_fixture_maps_to_magnet_download_and_external_ids() {
        let page: TorrentPage = serde_json::from_str(
            r#"{
                "results": [{
                    "id": 10062,
                    "state": "completed",
                    "has_nzb": 1,
                    "main_source": 1,
                    "nyaa_id": 2127842,
                    "name": "[Feibanyama] Wistoria Wand and Sword S02E12 [BILIBILI WebRip 2160p NVENC AAC Multi-Subs]",
                    "btih": "8139954265daaedac6e24aef7ef4034b59b1b91e",
                    "is_adult": 0,
                    "totalsize": 1130485424,
                    "filecount": 1,
                    "audiolangs": ["ja"],
                    "sublangs": ["zh-Hans", "en"],
                    "episode_no": 12,
                    "source_date": 1783111465,
                    "anime": {"id": 75, "title": "Tsue to Tsurugi no Wistoria Season 2", "anilist": 182300, "mal": 59983, "anidb": 18889},
                    "group": {"id": 6, "name": "Feibanyama", "is_fansub": 0}
                }]
            }"#,
        )
        .expect("fixture parses");

        let results = torrents_to_search_results(page.results, false, 1);
        assert_eq!(results.len(), 2);

        let result = &results[0];
        let nzb = &results[1];

        assert_eq!(
            result.download_url.as_deref(),
            Some("https://nyaa.si/download/2127842.torrent")
        );
        assert_eq!(
            result.info_hash_v1.as_deref(),
            Some("8139954265daaedac6e24aef7ef4034b59b1b91e")
        );
        assert!(
            result
                .magnet_url
                .as_deref()
                .unwrap()
                .starts_with("magnet:?xt=urn:btih:8139954265daaedac6e24aef7ef4034b59b1b91e&dn=")
        );
        assert_eq!(
            result.external_ids.get("anidb_id").map(String::as_str),
            Some("18889")
        );
        assert_eq!(
            result.subtitles,
            vec!["zh-Hans".to_string(), "en".to_string()]
        );
        assert_eq!(result.source_kind, Some(IndexerSourceKind::Torrent));
        assert_eq!(result.protocol, Some(IndexerProtocol::Torrent));
        assert_eq!(
            result
                .provider_extra
                .get("download_kind")
                .and_then(|value| value.as_str()),
            Some("torrent")
        );

        assert_eq!(
            nzb.download_url.as_deref(),
            Some(
                "https://storage.tsukihime.org/nzbs/10062/%5BFeibanyama%5D%20Wistoria%20Wand%20and%20Sword%20S02E12%20%5BBILIBILI%20WebRip%202160p%20NVENC%20AAC%20Multi-Subs%5D.nzb.gz"
            )
        );
        assert_eq!(nzb.source_kind, Some(IndexerSourceKind::Usenet));
        assert_eq!(nzb.protocol, Some(IndexerProtocol::Usenet));
        assert_eq!(nzb.guid.as_deref(), Some("tsukihime-10062-nzb"));
        assert_eq!(
            nzb.provider_extra
                .get("download_kind")
                .and_then(|value| value.as_str()),
            Some("nzb")
        );
    }

    #[test]
    fn one_character_search_is_not_sent_to_api() {
        let request = SearchRequest {
            query: "a".to_string(),
            ..SearchRequest::default()
        };

        assert_eq!(search_query(&request), "a");
        assert!(search_query(&request).chars().count() < 2);
    }

    #[test]
    fn compact_error_body_truncates_on_utf8_boundary() {
        let body = "界".repeat(241);
        let compact = compact_error_body(body.as_bytes());

        assert_eq!(compact.chars().count(), 243);
        assert!(compact.ends_with("..."));
    }

    #[test]
    fn local_rate_limit_state_uses_fixed_minute_windows() {
        let now = 1783119040;
        let mut state = RateLimitState {
            window_id: now / RATE_LIMIT_WINDOW_SECONDS,
            count: API_RATE_LIMIT_PER_MINUTE,
            blocked_until: 0,
        };

        assert_eq!(
            rate_limit_retry_after(&state, API_RATE_LIMIT_PER_MINUTE, now),
            Some(
                state
                    .window_id
                    .saturating_add(1)
                    .saturating_mul(RATE_LIMIT_WINDOW_SECONDS)
                    .saturating_sub(now) as i64
            )
        );

        normalize_rate_limit_state(&mut state, now + RATE_LIMIT_WINDOW_SECONDS);
        assert_eq!(state.count, 0);
        assert_eq!(
            rate_limit_retry_after(&state, API_RATE_LIMIT_PER_MINUTE, now + 60),
            None
        );
    }

    #[test]
    fn search_torrent_requests_use_dedicated_lower_budget() {
        assert!(is_search_torrents_path("search/torrents?q=wistoria"));
        assert!(is_search_torrents_path("/search/torrents?limit=50"));
        assert!(!is_search_torrents_path("torrents?limit=50"));
        assert_eq!(SEARCH_RATE_LIMIT_PER_MINUTE, 25);
    }

    #[test]
    fn rate_limit_headers_are_case_insensitive() {
        let mut headers = HashMap::new();
        headers.insert("x-ratelimit-remaining".to_string(), "0".to_string());
        headers.insert("X-RateLimit-Reset".to_string(), "12".to_string());
        headers.insert("retry-after".to_string(), "9".to_string());

        assert_eq!(retry_after_seconds(&headers), Some(9));
        assert_eq!(retry_after_from_remaining_headers(&headers, 100), Some(12));
    }

    #[test]
    fn timestamp_formatter_outputs_rfc3339_utc() {
        assert_eq!(format_unix_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_timestamp(1_783_111_465), "2026-07-03T20:44:25Z");
    }

    #[test]
    fn adult_and_incomplete_releases_are_filtered_by_default() {
        let mut torrent: Torrent = serde_json::from_str(
            r#"{"id": 1, "state": "processing", "name": "Example", "is_adult": 0}"#,
        )
        .expect("torrent parses");
        assert!(!include_torrent(&torrent, false));

        torrent.state = Some("completed".to_string());
        torrent.is_adult = Some(1);
        assert!(!include_torrent(&torrent, false));
        assert!(include_torrent(&torrent, true));
    }
}
