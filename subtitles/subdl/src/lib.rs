use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginResult,
    ProviderDescriptor, SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint,
    SubtitleMatchHintKind, SubtitlePluginCandidate, SubtitlePluginDownloadRequest,
    SubtitlePluginDownloadResponse, SubtitlePluginSearchRequest, SubtitlePluginSearchResponse,
    SubtitlePluginValidateConfigRequest, SubtitlePluginValidateConfigResponse,
    SubtitleProviderMode, SubtitleQueryMediaKind, SubtitleValidateConfigStatus, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://api.subdl.com/api/v1";
const DOWNLOAD_BASE: &str = "https://dl.subdl.com";
const PAGE_BASE: &str = "https://subdl.com";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const RETRY_AMOUNT: usize = 3;
const RETRY_TIMEOUT_SECS: u64 = 5;
const MAX_RATE_LIMIT_WAIT_SECONDS: i64 = 10;
const VALIDATION_PROBE_TITLE: &str = "Inception";
const SUBS_PER_PAGE: &str = "30";

#[derive(Clone)]
struct SubdlConfig {
    api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    InvalidConfig,
    AuthFailed,
    RateLimited,
    Unreachable,
    Unsupported,
    Provider,
}

#[derive(Debug, Clone)]
struct Failure {
    kind: FailureKind,
    message: String,
    retry_after_seconds: Option<i64>,
}

#[derive(Debug, Clone)]
struct SubdlQuery {
    movie_title: Option<String>,
    imdb_id: Option<String>,
    tmdb_id: Option<String>,
    languages: Vec<String>,
    media_kind: SubtitleQueryMediaKind,
    season: Option<i32>,
    episode: Option<i32>,
}

#[derive(Debug, Clone)]
struct SearchContext {
    imdb_hint: bool,
    series_imdb_hint: bool,
    external_id_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SubdlDownloadRef {
    download_url: String,
    filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubdlSearchResponse {
    #[serde(default)]
    status: Option<bool>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<SubdlResult>,
    #[serde(default)]
    subtitles: Vec<SubdlSubtitleItem>,
}

#[derive(Debug, Deserialize)]
struct SubdlResult {
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    tmdb_id: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct SubdlSubtitleItem {
    language: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    hi: bool,
    url: String,
    #[serde(default, alias = "subtitlePage")]
    subtitle_page: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    releases: Vec<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    season: Option<i32>,
    #[serde(default)]
    episode: Option<i32>,
    #[serde(default)]
    episode_from: Option<i32>,
    #[serde(default)]
    episode_end: Option<i32>,
}

#[derive(Debug)]
struct DownloadArtifact {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let response = match SubdlConfig::from_extism() {
        Ok(config) => match validate_config_impl(&config) {
            Ok(()) => SubtitlePluginValidateConfigResponse {
                status: SubtitleValidateConfigStatus::Valid,
                message: None,
                retry_after_seconds: None,
            },
            Err(failure) => validation_error_response(&failure),
        },
        Err(failure) => validation_error_response(&failure),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = SubdlConfig::from_extism().map_err(|failure| Error::msg(failure.message))?;
    let results =
        search_subtitles_impl(&config, &request).map_err(|failure| Error::msg(failure.message))?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        SubtitlePluginSearchResponse { results },
    ))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let config = SubdlConfig::from_extism().map_err(|failure| Error::msg(failure.message))?;
    let reference: SubdlDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(Error::msg)?;
    let response = download_subtitle_impl(&config, &reference)
        .map_err(|failure| Error::msg(failure.message))?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

impl SubdlConfig {
    fn from_extism() -> Result<Self, Failure> {
        Ok(Self {
            api_key: config_required_string("api_key")?,
        })
    }
}

impl Failure {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    fn with_retry_after(mut self, retry_after_seconds: Option<i64>) -> Self {
        self.retry_after_seconds = retry_after_seconds;
        self
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "subdl".to_string(),
        name: "Subdl".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "subdl".to_string(),
            provider_aliases: vec![],
            config_fields: vec![ConfigFieldDef {
                key: "api_key".to_string(),
                label: "Subdl API Key".to_string(),
                field_type: ConfigFieldType::Password,
                required: true,
                default_value: None,
                value_source: ConfigFieldValueSource::User,
                role: None,
                host_binding: None,
                options: vec![],
                help_text: Some("API key from your Subdl account.".to_string()),
            }],
            default_base_url: Some(API_BASE.to_string()),
            allowed_hosts: vec![
                "api.subdl.com".to_string(),
                "dl.subdl.com".to_string(),
                "subdl.com".to_string(),
            ],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![
                    SubtitleQueryMediaKind::Movie,
                    SubtitleQueryMediaKind::Episode,
                ],
                recommended_facets: vec!["movie".to_string(), "series".to_string()],
                supports_hash_lookup: false,
                supports_forced: true,
                supports_hearing_impaired: true,
                supports_ai_translated: false,
                supports_machine_translated: false,
                supported_languages: supported_languages(),
            },
        }),
    }
}

fn supported_languages() -> Vec<String> {
    language_mappings()
        .iter()
        .map(|(_, code)| (*code).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_config_impl(config: &SubdlConfig) -> Result<(), Failure> {
    let query = SubdlQuery {
        movie_title: Some(VALIDATION_PROBE_TITLE.to_string()),
        imdb_id: None,
        tmdb_id: None,
        languages: vec!["EN".to_string()],
        media_kind: SubtitleQueryMediaKind::Movie,
        season: None,
        episode: None,
    };
    match execute_search_request(config, &query) {
        Ok(_) => Ok(()),
        Err(failure) if looks_like_not_found(&failure.message) => Ok(()),
        Err(failure) => Err(failure),
    }
}

fn search_subtitles_impl(
    config: &SubdlConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, Failure> {
    let Some(query) = build_query(request) else {
        return Ok(Vec::new());
    };

    let primary_context = SearchContext::from_response_hint(request, None);
    let response = execute_search_request(config, &query)?;
    let mut results = response_to_candidates(request, &response, &primary_context);

    if request.media_kind == SubtitleQueryMediaKind::Movie
        && should_try_tmdb_fallback(&query, &response)
    {
        let fallback_query = SubdlQuery {
            movie_title: None,
            imdb_id: None,
            tmdb_id: query.tmdb_id.clone(),
            languages: query.languages.clone(),
            media_kind: SubtitleQueryMediaKind::Movie,
            season: None,
            episode: None,
        };
        let tmdb_response = execute_search_request(config, &fallback_query)?;
        let context = SearchContext::from_response_hint(request, tmdb_response.results.first());
        results = response_to_candidates(request, &tmdb_response, &context);
    }

    Ok(results)
}

fn build_query(request: &SubtitlePluginSearchRequest) -> Option<SubdlQuery> {
    let languages = requested_languages(request)?;
    let movie_title = match request.media_kind {
        SubtitleQueryMediaKind::Movie => title_for_search(request),
        SubtitleQueryMediaKind::Episode => title_for_search(request),
    };

    let imdb_id = match request.media_kind {
        SubtitleQueryMediaKind::Episode => request.series_imdb_id.clone(),
        SubtitleQueryMediaKind::Movie => request.imdb_id.clone(),
    }
    .filter(|value| !value.trim().is_empty());

    let tmdb_id = if request.media_kind == SubtitleQueryMediaKind::Movie {
        first_external_id(request, "tmdb")
    } else {
        None
    };

    Some(SubdlQuery {
        movie_title: if imdb_id.is_some() { None } else { movie_title },
        imdb_id,
        tmdb_id,
        languages,
        media_kind: request.media_kind,
        season: request.season,
        episode: request.episode,
    })
}

fn requested_languages(request: &SubtitlePluginSearchRequest) -> Option<Vec<String>> {
    let mapped = request
        .languages
        .iter()
        .filter_map(|language| to_subdl_language(language))
        .collect::<BTreeSet<_>>();
    (!mapped.is_empty()).then(|| mapped.into_iter().collect())
}

fn title_for_search(request: &SubtitlePluginSearchRequest) -> Option<String> {
    request
        .title_candidates
        .iter()
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
        .find_map(|candidate| normalize_non_empty(candidate))
}

fn first_external_id(request: &SubtitlePluginSearchRequest, source: &str) -> Option<String> {
    request
        .external_ids
        .get(source)
        .and_then(|values| values.iter().find_map(|value| normalize_non_empty(value)))
}

fn execute_search_request(
    config: &SubdlConfig,
    query: &SubdlQuery,
) -> Result<SubdlSearchResponse, Failure> {
    let url = search_url(config, query);
    retry_request(|| http_get_json(&url), RETRY_AMOUNT, RETRY_TIMEOUT_SECS)
}

fn should_try_tmdb_fallback(query: &SubdlQuery, response: &SubdlSearchResponse) -> bool {
    query.tmdb_id.is_some() && !response.success_flag() && query.imdb_id.is_some()
}

fn search_url(config: &SubdlConfig, query: &SubdlQuery) -> String {
    let mut params = vec![
        ("api_key", config.api_key.clone()),
        ("languages", query.languages.join(",")),
        ("subs_per_page", SUBS_PER_PAGE.to_string()),
        ("comment", "1".to_string()),
        ("releases", "1".to_string()),
        ("bazarr", "1".to_string()),
    ];

    match query.media_kind {
        SubtitleQueryMediaKind::Episode => {
            params.push(("type", "tv".to_string()));
            if let Some(episode) = query.episode {
                params.push(("episode_number", episode.to_string()));
            }
            if let Some(season) = query.season {
                params.push(("season_number", season.to_string()));
            }
        }
        SubtitleQueryMediaKind::Movie => {
            params.push(("type", "movie".to_string()));
        }
    }

    if let Some(title) = query.movie_title.as_ref() {
        params.push(("film_name", title.clone()));
    }
    if let Some(imdb_id) = query.imdb_id.as_ref() {
        params.push(("imdb_id", imdb_id.clone()));
    }
    if let Some(tmdb_id) = query.tmdb_id.as_ref() {
        params.push(("tmdb_id", tmdb_id.clone()));
    }

    format!(
        "{}/subtitles?{}",
        API_BASE.trim_end_matches('/'),
        encode_query(&params)
    )
}

fn response_to_candidates(
    request: &SubtitlePluginSearchRequest,
    response: &SubdlSearchResponse,
    context: &SearchContext,
) -> Vec<SubtitlePluginCandidate> {
    response
        .subtitles
        .iter()
        .filter(|item| !is_season_pack(item, request.media_kind))
        .filter_map(|item| subtitle_item_to_candidate(request, item, context))
        .collect()
}

fn subtitle_item_to_candidate(
    request: &SubtitlePluginSearchRequest,
    item: &SubdlSubtitleItem,
    context: &SearchContext,
) -> Option<SubtitlePluginCandidate> {
    let language = from_subdl_language(&item.language)?;
    if !requested_language_matches(&request.languages, &language) {
        return None;
    }

    let forced = is_forced(item);
    let hearing_impaired = item.hi || is_hi(item);
    let filename = subtitle_filename(item);
    let download_url = absolute_url(DOWNLOAD_BASE, item.url.as_str());
    let page_url = item
        .subtitle_page
        .as_deref()
        .map(|url| absolute_url(PAGE_BASE, url));
    let provider_file_id = serde_json::to_string(&SubdlDownloadRef {
        download_url,
        filename,
        page_url,
    })
    .ok()?;

    let mut match_hints = vec![
        SubtitleMatchHint {
            kind: SubtitleMatchHintKind::Title,
            value: None,
        },
        SubtitleMatchHint {
            kind: SubtitleMatchHintKind::Language,
            value: Some(language.clone()),
        },
    ];
    if context.imdb_hint {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::ImdbId,
            value: None,
        });
    }
    if context.series_imdb_hint {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::SeriesImdbId,
            value: None,
        });
    }
    if request.media_kind == SubtitleQueryMediaKind::Episode
        && request.season == item.season
        && request.episode == item.episode
    {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::SeasonEpisode,
            value: None,
        });
    }
    for external_id in &context.external_id_hints {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::ExternalId,
            value: Some(external_id.clone()),
        });
    }
    for release in &item.releases {
        if let Some(release) = normalize_non_empty(release) {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::Release,
                value: Some(release),
            });
        }
    }

    Some(SubtitlePluginCandidate {
        provider_file_id,
        language,
        release_info: (!item.releases.is_empty()).then(|| item.releases.join(", ")),
        hearing_impaired,
        forced,
        ai_translated: false,
        machine_translated: false,
        uploader: normalize_non_empty(item.author.as_deref().unwrap_or_default()),
        download_count: None,
        match_hints,
    })
}

fn download_subtitle_impl(
    _config: &SubdlConfig,
    reference: &SubdlDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, Failure> {
    let artifact = retry_request(
        || http_get_download(reference.download_url.as_str()),
        RETRY_AMOUNT,
        RETRY_TIMEOUT_SECS,
    )?;

    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(artifact.bytes),
        format: file_extension(&reference.filename)
            .unwrap_or("zip")
            .to_string(),
        filename: Some(reference.filename.clone()),
        content_type: artifact.content_type,
    })
}

fn http_get_json(url: &str) -> Result<SubdlSearchResponse, Failure> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    let response = http::request::<Vec<u8>>(&request, None).map_err(|error| {
        Failure::new(
            FailureKind::Unreachable,
            format!("Subdl request failed: {error}"),
        )
    })?;

    map_http_status("Subdl search", &response)?;
    let parsed: SubdlSearchResponse =
        serde_json::from_slice(&response.body()).map_err(|error| {
            Failure::new(
                FailureKind::Unsupported,
                format!("Subdl JSON parse error: {error}"),
            )
        })?;
    if !parsed.success_flag() {
        let error = parsed
            .error
            .clone()
            .unwrap_or_else(|| "Subdl search failed".to_string());
        if looks_like_not_found(&error) {
            return Ok(parsed);
        }
        return Err(Failure::new(FailureKind::Provider, error));
    }
    Ok(parsed)
}

fn http_get_download(url: &str) -> Result<DownloadArtifact, Failure> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "*/*")
        .with_header("User-Agent", USER_AGENT);
    let response = http::request::<Vec<u8>>(&request, None).map_err(|error| {
        Failure::new(
            FailureKind::Unreachable,
            format!("Subdl download request failed: {error}"),
        )
    })?;

    let body_text = response_body_text(&response);
    if is_download_rate_limited(response.status_code(), body_text.as_str()) {
        return Err(download_rate_limited_failure(retry_after_seconds(
            &response,
        )));
    }

    map_http_status("Subdl download", &response)?;
    Ok(DownloadArtifact {
        bytes: response.body(),
        content_type: Some("application/zip".to_string()),
    })
}

fn map_http_status(label: &str, response: &HttpResponse) -> Result<(), Failure> {
    let body_text = response_body_text(response);
    map_http_status_details(
        label,
        response.status_code(),
        body_text.as_str(),
        retry_after_seconds(response),
    )
}

fn map_http_status_details(
    label: &str,
    status: u16,
    body_text: &str,
    retry_after_seconds: Option<i64>,
) -> Result<(), Failure> {
    match status {
        200 => Ok(()),
        403 => Err(Failure::new(
            FailureKind::AuthFailed,
            format!("{label} authentication failed"),
        )),
        429 => {
            let message = match retry_after_seconds {
                Some(seconds) if seconds > 0 => {
                    format!("{label} rate limited — retry after {seconds}s")
                }
                _ => format!("{label} rate limited — try again later"),
            };
            Err(Failure::new(FailureKind::RateLimited, message)
                .with_retry_after(retry_after_seconds))
        }
        status if status >= 500 => Err(Failure::new(
            FailureKind::Unsupported,
            format!("{label} returned HTTP {status}: {body_text}"),
        )),
        status => Err(Failure::new(
            FailureKind::Unsupported,
            format!("{label} returned HTTP {status}: {body_text}"),
        )),
    }
}

fn retry_request<T, F>(mut f: F, amount: usize, retry_timeout_secs: u64) -> Result<T, Failure>
where
    F: FnMut() -> Result<T, Failure>,
{
    let mut last_error = None;
    for attempt in 0..amount {
        match f() {
            Ok(value) => return Ok(value),
            Err(error) => {
                let retryable = matches!(
                    error.kind,
                    FailureKind::RateLimited | FailureKind::Unreachable
                );
                if !retryable || attempt + 1 >= amount {
                    return Err(error);
                }
                last_error = Some(error);
                std::thread::sleep(Duration::from_secs(retry_timeout_secs));
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| Failure::new(FailureKind::Unsupported, "Subdl request failed")))
}

fn validation_error_response(failure: &Failure) -> SubtitlePluginValidateConfigResponse {
    let status = match failure.kind {
        FailureKind::InvalidConfig => SubtitleValidateConfigStatus::InvalidConfig,
        FailureKind::AuthFailed => SubtitleValidateConfigStatus::AuthFailed,
        FailureKind::RateLimited => SubtitleValidateConfigStatus::RateLimited,
        FailureKind::Unreachable => SubtitleValidateConfigStatus::Unreachable,
        FailureKind::Unsupported | FailureKind::Provider => {
            SubtitleValidateConfigStatus::Unsupported
        }
    };
    SubtitlePluginValidateConfigResponse {
        status,
        message: Some(failure.message.clone()),
        retry_after_seconds: failure.retry_after_seconds,
    }
}

fn config_required_string(key: &str) -> Result<String, Failure> {
    match config::get(key) {
        Ok(Some(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(Failure::new(
                    FailureKind::InvalidConfig,
                    format!("missing required config value '{key}'"),
                ))
            } else {
                Ok(trimmed.to_string())
            }
        }
        Ok(None) => Err(Failure::new(
            FailureKind::InvalidConfig,
            format!("missing required config value '{key}'"),
        )),
        Err(error) => Err(Failure::new(
            FailureKind::InvalidConfig,
            format!("failed to read config value '{key}': {error}"),
        )),
    }
}

fn response_body_text(response: &HttpResponse) -> String {
    String::from_utf8_lossy(&response.body()).trim().to_string()
}

fn retry_after_seconds(response: &HttpResponse) -> Option<i64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-ratelimit-reset"))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seconds| *seconds > 0 && *seconds <= MAX_RATE_LIMIT_WAIT_SECONDS)
}

fn is_download_rate_limited(status: u16, body_text: &str) -> bool {
    status == 429 || (status == 500 && body_text.trim() == "Download limit exceeded")
}

fn download_rate_limited_failure(retry_after_seconds: Option<i64>) -> Failure {
    Failure::new(
        FailureKind::RateLimited,
        "Subdl rate limited — daily download limit exceeded",
    )
    .with_retry_after(retry_after_seconds)
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn absolute_url(base: &str, path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            trimmed.trim_start_matches('/')
        )
    }
}

fn is_season_pack(item: &SubdlSubtitleItem, media_kind: SubtitleQueryMediaKind) -> bool {
    media_kind == SubtitleQueryMediaKind::Episode && item.episode_from != item.episode_end
}

fn is_hi(item: &SubdlSubtitleItem) -> bool {
    let comment = item.comment.to_ascii_lowercase();
    for tag in [
        "hi remove",
        "non hi",
        "nonhi",
        "non-hi",
        "non-sdh",
        "non sdh",
        "nonsdh",
        "sdh remove",
    ] {
        if comment.contains(tag) {
            return false;
        }
    }

    if item.name.to_ascii_lowercase().contains("_hi_") {
        return true;
    }

    for tag in ["_hi_", " hi ", ".hi.", "hi ", " hi", "sdh", "𝓢𝓓𝓗"] {
        if comment.contains(tag) {
            return true;
        }
    }

    let lowered_releases = item
        .releases
        .iter()
        .map(|release| release.to_ascii_lowercase())
        .collect::<Vec<_>>();
    for tag in ["_hi_", " hi ", ".hi.", "hi ", " hi", "sdh", "𝓢𝓓𝓗"] {
        if lowered_releases.iter().any(|release| release == tag) {
            return true;
        }
    }

    false
}

fn is_forced(item: &SubdlSubtitleItem) -> bool {
    let comment = item.comment.to_ascii_lowercase();
    ["forced", "foreign"]
        .iter()
        .any(|tag| comment.contains(tag))
}

fn looks_like_not_found(error: &str) -> bool {
    error.to_ascii_lowercase().contains("can't find")
}

fn subtitle_filename(item: &SubdlSubtitleItem) -> String {
    normalize_non_empty(&item.name)
        .or_else(|| filename_from_url(item.url.as_str()))
        .unwrap_or_else(|| "subdl.zip".to_string())
}

fn filename_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    trimmed.rsplit('/').next().and_then(normalize_non_empty)
}

fn file_extension(filename: &str) -> Option<&str> {
    filename.rsplit_once('.').map(|(_, ext)| ext)
}

fn requested_language_matches(requested_languages: &[String], candidate_language: &str) -> bool {
    let Some(candidate_provider_code) = to_subdl_language(candidate_language) else {
        return false;
    };
    requested_languages
        .iter()
        .filter_map(|language| to_subdl_language(language))
        .any(|provider_code| provider_code.eq_ignore_ascii_case(&candidate_provider_code))
}

impl SearchContext {
    fn from_response_hint(
        request: &SubtitlePluginSearchRequest,
        result: Option<&SubdlResult>,
    ) -> Self {
        let imdb_hint = request.media_kind == SubtitleQueryMediaKind::Movie
            && request.imdb_id.is_some()
            && result
                .and_then(|result| result.imdb_id.as_ref())
                .is_none_or(|imdb| request.imdb_id.as_deref() == Some(imdb.as_str()));
        let series_imdb_hint = request.media_kind == SubtitleQueryMediaKind::Episode
            && request.series_imdb_id.is_some()
            && result
                .and_then(|result| result.imdb_id.as_ref())
                .is_none_or(|imdb| request.series_imdb_id.as_deref() == Some(imdb.as_str()));

        let mut external_id_hints = Vec::new();
        if let Some(tmdb_id) = result
            .and_then(|result| result.tmdb_id.as_ref())
            .and_then(tmdb_id_as_string)
        {
            if request
                .external_ids
                .get("tmdb")
                .is_some_and(|values| values.iter().any(|value| value == &tmdb_id))
            {
                external_id_hints.push(format!("tmdb:{tmdb_id}"));
            }
        }

        Self {
            imdb_hint,
            series_imdb_hint,
            external_id_hints,
        }
    }
}

fn tmdb_id_as_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => normalize_non_empty(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

impl SubdlSearchResponse {
    fn success_flag(&self) -> bool {
        self.success.or(self.status).unwrap_or(true)
    }
}

fn language_mappings() -> &'static [(&'static str, &'static str)] {
    &[
        ("AR", "ara"),
        ("DA", "dan"),
        ("NL", "nld"),
        ("EN", "eng"),
        ("FA", "fas"),
        ("FI", "fin"),
        ("FR", "fra"),
        ("ID", "ind"),
        ("IT", "ita"),
        ("NO", "nor"),
        ("RO", "ron"),
        ("ES", "spa"),
        ("SV", "swe"),
        ("VI", "vie"),
        ("SQ", "sqi"),
        ("AZ", "aze"),
        ("BE", "bel"),
        ("BN", "ben"),
        ("BS", "bos"),
        ("BG", "bul"),
        ("MY", "mya"),
        ("CA", "cat"),
        ("ZH", "zho"),
        ("HR", "hrv"),
        ("CS", "ces"),
        ("EO", "epo"),
        ("ET", "est"),
        ("KA", "kat"),
        ("DE", "deu"),
        ("EL", "ell"),
        ("KL", "kal"),
        ("HE", "heb"),
        ("HI", "hin"),
        ("HU", "hun"),
        ("IS", "isl"),
        ("JA", "jpn"),
        ("KO", "kor"),
        ("KU", "kur"),
        ("LV", "lav"),
        ("LT", "lit"),
        ("MK", "mkd"),
        ("MS", "msa"),
        ("ML", "mal"),
        ("PL", "pol"),
        ("PT", "por"),
        ("RU", "rus"),
        ("SR", "srp"),
        ("SI", "sin"),
        ("SK", "slk"),
        ("SL", "slv"),
        ("TL", "tgl"),
        ("TA", "tam"),
        ("TE", "tel"),
        ("TH", "tha"),
        ("TR", "tur"),
        ("UK", "ukr"),
        ("UR", "urd"),
        ("BR_PT", "pob"),
        ("ZH_BG", "zht"),
    ]
}

fn from_subdl_language(code: &str) -> Option<String> {
    let upper = code.trim().to_ascii_uppercase();
    language_mappings()
        .iter()
        .find_map(|(subdl_code, scryer_code)| {
            (*subdl_code == upper).then(|| (*scryer_code).to_string())
        })
}

fn to_subdl_language(code: &str) -> Option<String> {
    let upper = code.trim().replace('_', "-").to_ascii_uppercase();
    let normalized = match upper.as_str() {
        "ARA" | "AR" => Some("ara"),
        "DAN" | "DA" => Some("dan"),
        "NLD" | "NL" | "DUT" => Some("nld"),
        "ENG" | "EN" => Some("eng"),
        "FAS" | "FA" | "PER" => Some("fas"),
        "FIN" | "FI" => Some("fin"),
        "FRA" | "FR" | "FRE" => Some("fra"),
        "IND" | "ID" => Some("ind"),
        "ITA" | "IT" => Some("ita"),
        "NOR" | "NO" | "NB" | "NN" => Some("nor"),
        "RON" | "RO" | "RUM" => Some("ron"),
        "SPA" | "ES" => Some("spa"),
        "SWE" | "SV" => Some("swe"),
        "VIE" | "VI" => Some("vie"),
        "SQI" | "SQ" | "ALB" => Some("sqi"),
        "AZE" | "AZ" => Some("aze"),
        "BEL" | "BE" => Some("bel"),
        "BEN" | "BN" => Some("ben"),
        "BOS" | "BS" => Some("bos"),
        "BUL" | "BG" => Some("bul"),
        "MYA" | "MY" | "BUR" => Some("mya"),
        "CAT" | "CA" => Some("cat"),
        "ZHO" | "ZH" | "ZH-CN" => Some("zho"),
        "HRV" | "HR" => Some("hrv"),
        "CES" | "CS" | "CZE" => Some("ces"),
        "EPO" | "EO" => Some("epo"),
        "EST" | "ET" => Some("est"),
        "KAT" | "KA" | "GEO" => Some("kat"),
        "DEU" | "DE" | "GER" => Some("deu"),
        "ELL" | "EL" | "GRE" => Some("ell"),
        "KAL" | "KL" => Some("kal"),
        "HEB" | "HE" | "IW" => Some("heb"),
        "HIN" | "HI" => Some("hin"),
        "HUN" | "HU" => Some("hun"),
        "ISL" | "IS" | "ICE" => Some("isl"),
        "JPN" | "JA" | "JP" => Some("jpn"),
        "KOR" | "KO" => Some("kor"),
        "KUR" | "KU" => Some("kur"),
        "LAV" | "LV" => Some("lav"),
        "LIT" | "LT" => Some("lit"),
        "MKD" | "MK" | "MAC" => Some("mkd"),
        "MSA" | "MS" | "MAY" => Some("msa"),
        "MAL" | "ML" => Some("mal"),
        "POL" | "PL" => Some("pol"),
        "POR" | "PT" | "PT-PT" => Some("por"),
        "RUS" | "RU" => Some("rus"),
        "SRP" | "SR" | "SCC" => Some("srp"),
        "SIN" | "SI" => Some("sin"),
        "SLK" | "SK" | "SLO" => Some("slk"),
        "SLV" | "SL" => Some("slv"),
        "TGL" | "TL" => Some("tgl"),
        "TAM" | "TA" => Some("tam"),
        "TEL" | "TE" => Some("tel"),
        "THA" | "TH" => Some("tha"),
        "TUR" | "TR" => Some("tur"),
        "UKR" | "UK" => Some("ukr"),
        "URD" | "UR" => Some("urd"),
        "POB" | "PT-BR" | "PB" => Some("pob"),
        "ZHT" | "ZH-TW" | "CHT" | "BIG5" | "HANT" => Some("zht"),
        _ => None,
    };

    let normalized = normalized?;
    language_mappings()
        .iter()
        .find_map(|(subdl_code, scryer_code)| {
            (*scryer_code == normalized).then(|| (*subdl_code).to_string())
        })
}

fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={}", url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn movie_request() -> SubtitlePluginSearchRequest {
        let mut external_ids = BTreeMap::new();
        external_ids.insert("tmdb".to_string(), vec!["438631".to_string()]);
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Movie,
            facet: Some("movie".to_string()),
            file_hash: None,
            imdb_id: Some("tt1160419".to_string()),
            series_imdb_id: None,
            title: "Dune".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: Some(2021),
            season: None,
            episode: None,
            absolute_episode: None,
            external_ids,
            languages: vec!["eng".to_string(), "pob".to_string()],
            release_group: None,
            source: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            hearing_impaired: None,
            include_ai_translated: false,
            include_machine_translated: false,
        }
    }

    fn episode_request() -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            facet: Some("series".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: Some("tt0903747".to_string()),
            title: "Breaking Bad".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: Some(2008),
            season: Some(1),
            episode: Some(1),
            absolute_episode: None,
            external_ids: BTreeMap::new(),
            languages: vec!["eng".to_string()],
            release_group: Some("NTb".to_string()),
            source: Some("Web".to_string()),
            video_codec: Some("x265".to_string()),
            audio_codec: None,
            resolution: Some("720p".to_string()),
            hearing_impaired: Some(false),
            include_ai_translated: false,
            include_machine_translated: false,
        }
    }

    #[test]
    fn descriptor_declares_expected_config_and_capabilities() {
        let descriptor = descriptor();
        let ProviderDescriptor::Subtitle(subtitle) = &descriptor.provider else {
            panic!("subtitle descriptor");
        };
        assert_eq!(descriptor.id, "subdl");
        assert_eq!(subtitle.provider_type, "subdl");
        assert_eq!(subtitle.config_fields.len(), 1);
        let field = &subtitle.config_fields[0];
        assert_eq!(field.key, "api_key");
        assert_eq!(field.field_type, ConfigFieldType::Password);
        assert!(field.required);
        assert_eq!(
            subtitle.capabilities.supported_media_kinds,
            vec![
                SubtitleQueryMediaKind::Movie,
                SubtitleQueryMediaKind::Episode
            ]
        );
        assert_eq!(
            subtitle.capabilities.recommended_facets,
            vec!["movie".to_string(), "series".to_string()]
        );
        assert!(subtitle.capabilities.supports_hearing_impaired);
        assert!(subtitle.capabilities.supports_forced);
        assert!(!subtitle.capabilities.supports_hash_lookup);
    }

    #[test]
    fn build_movie_query_uses_imdb_and_tmdb_fallback_inputs() {
        let request = movie_request();
        let query = build_query(&request).expect("query");
        assert_eq!(query.imdb_id.as_deref(), Some("tt1160419"));
        assert_eq!(query.tmdb_id.as_deref(), Some("438631"));
        assert_eq!(query.movie_title, None);
        assert_eq!(query.languages, vec!["BR_PT".to_string(), "EN".to_string()]);
    }

    #[test]
    fn build_episode_query_uses_series_imdb_and_episode_numbers() {
        let request = episode_request();
        let query = build_query(&request).expect("query");
        assert_eq!(query.imdb_id.as_deref(), Some("tt0903747"));
        assert_eq!(query.season, Some(1));
        assert_eq!(query.episode, Some(1));
        assert_eq!(query.media_kind, SubtitleQueryMediaKind::Episode);
    }

    #[test]
    fn search_url_matches_bazarr_shape() {
        let config = SubdlConfig {
            api_key: "token".to_string(),
        };
        let url = search_url(
            &config,
            &SubdlQuery {
                movie_title: Some("Dune".to_string()),
                imdb_id: None,
                tmdb_id: None,
                languages: vec!["EN".to_string()],
                media_kind: SubtitleQueryMediaKind::Movie,
                season: None,
                episode: None,
            },
        );
        assert!(url.contains("api_key=token"));
        assert!(url.contains("film_name=Dune"));
        assert!(url.contains("languages=EN"));
        assert!(url.contains("type=movie"));
        assert!(url.contains("comment=1"));
        assert!(url.contains("releases=1"));
        assert!(url.contains("bazarr=1"));
    }

    #[test]
    fn language_round_trip_matches_bazarr_converter() {
        assert_eq!(to_subdl_language("eng").as_deref(), Some("EN"));
        assert_eq!(to_subdl_language("pob").as_deref(), Some("BR_PT"));
        assert_eq!(to_subdl_language("zht").as_deref(), Some("ZH_BG"));
        assert_eq!(from_subdl_language("EN").as_deref(), Some("eng"));
        assert_eq!(from_subdl_language("BR_PT").as_deref(), Some("pob"));
        assert_eq!(from_subdl_language("ZH_BG").as_deref(), Some("zht"));
    }

    #[test]
    fn season_packs_are_ignored_for_episode_queries() {
        let item = SubdlSubtitleItem {
            language: "EN".to_string(),
            comment: String::new(),
            hi: false,
            url: "/subtitle/file.zip".to_string(),
            subtitle_page: None,
            name: "show-s01-pack.zip".to_string(),
            releases: vec![],
            author: None,
            season: Some(1),
            episode: Some(1),
            episode_from: Some(1),
            episode_end: Some(7),
        };
        assert!(is_season_pack(&item, SubtitleQueryMediaKind::Episode));
        assert!(!is_season_pack(&item, SubtitleQueryMediaKind::Movie));
    }

    #[test]
    fn hi_detection_matches_bazarr_heuristics() {
        let mut item = SubdlSubtitleItem {
            language: "EN".to_string(),
            comment: "English SDH release".to_string(),
            hi: false,
            url: "/subtitle/file.zip".to_string(),
            subtitle_page: None,
            name: "show.zip".to_string(),
            releases: vec!["WEB".to_string()],
            author: None,
            season: Some(1),
            episode: Some(1),
            episode_from: Some(1),
            episode_end: Some(1),
        };
        assert!(is_hi(&item));
        item.comment = "non hi cleaned".to_string();
        assert!(!is_hi(&item));
        item.comment.clear();
        item.name = "show_HI_release.zip".to_string();
        assert!(is_hi(&item));
    }

    #[test]
    fn forced_detection_matches_bazarr_heuristics() {
        let item = SubdlSubtitleItem {
            language: "EN".to_string(),
            comment: "Forced foreign parts only".to_string(),
            hi: false,
            url: "/subtitle/file.zip".to_string(),
            subtitle_page: None,
            name: "show.zip".to_string(),
            releases: vec![],
            author: None,
            season: Some(1),
            episode: Some(1),
            episode_from: Some(1),
            episode_end: Some(1),
        };
        assert!(is_forced(&item));
    }

    #[test]
    fn candidate_mapping_flattens_release_names_and_preserves_metadata() {
        let request = movie_request();
        let response = SubdlSearchResponse {
            status: Some(true),
            success: None,
            error: None,
            results: vec![SubdlResult {
                imdb_id: Some("tt1160419".to_string()),
                tmdb_id: Some(serde_json::Value::String("438631".to_string())),
            }],
            subtitles: vec![SubdlSubtitleItem {
                language: "EN".to_string(),
                comment: "Forced SDH".to_string(),
                hi: false,
                url: "/subtitle/2808552-2770424.zip".to_string(),
                subtitle_page: Some("/s/info/ebC6BrLCOC".to_string()),
                name: "dune-2021-2770424.zip".to_string(),
                releases: vec!["Dune Part 1 WebDl".to_string()],
                author: Some("makoto77".to_string()),
                season: Some(0),
                episode: None,
                episode_from: Some(0),
                episode_end: Some(0),
            }],
        };
        let context = SearchContext::from_response_hint(&request, response.results.first());
        let candidate = response_to_candidates(&request, &response, &context)
            .into_iter()
            .next()
            .expect("candidate");
        assert_eq!(candidate.language, "eng");
        assert_eq!(candidate.release_info.as_deref(), Some("Dune Part 1 WebDl"));
        assert_eq!(candidate.uploader.as_deref(), Some("makoto77"));
        assert!(candidate.forced);
        assert!(candidate.hearing_impaired);
        let download_ref: SubdlDownloadRef =
            serde_json::from_str(&candidate.provider_file_id).expect("download ref");
        assert_eq!(
            download_ref.download_url,
            "https://dl.subdl.com/subtitle/2808552-2770424.zip"
        );
        assert_eq!(download_ref.filename, "dune-2021-2770424.zip");
    }

    #[test]
    fn download_limit_and_auth_errors_map_cleanly() {
        let auth = map_http_status_details("Subdl download", 403, "forbidden", None)
            .expect_err("auth error");
        assert_eq!(auth.kind, FailureKind::AuthFailed);

        let rate = http_download_failure_for_test(500, "Download limit exceeded", None);
        assert_eq!(rate.kind, FailureKind::RateLimited);

        let throttled = http_download_failure_for_test(429, "Too many requests", Some(30));
        assert_eq!(throttled.kind, FailureKind::RateLimited);
        assert_eq!(throttled.retry_after_seconds, Some(30));
    }

    #[test]
    fn validation_error_response_preserves_retry_after() {
        let failure =
            Failure::new(FailureKind::RateLimited, "too many requests").with_retry_after(Some(12));
        let response = validation_error_response(&failure);
        assert_eq!(response.status, SubtitleValidateConfigStatus::RateLimited);
        assert_eq!(response.retry_after_seconds, Some(12));
    }

    fn http_download_failure_for_test(
        status: u16,
        body_text: &str,
        retry_after_seconds: Option<i64>,
    ) -> Failure {
        if is_download_rate_limited(status, body_text) {
            download_rate_limited_failure(retry_after_seconds)
        } else {
            map_http_status_details("Subdl download", status, body_text, retry_after_seconds)
                .expect_err("failure")
        }
    }
}
