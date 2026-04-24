use std::collections::BTreeMap;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://jimaku.cc/api";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const MIN_SUBTITLE_BYTES: usize = 500;
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginDescriptor {
    name: String,
    version: String,
    sdk_version: String,
    plugin_type: String,
    provider_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_aliases: Vec<String>,
    #[serde(default)]
    capabilities: IndexerCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scoring_policies: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    config_fields: Vec<ConfigFieldDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limit_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification_capabilities: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    accepted_inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    isolation_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    download_client_capabilities: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle_capabilities: Option<SubtitleCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexerCapabilities {}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigFieldDef {
    key: String,
    label: String,
    field_type: ConfigFieldType,
    #[serde(default)]
    required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_value: Option<String>,
    #[serde(default)]
    value_source: ConfigFieldValueSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_binding: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    options: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    help_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigFieldType {
    #[default]
    String,
    Password,
    Bool,
    Number,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigFieldValueSource {
    #[default]
    User,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleProviderMode {
    #[default]
    Catalog,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SubtitleCapabilities {
    mode: SubtitleProviderMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_media_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recommended_facets: Vec<String>,
    #[serde(default)]
    supports_hash_lookup: bool,
    #[serde(default)]
    supports_forced: bool,
    #[serde(default)]
    supports_hearing_impaired: bool,
    #[serde(default)]
    supports_ai_translated: bool,
    #[serde(default)]
    supports_machine_translated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_languages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleValidateConfigStatus {
    Valid,
    InvalidConfig,
    AuthFailed,
    RateLimited,
    Unreachable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleMatchHintKind {
    ExternalId,
    Title,
    SeasonEpisode,
    Language,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitleMatchHint {
    kind: SubtitleMatchHintKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleQueryMediaKind {
    Movie,
    Episode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitlePluginSearchRequest {
    media_kind: SubtitleQueryMediaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    facet: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute_episode: Option<i32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub external_ids: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitlePluginCandidate {
    provider_file_id: String,
    language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_info: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    match_hints: Vec<SubtitleMatchHint>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SubtitlePluginSearchResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    results: Vec<SubtitlePluginCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitlePluginDownloadRequest {
    provider_file_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitlePluginDownloadResponse {
    content_base64: String,
    format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SubtitlePluginValidateConfigRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config_instance_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitlePluginValidateConfigResponse {
    status: SubtitleValidateConfigStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<i64>,
}

#[derive(Clone)]
struct JimakuConfig {
    api_key: String,
    enable_name_search_fallback: bool,
    enable_archives_download: bool,
    enable_ai_subs: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct JimakuEntry {
    id: i64,
    anilist_id: Option<i64>,
    #[serde(default)]
    flags: JimakuEntryFlags,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct JimakuEntryFlags {
    #[serde(default)]
    movie: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct JimakuFile {
    name: String,
    url: String,
    #[serde(default)]
    size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JimakuDownloadRef {
    url: String,
    filename: String,
    language: String,
    episode: Option<i32>,
}

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let response = match JimakuConfig::from_extism() {
        Ok(config) => {
            match jimaku_get_json::<Vec<JimakuEntry>>(&config, "entries/search?query=naruto") {
                Ok(_) => SubtitlePluginValidateConfigResponse {
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
    Ok(serde_json::to_string(&response)?)
}

#[plugin_fn]
pub fn search_subtitles(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = JimakuConfig::from_extism().map_err(Error::msg)?;
    let results = search_subtitles_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&SubtitlePluginSearchResponse {
        results,
    })?)
}

#[plugin_fn]
pub fn download_subtitle(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let reference: JimakuDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(Error::msg)?;
    let response = download_subtitle_impl(&reference).map_err(Error::msg)?;
    Ok(serde_json::to_string(&response)?)
}

impl JimakuConfig {
    fn from_extism() -> Result<Self, String> {
        Ok(Self {
            api_key: config_required_string("api_key")?,
            enable_name_search_fallback: config_bool("enable_name_search_fallback", true),
            enable_archives_download: config_bool("enable_archives_download", false),
            enable_ai_subs: config_bool("enable_ai_subs", false),
        })
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: "Jimaku".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "subtitle_provider".to_string(),
        provider_type: "jimaku".to_string(),
        provider_aliases: vec![],
        capabilities: IndexerCapabilities::default(),
        scoring_policies: vec![],
        config_fields: vec![
            config_field(
                "api_key",
                "Jimaku API Key",
                ConfigFieldType::Password,
                true,
                None,
            ),
            config_field(
                "enable_name_search_fallback",
                "Enable Name Search Fallback",
                ConfigFieldType::Bool,
                false,
                Some("true"),
            ),
            config_field(
                "enable_archives_download",
                "Enable Archive Downloads",
                ConfigFieldType::Bool,
                false,
                Some("false"),
            ),
            config_field(
                "enable_ai_subs",
                "Enable AI/Whisper Subtitles",
                ConfigFieldType::Bool,
                false,
                Some("false"),
            ),
        ],
        default_base_url: Some(API_BASE.to_string()),
        allowed_hosts: vec!["jimaku.cc".to_string()],
        rate_limit_seconds: Some(1),
        notification_capabilities: None,
        accepted_inputs: vec![],
        isolation_modes: vec![],
        download_client_capabilities: None,
        subtitle_capabilities: Some(SubtitleCapabilities {
            mode: SubtitleProviderMode::Catalog,
            supported_media_kinds: vec!["movie".to_string(), "episode".to_string()],
            recommended_facets: vec!["anime".to_string()],
            supports_hash_lookup: false,
            supports_forced: false,
            supports_hearing_impaired: false,
            supports_ai_translated: true,
            supports_machine_translated: false,
            supported_languages: vec!["jpn".to_string(), "eng".to_string()],
        }),
    }
}

fn config_field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        host_binding: None,
        options: vec![],
        help_text: None,
    }
}

fn search_subtitles_impl(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let entries = search_entries(config, request)?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(Vec::new());
    };

    let mut only_archives = false;
    let files = if request.media_kind == SubtitleQueryMediaKind::Episode && !entry.flags.movie {
        if let Some(episode) = request.episode.or(request.absolute_episode) {
            let files = entry_files(config, entry.id, Some(episode))?;
            if files.is_empty() {
                only_archives = true;
                entry_files(config, entry.id, None)?
            } else {
                files
            }
        } else {
            entry_files(config, entry.id, None)?
        }
    } else {
        entry_files(config, entry.id, None)?
    };

    let archive_count = files.iter().filter(|file| is_archive(&file.name)).count();
    let direct_count = files.len().saturating_sub(archive_count);
    let archive_only = archive_count > 0 && direct_count == 0;

    let mut results = Vec::new();
    for file in files {
        if file.size.unwrap_or(MIN_SUBTITLE_BYTES) < MIN_SUBTITLE_BYTES {
            continue;
        }
        if !config.enable_ai_subs && looks_like_ai_subtitle(&file.name) {
            continue;
        }
        let archive = is_archive(&file.name);
        if archive && !archive_only && !only_archives && !config.enable_archives_download {
            continue;
        }
        if !archive && !is_subtitle_file(&file.name) {
            continue;
        }

        let language = detect_language(&file.name);
        if !requested_language_matches(&request.languages, &language) {
            continue;
        }

        let provider_file_id = serde_json::to_string(&JimakuDownloadRef {
            url: file.url.clone(),
            filename: file.name.clone(),
            language: language.clone(),
            episode: request.episode.or(request.absolute_episode),
        })
        .map_err(|error| format!("failed to encode Jimaku download ref: {error}"))?;

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
        if request.media_kind == SubtitleQueryMediaKind::Episode && request.episode.is_some() {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::SeasonEpisode,
                value: None,
            });
        }
        if let Some(anilist_id) = entry.anilist_id {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::ExternalId,
                value: Some(format!("anilist:{anilist_id}")),
            });
        }

        results.push(SubtitlePluginCandidate {
            provider_file_id,
            language,
            release_info: Some(file.name),
            match_hints,
        });
    }

    Ok(results)
}

fn search_entries(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<JimakuEntry>, String> {
    for id in request
        .external_ids
        .get("anilist")
        .into_iter()
        .flatten()
        .filter(|id| !id.trim().is_empty())
    {
        let path = format!("entries/search?anilist_id={}", url_encode(id.trim()));
        let entries = jimaku_get_json::<Vec<JimakuEntry>>(config, &path)?;
        if !entries.is_empty() {
            return Ok(entries);
        }
    }

    if !config.enable_name_search_fallback && request.media_kind != SubtitleQueryMediaKind::Movie {
        return Ok(Vec::new());
    }

    let mut query = request.title.to_ascii_lowercase();
    if request.media_kind == SubtitleQueryMediaKind::Episode {
        if let Some(season) = request.season {
            if season > 1 {
                query = format!("{query} {season}");
            }
        }
    }
    let path = format!("entries/search?query={}", url_encode(&query));
    let entries = jimaku_get_json::<Vec<JimakuEntry>>(config, &path)?;
    if !entries.is_empty() {
        return Ok(entries);
    }

    let path = format!("entries/search?query={}&anime=false", url_encode(&query));
    jimaku_get_json::<Vec<JimakuEntry>>(config, &path)
}

fn entry_files(
    config: &JimakuConfig,
    entry_id: i64,
    episode: Option<i32>,
) -> Result<Vec<JimakuFile>, String> {
    let path = match episode {
        Some(episode) => format!("entries/{entry_id}/files?episode={episode}"),
        None => format!("entries/{entry_id}/files"),
    };
    jimaku_get_json(config, &path)
}

fn download_subtitle_impl(
    reference: &JimakuDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, String> {
    let response = http_get(&reference.url, None)?;
    if response.status_code() >= 400 {
        return Err(format!(
            "Jimaku subtitle download returned HTTP {}",
            response.status_code()
        ));
    }
    let bytes = response.body();
    if !is_archive(&reference.filename) && bytes.len() < MIN_SUBTITLE_BYTES {
        return Err("Jimaku subtitle file is too small".to_string());
    }
    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(bytes),
        format: file_extension(&reference.filename)
            .unwrap_or("ass")
            .to_string(),
        filename: Some(reference.filename.clone()),
        content_type: None,
    })
}

fn jimaku_get_json<T: for<'de> Deserialize<'de>>(
    config: &JimakuConfig,
    path: &str,
) -> Result<T, String> {
    let url = format!(
        "{}/{}",
        API_BASE.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let response = http_get(&url, Some(config.api_key.as_str()))?;
    if response.status_code() >= 400 {
        return Err(http_error("Jimaku", &response));
    }
    serde_json::from_slice(&response.body())
        .map_err(|error| format!("Jimaku JSON parse error: {error}"))
}

fn http_get(url: &str, api_key: Option<&str>) -> Result<HttpResponse, String> {
    let mut request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    if let Some(api_key) = api_key {
        request = request.with_header("Authorization", api_key);
    }

    let mut rate_limit_retry_used = false;
    loop {
        let response = http::request::<Vec<u8>>(&request, None)
            .map_err(|error| format!("Jimaku request failed: {error}"))?;
        if response.status_code() == 429 {
            let retry_after = retry_after_seconds(&response).unwrap_or(1).max(1);
            if rate_limit_retry_used || retry_after > MAX_RATE_LIMIT_WAIT_SECONDS {
                return Ok(response);
            }
            rate_limit_retry_used = true;
            std::thread::sleep(Duration::from_secs(retry_after));
            continue;
        }
        return Ok(response);
    }
}

fn http_error(provider: &str, response: &HttpResponse) -> String {
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    match status {
        401 => format!("{provider} authentication failed: {body}"),
        429 => format!(
            "{provider} rate limited — retry after {}s",
            retry_after_seconds(response).unwrap_or(1)
        ),
        _ => format!("{provider} returned HTTP {status}: {body}"),
    }
}

fn validation_error_response(error: &str) -> SubtitlePluginValidateConfigResponse {
    let status = if error.contains("authentication failed") {
        SubtitleValidateConfigStatus::AuthFailed
    } else if error.contains("rate limited") {
        SubtitleValidateConfigStatus::RateLimited
    } else if error.contains("required") || error.contains("missing") {
        SubtitleValidateConfigStatus::InvalidConfig
    } else if error.contains("request failed") {
        SubtitleValidateConfigStatus::Unreachable
    } else {
        SubtitleValidateConfigStatus::Unsupported
    };
    SubtitlePluginValidateConfigResponse {
        status,
        message: Some(error.to_string()),
        retry_after_seconds: None,
    }
}

fn config_required_string(key: &str) -> Result<String, String> {
    config::get(key)
        .map_err(|error| format!("failed to read config {key}: {error}"))?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Jimaku {key} is required"))
}

fn config_bool(key: &str, default: bool) -> bool {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn is_archive(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    lower.ends_with(".zip")
        || lower.ends_with(".rar")
        || lower.ends_with(".7z")
        || lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.zst")
        || lower.ends_with(".tzst")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
        || lower.ends_with(".gz")
        || lower.ends_with(".zst")
        || lower.ends_with(".xz")
}

fn is_subtitle_file(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    [".srt", ".ass", ".ssa", ".vtt"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn looks_like_ai_subtitle(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    lower.contains("whisper") || lower.contains("whisperai")
}

fn detect_language(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    if lower.contains(".en.") || lower.contains("[en]") || lower.contains("english") {
        "eng".to_string()
    } else {
        "jpn".to_string()
    }
}

fn requested_language_matches(requested: &[String], language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|candidate| normalize_lang(candidate) == normalize_lang(language))
}

fn normalize_lang(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "en" | "eng" | "english" => "eng",
        "ja" | "jpn" | "jp" | "japanese" => "jpn",
        _ => language,
    }
}

fn file_extension(filename: &str) -> Option<&str> {
    filename.rsplit_once('.').map(|(_, ext)| ext)
}

fn retry_after_seconds(response: &HttpResponse) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-ratelimit-reset"))
        .and_then(|value| value.parse::<u64>().ok())
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
