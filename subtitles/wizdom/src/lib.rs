//! Wizdom Hebrew subtitles, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:subtitle/subtitle-provider@1.1.0`: two
//! exports carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`,
//! `process` exchanges a `PluginCommandRequest` for a
//! `PluginCommandResponse`, and `process` is an `async func` on this world
//! revision), plus two imports: the shared `scryer:host/services@1.0.0` door
//! every non-archive family world uses for config, plugin state, and HTTP,
//! and the family-neutral typed `scryer:runtime/host@1.0.0` surface reached
//! through `scryer_plugin_pdk::runtime`.
//!
//! ZIP members are extracted by the bounded host archive service. The provider
//! selects a usable subtitle, preferring UTF-8 over legacy Hebrew encodings.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_pdk::host::{HostCallError, archive_extract};
use scryer_plugin_pdk::sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_pdk::{HttpRequest, HttpResponse, config, http};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::host::{PluginArchiveExtractRequest, PluginArchiveExtractedFile};
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor,
    PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint, SubtitleMatchHintKind,
    SubtitlePluginCandidate, SubtitlePluginDownloadRequest, SubtitlePluginDownloadResponse,
    SubtitlePluginSearchRequest, SubtitlePluginSearchResponse,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:subtitle/subtitle-provider@1.1.0",
    // Three packages, three paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["../../pdk/scryer-plugin-pdk/wit/host-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/runtime-v1.0.0", "../../pdk/scryer-plugin-pdk/wit/subtitle-v1.1.0"],
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

const DEFAULT_BASE_URL: &str = "https://wizdom.xyz";
const TMDB_API_BASE: &str = "https://api.themoviedb.org/3";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const PROVIDER_LANGUAGE: &str = "heb";
const RETRY_AMOUNT: usize = 3;
const RETRY_TIMEOUT_SECS: u64 = 5;
const MAX_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;
const ERROR_BODY_PREVIEW_LIMIT: usize = 240;
const VALIDATION_PROBE_IMDB_ID: &str = "tt1375666";

#[derive(Clone, Debug)]
struct WizdomConfig {
    base_url: String,
    tmdb_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    InvalidConfig,
    AuthFailed,
    RateLimited,
    Unreachable,
    Unsupported,
    MissingCapability,
}

#[derive(Debug, Clone)]
struct Failure {
    kind: FailureKind,
    message: String,
    retry_after_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WizdomDownloadRef {
    subtitle_id: String,
    filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page_url: Option<String>,
}

/// One subtitle row from `api/releases/{imdb_id}`. `version` carries the
/// uploader's release name and is the only match signal the API exposes.
#[derive(Debug, Clone, Deserialize)]
struct WizdomSub {
    id: Value,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Default, Deserialize)]
struct WizdomReleases {
    #[serde(default)]
    subs: Option<Value>,
}

/// One `process` invocation, dispatched by operation.
///
/// This is the whole of the world's request surface: `describe` is owned by
/// the PDK entry macro, and every operational failure is reported in-band
/// through [`PluginResult`], never as a world-level `invocation-error`.
async fn handle_subtitle_command(command: PluginSubtitleCommand) -> PluginSubtitleCommandResult {
    match command {
        PluginSubtitleCommand::ValidateConfig(_) => {
            PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(validate_config()))
        }
        PluginSubtitleCommand::Search(request) => PluginSubtitleCommandResult::Search(
            to_plugin_result(WizdomConfig::from_host().and_then(|config| {
                search_subtitles_impl(&config, &request)
                    .map(|results| SubtitlePluginSearchResponse { results })
            })),
        ),
        PluginSubtitleCommand::Download(request) => {
            PluginSubtitleCommandResult::Download(to_plugin_result(download(&request)))
        }
        // Wizdom is a catalog provider: it serves subtitles that already exist
        // upstream and has no generator. The host reads
        // `SubtitleCapabilities::mode` and never routes a generate here, so
        // this arm answers in-band rather than trapping.
        PluginSubtitleCommand::Generate(_) => {
            PluginSubtitleCommandResult::Generate(PluginResult::Err(unsupported(
                "Wizdom is a catalog subtitle provider and cannot generate subtitles",
            )))
        }
        // Every subtitle provider sees the alignment operation whether or not
        // it can serve one. Wizdom cannot: it advertises no `sync` capability.
        PluginSubtitleCommand::Sync(_) => PluginSubtitleCommandResult::Sync(PluginResult::Err(
            unsupported("Wizdom cannot align subtitles"),
        )),
    }
}

fn unsupported(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: message.to_string(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

fn validate_config() -> SubtitlePluginValidateConfigResponse {
    match WizdomConfig::from_host()
        .and_then(|config| fetch_releases(&config, VALIDATION_PROBE_IMDB_ID, false))
    {
        Ok(_) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
        Err(failure) => validation_error_response(&failure),
    }
}

fn download(
    request: &SubtitlePluginDownloadRequest,
) -> Result<SubtitlePluginDownloadResponse, Failure> {
    let config = WizdomConfig::from_host()?;
    let reference: WizdomDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(|error| {
            Failure::new(
                FailureKind::Unsupported,
                format!("Wizdom subtitle reference is not valid: {error}"),
            )
        })?;
    download_subtitle_impl(&config, &reference)
}

/// The typed counterpart of [`validation_error_response`]: both read the same
/// [`FailureKind`], so every operation reports a problem the same way.
fn to_plugin_result<T>(result: Result<T, Failure>) -> PluginResult<T> {
    let failure = match result {
        Ok(value) => return PluginResult::Ok(value),
        Err(failure) => failure,
    };
    let code = match failure.kind {
        FailureKind::InvalidConfig => PluginErrorCode::InvalidConfig,
        FailureKind::AuthFailed => PluginErrorCode::AuthFailed,
        FailureKind::RateLimited => PluginErrorCode::RateLimited,
        FailureKind::Unreachable => PluginErrorCode::UpstreamUnavailable,
        FailureKind::Unsupported => PluginErrorCode::Temporary,
        FailureKind::MissingCapability => PluginErrorCode::Unsupported,
    };
    PluginResult::Err(PluginError {
        code,
        public_message: failure.message,
        debug_message: None,
        retry_after_seconds: failure.retry_after_seconds,
        details: None,
    })
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "wizdom".to_string(),
        name: "Wizdom".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "wizdom".to_string(),
            provider_aliases: vec![],
            config_fields: vec![
                ConfigFieldDef {
                    key: "base_url".to_string(),
                    label: "API URL".to_string(),
                    field_type: ConfigFieldType::String,
                    required: true,
                    default_value: Some(DEFAULT_BASE_URL.to_string()),
                    value_source: ConfigFieldValueSource::User,
                    role: Some(ConfigFieldRole::ConnectionUrl),
                    help_text: Some("Wizdom API URL".to_string()),
                    ..Default::default()
                },
                ConfigFieldDef {
                    key: "tmdb_api_key".to_string(),
                    label: "TMDB API Key".to_string(),
                    field_type: ConfigFieldType::Password,
                    value_source: ConfigFieldValueSource::User,
                    help_text: Some(
                        "Optional TMDB key used to resolve an IMDb ID by title when Scryer has \
                         none"
                            .to_string(),
                    ),
                    ..Default::default()
                },
            ],
            default_base_url: Some(DEFAULT_BASE_URL.to_string()),
            allowed_hosts: vec!["wizdom.xyz".to_string(), "api.themoviedb.org".to_string()],
            capabilities: SubtitleCapabilities {
                mode: SubtitleProviderMode::Catalog,
                supported_media_kinds: vec![
                    SubtitleQueryMediaKind::Movie,
                    SubtitleQueryMediaKind::Episode,
                ],
                recommended_facets: vec!["movie".to_string(), "series".to_string()],
                supports_hash_lookup: false,
                supports_forced: false,
                supports_hearing_impaired: false,
                supports_ai_translated: false,
                supports_machine_translated: false,
                supported_languages: vec![PROVIDER_LANGUAGE.to_string()],
                sync: None,
            },
        }),
    }
}

impl WizdomConfig {
    fn from_host() -> Result<Self, Failure> {
        let base_url = config_string("base_url")?
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Failure::new(
                FailureKind::InvalidConfig,
                "base_url must be an http or https URL",
            ));
        }
        Ok(Self {
            base_url,
            tmdb_api_key: config_string("tmdb_api_key")?,
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

fn search_subtitles_impl(
    config: &WizdomConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, Failure> {
    if !is_searchable(request) {
        return Ok(Vec::new());
    }

    if let Some(imdb_id) = direct_imdb_id(request) {
        return search_by_imdb(config, request, &imdb_id);
    }
    let Some(api_key) = config.tmdb_api_key.as_deref() else {
        return Ok(Vec::new());
    };
    let mut searched_ids = std::collections::HashSet::new();
    for title in titles_for_search(request) {
        let Some(imdb_id) =
            lookup_imdb_id_via_tmdb(api_key, &title, request.year, request.media_kind)?
        else {
            continue;
        };
        if searched_ids.insert(imdb_id.clone()) {
            let candidates = search_by_imdb(config, request, &imdb_id)?;
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }
    }
    Ok(Vec::new())
}

fn search_by_imdb(
    config: &WizdomConfig,
    request: &SubtitlePluginSearchRequest,
    imdb_id: &str,
) -> Result<Vec<SubtitlePluginCandidate>, Failure> {
    let releases = fetch_releases(config, imdb_id, true)?;
    let subs = collect_subs(
        releases.subs.as_ref(),
        request.media_kind,
        request.season,
        request.episode,
    );
    Ok(subs
        .iter()
        .filter_map(|sub| sub_to_candidate(config, request, sub, imdb_id))
        .collect())
}

/// Wizdom serves Hebrew only, and its episode rows are reachable only through
/// both a season and an episode key.
fn is_searchable(request: &SubtitlePluginSearchRequest) -> bool {
    if !request
        .languages
        .iter()
        .any(|language| is_hebrew_language(language))
    {
        return false;
    }
    match request.media_kind {
        SubtitleQueryMediaKind::Movie => true,
        SubtitleQueryMediaKind::Episode => request.season.is_some() && request.episode.is_some(),
    }
}

/// The IMDb ID Scryer already holds, with no provider call involved.
fn direct_imdb_id(request: &SubtitlePluginSearchRequest) -> Option<String> {
    match request.media_kind {
        // Wizdom keys series on the series IMDb ID, so an episode-level ID
        // from `external_ids` would look up the wrong title.
        SubtitleQueryMediaKind::Episode => request.series_imdb_id.clone(),
        SubtitleQueryMediaKind::Movie => request.imdb_id.clone().or_else(|| {
            request
                .external_ids
                .get("imdb")
                .and_then(|values| values.iter().find(|value| !value.trim().is_empty()))
                .cloned()
        }),
    }
    .and_then(|value| normalize_imdb_id(&value))
}

fn lookup_imdb_id_via_tmdb(
    api_key: &str,
    title: &str,
    year: Option<i32>,
    media_kind: SubtitleQueryMediaKind,
) -> Result<Option<String>, Failure> {
    let category = match media_kind {
        SubtitleQueryMediaKind::Movie => "movie",
        SubtitleQueryMediaKind::Episode => "tv",
    };

    let mut params = vec![
        ("api_key", api_key.to_string()),
        ("query", title.to_string()),
        ("language", "en".to_string()),
    ];
    if let Some(year) = year {
        params.push(("year", year.to_string()));
    }
    let search_url = format!(
        "{TMDB_API_BASE}/search/{category}?{}",
        encode_query(&params)
    );
    let search = http_get_json("Wizdom TMDB search", &search_url)?;

    let Some(tmdb_id) = search
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(|result| result.get("id"))
        .and_then(Value::as_i64)
    else {
        return Ok(None);
    };

    let detail_path = match media_kind {
        SubtitleQueryMediaKind::Movie => format!("{TMDB_API_BASE}/movie/{tmdb_id}"),
        SubtitleQueryMediaKind::Episode => format!("{TMDB_API_BASE}/tv/{tmdb_id}/external_ids"),
    };
    let detail_url = format!(
        "{detail_path}?{}",
        encode_query(&[("api_key", api_key.to_string())])
    );
    let detail = http_get_json("Wizdom TMDB lookup", &detail_url)?;

    Ok(detail
        .get("imdb_id")
        .and_then(Value::as_str)
        .and_then(normalize_imdb_id))
}

fn fetch_releases(
    config: &WizdomConfig,
    imdb_id: &str,
    allow_missing: bool,
) -> Result<WizdomReleases, Failure> {
    let url = format!("{}/api/releases/{imdb_id}", config.base_url);
    let response = http_get("Wizdom releases", &url, "application/json", None)?;
    // The releases endpoint answers with HTTP 500 for an IMDb ID it does not
    // carry, which is a miss rather than a provider fault.
    if allow_missing && response.status_code() == 500 {
        return Ok(WizdomReleases::default());
    }
    map_http_status("Wizdom releases", &response)?;
    let body = response.body();
    if allow_missing && body.is_empty() {
        return Ok(WizdomReleases::default());
    }
    serde_json::from_slice(&body).map_err(|error| {
        Failure::new(
            FailureKind::Unsupported,
            format!("Wizdom JSON parse error: {error}"),
        )
    })
}

/// `subs` arrives in three shapes: a flat array for movies, and for series
/// either an array indexed by season number or an object keyed by the season
/// number as a string. Episodes are always keyed by the stringified number.
fn collect_subs(
    subs: Option<&Value>,
    media_kind: SubtitleQueryMediaKind,
    season: Option<i32>,
    episode: Option<i32>,
) -> Vec<WizdomSub> {
    let node = match media_kind {
        SubtitleQueryMediaKind::Movie => subs,
        SubtitleQueryMediaKind::Episode => subs
            .zip(season)
            .and_then(|(subs, season)| match subs {
                Value::Array(seasons) => usize::try_from(season)
                    .ok()
                    .and_then(|index| seasons.get(index)),
                Value::Object(_) => subs.get(season.to_string()),
                _ => None,
            })
            .zip(episode)
            .and_then(|(season_node, episode)| season_node.get(episode.to_string())),
    };
    node.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| WizdomSub::deserialize(item).ok())
        .filter(|sub| subtitle_id(sub).is_some())
        .collect()
}

fn sub_to_candidate(
    config: &WizdomConfig,
    request: &SubtitlePluginSearchRequest,
    sub: &WizdomSub,
    imdb_id: &str,
) -> Option<SubtitlePluginCandidate> {
    let subtitle_id = subtitle_id(sub)?;
    let provider_file_id = serde_json::to_string(&WizdomDownloadRef {
        filename: format!("{subtitle_id}.zip"),
        subtitle_id,
        page_url: Some(page_url(config, imdb_id, request.media_kind)),
    })
    .ok()?;

    Some(SubtitlePluginCandidate {
        provider_file_id,
        language: PROVIDER_LANGUAGE.to_string(),
        release_info: normalize_non_empty(&sub.version),
        hearing_impaired: false,
        forced: false,
        ai_translated: false,
        machine_translated: false,
        uploader: None,
        download_count: None,
        match_hints: build_match_hints(request, sub),
    })
}

fn build_match_hints(
    request: &SubtitlePluginSearchRequest,
    sub: &WizdomSub,
) -> Vec<SubtitleMatchHint> {
    let hint = |kind, value| SubtitleMatchHint { kind, value };
    let mut match_hints = vec![
        hint(SubtitleMatchHintKind::Title, None),
        hint(
            SubtitleMatchHintKind::Language,
            Some(PROVIDER_LANGUAGE.to_string()),
        ),
    ];

    match request.media_kind {
        SubtitleQueryMediaKind::Movie => {
            if request.imdb_id.is_some() {
                match_hints.push(hint(SubtitleMatchHintKind::ImdbId, None));
            }
        }
        SubtitleQueryMediaKind::Episode => {
            if request.series_imdb_id.is_some() {
                match_hints.push(hint(SubtitleMatchHintKind::SeriesImdbId, None));
            }
            // The season/episode keys are how the row was located, so a
            // returned row always matches the requested episode.
            match_hints.push(hint(SubtitleMatchHintKind::SeasonEpisode, None));
        }
    }

    if let Some(release) = normalize_non_empty(&sub.version) {
        match_hints.push(hint(SubtitleMatchHintKind::Release, Some(release)));
    }

    match_hints
}

fn download_subtitle_impl(
    config: &WizdomConfig,
    reference: &WizdomDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, Failure> {
    let url = format!(
        "{}/api/files/sub/{}",
        config.base_url, reference.subtitle_id
    );
    let response = http_get(
        "Wizdom download",
        &url,
        "*/*",
        reference.page_url.as_deref(),
    )?;
    map_http_status("Wizdom download", &response)?;

    let bytes = response.body();
    if bytes.is_empty() {
        return Err(Failure::new(
            FailureKind::Unsupported,
            "Wizdom download returned an empty body",
        ));
    }
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(Failure::new(
            FailureKind::Unsupported,
            format!(
                "Wizdom download exceeded {MAX_DOWNLOAD_BYTES} bytes ({} bytes)",
                bytes.len()
            ),
        ));
    }

    let filename = response_header(&response, "content-disposition")
        .and_then(content_disposition_filename)
        .or_else(|| normalize_non_empty(&reference.filename))
        .unwrap_or_else(|| format!("{}.zip", reference.subtitle_id));
    let extracted = archive_extract(PluginArchiveExtractRequest {
        content: bytes,
        format: "zip".to_string(),
        filename: Some(filename),
        password: None,
    })
    .map_err(|error| {
        let kind = match &error {
            HostCallError::Service(error) if error.code == PluginErrorCode::Unsupported => {
                FailureKind::MissingCapability
            }
            _ => FailureKind::Unsupported,
        };
        Failure::new(kind, format!("Wizdom subtitle extraction failed: {error}"))
    })?;
    select_subtitle(extracted.files)
}

/// Wizdom may bundle the same subtitle in UTF-8 and Windows-1255. Validate
/// cue structure before choosing a member, and keep legacy bytes intact for
/// consumers which detect their encoding. The host owns archive/path limits.
fn select_subtitle(
    files: Vec<PluginArchiveExtractedFile>,
) -> Result<SubtitlePluginDownloadResponse, Failure> {
    let mut candidates = files
        .into_iter()
        .filter_map(|file| {
            let format = file.relative_path.rsplit_once('.')?.1.to_ascii_lowercase();
            let text = String::from_utf8_lossy(&file.content)
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            if file.content.len() > MAX_DOWNLOAD_BYTES
                || file.content.contains(&0)
                || !valid_subtitle(&text, &format)
            {
                return None;
            }
            Some((file, format))
        })
        .collect::<Vec<_>>();
    // Stable ordering retains the provider's order within each encoding.
    candidates.sort_by_key(|(file, _)| std::str::from_utf8(&file.content).is_err());
    let Some((file, format)) = candidates.into_iter().next() else {
        return Err(Failure::new(
            FailureKind::Unsupported,
            "Wizdom archive contained no valid SRT or SUB subtitle",
        ));
    };
    let mut content = Vec::with_capacity(file.content.len());
    let mut bytes = file.content.into_iter().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\r' {
            if bytes.peek() == Some(&b'\n') {
                bytes.next();
            }
            content.push(b'\n');
        } else {
            content.push(byte);
        }
    }
    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(content),
        content_type: Some("text/plain".to_string()),
        filename: Some(
            file.relative_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("subtitle.srt")
                .to_string(),
        ),
        format,
    })
}

fn valid_subtitle(text: &str, format: &str) -> bool {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return false;
    }
    match format {
        "srt" => text
            .split("\n\n")
            .filter(|block| !block.trim().is_empty())
            .all(valid_srt_cue),
        "sub" => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(valid_microdvd_cue),
        _ => false,
    }
}

fn valid_srt_cue(block: &str) -> bool {
    let mut lines = block.lines();
    let Some(index) = lines.next() else {
        return false;
    };
    if index.trim().parse::<u32>().is_err() {
        return false;
    }
    let Some((start, end)) = lines.next().and_then(|line| line.split_once("-->")) else {
        return false;
    };
    let (Some(start), Some(end)) = (
        srt_time(start.trim()),
        srt_time(end.split_whitespace().next().unwrap_or_default()),
    ) else {
        return false;
    };
    end > start && lines.any(|line| !line.trim().is_empty())
}

fn valid_microdvd_cue(line: &str) -> bool {
    let Some((start, rest)) = line
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.split_once('}'))
    else {
        return false;
    };
    let Some((end, content)) = rest.strip_prefix('{').and_then(|s| s.split_once('}')) else {
        return false;
    };
    matches!((start.parse::<u32>(), end.parse::<u32>()), (Ok(start), Ok(end)) if end >= start)
        && !content.trim().is_empty()
}

fn srt_time(value: &str) -> Option<u64> {
    let mut fields = value.split([':', ',', '.']);
    let hours = fields.next()?.parse::<u64>().ok()?;
    let minutes = fields.next()?.parse::<u64>().ok()?;
    let seconds = fields.next()?.parse::<u64>().ok()?;
    let millis = fields.next()?.parse::<u64>().ok()?;
    if fields.next().is_some() || minutes >= 60 || seconds >= 60 || millis >= 1000 {
        return None;
    }
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000 + seconds * 1000 + millis)
}

fn http_get_json(label: &str, url: &str) -> Result<Value, Failure> {
    let response = http_get(label, url, "application/json", None)?;
    map_http_status(label, &response)?;
    serde_json::from_slice(&response.body()).map_err(|error| {
        Failure::new(
            FailureKind::Unsupported,
            format!("{label} JSON parse error: {error}"),
        )
    })
}

/// One GET with bounded transport retries. Rate limits return to the host. The
/// status is classified here only to decide whether to retry; callers map it.
fn http_get(
    label: &str,
    url: &str,
    accept: &str,
    referer: Option<&str>,
) -> Result<HttpResponse, Failure> {
    let mut request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", accept)
        .with_header("User-Agent", USER_AGENT);
    if let Some(referer) = referer {
        request = request.with_header("Referer", referer);
    }

    let mut attempt = 1;
    loop {
        let result = http::request::<Vec<u8>>(&request, None)
            .map_err(|error| {
                Failure::new(
                    FailureKind::Unreachable,
                    format!("{label} request failed: {error}"),
                )
            })
            .and_then(|response| match response.status_code() {
                429 => map_http_status(label, &response).map(|()| response),
                _ => Ok(response),
            });
        match result {
            Err(failure) if attempt < RETRY_AMOUNT && failure.kind == FailureKind::Unreachable => {
                attempt += 1;
                std::thread::sleep(Duration::from_secs(RETRY_TIMEOUT_SECS));
            }
            result => return result,
        }
    }
}

fn map_http_status(label: &str, response: &HttpResponse) -> Result<(), Failure> {
    let status = response.status_code();
    // Only decode a body preview on the error paths; a successful download body
    // is a zip that would otherwise be read twice.
    if (200..=299).contains(&status) {
        return Ok(());
    }
    let retry_after_seconds = response_header(response, "retry-after")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0);
    map_http_status_details(
        label,
        status,
        &response_body_preview(response),
        retry_after_seconds,
    )
}

fn map_http_status_details(
    label: &str,
    status: u16,
    body_text: &str,
    retry_after_seconds: Option<i64>,
) -> Result<(), Failure> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(Failure::new(
            FailureKind::AuthFailed,
            format!("{label} authentication failed"),
        )),
        429 => {
            // Let the host schedule the next invocation; never retry before
            // an upstream cooldown, or discard a long Retry-After hint.
            let retry_after_seconds = retry_after_seconds.or(Some(RETRY_TIMEOUT_SECS as i64));
            let message = match retry_after_seconds {
                Some(seconds) => format!("{label} rate limited — retry after {seconds}s"),
                None => format!("{label} rate limited — try again later"),
            };
            Err(Failure::new(FailureKind::RateLimited, message)
                .with_retry_after(retry_after_seconds))
        }
        status => Err(Failure::new(
            if status >= 500 {
                FailureKind::Unreachable
            } else {
                FailureKind::Unsupported
            },
            format!("{label} returned HTTP {status}: {body_text}"),
        )),
    }
}

fn validation_error_response(failure: &Failure) -> SubtitlePluginValidateConfigResponse {
    let status = match failure.kind {
        FailureKind::InvalidConfig => SubtitleValidateConfigStatus::InvalidConfig,
        FailureKind::AuthFailed => SubtitleValidateConfigStatus::AuthFailed,
        FailureKind::RateLimited => SubtitleValidateConfigStatus::RateLimited,
        FailureKind::Unreachable => SubtitleValidateConfigStatus::Unreachable,
        FailureKind::Unsupported | FailureKind::MissingCapability => {
            SubtitleValidateConfigStatus::Unsupported
        }
    };
    SubtitlePluginValidateConfigResponse {
        status,
        message: Some(failure.message.clone()),
        retry_after_seconds: failure.retry_after_seconds,
    }
}

fn config_string(key: &str) -> Result<Option<String>, Failure> {
    config::get(key)
        .map(|value| value.as_deref().and_then(normalize_non_empty))
        .map_err(|error| {
            Failure::new(
                FailureKind::InvalidConfig,
                format!("failed to read config value '{key}': {error}"),
            )
        })
}

fn is_hebrew_language(code: &str) -> bool {
    let normalized = code.trim().to_ascii_lowercase();
    let base = normalized.split(['-', '_']).next().unwrap_or_default();
    // `iw` is the legacy ISO 639-1 code for Hebrew and still appears in the wild.
    matches!(base, "heb" | "he" | "iw")
}

fn titles_for_search(request: &SubtitlePluginSearchRequest) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    request
        .title_candidates
        .iter()
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
        .filter_map(|candidate| normalize_non_empty(candidate))
        .filter(|title| seen.insert(title.to_lowercase()))
        .collect()
}

fn normalize_imdb_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let digits = trimmed.strip_prefix("tt").unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(format!("tt{digits}"))
}

fn subtitle_id(sub: &WizdomSub) -> Option<String> {
    match &sub.id {
        Value::String(value) => normalize_non_empty(value),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn page_url(config: &WizdomConfig, imdb_id: &str, media_kind: SubtitleQueryMediaKind) -> String {
    let section = match media_kind {
        SubtitleQueryMediaKind::Movie => "movies",
        SubtitleQueryMediaKind::Episode => "series",
    };
    format!("{}/{section}/{imdb_id}", config.base_url)
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Header names are matched case-insensitively; the host does not promise a
/// casing.
fn response_header<'a>(response: &'a HttpResponse, name: &str) -> Option<&'a str> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn content_disposition_filename(value: &str) -> Option<String> {
    value
        .split(';')
        .skip(1)
        .find_map(|part| part.trim().strip_prefix("filename="))
        .and_then(|raw| normalize_non_empty(raw.trim().trim_matches('"')))
}

fn response_body_preview(response: &HttpResponse) -> String {
    let body = response.body();
    let text = String::from_utf8_lossy(&body);
    text.trim().chars().take(ERROR_BODY_PREVIEW_LIMIT).collect()
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
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WizdomConfig {
        WizdomConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            tmdb_api_key: None,
        }
    }

    fn sub(id: i64, version: &str) -> Value {
        serde_json::json!({ "id": id, "version": version })
    }

    fn search_request(value: Value) -> SubtitlePluginSearchRequest {
        serde_json::from_value(value).expect("search request fixture")
    }

    #[test]
    fn movie_lookups_prefer_the_request_imdb_id() {
        let request = search_request(serde_json::json!({
            "media_kind": "movie",
            "title": "Inception",
            "imdb_id": "tt1375666",
            "external_ids": { "imdb": ["tt9999999"] },
        }));
        assert_eq!(direct_imdb_id(&request), Some("tt1375666".to_string()));
    }

    #[test]
    fn movie_lookups_fall_back_to_external_imdb_ids() {
        let request = search_request(serde_json::json!({
            "media_kind": "movie",
            "title": "Inception",
            "external_ids": { "imdb": ["tt1375666"] },
        }));
        assert_eq!(direct_imdb_id(&request), Some("tt1375666".to_string()));
    }

    #[test]
    fn episode_lookups_use_the_series_id_and_ignore_external_imdb_ids() {
        let request = search_request(serde_json::json!({
            "media_kind": "episode",
            "title": "Breaking Bad",
            "series_imdb_id": "tt0903747",
            "imdb_id": "tt1054724",
            "external_ids": { "imdb": ["tt1054724"] },
            "season": 1,
            "episode": 1,
        }));
        assert_eq!(direct_imdb_id(&request), Some("tt0903747".to_string()));

        // Without a series ID there is nothing safe to query, and no TMDB key
        // is configured to backfill one.
        let episode_only = search_request(serde_json::json!({
            "media_kind": "episode",
            "title": "Breaking Bad",
            "external_ids": { "imdb": ["tt1054724"] },
            "season": 1,
            "episode": 1,
        }));
        assert_eq!(direct_imdb_id(&episode_only), None);
    }

    #[test]
    fn searches_without_hebrew_return_no_candidates() {
        let request = search_request(serde_json::json!({
            "media_kind": "movie",
            "title": "Inception",
            "imdb_id": "tt1375666",
            "languages": ["eng", "fra"],
        }));
        assert!(!is_searchable(&request));
    }

    #[test]
    fn episode_searches_without_a_season_or_episode_return_no_candidates() {
        let request = search_request(serde_json::json!({
            "media_kind": "episode",
            "title": "Breaking Bad",
            "series_imdb_id": "tt0903747",
            "languages": ["heb"],
            "season": 1,
        }));
        assert!(!is_searchable(&request));
    }

    #[test]
    fn hebrew_language_codes_are_recognized() {
        for code in ["heb", "he", "HE", "iw", "he-IL", "heb_IL", " heb "] {
            assert!(is_hebrew_language(code), "expected {code} to be Hebrew");
        }
        for code in ["eng", "en", "hin", ""] {
            assert!(!is_hebrew_language(code), "expected {code} to be rejected");
        }
    }

    #[test]
    fn imdb_ids_normalize_to_the_tt_prefixed_form() {
        assert_eq!(normalize_imdb_id("tt1375666").as_deref(), Some("tt1375666"));
        assert_eq!(normalize_imdb_id("1375666").as_deref(), Some("tt1375666"));
        assert_eq!(
            normalize_imdb_id(" tt1375666 ").as_deref(),
            Some("tt1375666")
        );
        assert_eq!(normalize_imdb_id("tt"), None);
        assert_eq!(normalize_imdb_id("not-an-id"), None);
        assert_eq!(normalize_imdb_id(""), None);
    }

    #[test]
    fn movie_subs_parse_from_a_flat_array() {
        let subs = serde_json::json!([sub(11, "1080p.WEB"), sub(12, "720p.BluRay")]);
        let parsed = collect_subs(Some(&subs), SubtitleQueryMediaKind::Movie, None, None);
        assert_eq!(parsed.len(), 2);
        assert_eq!(subtitle_id(&parsed[0]).as_deref(), Some("11"));
        assert_eq!(parsed[1].version, "720p.BluRay");
    }

    #[test]
    fn episode_subs_parse_from_a_season_indexed_array() {
        // Index 0 is the placeholder slot, so season 1 sits at index 1.
        let subs = serde_json::json!([
            {},
            { "3": [sub(21, "S01E03.WEB")] },
        ]);
        let parsed = collect_subs(
            Some(&subs),
            SubtitleQueryMediaKind::Episode,
            Some(1),
            Some(3),
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(subtitle_id(&parsed[0]).as_deref(), Some("21"));
    }

    #[test]
    fn episode_subs_parse_from_a_season_keyed_object() {
        let subs = serde_json::json!({
            "2": { "5": [sub(31, "S02E05.HDTV"), sub(32, "S02E05.WEB")] },
        });
        let parsed = collect_subs(
            Some(&subs),
            SubtitleQueryMediaKind::Episode,
            Some(2),
            Some(5),
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].version, "S02E05.HDTV");
    }

    #[test]
    fn missing_season_or_episode_keys_yield_no_subs() {
        let subs = serde_json::json!({ "2": { "5": [sub(31, "S02E05.HDTV")] } });
        for (season, episode) in [(Some(3), Some(5)), (Some(2), Some(9))] {
            let parsed = collect_subs(
                Some(&subs),
                SubtitleQueryMediaKind::Episode,
                season,
                episode,
            );
            assert!(parsed.is_empty());
        }
        assert!(collect_subs(None, SubtitleQueryMediaKind::Movie, None, None).is_empty());
    }

    #[test]
    fn rows_without_a_usable_id_are_dropped() {
        let subs = serde_json::json!([
            sub(11, "kept"),
            { "version": "no id" },
            { "id": null, "version": "null id" },
        ]);
        let parsed = collect_subs(Some(&subs), SubtitleQueryMediaKind::Movie, None, None);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].version, "kept");
    }

    #[test]
    fn download_refs_round_trip_through_the_provider_file_id() {
        let reference = WizdomDownloadRef {
            subtitle_id: "42".to_string(),
            filename: "42.zip".to_string(),
            page_url: Some("https://wizdom.xyz/movies/tt1375666".to_string()),
        };
        let encoded = serde_json::to_string(&reference).expect("serialize");
        let decoded: WizdomDownloadRef = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, reference);
    }

    #[test]
    fn page_urls_use_the_media_specific_section() {
        let config = config();
        assert_eq!(
            page_url(&config, "tt1375666", SubtitleQueryMediaKind::Movie),
            "https://wizdom.xyz/movies/tt1375666"
        );
        assert_eq!(
            page_url(&config, "tt0903747", SubtitleQueryMediaKind::Episode),
            "https://wizdom.xyz/series/tt0903747"
        );
    }

    #[test]
    fn http_status_mapping_matches_provider_semantics() {
        assert!(map_http_status_details("probe", 200, "", None).is_ok());
        assert_eq!(
            map_http_status_details("probe", 403, "", None)
                .unwrap_err()
                .kind,
            FailureKind::AuthFailed
        );
        let rate_limited = map_http_status_details("probe", 429, "", Some(5)).unwrap_err();
        assert_eq!(rate_limited.kind, FailureKind::RateLimited);
        assert_eq!(rate_limited.retry_after_seconds, Some(5));
        assert_eq!(
            map_http_status_details("probe", 404, "missing", None)
                .unwrap_err()
                .kind,
            FailureKind::Unsupported
        );
    }

    #[test]
    fn content_disposition_filenames_are_extracted() {
        assert_eq!(
            content_disposition_filename("attachment; filename=\"wizdom.42.zip\"").as_deref(),
            Some("wizdom.42.zip")
        );
        assert_eq!(content_disposition_filename("attachment"), None);
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            encode_query(&[("query", "The Dark Knight".to_string())]),
            "query=The%20Dark%20Knight"
        );
    }

    // The two fixtures below are trimmed captures of real
    // `api/releases/{imdb_id}` responses, kept so the parser stays pinned to
    // the shapes the provider actually serves.

    #[test]
    fn live_movie_payload_yields_candidates() {
        let payload: Value = serde_json::from_str(
            r#"{"subs":[
                {"id":9229,"version":"Inception.2010.Bluray.1080p.DTS-HDMA.x264.dxva-FraMeSToR",
                 "date":"10/24/2016","resolution":"1080p","format":"BluRay",
                 "video_codec":"h264","audio_codec":"DTS","release_group":"FraMeSToR"},
                {"id":90675,"version":"Inception.2010.DVDRip.XviD.RoSubbed-playOFF",
                 "date":"12/9/2016","format":"DVD","video_codec":"XviD","release_group":"playOFF"}
            ]}"#,
        )
        .expect("movie payload");
        let subs = collect_subs(
            payload.get("subs"),
            SubtitleQueryMediaKind::Movie,
            None,
            None,
        );
        assert_eq!(subs.len(), 2);
        assert_eq!(subtitle_id(&subs[0]).as_deref(), Some("9229"));
        assert!(subs[0].version.contains("FraMeSToR"));
    }

    #[test]
    fn live_series_payload_yields_candidates() {
        // Real series responses key `subs` by season string, then by episode
        // string — not by array position.
        let payload: Value = serde_json::from_str(
            r#"{"subs":{"1":{"1":[
                {"id":60186,"version":"Breaking.Bad.S01E01.720p.BluRay.x264-CtrlHD",
                 "date":"10/26/2016","resolution":"720p","format":"BluRay",
                 "video_codec":"h264","release_group":"CtrlHD"},
                {"id":1066,"version":"Breaking.Bad.S01E01.720p.HDTV.X264-BiA",
                 "date":"10/23/2016","resolution":"720p","format":"HDTV",
                 "video_codec":"h264","release_group":"BiA"}
            ]}}}"#,
        )
        .expect("series payload");
        let subs = collect_subs(
            payload.get("subs"),
            SubtitleQueryMediaKind::Episode,
            Some(1),
            Some(1),
        );
        assert_eq!(subs.len(), 2);
        assert_eq!(subtitle_id(&subs[0]).as_deref(), Some("60186"));

        // A season or episode the payload does not carry stays empty.
        assert!(
            collect_subs(
                payload.get("subs"),
                SubtitleQueryMediaKind::Episode,
                Some(2),
                Some(1)
            )
            .is_empty()
        );
    }

    fn archive_file(name: &str, content: &[u8]) -> PluginArchiveExtractedFile {
        PluginArchiveExtractedFile {
            relative_path: name.into(),
            content: content.to_vec(),
        }
    }

    #[test]
    fn archive_selection_skips_invalid_members_and_prefers_utf8() {
        let legacy = b"1\r\n00:00:01,000 --> 00:00:02,000\r\n\xf9\xec\xe5\xed\r\n";
        let utf8 = "1\r\n00:00:01,000 --> 00:00:02,000\r\nשלום\r\n";
        let selected = select_subtitle(vec![
            archive_file("broken.srt", b"this is not a subtitle"),
            archive_file("legacy.srt", legacy),
            archive_file("utf8.SRT", utf8.as_bytes()),
        ])
        .unwrap();
        assert_eq!(selected.filename.as_deref(), Some("utf8.SRT"));
        assert_eq!(selected.format, "srt");
        assert_eq!(
            BASE64.decode(selected.content_base64).unwrap(),
            utf8.replace("\r\n", "\n").as_bytes()
        );
    }

    #[test]
    fn archive_selection_preserves_legacy_hebrew_and_sub_files() {
        let legacy = b"1\n00:00:01,000 --> 00:00:02,000\n\xf9\xec\xe5\xed\n";
        let selected = select_subtitle(vec![archive_file("legacy.srt", legacy)]).unwrap();
        assert_eq!(BASE64.decode(selected.content_base64).unwrap(), legacy);
        let microdvd = b"{1}{25}hello\r\n{26}{50}world\r\n";
        let selected = select_subtitle(vec![archive_file("subtitle.sub", microdvd)]).unwrap();
        assert_eq!(selected.format, "sub");
        assert_eq!(
            BASE64.decode(selected.content_base64).unwrap(),
            b"{1}{25}hello\n{26}{50}world\n"
        );
    }

    #[test]
    fn malformed_subtitles_are_not_selected() {
        for content in [
            "",
            "garbage",
            "1\n00:00:01,000 --> 00:00:02,000\n",
            "1\n00:00:03,000 --> 00:00:02,000\nhello\n",
            "1\n00:99:01,000 --> 00:99:02,000\nhello\n",
            "1\n00:00:01,000 --> 00:00:02,000\nhello\0\n",
        ] {
            assert!(
                select_subtitle(vec![archive_file("bad.srt", content.as_bytes())]).is_err(),
                "{content:?}"
            );
        }
        assert!(select_subtitle(vec![archive_file("readme.txt", b"hello")]).is_err());
        assert!(select_subtitle(vec![]).is_err());
    }

    #[test]
    fn alternate_titles_are_trimmed_deduplicated_and_ordered() {
        let request = search_request(serde_json::json!({
            "media_kind": "movie", "title": "Original",
            "title_candidates": ["", " Original ", "Translated"],
            "title_aliases": ["original", "Alias", "Translated"],
        }));
        assert_eq!(
            titles_for_search(&request),
            ["Original", "Translated", "Alias"]
        );
    }

    #[test]
    fn long_rate_limit_hints_are_preserved() {
        let failure = map_http_status_details("probe", 429, "", Some(3600)).unwrap_err();
        assert_eq!(failure.retry_after_seconds, Some(3600));
        let PluginResult::<()>::Err(error) = to_plugin_result(Err(failure)) else {
            panic!("expected failure")
        };
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(3600));
    }
}
