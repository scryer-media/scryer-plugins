use std::collections::BTreeMap;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use serde::{Deserialize, Serialize};

const FEED_API_URL: &str = "https://feed.animetosho.org/json";
const STORAGE_BASE_URL: &str = "https://animetosho.org/storage/attach";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const DEFAULT_SEARCH_THRESHOLD: usize = 6;
const MAX_SEARCH_THRESHOLD: usize = 15;
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 10;
const XZ_MAGIC: &[u8] = b"\xFD\x37\x7A\x58\x5A\x00";

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_languages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleValidateConfigStatus {
    Valid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubtitleMatchHintKind {
    ExternalId,
    AbsoluteEpisode,
    SeasonEpisode,
    Title,
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
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    season: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    episode: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    absolute_episode: Option<i32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    external_ids: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    languages: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnimeToshoDownloadRef {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
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
struct AnimeToshoConfig {
    search_threshold: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct AnimeToshoEntry {
    id: i64,
    title: Option<String>,
    status: Option<String>,
    timestamp: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentDetails {
    #[serde(default)]
    files: Vec<TorrentFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentFile {
    filename: Option<String>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Deserialize)]
struct Attachment {
    id: i64,
    #[serde(rename = "type")]
    attachment_type: String,
    #[serde(default)]
    info: AttachmentInfo,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AttachmentInfo {
    lang: Option<String>,
    name: Option<String>,
}

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let _ = AnimeToshoConfig::from_extism();
    Ok(serde_json::to_string(
        &SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
    )?)
}

#[plugin_fn]
pub fn search_subtitles(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = AnimeToshoConfig::from_extism();
    let results = search_subtitles_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&SubtitlePluginSearchResponse {
        results,
    })?)
}

#[plugin_fn]
pub fn download_subtitle(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let reference = parse_download_ref(&request.provider_file_id).map_err(Error::msg)?;
    let response = download_subtitle_impl(&reference).map_err(Error::msg)?;
    Ok(serde_json::to_string(&response)?)
}

impl AnimeToshoConfig {
    fn from_extism() -> Self {
        let configured = config::get("search_threshold")
            .ok()
            .flatten()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_SEARCH_THRESHOLD);
        Self {
            search_threshold: configured.clamp(1, MAX_SEARCH_THRESHOLD),
        }
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: "AnimeTosho Subtitles".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "subtitle_provider".to_string(),
        provider_type: "animetosho".to_string(),
        provider_aliases: vec![],
        capabilities: IndexerCapabilities::default(),
        scoring_policies: vec![],
        config_fields: vec![ConfigFieldDef {
            key: "search_threshold".to_string(),
            label: "Search Threshold".to_string(),
            field_type: ConfigFieldType::Number,
            required: false,
            default_value: Some(DEFAULT_SEARCH_THRESHOLD.to_string()),
            value_source: ConfigFieldValueSource::User,
            host_binding: None,
            options: vec![],
            help_text: Some("Maximum AnimeTosho entries to inspect, from 1 to 15.".to_string()),
        }],
        default_base_url: Some(FEED_API_URL.to_string()),
        allowed_hosts: vec![
            "feed.animetosho.org".to_string(),
            "animetosho.org".to_string(),
        ],
        rate_limit_seconds: Some(1),
        notification_capabilities: None,
        accepted_inputs: vec![],
        isolation_modes: vec![],
        download_client_capabilities: None,
        subtitle_capabilities: Some(SubtitleCapabilities {
            mode: SubtitleProviderMode::Catalog,
            supported_media_kinds: vec!["episode".to_string()],
            recommended_facets: vec!["anime".to_string()],
            supported_languages: vec![],
        }),
    }
}

fn search_subtitles_impl(
    config: &AnimeToshoConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    if request.facet.as_deref() != Some("anime")
        || request.media_kind != SubtitleQueryMediaKind::Episode
    {
        return Ok(Vec::new());
    }

    let Some(anidb_episode_id) = request
        .external_ids
        .get("anidb_episode")
        .and_then(|values| values.iter().find(|value| !value.trim().is_empty()))
    else {
        return Ok(Vec::new());
    };

    let mut entries = fetch_entries(anidb_episode_id)?;
    entries.retain(|entry| entry.status.as_deref() == Some("complete"));
    entries.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    entries.truncate(config.search_threshold);

    let mut results = Vec::new();
    for entry in entries {
        let details = fetch_torrent_details(entry.id)?;
        for file in details.files {
            for attachment in &file.attachments {
                if attachment.attachment_type != "subtitle" {
                    continue;
                }
                let language = normalize_language(
                    attachment.info.lang.as_deref().unwrap_or("eng"),
                    attachment.info.name.as_deref(),
                );
                if !requested_language_matches(&request.languages, &language) {
                    continue;
                }
                let url = attachment_download_url(attachment.id);
                let filename = attachment_filename(&entry, &file, &attachment);
                let provider_file_id = serde_json::to_string(&AnimeToshoDownloadRef {
                    url,
                    filename: Some(filename),
                    language: Some(language.clone()),
                })
                .map_err(|error| format!("failed to encode AnimeTosho download ref: {error}"))?;
                results.push(SubtitlePluginCandidate {
                    provider_file_id,
                    language: language.clone(),
                    release_info: entry
                        .title
                        .clone()
                        .or_else(|| file.filename.clone())
                        .or_else(|| attachment.info.name.clone()),
                    match_hints: vec![
                        SubtitleMatchHint {
                            kind: SubtitleMatchHintKind::ExternalId,
                            value: Some(format!("anidb_episode:{anidb_episode_id}")),
                        },
                        SubtitleMatchHint {
                            kind: SubtitleMatchHintKind::AbsoluteEpisode,
                            value: request.absolute_episode.map(|episode| episode.to_string()),
                        },
                        SubtitleMatchHint {
                            kind: SubtitleMatchHintKind::SeasonEpisode,
                            value: None,
                        },
                        SubtitleMatchHint {
                            kind: SubtitleMatchHintKind::Title,
                            value: None,
                        },
                        SubtitleMatchHint {
                            kind: SubtitleMatchHintKind::Language,
                            value: Some(language),
                        },
                    ],
                });
            }
        }
    }

    Ok(results)
}

fn fetch_entries(anidb_episode_id: &str) -> Result<Vec<AnimeToshoEntry>, String> {
    let url = format!("{FEED_API_URL}?eid={}", url_encode(anidb_episode_id));
    http_get_json(&url)
}

fn fetch_torrent_details(id: i64) -> Result<TorrentDetails, String> {
    let url = format!("{FEED_API_URL}?show=torrent&id={id}");
    http_get_json(&url)
}

fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    let response = http_get(url)?;
    if response.status_code() >= 400 {
        return Err(http_error("AnimeTosho", &response));
    }
    serde_json::from_slice(&response.body())
        .map_err(|error| format!("AnimeTosho JSON parse error: {error}"))
}

fn parse_download_ref(provider_file_id: &str) -> Result<AnimeToshoDownloadRef, String> {
    if provider_file_id.starts_with(STORAGE_BASE_URL) {
        return Ok(AnimeToshoDownloadRef {
            url: provider_file_id.to_string(),
            filename: filename_from_url(provider_file_id),
            language: None,
        });
    }
    serde_json::from_str(provider_file_id)
        .map_err(|error| format!("invalid AnimeTosho download reference: {error}"))
}

fn download_subtitle_impl(
    reference: &AnimeToshoDownloadRef,
) -> Result<SubtitlePluginDownloadResponse, String> {
    let url = reference.url.as_str();
    if !url.starts_with(STORAGE_BASE_URL) {
        return Err("invalid AnimeTosho subtitle attachment URL".to_string());
    }
    let response = http_get(url)?;
    if response.status_code() >= 400 {
        return Err(http_error("AnimeTosho attachment", &response));
    }
    let bytes = response.body();
    if !bytes.starts_with(XZ_MAGIC) {
        return Err("AnimeTosho attachment is not an XZ stream".to_string());
    }
    let filename = reference
        .filename
        .clone()
        .or_else(|| filename_from_url(url));

    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(bytes),
        format: filename
            .as_deref()
            .and_then(compressed_subtitle_format_hint)
            .unwrap_or("ass")
            .to_string(),
        filename,
        content_type: Some("application/x-xz".to_string()),
    })
}

fn http_get(url: &str) -> Result<HttpResponse, String> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    const BACKOFF_SECS: &[u64] = &[2, 5, 10];

    let mut next_delay = 0;
    for attempt in 0..=BACKOFF_SECS.len() {
        if next_delay > 0 {
            std::thread::sleep(Duration::from_secs(next_delay));
        }
        let response = http::request::<Vec<u8>>(&request, None)
            .map_err(|error| format!("AnimeTosho request failed: {error}"))?;
        if response.status_code() != 429 {
            return Ok(response);
        }
        if attempt >= BACKOFF_SECS.len() {
            return Ok(response);
        }
        next_delay = match retry_after_seconds(&response) {
            Some(seconds) if seconds > MAX_RATE_LIMIT_WAIT_SECONDS => return Ok(response),
            Some(seconds) => seconds.max(1),
            None => BACKOFF_SECS[attempt],
        };
    }

    Err("AnimeTosho request exhausted retries".to_string())
}

fn http_error(provider: &str, response: &HttpResponse) -> String {
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    match status {
        429 => format!(
            "{provider} rate limited — retry after {}s",
            retry_after_seconds(response).unwrap_or(1)
        ),
        _ => format!("{provider} returned HTTP {status}: {body}"),
    }
}

fn retry_after_seconds(response: &HttpResponse) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-retry-after"))
        .and_then(|value| value.parse::<u64>().ok())
}

fn attachment_download_url(id: i64) -> String {
    format!("{STORAGE_BASE_URL}/{id:08x}/{id}.xz")
}

fn attachment_filename(
    entry: &AnimeToshoEntry,
    file: &TorrentFile,
    attachment: &Attachment,
) -> String {
    let base = attachment
        .info
        .name
        .as_deref()
        .or(file.filename.as_deref())
        .or(entry.title.as_deref())
        .unwrap_or("animetosho-subtitle");
    let mut filename = sanitize_filename(base);
    if !has_extension(&filename) {
        filename.push_str(".ass");
    }
    if !filename.to_ascii_lowercase().ends_with(".xz") {
        filename.push_str(".xz");
    }
    filename
}

fn filename_from_url(url: &str) -> Option<String> {
    url.rsplit('/')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(sanitize_filename)
}

fn compressed_subtitle_format_hint(filename: &str) -> Option<&'static str> {
    let lower = filename.to_ascii_lowercase();
    for suffix in [
        ".ass.xz", ".ssa.xz", ".srt.xz", ".vtt.xz", ".sub.xz", ".idx.xz",
    ] {
        if lower.ends_with(suffix) {
            return Some(&suffix[1..4]);
        }
    }
    None
}

fn sanitize_filename(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("animetosho-subtitle")
        .trim()
        .chars()
        .map(|ch| if ch.is_control() { '_' } else { ch })
        .collect::<String>()
}

fn has_extension(filename: &str) -> bool {
    filename
        .rsplit_once('.')
        .is_some_and(|(_, extension)| !extension.trim().is_empty())
}

fn requested_language_matches(requested: &[String], language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|candidate| normalize_language(candidate, None) == language)
}

fn normalize_language(language: &str, name: Option<&str>) -> String {
    let language = language.trim().to_ascii_lowercase();
    let normalized = match language.as_str() {
        "en" | "eng" | "english" => "eng",
        "ja" | "jpn" | "jp" | "japanese" => "jpn",
        "pt" | "por" | "portuguese" => "por",
        "fr" | "fra" | "fre" | "french" => "fra",
        "de" | "deu" | "ger" | "german" => "deu",
        "es" | "spa" | "spanish" => "spa",
        "it" | "ita" | "italian" => "ita",
        "ru" | "rus" | "russian" => "rus",
        "pl" | "pol" | "polish" => "pol",
        other if other.len() == 3 => other,
        _ => "eng",
    };

    if normalized == "por"
        && name
            .map(|value| value.to_ascii_lowercase().contains("brazil"))
            .unwrap_or(false)
    {
        "pob".to_string()
    } else {
        normalized.to_string()
    }
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
