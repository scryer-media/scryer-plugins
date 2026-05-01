use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginHostBindingId,
    PluginResult, ProviderDescriptor, SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint,
    SubtitleMatchHintKind, SubtitlePluginCandidate, SubtitlePluginDownloadRequest,
    SubtitlePluginDownloadResponse, SubtitlePluginSearchRequest, SubtitlePluginSearchResponse,
    SubtitlePluginValidateConfigRequest, SubtitlePluginValidateConfigResponse,
    SubtitleProviderMode, SubtitleQueryMediaKind, SubtitleValidateConfigStatus, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const DEFAULT_API_BASE: &str = "https://api.opensubtitles.com/api/v1";
const TOKEN_LIFETIME_SECONDS: u64 = 11 * 60 * 60;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 10;
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 10;
const MIN_API_REQUEST_INTERVAL_MILLIS: u64 = 1_500;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const FEATURE_LOOKUP_TITLE_VARIANTS: &[(&str, &[&str])] = &[
    ("superman and lois", &["Superman & Lois"]),
    ("law and order", &["Law & Order"]),
    (
        "marvels agents of shield",
        &["Marvel's Agents of S.H.I.E.L.D."],
    ),
];

static RUNTIME_STATE: OnceLock<Mutex<RuntimeState>> = OnceLock::new();

#[derive(Default)]
struct RuntimeState {
    token: Option<String>,
    token_expires_at: u64,
    auth_fingerprint: Option<String>,
    api_base: String,
    rate_limited_until: Option<u64>,
    last_api_request_at: Option<Instant>,
}

#[derive(Clone)]
struct OpenSubtitlesConfig {
    api_key: String,
    username: String,
    password: String,
    enable_hash_lookup: bool,
}

#[derive(Serialize)]
struct LoginRequestBody<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: String,
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    attributes: SearchAttributes,
}

#[derive(Deserialize)]
struct SearchAttributes {
    language: Option<String>,
    hearing_impaired: Option<bool>,
    foreign_parts_only: Option<bool>,
    ai_translated: Option<bool>,
    machine_translated: Option<bool>,
    release: Option<String>,
    uploader: Option<SearchUploader>,
    download_count: Option<i64>,
    files: Vec<SearchFile>,
    #[serde(default)]
    moviehash_match: bool,
    feature_details: Option<FeatureDetails>,
}

#[derive(Deserialize)]
struct SearchUploader {
    name: Option<String>,
}

#[derive(Deserialize)]
struct SearchFile {
    file_id: i64,
}

#[derive(Deserialize)]
struct FeatureDetails {
    movie_name: Option<String>,
    #[allow(dead_code)]
    year: Option<i32>,
    season_number: Option<i32>,
    episode_number: Option<i32>,
}

#[derive(Deserialize)]
struct FeatureLookupResponse {
    data: Vec<FeatureLookupResult>,
}

#[derive(Deserialize)]
struct FeatureLookupResult {
    id: String,
    attributes: FeatureLookupAttributes,
}

#[derive(Deserialize)]
struct FeatureLookupAttributes {
    title: Option<String>,
    year: Option<i32>,
}

#[derive(Serialize)]
struct DownloadRequestBody {
    file_id: i64,
    sub_format: &'static str,
}

#[derive(Deserialize)]
struct DownloadResponse {
    link: String,
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let response = match OpenSubtitlesConfig::from_extism() {
        Ok(config) => {
            clear_token();
            match validate_authenticated_session(&config) {
                Ok(()) => SubtitlePluginValidateConfigResponse {
                    status: SubtitleValidateConfigStatus::Valid,
                    message: None,
                    retry_after_seconds: None,
                },
                Err(error) => validation_error_response(&error),
            }
        }
        Err(error) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error),
            retry_after_seconds: None,
        },
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = OpenSubtitlesConfig::from_extism().map_err(Error::msg)?;
    let results = search_subtitles_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        SubtitlePluginSearchResponse { results },
    ))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let config = OpenSubtitlesConfig::from_extism().map_err(Error::msg)?;
    let response = download_subtitle_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

impl OpenSubtitlesConfig {
    fn from_extism() -> Result<Self, String> {
        let api_key = config_required_string("api_key")?;
        let username = config_required_string("username")?;
        let password = config_required_string("password")?;
        Ok(Self {
            api_key,
            username,
            password,
            enable_hash_lookup: config_bool("enable_hash_lookup", true),
        })
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "opensubtitles".to_string(),
        name: "OpenSubtitles".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "opensubtitles".to_string(),
            provider_aliases: vec![],
            config_fields: vec![
                ConfigFieldDef {
                    key: "api_key".to_string(),
                    label: "OpenSubtitles API Key".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    default_value: None,
                    value_source: ConfigFieldValueSource::HostBinding,
                    host_binding: Some(PluginHostBindingId::SmgOpenSubtitlesApiKey),
                    options: vec![],
                    help_text: Some(
                        "Provided by SMG for the built-in OpenSubtitles plugin.".to_string(),
                    ),
                },
                ConfigFieldDef {
                    key: "username".to_string(),
                    label: "Username".to_string(),
                    field_type: ConfigFieldType::String,
                    required: true,
                    default_value: None,
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: vec![],
                    help_text: Some("OpenSubtitles account username.".to_string()),
                },
                ConfigFieldDef {
                    key: "password".to_string(),
                    label: "Password".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    default_value: None,
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: vec![],
                    help_text: Some("OpenSubtitles account password.".to_string()),
                },
                ConfigFieldDef {
                    key: "enable_hash_lookup".to_string(),
                    label: "Enable Hash Lookup".to_string(),
                    field_type: ConfigFieldType::Bool,
                    required: false,
                    default_value: Some("true".to_string()),
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: vec![],
                    help_text: Some(
                        "Use OpenSubtitles file-hash lookups when available.".to_string(),
                    ),
                },
            ],
            default_base_url: None,
            allowed_hosts: vec!["*.opensubtitles.com".to_string()],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![
                    SubtitleQueryMediaKind::Movie,
                    SubtitleQueryMediaKind::Episode,
                ],
                recommended_facets: vec!["movie".to_string(), "series".to_string()],
                supports_hash_lookup: true,
                supports_forced: true,
                supports_hearing_impaired: true,
                supports_ai_translated: true,
                supports_machine_translated: true,
                supported_languages: vec![],
            },
        }),
    }
}

fn search_subtitles_impl(
    config: &OpenSubtitlesConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let requested_languages: Vec<String> = request
        .languages
        .iter()
        .filter_map(|language| normalize_subtitle_language_code(language))
        .collect();
    let provider_languages: Vec<String> = requested_languages
        .iter()
        .filter_map(|language| to_opensubtitles_language(language))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let title_candidates = collect_title_candidates(request);
    let feature_id = if request.imdb_id.is_none() && request.series_imdb_id.is_none() {
        search_feature_id(config, &title_candidates, request.year)?
    } else {
        None
    };

    let mut params: Vec<(&str, String)> = Vec::new();
    let mut movie_identifier_match = false;
    let mut series_identifier_match = false;

    if config.enable_hash_lookup {
        if let Some(hash) = request.file_hash.clone() {
            params.push(("moviehash", hash));
        }
    }

    match request.media_kind {
        SubtitleQueryMediaKind::Movie => {
            if let Some(imdb) = request.imdb_id.as_deref().and_then(sanitize_imdb_id) {
                params.push(("imdb_id", imdb));
                movie_identifier_match = true;
            } else if let Some(feature_id) = feature_id.clone() {
                params.push(("id", feature_id));
                movie_identifier_match = true;
            } else {
                params.push(("query", request.title.clone()));
            }
        }
        SubtitleQueryMediaKind::Episode => {
            if let Some(season) = request.season {
                params.push(("season_number", season.to_string()));
            }
            if let Some(episode) = request.episode {
                params.push(("episode_number", episode.to_string()));
            }

            if let Some(imdb) = request
                .series_imdb_id
                .as_deref()
                .or(request.imdb_id.as_deref())
                .and_then(sanitize_imdb_id)
            {
                params.push(("parent_imdb_id", imdb));
                series_identifier_match = true;
            } else if let Some(feature_id) = feature_id.clone() {
                params.push(("parent_feature_id", feature_id));
                series_identifier_match = true;
            } else {
                return Ok(Vec::new());
            }
        }
    }

    if let Some(year) = request.year {
        params.push(("year", year.to_string()));
    }
    if !provider_languages.is_empty() {
        params.push(("languages", provider_languages.join(",")));
    }

    append_translation_filter_params(
        &mut params,
        request.include_ai_translated,
        request.include_machine_translated,
    );
    params.sort_by(|left, right| left.0.cmp(right.0));

    execute_subtitle_search(
        config,
        request,
        &requested_languages,
        &params,
        movie_identifier_match,
        series_identifier_match,
    )
}

fn execute_subtitle_search(
    config: &OpenSubtitlesConfig,
    request: &SubtitlePluginSearchRequest,
    requested_languages: &[String],
    params: &[(&str, String)],
    movie_identifier_match: bool,
    series_identifier_match: bool,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let response = send_request_json(config, "GET", "subtitles", Some(&params), None)?;
    if response.status_code() >= 400 {
        return Err(http_error(config, "search", &response));
    }

    let search_response: SearchResponse = serde_json::from_slice(&response.body())
        .map_err(|error| format!("OpenSubtitles search parse error: {error}"))?;

    let mut results = Vec::new();
    for result in search_response.data {
        let attrs = result.attributes;
        let Some(file) = attrs.files.first() else {
            continue;
        };

        let ai_translated = attrs.ai_translated.unwrap_or(false);
        let machine_translated = attrs.machine_translated.unwrap_or(false);
        if ai_translated && !request.include_ai_translated {
            continue;
        }
        if machine_translated && !request.include_machine_translated {
            continue;
        }

        let hearing_impaired = attrs.hearing_impaired.unwrap_or(false);
        let forced = is_real_forced(attrs.foreign_parts_only.unwrap_or(false), hearing_impaired);
        let language = attrs
            .language
            .as_deref()
            .and_then(from_opensubtitles_language)
            .unwrap_or_default();
        if language.is_empty() {
            continue;
        }
        if !requested_languages.is_empty()
            && !requested_languages
                .iter()
                .any(|requested| same_subtitle_language(requested, &language))
        {
            continue;
        }

        let mut match_hints = Vec::new();
        if attrs.moviehash_match {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::Hash,
                value: None,
            });
        }
        if movie_identifier_match {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::ImdbId,
                value: None,
            });
        }
        if series_identifier_match {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::SeriesImdbId,
                value: None,
            });
        }
        if let Some(details) = &attrs.feature_details {
            if title_matches_query(details.movie_name.as_deref(), request) {
                match_hints.push(SubtitleMatchHint {
                    kind: SubtitleMatchHintKind::Title,
                    value: None,
                });
            }
            if request.season.is_some()
                && request.episode.is_some()
                && details.season_number == request.season
                && details.episode_number == request.episode
            {
                match_hints.push(SubtitleMatchHint {
                    kind: SubtitleMatchHintKind::SeasonEpisode,
                    value: None,
                });
            }
        }

        results.push(SubtitlePluginCandidate {
            provider_file_id: file.file_id.to_string(),
            language,
            release_info: attrs.release,
            hearing_impaired,
            forced,
            ai_translated,
            machine_translated,
            uploader: attrs.uploader.and_then(|uploader| uploader.name),
            download_count: attrs.download_count,
            match_hints,
        });
    }

    Ok(results)
}

fn is_real_forced(foreign_parts_only: bool, hearing_impaired: bool) -> bool {
    foreign_parts_only && !hearing_impaired
}

fn append_translation_filter_params(
    params: &mut Vec<(&'static str, String)>,
    include_ai_translated: bool,
    include_machine_translated: bool,
) {
    if !include_ai_translated {
        params.push(("ai_translated", "exclude".to_string()));
    }

    // OpenSubtitles redirects `machine_translated=exclude` to the same URL without
    // the parameter; ureq strips Authorization on redirects, so omit the default
    // and keep excluding machine-translated rows client-side.
    if include_machine_translated {
        params.push(("machine_translated", "include".to_string()));
    }
}

fn download_subtitle_impl(
    config: &OpenSubtitlesConfig,
    request: &SubtitlePluginDownloadRequest,
) -> Result<SubtitlePluginDownloadResponse, String> {
    let file_id = request
        .provider_file_id
        .trim()
        .parse::<i64>()
        .map_err(|error| format!("invalid OpenSubtitles file id: {error}"))?;
    let response = send_request_json(
        config,
        "POST",
        "download",
        None,
        Some(
            serde_json::to_vec(&DownloadRequestBody {
                file_id,
                sub_format: "srt",
            })
            .map_err(|error| format!("failed to encode download request: {error}"))?,
        ),
    )?;
    if response.status_code() >= 400 {
        return Err(http_error(config, "download", &response));
    }

    let download: DownloadResponse = serde_json::from_slice(&response.body())
        .map_err(|error| format!("OpenSubtitles download parse error: {error}"))?;

    let content_request = HttpRequest::new(&download.link).with_method("GET");
    let content = http::request::<Vec<u8>>(&content_request, None)
        .map_err(|error| format!("OpenSubtitles subtitle fetch failed: {error}"))?;
    if content.status_code() >= 400 {
        return Err(http_error(config, "subtitle fetch", &content));
    }

    let content = normalize_line_endings(content.body());
    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(content),
        format: "srt".to_string(),
        filename: None,
        content_type: Some("text/plain; charset=utf-8".to_string()),
    })
}

fn search_feature_id(
    config: &OpenSubtitlesConfig,
    titles: &[String],
    year: Option<i32>,
) -> Result<Option<String>, String> {
    for title in titles {
        let wanted_variants = normalized_title_match_variants(title);
        for query_title in feature_lookup_queries(title) {
            let params = vec![("query", query_title.to_ascii_lowercase())];
            let response = send_request_json(config, "GET", "features", Some(&params), None)?;
            if response.status_code() >= 400 {
                return Err(http_error(config, "feature lookup", &response));
            }

            let body: FeatureLookupResponse = serde_json::from_slice(&response.body())
                .map_err(|error| format!("OpenSubtitles feature lookup parse error: {error}"))?;

            let mut exact_year_match = None;
            let mut fallback = None;
            for result in body.data {
                let Some(candidate_title) = result.attributes.title.as_deref() else {
                    continue;
                };
                let normalized_candidate = normalize_title_for_match(candidate_title);
                if !wanted_variants.contains(&normalized_candidate) {
                    continue;
                }

                if year.is_some() && result.attributes.year == year {
                    exact_year_match = Some(result.id);
                    break;
                }
                fallback = Some(result.id);
            }

            if let Some(id) = exact_year_match.or(fallback) {
                return Ok(Some(id));
            }
        }
    }

    Ok(None)
}

fn send_request_json(
    config: &OpenSubtitlesConfig,
    method: &str,
    path: &str,
    params: Option<&[(&str, String)]>,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, String> {
    ensure_authenticated(config)?;
    let mut auth_retry_used = false;
    let mut rate_limit_retry_used = false;

    loop {
        let response = send_request_once(config, method, path, params, body.clone())?;

        if response.status_code() == 401 && !auth_retry_used {
            clear_token();
            ensure_authenticated(config)?;
            auth_retry_used = true;
            continue;
        }

        if response.status_code() == 429 {
            let retry_after = retry_after_seconds(&response)
                .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
                .max(1);
            record_rate_limit(retry_after);
            if rate_limit_retry_used || retry_after > MAX_RATE_LIMIT_WAIT_SECONDS {
                return Ok(response);
            }
            rate_limit_retry_used = true;
            continue;
        }

        clear_rate_limit();
        return Ok(response);
    }
}

fn send_request_once(
    config: &OpenSubtitlesConfig,
    method: &str,
    path: &str,
    params: Option<&[(&str, String)]>,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, String> {
    let mut url = current_api_base();
    url.push('/');
    url.push_str(path.trim_start_matches('/'));
    if let Some(params) = params {
        let query = encode_query(params);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
    }

    let mut request = HttpRequest::new(&url)
        .with_method(method)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json")
        .with_header("Api-Key", &config.api_key);
    if let Some(token) = current_token(config) {
        request = request.with_header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.with_header("Content-Type", "application/json");
    }

    send_open_subtitles_request(&request, body)
}

fn ensure_authenticated(config: &OpenSubtitlesConfig) -> Result<(), String> {
    if current_token(config).is_some() {
        return Ok(());
    }

    let auth_fingerprint = config_auth_fingerprint(config);
    let login_request = HttpRequest::new(format!("{DEFAULT_API_BASE}/login"))
        .with_method("POST")
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "application/json")
        .with_header("Api-Key", &config.api_key)
        .with_header("Content-Type", "application/json");
    let body = serde_json::to_vec(&LoginRequestBody {
        username: &config.username,
        password: &config.password,
    })
    .map_err(|error| format!("failed to encode login request: {error}"))?;
    let response = send_open_subtitles_request(&login_request, Some(body))?;

    if response.status_code() >= 400 {
        return Err(http_error(config, "login", &response));
    }

    let login: LoginResponse = serde_json::from_slice(&response.body())
        .map_err(|error| format!("OpenSubtitles login parse error: {error}"))?;
    let mut state = runtime_state()
        .lock()
        .map_err(|_| "OpenSubtitles runtime state is poisoned".to_string())?;
    state.token = Some(login.token);
    state.token_expires_at = now_epoch_seconds() + TOKEN_LIFETIME_SECONDS;
    state.auth_fingerprint = Some(auth_fingerprint);
    state.api_base = login
        .base_url
        .as_deref()
        .and_then(normalize_api_base)
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    Ok(())
}

fn validate_authenticated_session(config: &OpenSubtitlesConfig) -> Result<(), String> {
    ensure_authenticated(config)?;
    let response = send_request_json(config, "GET", "infos/user", None, None)?;
    if response.status_code() >= 400 {
        return Err(http_error(config, "user info", &response));
    }
    Ok(())
}

fn runtime_state() -> &'static Mutex<RuntimeState> {
    RUNTIME_STATE.get_or_init(|| {
        Mutex::new(RuntimeState {
            token: None,
            token_expires_at: 0,
            auth_fingerprint: None,
            api_base: DEFAULT_API_BASE.to_string(),
            rate_limited_until: None,
            last_api_request_at: None,
        })
    })
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn current_api_base() -> String {
    runtime_state()
        .lock()
        .ok()
        .map(|state| {
            if state.api_base.is_empty() {
                DEFAULT_API_BASE.to_string()
            } else {
                state.api_base.clone()
            }
        })
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

fn current_token(config: &OpenSubtitlesConfig) -> Option<String> {
    let auth_fingerprint = config_auth_fingerprint(config);
    runtime_state().lock().ok().and_then(|state| {
        let now = now_epoch_seconds();
        if state.token_expires_at > now + 60
            && state.auth_fingerprint.as_deref() == Some(auth_fingerprint.as_str())
        {
            state.token.clone()
        } else {
            None
        }
    })
}

fn clear_token() {
    if let Ok(mut state) = runtime_state().lock() {
        state.token = None;
        state.token_expires_at = 0;
        state.auth_fingerprint = None;
    }
}

fn config_auth_fingerprint(config: &OpenSubtitlesConfig) -> String {
    let mut hasher = DefaultHasher::new();
    config.api_key.hash(&mut hasher);
    config.username.hash(&mut hasher);
    config.password.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn record_rate_limit(seconds: u64) {
    if let Ok(mut state) = runtime_state().lock() {
        let deadline = now_epoch_seconds() + seconds.max(1);
        state.rate_limited_until = Some(state.rate_limited_until.unwrap_or_default().max(deadline));
    }
}

fn clear_rate_limit() {
    if let Ok(mut state) = runtime_state().lock() {
        state.rate_limited_until = None;
    }
}

enum ApiRequestGate {
    Ready,
    Wait(Duration),
    RateLimited(u64),
}

fn send_open_subtitles_request(
    request: &HttpRequest,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse, String> {
    wait_for_api_request_slot()?;
    http::request::<Vec<u8>>(request, body)
        .map_err(|error| format!("OpenSubtitles request failed: {error}"))
}

fn wait_for_api_request_slot() -> Result<(), String> {
    loop {
        match reserve_api_request_slot()? {
            ApiRequestGate::Ready => return Ok(()),
            ApiRequestGate::Wait(duration) => wait_duration(duration),
            ApiRequestGate::RateLimited(seconds) => return Err(rate_limit_message(seconds)),
        }
    }
}

fn reserve_api_request_slot() -> Result<ApiRequestGate, String> {
    let mut state = runtime_state()
        .lock()
        .map_err(|_| "OpenSubtitles runtime state is poisoned".to_string())?;

    let now_epoch = now_epoch_seconds();
    if let Some(deadline) = state.rate_limited_until {
        let remaining = deadline.saturating_sub(now_epoch);
        if remaining > MAX_RATE_LIMIT_WAIT_SECONDS {
            return Ok(ApiRequestGate::RateLimited(remaining));
        }
        if remaining > 0 {
            return Ok(ApiRequestGate::Wait(Duration::from_secs(remaining)));
        }
        state.rate_limited_until = None;
    }

    let min_interval = Duration::from_millis(MIN_API_REQUEST_INTERVAL_MILLIS);
    if let Some(last_request) = state.last_api_request_at {
        let elapsed = last_request.elapsed();
        if elapsed < min_interval {
            return Ok(ApiRequestGate::Wait(min_interval - elapsed));
        }
    }

    state.last_api_request_at = Some(Instant::now());
    Ok(ApiRequestGate::Ready)
}

fn wait_duration(duration: Duration) {
    if !duration.is_zero() {
        std::thread::sleep(duration);
    }
}

fn rate_limit_message(retry_after: u64) -> String {
    format!("OpenSubtitles rate limited — retry after {retry_after}s")
}

fn retry_after_seconds(response: &HttpResponse) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-retry-after"))
        .and_then(|value| value.parse::<u64>().ok())
}

fn validation_error_response(error: &str) -> SubtitlePluginValidateConfigResponse {
    if error.contains("authentication failed") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::AuthFailed,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else if error.contains("rate limited") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::RateLimited,
            message: Some(error.to_string()),
            retry_after_seconds: runtime_state()
                .lock()
                .ok()
                .and_then(|state| state.rate_limited_until)
                .map(|deadline| deadline.saturating_sub(now_epoch_seconds()) as i64),
        }
    } else if error.contains("required") || error.contains("missing") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else if error.contains("request failed") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Unreachable,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Unsupported,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    }
}

fn http_error(config: &OpenSubtitlesConfig, action: &str, response: &HttpResponse) -> String {
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    match status {
        401 => format!(
            "OpenSubtitles {action} authentication failed on {} (bearer={}): {}",
            current_api_host(),
            if current_token(config).is_some() {
                "present"
            } else {
                "missing"
            },
            compact_error_body(&body)
        ),
        406 => format!(
            "OpenSubtitles daily quota reached during {action}: {}",
            compact_error_body(&body)
        ),
        410 => format!("OpenSubtitles {action} link expired"),
        429 => {
            let retry_after = retry_after_seconds(response).unwrap_or(DEFAULT_RETRY_AFTER_SECONDS);
            format!("OpenSubtitles rate limited — retry after {retry_after}s")
        }
        500..=599 => format!(
            "OpenSubtitles {action} failed with HTTP {status}: {}",
            compact_error_body(&body)
        ),
        _ => format!(
            "OpenSubtitles {action} returned HTTP {status}: {}",
            compact_error_body(&body)
        ),
    }
}

fn current_api_host() -> String {
    current_api_base()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn normalize_api_base(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let normalized = if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    Some(if normalized.ends_with("/api/v1") {
        normalized
    } else {
        format!("{normalized}/api/v1")
    })
}

fn sanitize_imdb_id(imdb_id: &str) -> Option<String> {
    let trimmed = imdb_id
        .trim()
        .trim_start_matches("tt")
        .trim_start_matches('0');
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(trimmed.to_string())
}

fn collect_title_candidates(request: &SubtitlePluginSearchRequest) -> Vec<String> {
    let mut candidates =
        Vec::with_capacity(request.title_candidates.len() + request.title_aliases.len() + 1);
    let mut seen = HashSet::new();

    for candidate in request
        .title_candidates
        .iter()
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
    {
        let normalized = normalize_title_for_match(candidate);
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        candidates.push(candidate.trim().to_string());
    }

    candidates
}

fn normalize_title_for_match(title: &str) -> String {
    let normalized = title
        .chars()
        .fold(String::with_capacity(title.len()), |mut acc, ch| {
            if ch.is_alphanumeric() {
                acc.push(ch.to_ascii_lowercase());
            } else if ch == '&' {
                acc.push_str(" and ");
            } else if ch.is_whitespace() || matches!(ch, '.' | '-' | '_') {
                acc.push(' ');
            }
            acc
        });
    collapse_title_initialisms(normalized.split_whitespace().collect::<Vec<_>>()).join(" ")
}

fn collapse_title_initialisms(tokens: Vec<&str>) -> Vec<String> {
    let mut collapsed = Vec::with_capacity(tokens.len());
    let mut idx = 0;

    while idx < tokens.len() {
        if tokens[idx].len() == 1 && tokens[idx].chars().all(|ch| ch.is_ascii_alphabetic()) {
            let start = idx;
            while idx < tokens.len()
                && tokens[idx].len() == 1
                && tokens[idx].chars().all(|ch| ch.is_ascii_alphabetic())
            {
                idx += 1;
            }

            if idx - start > 1 {
                collapsed.push(tokens[start..idx].concat());
                continue;
            }
        }

        collapsed.push(tokens[idx].to_string());
        idx += 1;
    }

    collapsed
}

fn feature_lookup_queries(title: &str) -> Vec<String> {
    let trimmed = title.trim();
    let mut queries = Vec::new();
    let mut seen = HashSet::new();

    let mut push_query = |candidate: String| {
        let trimmed_candidate = candidate.trim();
        let normalized = normalize_title_for_match(trimmed_candidate);
        let dedupe_key = trimmed_candidate.to_ascii_lowercase();
        if !trimmed_candidate.is_empty() && !normalized.is_empty() && seen.insert(dedupe_key) {
            queries.push(trimmed_candidate.to_string());
        }
    };

    push_query(trimmed.to_string());
    if trimmed.contains('&') {
        push_query(trimmed.replace('&', "and"));
    }
    if trimmed.to_ascii_lowercase().contains(" and ") {
        push_query(trimmed.replace(" and ", " & "));
        push_query(trimmed.replace(" And ", " & "));
    }

    let normalized = normalize_title_for_match(trimmed);
    if !normalized.is_empty() {
        push_query(normalized.clone());
    }

    for (canonical, variants) in FEATURE_LOOKUP_TITLE_VARIANTS {
        if normalized == *canonical {
            for variant in *variants {
                push_query((*variant).to_string());
            }
        }
    }

    queries
}

fn normalized_title_match_variants(title: &str) -> HashSet<String> {
    feature_lookup_queries(title)
        .into_iter()
        .map(|candidate| normalize_title_for_match(&candidate))
        .filter(|candidate| !candidate.is_empty())
        .collect()
}

fn title_matches_query(candidate: Option<&str>, query: &SubtitlePluginSearchRequest) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let candidate = normalize_title_for_match(candidate);
    collect_title_candidates(query)
        .into_iter()
        .any(|title| normalize_title_for_match(&title) == candidate)
}

fn normalize_subtitle_language_code(code: &str) -> Option<String> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return None;
    }

    let upper = trimmed.replace('_', "-").to_ascii_uppercase();
    let normalized = match upper.as_str() {
        "ALB" | "SQ" | "SQI" => "sqi",
        "ARA" | "AR" => "ara",
        "ARM" | "HY" | "HYE" => "hye",
        "BAQ" | "EU" | "EUS" => "eus",
        "BEN" | "BN" => "ben",
        "BOS" | "BS" => "bos",
        "BUL" | "BG" | "BGAUDIO" | "BG-AUDIO" => "bul",
        "BUR" | "MY" | "MYA" => "mya",
        "CAT" | "CA" => "cat",
        "CHI" | "ZH" | "ZHO" | "ZH-CN" | "CHS" | "SC" | "ZHS" | "HANS" | "GB" => "zho",
        "CHT" | "TC" | "ZHT" | "HANT" | "BIG5" | "ZH-TW" => "zht",
        "CES" | "CS" | "CZE" => "ces",
        "DAN" | "DA" | "DK" => "dan",
        "DE" | "DEU" | "GER" | "GERMAN" => "deu",
        "DUT" | "NL" | "NLD" => "nld",
        "EA" | "ES-MX" => "ea",
        "EL" | "ELL" | "GRE" => "ell",
        "EN" | "ENG" | "EN-GB" | "EN-US" => "eng",
        "ES" | "SPA" | "ESP" => "spa",
        "EST" | "ET" => "est",
        "FA" | "FAS" | "PER" => "fas",
        "FI" | "FIN" => "fin",
        "FRA" | "FR" | "FRE" | "VF" | "VF2" | "VFF" | "VFQ" => "fra",
        "GEO" | "KA" | "KAT" => "kat",
        "HE" | "HEB" | "IW" => "heb",
        "HI" | "HIN" => "hin",
        "HR" | "HRV" => "hrv",
        "HU" | "HUN" => "hun",
        "ICE" | "IS" | "ISL" => "isl",
        "ID" | "IND" => "ind",
        "IT" | "ITA" => "ita",
        "JA" | "JPN" | "JP" => "jpn",
        "KO" | "KOR" | "KORSUB" | "KORSUBS" => "kor",
        "LAV" | "LV" => "lav",
        "LIT" | "LT" => "lit",
        "MAC" | "MK" | "MKD" => "mkd",
        "MAY" | "MS" | "MSA" => "msa",
        "NOR" | "NB" | "NN" | "NO" => "nor",
        "PL" | "POL" => "pol",
        "POB" | "PB" | "PT-BR" => "pob",
        "POR" | "PT" | "PT-PT" => "por",
        "RO" | "RON" | "RUM" | "RODUBBED" => "ron",
        "RU" | "RUS" => "rus",
        "SCC" | "SR" | "SRP" => "srp",
        "SIN" | "SI" => "sin",
        "SK" | "SLK" | "SLO" => "slk",
        "SLV" | "SL" => "slv",
        "SV" | "SWE" => "swe",
        "TH" | "THA" => "tha",
        "TR" | "TUR" => "tur",
        "UK" | "UKR" => "ukr",
        "UR" | "URD" => "urd",
        "VI" | "VIE" => "vie",
        _ if upper.len() == 3 && upper.chars().all(|ch| ch.is_ascii_alphanumeric()) => {
            return Some(upper.to_ascii_lowercase());
        }
        _ => return None,
    };

    Some(normalized.to_string())
}

fn same_subtitle_language(left: &str, right: &str) -> bool {
    match (
        normalize_subtitle_language_code(left),
        normalize_subtitle_language_code(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn to_opensubtitles_language(code: &str) -> Option<String> {
    let normalized = normalize_subtitle_language_code(code)?;
    let provider_code = match normalized.as_str() {
        "sqi" => "sq",
        "ara" => "ar",
        "hye" => "hy",
        "eus" => "eu",
        "ben" => "bn",
        "bos" => "bs",
        "bul" => "bg",
        "mya" => "my",
        "cat" => "ca",
        "zho" => "zh-cn",
        "zht" => "zh-tw",
        "ces" => "cs",
        "dan" => "da",
        "deu" => "de",
        "nld" => "nl",
        "ea" => "es",
        "ell" => "el",
        "eng" => "en",
        "spa" => "es",
        "est" => "et",
        "fas" => "fa",
        "fin" => "fi",
        "fra" => "fr",
        "kat" => "ka",
        "heb" => "he",
        "hin" => "hi",
        "hrv" => "hr",
        "hun" => "hu",
        "isl" => "is",
        "ind" => "id",
        "ita" => "it",
        "jpn" => "ja",
        "kor" => "ko",
        "lav" => "lv",
        "lit" => "lt",
        "mkd" => "mk",
        "msa" => "ms",
        "nor" => "no",
        "pob" => "pt-br",
        "por" => "pt-pt",
        "pol" => "pl",
        "ron" => "ro",
        "rus" => "ru",
        "srp" => "sr",
        "sin" => "si",
        "slk" => "sk",
        "slv" => "sl",
        "swe" => "sv",
        "tha" => "th",
        "tur" => "tr",
        "ukr" => "uk",
        "urd" => "ur",
        "vie" => "vi",
        other => other,
    };
    Some(provider_code.to_string())
}

fn from_opensubtitles_language(code: &str) -> Option<String> {
    match code.trim().replace('_', "-").as_str() {
        "pt-PT" => Some("por".to_string()),
        "zh-CN" => Some("zho".to_string()),
        "zh-TW" => Some("zht".to_string()),
        "es-MX" => Some("ea".to_string()),
        other => normalize_subtitle_language_code(other),
    }
}

fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn normalize_line_endings(body: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&body).replace("\r\n", "\n");
    text.into_bytes()
}

fn compact_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response".to_string()
    } else if trimmed.len() > 240 {
        format!("{}...", &trimmed[..240])
    } else {
        trimmed.to_string()
    }
}

fn config_required_string(key: &str) -> Result<String, String> {
    let value = config::get(key)
        .map_err(|error| format!("failed to read config field '{key}': {error}"))?
        .unwrap_or_default()
        .trim()
        .to_string();
    if value.is_empty() {
        Err(format!("missing required config field '{key}'"))
    } else {
        Ok(value)
    }
}

fn config_bool(key: &str, default: bool) -> bool {
    match config::get(key) {
        Ok(Some(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Ok(None) => default,
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_translation_filter_params, config_auth_fingerprint, is_real_forced,
        to_opensubtitles_language, OpenSubtitlesConfig,
    };

    #[test]
    fn auth_fingerprint_changes_when_credentials_change() {
        let base = OpenSubtitlesConfig {
            api_key: "api-key".to_string(),
            username: "user".to_string(),
            password: "password-one".to_string(),
            enable_hash_lookup: true,
        };
        let changed = OpenSubtitlesConfig {
            password: "password-two".to_string(),
            ..base.clone()
        };

        assert_ne!(
            config_auth_fingerprint(&base),
            config_auth_fingerprint(&changed)
        );
    }

    #[test]
    fn maps_internal_language_codes_to_opensubtitles_codes() {
        assert_eq!(to_opensubtitles_language("eng").as_deref(), Some("en"));
        assert_eq!(to_opensubtitles_language("deu").as_deref(), Some("de"));
        assert_eq!(to_opensubtitles_language("pob").as_deref(), Some("pt-br"));
        assert_eq!(to_opensubtitles_language("zho").as_deref(), Some("zh-cn"));
    }

    #[test]
    fn omits_default_machine_translation_exclude_filter() {
        let mut params = Vec::new();
        append_translation_filter_params(&mut params, false, false);

        assert!(params.contains(&("ai_translated", "exclude".to_string())));
        assert!(!params.iter().any(|(key, _)| *key == "machine_translated"));
    }

    #[test]
    fn forced_subtitle_excludes_hearing_impaired_rows() {
        assert!(is_real_forced(true, false));
        assert!(!is_real_forced(true, true));
        assert!(!is_real_forced(false, false));
    }
}
