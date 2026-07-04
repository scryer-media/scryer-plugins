use std::collections::HashMap;
use std::fmt;
use std::io::{self, Cursor, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor,
    PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint, SubtitleMatchHintKind,
    SubtitlePluginCandidate, SubtitlePluginDownloadRequest, SubtitlePluginDownloadResponse,
    SubtitlePluginSearchRequest, SubtitlePluginSearchResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PROVIDER_ID: &str = "tsukihime-subtitles";
const PROVIDER_TYPE: &str = "tsukihime";
const DEFAULT_BASE_URL: &str = "https://api.tsukihime.org/v1";
const STORAGE_BASE_URL: &str = "https://storage.tsukihime.org";
const DEFAULT_USER_AGENT: &str = "Scryer Tsukihime Subtitles/0.1";
const DEFAULT_MAX_RESULTS: usize = 50;
const DEFAULT_MAX_DETAIL_FETCHES: usize = 10;
const API_MAX_RESULTS: usize = 100;
const API_RATE_LIMIT_PER_MINUTE: u32 = 60;
const SEARCH_RATE_LIMIT_PER_MINUTE: u32 = 25;
const RATE_LIMIT_WINDOW_SECONDS: u64 = 60;
const API_RATE_LIMIT_VAR_KEY: &str = "tsukihime-subtitles-api-rate-limit-v1";
const SEARCH_RATE_LIMIT_VAR_KEY: &str = "tsukihime-subtitles-search-rate-limit-v1";
const MAX_COMPRESSED_SUBTITLE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DECOMPRESSED_SUBTITLE_BYTES: usize = 16 * 1024 * 1024;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "Tsukihime Subtitles".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: vec!["tsukihime.org".to_string()],
            config_fields: config_fields(),
            default_base_url: Some(DEFAULT_BASE_URL.to_string()),
            allowed_hosts: vec![
                "api.tsukihime.org".to_string(),
                "storage.tsukihime.org".to_string(),
            ],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![
                    SubtitleQueryMediaKind::Movie,
                    SubtitleQueryMediaKind::Episode,
                ],
                recommended_facets: vec!["anime".to_string()],
                supports_forced: true,
                supported_languages: vec![
                    "ara".to_string(),
                    "deu".to_string(),
                    "eng".to_string(),
                    "fra".to_string(),
                    "ita".to_string(),
                    "jpn".to_string(),
                    "por".to_string(),
                    "rus".to_string(),
                    "spa".to_string(),
                    "zho".to_string(),
                ],
                ..SubtitleCapabilities::default()
            },
        }),
    }
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let config = TsukihimeConfig::from_extism();
    let response = match get_json::<Value>(&config, "stats") {
        Ok(_) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
        Err(error) => validation_error_response(error),
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let response = match subtitle_search_impl(&request) {
        Ok(results) => SubtitlePluginSearchResponse { results },
        Err(TsukihimeError::RateLimited(_)) => SubtitlePluginSearchResponse::default(),
        Err(error) => return Err(Error::msg(error.to_string()).into()),
    };
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let reference: TsukihimeDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(Error::msg)?;
    let result = match subtitle_download_impl(&reference) {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(plugin_error(error)),
    };
    Ok(serde_json::to_string(&result)?)
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
            Some("Default search result count; hard-capped at 100"),
        ),
        field(
            "max_detail_fetches",
            "Max Detail Fetches",
            ConfigFieldType::Number,
            false,
            Some(DEFAULT_MAX_DETAIL_FETCHES.to_string()),
            Some("Maximum per-torrent detail requests during a subtitle search"),
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

fn subtitle_search_impl(
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, TsukihimeError> {
    let config = TsukihimeConfig::from_extism();
    let limit = config.limit_for_request(DEFAULT_MAX_RESULTS);
    let summaries = initial_torrent_summaries(&config, request, limit)?;
    let mut results = Vec::new();

    for summary in summaries
        .into_iter()
        .filter(|torrent| include_torrent_summary(torrent, &config, request))
        .take(config.max_detail_fetches)
    {
        let detail = match get_json::<TorrentDetail>(&config, &format!("torrents/{}", summary.id)) {
            Ok(detail) => detail,
            Err(TsukihimeError::RateLimited(_)) => break,
            Err(error) => return Err(error),
        };
        append_detail_candidates(&mut results, &config, request, detail)?;
    }

    Ok(results)
}

fn initial_torrent_summaries(
    config: &TsukihimeConfig,
    request: &SubtitlePluginSearchRequest,
    limit: usize,
) -> Result<Vec<TorrentSummary>, TsukihimeError> {
    if let Some(anime_id) = resolve_anime_id(config, request)? {
        let episode = request.episode.or(request.absolute_episode);
        let page = if let Some(episode) = episode.filter(|episode| *episode > 0) {
            get_json::<TorrentPage>(config, &format!("animes/{anime_id}/episodes/{episode}"))?
        } else {
            get_json::<TorrentPage>(config, &format!("animes/{anime_id}?limit={limit}&offset=0"))?
        };
        return Ok(page.results);
    }

    let Some(query) = title_query(request) else {
        return Ok(Vec::new());
    };
    if query.chars().count() < 2 {
        return Ok(Vec::new());
    }

    let page = get_json::<TorrentPage>(
        config,
        &format!(
            "search/torrents?q={}&limit={limit}&offset=0",
            url_encode(&query)
        ),
    )?;
    Ok(page.results)
}

fn resolve_anime_id(
    config: &TsukihimeConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Option<i64>, TsukihimeError> {
    for (keys, endpoint) in [
        (["anidb_id", "anidb"].as_slice(), "anidb"),
        (["anilist_id", "anilist"].as_slice(), "anilist"),
        (["mal_id", "mal"].as_slice(), "mal"),
    ] {
        let Some(id) = first_external_id(request, keys) else {
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

fn first_external_id(request: &SubtitlePluginSearchRequest, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        request
            .external_ids
            .get(*key)
            .into_iter()
            .flatten()
            .map(|value| value.trim())
            .find(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn title_query(request: &SubtitlePluginSearchRequest) -> Option<String> {
    request
        .title_candidates
        .iter()
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn include_torrent_summary(
    torrent: &TorrentSummary,
    config: &TsukihimeConfig,
    request: &SubtitlePluginSearchRequest,
) -> bool {
    let completed = torrent.state.as_deref().unwrap_or("completed") == "completed";
    let adult = torrent.is_adult.unwrap_or(0) != 0;
    completed
        && (config.include_adult || !adult)
        && torrent
            .sublangs
            .iter()
            .any(|language| requested_language_matches(&request.languages, language))
}

fn append_detail_candidates(
    results: &mut Vec<SubtitlePluginCandidate>,
    config: &TsukihimeConfig,
    request: &SubtitlePluginSearchRequest,
    detail: TorrentDetail,
) -> Result<(), TsukihimeError> {
    if !include_torrent_summary(&detail.summary, config, request) {
        return Ok(());
    }

    let match_hints_base = detail_match_hints(request, &detail);
    for file in &detail.files {
        for attachment in &file.attachments {
            if attachment.kind != 1 || !attachment.cached() {
                continue;
            }
            let Some(info) = attachment.info.as_ref() else {
                continue;
            };
            let Some(language) = info.lang.as_deref() else {
                continue;
            };
            if !requested_language_matches(&request.languages, language) {
                continue;
            }
            let Some(url) = storage_url(file, attachment) else {
                continue;
            };
            let format = info
                .codec
                .as_deref()
                .map(|codec| codec.trim().to_ascii_lowercase())
                .filter(|codec| !codec.is_empty())
                .unwrap_or_else(|| "ass".to_string());
            let filename = storage_filename(file, attachment)
                .map(|filename| filename.trim_end_matches(".xz").to_string())
                .unwrap_or_else(|| format!("tsukihime-{}.{format}", attachment.id));
            let provider_file_id = serde_json::to_string(&TsukihimeDownloadRef {
                torrent_id: detail.summary.id,
                file_id: file.id,
                attachment_id: attachment.id,
                url,
                filename: filename.clone(),
                format: format.clone(),
                language: language.to_string(),
            })
            .map_err(|error| {
                TsukihimeError::Message(format!("failed to encode Tsukihime subtitle ref: {error}"))
            })?;
            let mut match_hints = match_hints_base.clone();
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::Language,
                value: Some(language_to_scryer(language).to_string()),
            });
            results.push(SubtitlePluginCandidate {
                provider_file_id,
                language: language_to_scryer(language).to_string(),
                release_info: Some(format!(
                    "{} / {} / {}",
                    detail.summary.name,
                    file.filename,
                    info.name.as_deref().unwrap_or(language)
                )),
                hearing_impaired: false,
                forced: info.flag("forced"),
                ai_translated: false,
                machine_translated: false,
                uploader: detail
                    .summary
                    .group
                    .as_ref()
                    .map(|group| group.name.clone()),
                download_count: None,
                match_hints,
            });
        }
    }
    Ok(())
}

fn detail_match_hints(
    request: &SubtitlePluginSearchRequest,
    detail: &TorrentDetail,
) -> Vec<SubtitleMatchHint> {
    let mut hints = Vec::new();
    hints.push(SubtitleMatchHint {
        kind: SubtitleMatchHintKind::Title,
        value: None,
    });
    if request.media_kind == SubtitleQueryMediaKind::Episode
        && request.episode.or(request.absolute_episode).is_some()
    {
        hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::SeasonEpisode,
            value: detail
                .summary
                .episode_no
                .map(|episode_no| episode_no.to_string()),
        });
    }
    if let Some(anime) = &detail.summary.anime {
        for (source, value) in [
            ("anidb", anime.anidb),
            ("anilist", anime.anilist),
            ("mal", anime.mal),
        ] {
            if let Some(value) = value {
                hints.push(SubtitleMatchHint {
                    kind: SubtitleMatchHintKind::ExternalId,
                    value: Some(format!("{source}:{value}")),
                });
            }
        }
    }
    hints
}

fn subtitle_download_impl(
    reference: &TsukihimeDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, TsukihimeError> {
    let response = http_get(&reference.url, "application/x-xz")?;
    match response.status {
        200..=299 => {}
        429 => {
            return Err(TsukihimeError::RateLimited(retry_after_seconds(
                &response.headers,
            )));
        }
        404 => return Err(TsukihimeError::NotFound),
        status => {
            return Err(TsukihimeError::Message(format!(
                "Tsukihime subtitle storage returned HTTP {status}: {}",
                compact_error_body(&response.body)
            )));
        }
    }
    if response.body.len() > MAX_COMPRESSED_SUBTITLE_BYTES {
        return Err(TsukihimeError::Message(format!(
            "Tsukihime subtitle is too large: {} compressed bytes",
            response.body.len()
        )));
    }
    let content = decompress_xz(&response.body)?;
    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(content),
        format: reference.format.clone(),
        filename: Some(reference.filename.clone()),
        content_type: subtitle_content_type(&reference.format).map(str::to_string),
    })
}

fn storage_url(file: &TsukihimeFile, attachment: &TsukihimeAttachment) -> Option<String> {
    storage_filename(file, attachment).map(|filename| {
        format!(
            "{}/attach/{:08X}/{}",
            STORAGE_BASE_URL,
            attachment.id,
            url_encode(&filename)
        )
    })
}

fn storage_filename(file: &TsukihimeFile, attachment: &TsukihimeAttachment) -> Option<String> {
    let info = attachment.info.as_ref()?;
    let tracknum = info.tracknum?;
    let language = info.lang.as_deref()?.trim();
    let codec = info
        .codec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("ass")
        .to_ascii_lowercase();
    let stem = file_stem(&file.filename);
    Some(format!("{stem}_track{tracknum}.{language}.{codec}.xz"))
}

fn file_stem(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(filename)
}

fn decompress_xz(bytes: &[u8]) -> Result<Vec<u8>, TsukihimeError> {
    let mut input = Cursor::new(bytes);
    let mut output = LimitWriter::new(MAX_DECOMPRESSED_SUBTITLE_BYTES);
    lzma_rs::xz_decompress(&mut input, &mut output)
        .map_err(|error| TsukihimeError::Message(format!("Tsukihime XZ decode failed: {error}")))?;
    Ok(output.into_inner())
}

struct LimitWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed subtitle exceeds size limit",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn subtitle_content_type(format: &str) -> Option<&'static str> {
    match format.trim().to_ascii_lowercase().as_str() {
        "ass" | "ssa" => Some("text/x-ssa"),
        "srt" => Some("application/x-subrip"),
        "vtt" => Some("text/vtt"),
        _ => None,
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
    let response = http_get(&url, "application/json")?;
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

fn http_get(url: &str, accept: &str) -> Result<TsukihimeHttpResponse, TsukihimeError> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", accept)
        .with_header("User-Agent", DEFAULT_USER_AGENT);
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| TsukihimeError::Message(format!("Tsukihime request failed: {error}")))?;
    Ok(TsukihimeHttpResponse {
        status: response.status_code(),
        headers: response.headers().clone(),
        body: response.body(),
    })
}

fn validation_error_response(error: TsukihimeError) -> SubtitlePluginValidateConfigResponse {
    let status = match error {
        TsukihimeError::RateLimited(_) => SubtitleValidateConfigStatus::RateLimited,
        TsukihimeError::NotFound => SubtitleValidateConfigStatus::Unreachable,
        TsukihimeError::Message(ref message) if message.contains("request failed") => {
            SubtitleValidateConfigStatus::Unreachable
        }
        TsukihimeError::Message(_) => SubtitleValidateConfigStatus::Unsupported,
    };
    SubtitlePluginValidateConfigResponse {
        status,
        message: Some(error.to_string()),
        retry_after_seconds: match error {
            TsukihimeError::RateLimited(seconds) => seconds,
            _ => None,
        },
    }
}

fn plugin_error(error: TsukihimeError) -> PluginError {
    let (code, retry_after_seconds) = match error {
        TsukihimeError::RateLimited(seconds) => (PluginErrorCode::RateLimited, seconds),
        TsukihimeError::NotFound => (PluginErrorCode::UpstreamUnavailable, None),
        TsukihimeError::Message(_) => (PluginErrorCode::Temporary, None),
    };
    PluginError {
        code,
        public_message: error.to_string(),
        debug_message: None,
        retry_after_seconds,
    }
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
            Self::NotFound => formatter.write_str("Tsukihime resource was not found"),
            Self::RateLimited(Some(seconds)) => {
                write!(formatter, "Tsukihime rate limited; retry after {seconds}s")
            }
            Self::RateLimited(None) => formatter.write_str("Tsukihime rate limited"),
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
    max_detail_fetches: usize,
    include_adult: bool,
}

impl TsukihimeConfig {
    fn from_extism() -> Self {
        Self {
            base_url: config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_results: config_usize("max_results", DEFAULT_MAX_RESULTS),
            max_detail_fetches: config_usize("max_detail_fetches", DEFAULT_MAX_DETAIL_FETCHES)
                .clamp(1, API_MAX_RESULTS),
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
    results: Vec<TorrentSummary>,
}

#[derive(Clone, Debug, Deserialize)]
struct TorrentSummary {
    id: i64,
    #[serde(default)]
    state: Option<String>,
    name: String,
    #[serde(default)]
    is_adult: Option<i64>,
    #[serde(default)]
    sublangs: Vec<String>,
    #[serde(default)]
    episode_no: Option<i64>,
    #[serde(default)]
    anime: Option<Anime>,
    #[serde(default)]
    group: Option<Group>,
}

#[derive(Debug, Deserialize)]
struct TorrentDetail {
    #[serde(flatten)]
    summary: TorrentSummary,
    #[serde(default)]
    files: Vec<TsukihimeFile>,
}

#[derive(Clone, Debug, Deserialize)]
struct Anime {
    id: i64,
    #[serde(default)]
    anilist: Option<i64>,
    #[serde(default)]
    mal: Option<i64>,
    #[serde(default)]
    anidb: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct Group {
    name: String,
}

#[derive(Debug, Deserialize)]
struct TsukihimeFile {
    id: i64,
    filename: String,
    #[serde(default)]
    attachments: Vec<TsukihimeAttachment>,
}

#[derive(Debug, Deserialize)]
struct TsukihimeAttachment {
    id: i64,
    #[serde(rename = "type")]
    kind: i64,
    #[serde(default)]
    info: Option<AttachmentInfo>,
}

impl TsukihimeAttachment {
    fn cached(&self) -> bool {
        self.info.as_ref().is_some_and(|info| info.flag("cached"))
    }
}

#[derive(Debug, Deserialize)]
struct AttachmentInfo {
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tracknum: Option<i64>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

impl AttachmentInfo {
    fn flag(&self, key: &str) -> bool {
        self.extra.get(key).is_some_and(truthy_value)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TsukihimeDownloadRef {
    torrent_id: i64,
    file_id: i64,
    attachment_id: i64,
    url: String,
    filename: String,
    format: String,
    language: String,
}

fn truthy_value(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_i64().is_some_and(|value| value != 0),
        Value::String(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        _ => false,
    }
}

fn requested_language_matches(requested: &[String], tsukihime_language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|language| normalize_language(language) == normalize_language(tsukihime_language))
}

fn language_to_scryer(language: &str) -> &str {
    normalize_language(language)
}

fn normalize_language(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "ar" | "ara" | "arabic" => "ara",
        "de" | "deu" | "ger" | "german" => "deu",
        "en" | "en-us" | "en-gb" | "eng" | "english" => "eng",
        "es" | "es-419" | "es-es" | "spa" | "spanish" => "spa",
        "fr" | "fra" | "fre" | "french" => "fra",
        "it" | "ita" | "italian" => "ita",
        "ja" | "jp" | "jpn" | "japanese" => "jpn",
        "pt" | "pt-br" | "por" | "portuguese" => "por",
        "ru" | "rus" | "russian" => "rus",
        "zh" | "zho" | "chi" | "zh-hans" | "zh-hant" | "chinese" => "zho",
        _ => language,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_detail() -> TorrentDetail {
        serde_json::from_str(
            r#"{
                "id": 10062,
                "state": "completed",
                "name": "[Feibanyama] Wistoria Wand and Sword S02E12 [BILIBILI WebRip 2160p NVENC AAC Multi-Subs] (Tsue to Tsurugi no Wistoria)",
                "is_adult": 0,
                "sublangs": ["zh-Hans", "en"],
                "episode_no": 12,
                "anime": {"id": 75, "anilist": 182300, "mal": 59983, "anidb": 18889},
                "group": {"name": "Feibanyama"},
                "files": [{
                    "id": 12436,
                    "filename": "[Feibanyama] Wistoria Wand and Sword S02E12 [BILIBILI WebRip 2160p NVENC AAC Multi-Subs].mkv",
                    "attachments": [{
                        "id": 68652,
                        "type": 1,
                        "info": {
                            "codec": "ASS",
                            "lang": "en",
                            "name": "English",
                            "cached": 1,
                            "forced": 0,
                            "tracknum": 5
                        }
                    }]
                }]
            }"#,
        )
        .expect("fixture parses")
    }

    #[test]
    fn descriptor_is_catalog_subtitle_provider() {
        let descriptor = build_descriptor();
        assert_eq!(descriptor.id, "tsukihime-subtitles");
        let ProviderDescriptor::Subtitle(subtitle) = descriptor.provider else {
            panic!("expected subtitle descriptor");
        };
        assert_eq!(subtitle.provider_type, "tsukihime");
        assert_eq!(subtitle.default_base_url.as_deref(), Some(DEFAULT_BASE_URL));
        assert!(
            subtitle
                .allowed_hosts
                .contains(&"storage.tsukihime.org".to_string())
        );
        assert!(subtitle.capabilities.supports_forced);
    }

    #[test]
    fn storage_url_uses_uppercase_hex_attachment_folder_and_encoded_filename() {
        let detail = fixture_detail();
        let file = &detail.files[0];
        let attachment = &file.attachments[0];

        assert_eq!(
            storage_filename(file, attachment).as_deref(),
            Some(
                "[Feibanyama] Wistoria Wand and Sword S02E12 [BILIBILI WebRip 2160p NVENC AAC Multi-Subs]_track5.en.ass.xz"
            )
        );
        assert_eq!(
            storage_url(file, attachment).as_deref(),
            Some(
                "https://storage.tsukihime.org/attach/00010C2C/%5BFeibanyama%5D%20Wistoria%20Wand%20and%20Sword%20S02E12%20%5BBILIBILI%20WebRip%202160p%20NVENC%20AAC%20Multi-Subs%5D_track5.en.ass.xz"
            )
        );
    }

    #[test]
    fn detail_candidates_include_cached_matching_subtitle_tracks() {
        let mut results = Vec::new();
        let request = SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "Wistoria: Wand and Sword".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: None,
            season: Some(2),
            episode: Some(12),
            absolute_episode: None,
            external_ids: Default::default(),
            languages: vec!["eng".to_string()],
            release_group: None,
            source: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            hearing_impaired: None,
            include_ai_translated: false,
            include_machine_translated: false,
        };
        let config = TsukihimeConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            max_results: DEFAULT_MAX_RESULTS,
            max_detail_fetches: DEFAULT_MAX_DETAIL_FETCHES,
            include_adult: false,
        };

        append_detail_candidates(&mut results, &config, &request, fixture_detail())
            .expect("candidate mapping succeeds");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].language, "eng");
        assert!(results[0].provider_file_id.contains("00010C2C"));
        assert!(
            results[0]
                .match_hints
                .iter()
                .any(|hint| matches!(hint.kind, SubtitleMatchHintKind::ExternalId))
        );
    }

    #[test]
    fn language_matching_accepts_bcp47_and_iso3_variants() {
        assert!(requested_language_matches(&["eng".to_string()], "en-US"));
        assert!(requested_language_matches(&["zho".to_string()], "zh-Hans"));
        assert!(requested_language_matches(&["spa".to_string()], "es-419"));
        assert!(!requested_language_matches(&["jpn".to_string()], "en"));
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
    fn xz_decoder_returns_subtitle_bytes() {
        let compressed = BASE64
            .decode("/Td6WFoAAATm1rRGBMAeGiEBFgAAAAAAAAAAAPycLfcBABlbU2NyaXB0IEluZm9dClRpdGxlOiBUZXN0CgAAABKoqqDNCqTNAAE6GiiSTfgftvN9AQAAAAAEWVo=")
            .expect("fixture base64");
        let decoded = decompress_xz(&compressed).expect("xz fixture decodes");

        assert_eq!(decoded, b"[Script Info]\nTitle: Test\n");
    }
}
