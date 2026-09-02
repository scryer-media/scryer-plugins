//! AnimeTosho.xyz subtitles, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:subtitle/subtitle-provider@1.0.0`: two
//! exports carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`,
//! `process` exchanges a `PluginCommandRequest` for a
//! `PluginCommandResponse`), plus the shared `scryer:host/services@1.0.0`
//! import every non-archive family world uses for config, plugin state, and
//! HTTP.
//!
//! ## What the migration changed
//!
//! The previous artifact was a plain `cdylib` with four exported entry
//! points (`scryer_describe`, `scryer_validate_config`,
//! `scryer_subtitle_search`, `scryer_subtitle_download`) whose host services
//! arrived through the core-module `scryer:host/v1` pointer ABI. A component
//! has no exported linear memory for a host to slice, so both halves move onto
//! the canonical ABI: the four entry points collapse into one `process` export
//! dispatching the SDK's [`PluginSubtitleCommand`], and host services cross as
//! postcard `list<u8>` values through [`scryer_plugin_pdk::host`].
//!
//! Provider behaviour is unchanged — the same JSON API and release-page
//! scraping, the same redirect and 429 backoff policy, the same candidates and
//! match hints. What used to be a hard ABI failure is now the
//! SDK's typed [`PluginResult::Err`]; the meaning is identical, the channel is
//! not.
//!
//! ## No host archive extraction here
//!
//! AnimeTosho serves XZ-compressed attachments, but this provider has never
//! decompressed them: it verifies the XZ magic and hands the container to
//! Scryer as `application/x-xz`. There is therefore nothing to route through
//! [`scryer_plugin_pdk::host::archive_extract`], and the plugin does not enable
//! the PDK's `archive-extract` feature.

use std::collections::HashSet;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_pdk::sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_pdk::{HttpRequest, HttpResponse, config, http};
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

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:subtitle/subtitle-provider@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/subtitle-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_subtitle_component_main!(
    descriptor = descriptor,
    handler = handle_subtitle_command,
);

const DEFAULT_BASE_URL: &str = "https://feed.animetosho.xyz";
const DEFAULT_SITE_URL: &str = "https://animetosho.xyz";
const STORAGE_BASE_URL: &str = "https://storage.animetosho.xyz";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const DEFAULT_SEARCH_THRESHOLD: usize = 6;
const MAX_SEARCH_THRESHOLD: usize = 15;
const MAX_REDIRECTS: usize = 4;
const RATE_LIMIT_BACKOFF_SECONDS: &[u64] = &[2, 5, 10];
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 10;
const XZ_MAGIC: &[u8] = b"\xFD\x37\x7A\x58\x5A\x00";

#[derive(Clone, Debug)]
struct AnimeToshoConfig {
    base_url: String,
    site_url: String,
    api_key: String,
    search_threshold: usize,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct EpisodeResponse {
    #[serde(default)]
    releases: Vec<ReleaseSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseSummary {
    id: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    is_multisub_release: bool,
}

#[derive(Debug, Clone)]
struct SubtitleLink {
    url: String,
    subtitle_id: String,
    label: String,
    language: String,
    format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnimeToshoDownloadRef {
    url: String,
    filename: String,
    language: String,
    format: String,
}

/// One `process` invocation, dispatched by operation.
///
/// This is the whole of the world's request surface: `describe` is owned by
/// the PDK entry macro, and every operational failure is reported in-band
/// through [`PluginResult`], never as a world-level `invocation-error`.
fn handle_subtitle_command(command: PluginSubtitleCommand) -> PluginSubtitleCommandResult {
    match command {
        PluginSubtitleCommand::ValidateConfig(request) => {
            PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(validate_config(&request)))
        }
        PluginSubtitleCommand::Search(request) => {
            PluginSubtitleCommandResult::Search(search(&request))
        }
        PluginSubtitleCommand::Download(request) => {
            PluginSubtitleCommandResult::Download(download(&request))
        }
        // AnimeTosho is a catalog provider: it serves subtitles that already
        // exist upstream and has no generator. The host reads
        // `SubtitleCapabilities::mode` and never routes a generate here, so
        // this arm answers in-band rather than trapping.
        PluginSubtitleCommand::Generate(_) => {
            PluginSubtitleCommandResult::Generate(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "AnimeTosho.xyz is a catalog subtitle provider and cannot \
                                 generate subtitles"
                    .to_string(),
                debug_message: Some(
                    "SubtitleProviderMode::Catalog advertises no generate capability".to_string(),
                ),
                retry_after_seconds: None,
                details: None,
            }))
        }
        // Alignment moved into this envelope when the subtitle-sync plugin
        // migrated off its own transport, so every subtitle provider now sees
        // the operation whether or not it can serve one. AnimeTosho.xyz cannot: it has
        // no audio decoder and advertises no `sync` capability. Same in-band
        // refusal as `Generate`, for the same reason.
        PluginSubtitleCommand::Sync(_) => {
            PluginSubtitleCommandResult::Sync(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "AnimeTosho.xyz cannot align subtitles".to_string(),
                debug_message: Some(
                    "SubtitleCapabilities::sync is None for this provider".to_string(),
                ),
                retry_after_seconds: None,
                details: None,
            }))
        }
    }
}

fn validate_config(
    _request: &SubtitlePluginValidateConfigRequest,
) -> SubtitlePluginValidateConfigResponse {
    match AnimeToshoConfig::from_host() {
        Ok(config) => {
            match get_json::<Vec<ReleaseSummary>>(&config, "/json/v1/search?q=naruto&limit=1") {
                Ok(_) => SubtitlePluginValidateConfigResponse {
                    status: SubtitleValidateConfigStatus::Valid,
                    message: None,
                    retry_after_seconds: None,
                },
                Err(error) => SubtitlePluginValidateConfigResponse {
                    status: SubtitleValidateConfigStatus::InvalidConfig,
                    message: Some(error),
                    retry_after_seconds: None,
                },
            }
        }
        Err(error) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error),
            retry_after_seconds: None,
        },
    }
}

/// Search failures were a hard ABI failure before the
/// migration; they are the SDK's typed [`PluginResult::Err`] now. Same
/// meaning, typed channel — the host keeps this provider's own diagnosis
/// instead of a generic ABI fault.
fn search(request: &SubtitlePluginSearchRequest) -> PluginResult<SubtitlePluginSearchResponse> {
    let config = match AnimeToshoConfig::from_host() {
        Ok(config) => config,
        Err(error) => return PluginResult::Err(plugin_error(error)),
    };
    match search_subtitles_impl(&config, request) {
        Ok(results) => PluginResult::Ok(SubtitlePluginSearchResponse { results }),
        Err(error) => PluginResult::Err(plugin_error(error)),
    }
}

fn download(
    request: &SubtitlePluginDownloadRequest,
) -> PluginResult<SubtitlePluginDownloadResponse> {
    let config = match AnimeToshoConfig::from_host() {
        Ok(config) => config,
        Err(error) => return PluginResult::Err(plugin_error(error)),
    };
    let reference: AnimeToshoDownloadRef = match serde_json::from_str(&request.provider_file_id) {
        Ok(reference) => reference,
        Err(error) => {
            return PluginResult::Err(plugin_error(format!(
                "AnimeTosho subtitle reference is not valid: {error}"
            )));
        }
    };
    match download_subtitle_impl(&config, &reference) {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(plugin_error(error)),
    }
}

/// Map this provider's string failures onto the SDK's typed error.
///
/// The rate-limit branch reads the message the provider already produced, so
/// the retry hint the upstream sent survives the migration rather than being
/// flattened into a generic temporary failure.
fn plugin_error(error: String) -> PluginError {
    let (code, retry_after_seconds) = if error.contains("rate limited") {
        (
            PluginErrorCode::RateLimited,
            retry_after_from_message(&error),
        )
    } else {
        (PluginErrorCode::Temporary, None)
    };
    PluginError {
        code,
        public_message: error,
        debug_message: None,
        retry_after_seconds,
        details: None,
    }
}

fn retry_after_from_message(error: &str) -> Option<i64> {
    let (_, tail) = error.split_once("retry after ")?;
    tail.trim_end_matches('s').trim().parse::<i64>().ok()
}

impl AnimeToshoConfig {
    fn from_host() -> Result<Self, String> {
        Ok(Self {
            base_url: config_string("base_url")?.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            site_url: config_string("site_url")?.unwrap_or_else(|| DEFAULT_SITE_URL.to_string()),
            api_key: config_string("api_key")?
                .ok_or_else(|| "api_key is not configured".to_string())?,
            search_threshold: config_string("search_threshold")?
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(DEFAULT_SEARCH_THRESHOLD)
                .clamp(1, MAX_SEARCH_THRESHOLD),
        })
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "animetosho-xyz-subtitles".to_string(),
        name: "AnimeTosho.xyz Subtitles".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "animetosho-xyz".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: Some(DEFAULT_BASE_URL.to_string()),
            allowed_hosts: vec![
                "feed.animetosho.xyz".to_string(),
                "animetosho.xyz".to_string(),
                "storage.animetosho.xyz".to_string(),
            ],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![SubtitleQueryMediaKind::Episode],
                recommended_facets: vec!["anime".to_string()],
                supports_hash_lookup: false,
                supports_forced: false,
                supports_hearing_impaired: false,
                supports_ai_translated: false,
                supports_machine_translated: false,
                supported_languages: vec![
                    "ara".to_string(),
                    "deu".to_string(),
                    "eng".to_string(),
                    "fra".to_string(),
                    "ita".to_string(),
                    "jpn".to_string(),
                    "pob".to_string(),
                    "por".to_string(),
                    "rus".to_string(),
                    "spa".to_string(),
                    "zho".to_string(),
                ],
                sync: None,
            },
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "base_url".to_string(),
            label: "API Base URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: Some(DEFAULT_BASE_URL.to_string()),
            value_source: ConfigFieldValueSource::User,
            role: Some(ConfigFieldRole::ConnectionUrl),
            host_binding: None,
            options: vec![],
            help_text: Some("AnimeTosho.xyz JSON API base URL".to_string()),
        },
        ConfigFieldDef {
            key: "site_url".to_string(),
            label: "Site URL".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: Some(DEFAULT_SITE_URL.to_string()),
            value_source: ConfigFieldValueSource::User,
            role: Some(ConfigFieldRole::ConnectionUrl),
            host_binding: None,
            options: vec![],
            help_text: Some("AnimeTosho.xyz web site URL used for subtitle links".to_string()),
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
            key: "search_threshold".to_string(),
            label: "Search Threshold".to_string(),
            field_type: ConfigFieldType::Number,
            required: false,
            default_value: Some(DEFAULT_SEARCH_THRESHOLD.to_string()),
            value_source: ConfigFieldValueSource::User,
            role: None,
            host_binding: None,
            options: vec![],
            help_text: Some("Maximum AnimeTosho releases to inspect, from 1 to 15".to_string()),
        },
    ]
}

fn search_subtitles_impl(
    config: &AnimeToshoConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    if request.media_kind != SubtitleQueryMediaKind::Episode
        || request.facet.as_deref() != Some("anime")
    {
        return Ok(Vec::new());
    }

    let releases = if let Some(anidb_episode_id) = external_id(request, "anidb_episode") {
        episode_releases(config, anidb_episode_id)?
    } else {
        search_releases(config, request)?
    };

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for release in releases.into_iter().take(config.search_threshold) {
        for link in release_subtitle_links(config, release.id)? {
            if !requested_language_matches(&request.languages, &link.language) {
                continue;
            }
            let key = format!("{}:{}", release.id, link.subtitle_id);
            if !seen.insert(key) {
                continue;
            }
            results.push(candidate_for_link(request, &release, link)?);
        }
    }

    Ok(results)
}

fn episode_releases(
    config: &AnimeToshoConfig,
    anidb_episode_id: &str,
) -> Result<Vec<ReleaseSummary>, String> {
    let path = format!("/json/v1/episodes/{}", url_encode(anidb_episode_id));
    let episode: EpisodeResponse = get_json(config, &path)?;
    Ok(episode.releases)
}

fn search_releases(
    config: &AnimeToshoConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<ReleaseSummary>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for query in search_queries(request).into_iter().take(6) {
        let path = format!(
            "/json/v1/search?q={}&limit={}",
            url_encode(&query),
            config.search_threshold
        );
        let releases: Vec<ReleaseSummary> = get_json(config, &path)?;
        for release in releases {
            if release.is_multisub_release && seen.insert(release.id) {
                out.push(release);
            }
        }
        if out.len() >= config.search_threshold {
            break;
        }
    }
    Ok(out)
}

fn release_subtitle_links(
    config: &AnimeToshoConfig,
    release_id: i64,
) -> Result<Vec<SubtitleLink>, String> {
    let url = format!(
        "{}/view/{release_id}",
        config.site_url.trim_end_matches('/')
    );
    let response = http_get_follow(&url, "text/html, */*")?;
    if response.status_code() >= 400 {
        return Err(http_error("AnimeTosho page", &response));
    }
    let html = String::from_utf8_lossy(&response.body()).to_string();
    Ok(parse_subtitle_links(
        &html,
        release_id,
        config.site_url.trim_end_matches('/'),
    ))
}

fn candidate_for_link(
    request: &SubtitlePluginSearchRequest,
    release: &ReleaseSummary,
    link: SubtitleLink,
) -> Result<SubtitlePluginCandidate, String> {
    let filename = subtitle_filename(release.id, &link);
    let provider_file_id = serde_json::to_string(&AnimeToshoDownloadRef {
        url: link.url.clone(),
        filename,
        language: link.language.clone(),
        format: link.format.clone(),
    })
    .map_err(|error| format!("failed to encode AnimeTosho download ref: {error}"))?;

    Ok(SubtitlePluginCandidate {
        provider_file_id,
        language: link.language.clone(),
        release_info: release
            .title
            .clone()
            .map(|title| format!("{title} - {}", link.label))
            .or(Some(link.label.clone())),
        hearing_impaired: false,
        forced: false,
        ai_translated: false,
        machine_translated: false,
        uploader: None,
        download_count: None,
        match_hints: vec![
            SubtitleMatchHint {
                kind: SubtitleMatchHintKind::ExternalId,
                value: external_id(request, "anidb_episode")
                    .map(|value| format!("anidb_episode:{value}")),
            },
            SubtitleMatchHint {
                kind: SubtitleMatchHintKind::AbsoluteEpisode,
                value: request.absolute_episode.map(|episode| episode.to_string()),
            },
            SubtitleMatchHint {
                kind: SubtitleMatchHintKind::SeasonEpisode,
                value: request
                    .season
                    .zip(request.episode)
                    .map(|(season, episode)| format!("S{season:02}E{episode:02}")),
            },
            SubtitleMatchHint {
                kind: SubtitleMatchHintKind::Language,
                value: Some(link.language),
            },
        ],
    })
}

fn download_subtitle_impl(
    config: &AnimeToshoConfig,
    reference: &AnimeToshoDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, String> {
    let site_prefix = format!("{}/download/", config.site_url.trim_end_matches('/'));
    let storage_prefix = format!("{STORAGE_BASE_URL}/releases/");
    if !reference.url.starts_with(&site_prefix) && !reference.url.starts_with(&storage_prefix) {
        return Err("invalid AnimeTosho subtitle download URL".to_string());
    }

    let response = http_get_follow(&reference.url, "application/octet-stream, */*")?;
    if response.status_code() >= 400 {
        return Err(http_error("AnimeTosho subtitle", &response));
    }
    let bytes = response.body();
    if !bytes.starts_with(XZ_MAGIC) {
        return Err("AnimeTosho subtitle is not an XZ stream".to_string());
    }

    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(bytes),
        format: reference.format.clone(),
        filename: Some(reference.filename.clone()),
        content_type: response
            .headers()
            .get("content-type")
            .or_else(|| response.headers().get("Content-Type"))
            .map(ToString::to_string)
            .or_else(|| Some("application/x-xz".to_string())),
    })
}

fn get_json<T: for<'de> Deserialize<'de>>(
    config: &AnimeToshoConfig,
    path: &str,
) -> Result<T, String> {
    let url = api_url(config, path);
    let response = http_get_follow(&url, "application/json")?;
    if response.status_code() >= 400 {
        return Err(http_error("AnimeTosho JSON API", &response));
    }
    let envelope: ApiEnvelope<T> = serde_json::from_slice(&response.body())
        .map_err(|error| format!("AnimeTosho JSON parse error: {error}"))?;
    Ok(envelope.data)
}

fn api_url(config: &AnimeToshoConfig, path: &str) -> String {
    let mut url = format!(
        "{}/{}",
        config.base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if url.contains('?') {
        url.push('&');
    } else {
        url.push('?');
    }
    url.push_str("apikey=");
    url.push_str(&url_encode(&config.api_key));
    url
}

fn http_get_follow(url: &str, accept: &str) -> Result<HttpResponse, String> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let response = http_get_with_rate_limit_retry(&current, accept)?;
        if !matches!(response.status_code(), 300..=399) {
            return Ok(response);
        }
        let Some(location) = response
            .headers()
            .get("location")
            .or_else(|| response.headers().get("Location"))
        else {
            return Ok(response);
        };
        current = resolve_location(&current, location)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("AnimeTosho request exceeded redirect limit".to_string())
}

fn http_get_with_rate_limit_retry(url: &str, accept: &str) -> Result<HttpResponse, String> {
    let mut next_delay = 0;
    let fallback_delay = RATE_LIMIT_BACKOFF_SECONDS.last().copied().unwrap_or(1);
    for (attempt, default_delay) in RATE_LIMIT_BACKOFF_SECONDS
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
    {
        if next_delay > 0 {
            std::thread::sleep(Duration::from_secs(next_delay));
        }

        let request = HttpRequest::new(url)
            .with_method("GET")
            .with_header("Accept", accept)
            .with_header("Accept-Language", "en-US,en;q=0.9")
            .with_header("User-Agent", USER_AGENT);
        let response = http::request::<Vec<u8>>(&request, None)
            .map_err(|error| format!("AnimeTosho request failed: {error}"))?;
        if response.status_code() != 429 {
            return Ok(response);
        }
        if attempt >= RATE_LIMIT_BACKOFF_SECONDS.len() {
            return Ok(response);
        }

        next_delay = match retry_after_seconds(&response) {
            Some(seconds) if seconds > MAX_RATE_LIMIT_WAIT_SECONDS => return Ok(response),
            Some(seconds) => seconds.max(1),
            None => default_delay.unwrap_or(fallback_delay),
        };
    }

    Err("AnimeTosho request exhausted retries".to_string())
}

fn http_error(provider: &str, response: &HttpResponse) -> String {
    if response.status_code() == 429 {
        return format!(
            "{provider} rate limited; retry after {}s",
            retry_after_seconds(response).unwrap_or(1)
        );
    }

    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    if body.is_empty() {
        format!("{provider} returned HTTP {}", response.status_code())
    } else {
        format!(
            "{provider} returned HTTP {}: {body}",
            response.status_code()
        )
    }
}

fn retry_after_seconds(response: &HttpResponse) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("Retry-After"))
        .or_else(|| response.headers().get("x-retry-after"))
        .or_else(|| response.headers().get("X-Retry-After"))
        .and_then(|value| value.parse::<u64>().ok())
}

fn resolve_location(current: &str, location: &str) -> Result<String, String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    if !location.starts_with('/') {
        return Err(format!(
            "unsupported relative redirect location: {location}"
        ));
    }
    let scheme_end = current
        .find("://")
        .ok_or_else(|| format!("invalid redirect base URL: {current}"))?
        + 3;
    let host_end = current[scheme_end..]
        .find('/')
        .map(|offset| scheme_end + offset)
        .unwrap_or(current.len());
    Ok(format!("{}{}", &current[..host_end], location))
}

fn parse_subtitle_links(html: &str, release_id: i64, site_url: &str) -> Vec<SubtitleLink> {
    let needle = format!("href=\"/download/{release_id}/subs/file/");
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = html[cursor..].find(&needle) {
        let href_start = cursor + offset + "href=\"".len();
        let Some(href_end_offset) = html[href_start..].find('"') else {
            break;
        };
        let href_end = href_start + href_end_offset;
        let href = &html[href_start..href_end];
        let Some(text_start_offset) = html[href_end..].find('>') else {
            break;
        };
        let text_start = href_end + text_start_offset + 1;
        let Some(text_end_offset) = html[text_start..].find("</a>") else {
            break;
        };
        let text_end = text_start + text_end_offset;
        let label = html_unescape(&html[text_start..text_end]);
        if let Some((language, format)) = parse_subtitle_label(&label) {
            links.push(SubtitleLink {
                url: format!("{}{}", site_url.trim_end_matches('/'), href),
                subtitle_id: href.rsplit('/').next().unwrap_or_default().to_string(),
                label,
                language,
                format,
            });
        }
        cursor = text_end + "</a>".len();
    }
    links
}

fn parse_subtitle_label(label: &str) -> Option<(String, String)> {
    let start = label.rfind('[')?;
    let end = label[start..].find(']').map(|offset| start + offset)?;
    let metadata = &label[start + 1..end];
    let mut parts = metadata.split(',').map(str::trim);
    let language = normalize_language(parts.next()?, Some(label));
    let format = parts
        .next()
        .map(|value| value.to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ass".to_string());
    Some((language, format))
}

fn subtitle_filename(release_id: i64, link: &SubtitleLink) -> String {
    let mut base = sanitize_filename(&format!(
        "animetosho-xyz-{release_id}-{}-{}.{}",
        link.subtitle_id, link.language, link.format
    ));
    if !base.ends_with(".xz") {
        base.push_str(".xz");
    }
    base
}

fn search_queries(request: &SubtitlePluginSearchRequest) -> Vec<String> {
    let mut titles = Vec::new();
    titles.push(request.title.clone());
    titles.extend(request.title_candidates.clone());
    titles.extend(request.title_aliases.clone());

    let mut out = Vec::new();
    for title in titles {
        let title = strip_query_context(&title);
        if title.is_empty() {
            continue;
        }
        if let Some(episode) = request.absolute_episode.or(request.episode) {
            out.push(format!("{title} {episode:02}"));
            if let Some(season) = request.season {
                out.push(format!("{title} S{season:02}E{episode:02}"));
            }
        }
        out.push(title);
    }
    dedupe(out)
}

fn external_id<'a>(request: &'a SubtitlePluginSearchRequest, key: &str) -> Option<&'a str> {
    request
        .external_ids
        .get(key)
        .and_then(|values| values.iter().find(|value| !value.trim().is_empty()))
        .map(String::as_str)
}

fn requested_language_matches(requested: &[String], language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|candidate| normalize_language(candidate, None) == language)
}

fn normalize_language(language: &str, label: Option<&str>) -> String {
    let language = language.trim().to_ascii_lowercase();
    let normalized = match language.as_str() {
        "ar" | "ara" | "arabic" => "ara",
        "de" | "deu" | "ger" | "german" => "deu",
        "en" | "eng" | "english" => "eng",
        "fr" | "fra" | "fre" | "french" => "fra",
        "it" | "ita" | "italian" => "ita",
        "ja" | "jpn" | "jp" | "japanese" => "jpn",
        "pt" | "por" | "portuguese" => "por",
        "ru" | "rus" | "russian" => "rus",
        "es" | "spa" | "spanish" => "spa",
        "zh" | "zho" | "chi" | "chinese" => "zho",
        other if other.len() == 3 => other,
        _ => "eng",
    };

    if normalized == "por"
        && label
            .map(|value| {
                let lower = value.to_ascii_lowercase();
                lower.contains("[br]") || lower.contains("brazil")
            })
            .unwrap_or(false)
    {
        "pob".to_string()
    } else {
        normalized.to_string()
    }
}

fn strip_query_context(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return query.trim().to_string();
    }

    let mut start = tokens.len();
    for index in (0..tokens.len()).rev() {
        if looks_like_context_token(tokens[index]) {
            start = index;
        } else if start != tokens.len() {
            break;
        }
    }

    if start == tokens.len() {
        query.trim().to_string()
    } else {
        query[..query.rfind(tokens[start]).unwrap_or(query.len())]
            .trim()
            .to_string()
    }
}

fn looks_like_context_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        return false;
    }
    let upper = trimmed.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix('S') {
        return rest.chars().all(|ch| ch.is_ascii_digit())
            || rest.split_once('E').is_some_and(|(season, episode)| {
                !season.is_empty()
                    && !episode.is_empty()
                    && season.chars().all(|ch| ch.is_ascii_digit())
                    && episode.chars().all(|ch| ch.is_ascii_digit())
            });
    }
    trimmed.chars().all(|ch| ch.is_ascii_digit())
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if !value.trim().is_empty()
            && out
                .iter()
                .all(|existing: &String| !existing.eq_ignore_ascii_case(&value))
        {
            out.push(value);
        }
    }
    out
}

fn sanitize_filename(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("animetosho-xyz-subtitle")
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn config_string(key: &str) -> Result<Option<String>, String> {
    Ok(config::get(key)
        .map_err(|error| format!("failed to read config {key}: {error}"))?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
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
    use std::collections::BTreeMap;

    #[test]
    fn descriptor_requires_api_key() {
        let descriptor = descriptor();
        let ProviderDescriptor::Subtitle(provider) = descriptor.provider else {
            panic!("expected subtitle provider");
        };
        let api_key = provider
            .config_fields
            .iter()
            .find(|field| field.key == "api_key")
            .expect("api key field");
        assert!(api_key.required);
        assert_eq!(provider.provider_type, "animetosho-xyz");
    }

    #[test]
    fn parses_release_page_subtitle_links() {
        let html = r#"
          <a href="/download/619135/subs/all">All Attachments</a> |
          <a href="/download/619135/subs/file/94001">English [eng, ASS]</a>,
          <a href="/download/619135/subs/file/94006">Portuguese[BR] [por, ASS]</a>,
          <a href="/download/619135/subs/file/94009">Spanish [spa, ASS]</a>
        "#;

        let links = parse_subtitle_links(html, 619135, DEFAULT_SITE_URL);

        assert_eq!(links.len(), 3);
        assert_eq!(links[0].subtitle_id, "94001");
        assert_eq!(links[0].language, "eng");
        assert_eq!(links[0].format, "ass");
        assert_eq!(links[1].language, "pob");
        assert_eq!(
            links[2].url,
            "https://animetosho.xyz/download/619135/subs/file/94009"
        );
    }

    #[test]
    fn requested_language_matching_normalizes_codes() {
        assert!(requested_language_matches(&["english".to_string()], "eng"));
        assert!(requested_language_matches(&["chi".to_string()], "zho"));
        assert!(requested_language_matches(&["pt".to_string()], "por"));
        assert!(!requested_language_matches(&["jpn".to_string()], "eng"));
    }

    #[test]
    fn builds_episode_search_queries() {
        let request = SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "Wistoria Wand and Sword S02E12".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: None,
            season: Some(2),
            episode: Some(12),
            absolute_episode: Some(12),
            external_ids: BTreeMap::new(),
            languages: vec![],
            release_group: None,
            source: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            hearing_impaired: None,
            include_ai_translated: false,
            include_machine_translated: false,
        };

        let queries = search_queries(&request);

        assert!(queries.contains(&"Wistoria Wand and Sword 12".to_string()));
        assert!(queries.contains(&"Wistoria Wand and Sword S02E12".to_string()));
    }
}
