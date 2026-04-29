use std::collections::HashSet;
#[cfg(test)]
use std::collections::BTreeMap;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginResult,
    ProviderDescriptor, SDK_VERSION, SubtitleCapabilities, SubtitleDescriptor,
    SubtitleMatchHint, SubtitleMatchHintKind, SubtitlePluginCandidate,
    SubtitlePluginDownloadRequest, SubtitlePluginDownloadResponse, SubtitlePluginSearchRequest,
    SubtitlePluginSearchResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://jimaku.cc/api";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const MIN_SUBTITLE_BYTES: usize = 500;
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 5;
const MAX_SEARCH_ENTRY_CANDIDATES: usize = 5;
const MAX_SEARCH_QUERIES: usize = 12;

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
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
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
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(input: String) -> FnResult<String> {
    let request: SubtitlePluginSearchRequest = serde_json::from_str(&input)?;
    let config = JimakuConfig::from_extism().map_err(Error::msg)?;
    let results = search_subtitles_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(SubtitlePluginSearchResponse {
        results,
    }))?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(input: String) -> FnResult<String> {
    let request: SubtitlePluginDownloadRequest = serde_json::from_str(&input)?;
    let reference: JimakuDownloadRef =
        serde_json::from_str(&request.provider_file_id).map_err(Error::msg)?;
    let response = download_subtitle_impl(&reference).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
        id: "jimaku".to_string(),
        name: "Jimaku".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "jimaku".to_string(),
            provider_aliases: vec![],
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
            capabilities: SubtitleCapabilities {
            mode: SubtitleProviderMode::Catalog,
            supported_media_kinds: vec![
                SubtitleQueryMediaKind::Movie,
                SubtitleQueryMediaKind::Episode,
            ],
            recommended_facets: vec!["anime".to_string()],
            supports_hash_lookup: false,
            supports_forced: false,
            supports_hearing_impaired: false,
            supports_ai_translated: true,
            supports_machine_translated: false,
            supported_languages: vec!["jpn".to_string(), "eng".to_string()],
            },
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
    let mut results = Vec::new();
    for entry in entries.into_iter().take(MAX_SEARCH_ENTRY_CANDIDATES) {
        let mut entry_results = search_entry_subtitles(config, request, entry)?;
        results.append(&mut entry_results);
    }

    Ok(results)
}

fn search_entry_subtitles(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
    entry: JimakuEntry,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
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

        let language = detect_language(&file.name, &request.languages);
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

        let ai_translated = looks_like_ai_subtitle(&file.name);
        results.push(SubtitlePluginCandidate {
            provider_file_id,
            language,
            release_info: Some(file.name),
            hearing_impaired: false,
            forced: false,
            ai_translated,
            machine_translated: false,
            uploader: None,
            download_count: None,
            match_hints,
        });
    }

    Ok(results)
}

fn search_entries(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<JimakuEntry>, String> {
    let mut entries = Vec::new();
    let mut seen_ids = HashSet::<i64>::new();

    let season_number = request.season.unwrap_or(1);
    let should_prefer_season_name_search =
        request.media_kind == SubtitleQueryMediaKind::Episode && season_number > 1;

    if !should_prefer_season_name_search {
        append_anilist_entries(config, request, &mut entries, &mut seen_ids)?;
    }

    if config.enable_name_search_fallback || request.media_kind == SubtitleQueryMediaKind::Movie {
        let queries = search_query_candidates(request);
        for query in &queries {
            append_search_query_entries(config, &query, None, &mut entries, &mut seen_ids)?;
            if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
                return Ok(entries);
            }
        }

        if entries.is_empty() {
            for query in &queries {
                append_search_query_entries(
                    config,
                    &query,
                    Some(false),
                    &mut entries,
                    &mut seen_ids,
                )?;
                if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
                    return Ok(entries);
                }
            }
        }
    }

    if should_prefer_season_name_search {
        append_anilist_entries(config, request, &mut entries, &mut seen_ids)?;
    }

    Ok(entries)
}

fn append_anilist_entries(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
    entries: &mut Vec<JimakuEntry>,
    seen_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    for id in request
        .external_ids
        .get("anilist")
        .into_iter()
        .flatten()
        .filter(|id| !id.trim().is_empty())
    {
        let path = format!("entries/search?anilist_id={}", url_encode(id.trim()));
        append_entries(jimaku_get_json(config, &path)?, entries, seen_ids);
        if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
            break;
        }
    }
    Ok(())
}

fn append_search_query_entries(
    config: &JimakuConfig,
    query: &str,
    anime: Option<bool>,
    entries: &mut Vec<JimakuEntry>,
    seen_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    let path = match anime {
        Some(value) => format!(
            "entries/search?query={}&anime={value}",
            url_encode(query.trim())
        ),
        None => format!("entries/search?query={}", url_encode(query.trim())),
    };
    append_entries(jimaku_get_json(config, &path)?, entries, seen_ids);
    Ok(())
}

fn append_entries(
    found: Vec<JimakuEntry>,
    entries: &mut Vec<JimakuEntry>,
    seen_ids: &mut HashSet<i64>,
) {
    for entry in found {
        if seen_ids.insert(entry.id) {
            entries.push(entry);
        }
    }
}

fn search_query_candidates(request: &SubtitlePluginSearchRequest) -> Vec<String> {
    let mut bases = Vec::new();
    let mut seen_bases = HashSet::new();
    for candidate in request
        .title_candidates
        .iter()
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
    {
        let normalized = normalize_query(candidate);
        if !normalized.is_empty() && seen_bases.insert(normalized.clone()) {
            bases.push(normalized);
        }
    }

    let mut queries = Vec::new();
    let mut seen_queries = HashSet::new();
    let season = request.season.filter(|season| *season > 1);
    for base in &bases {
        if let Some(season) = season {
            push_query_candidate(&mut queries, &mut seen_queries, format!("{base} {season}"));
            push_query_candidate(
                &mut queries,
                &mut seen_queries,
                format!("{base} season {season}"),
            );
            push_query_candidate(&mut queries, &mut seen_queries, format!("{base} s{season}"));
        }
        push_query_candidate(&mut queries, &mut seen_queries, base.clone());
        if queries.len() >= MAX_SEARCH_QUERIES {
            break;
        }
    }

    queries
}

fn push_query_candidate(queries: &mut Vec<String>, seen: &mut HashSet<String>, query: String) {
    if queries.len() >= MAX_SEARCH_QUERIES {
        return;
    }
    let query = normalize_query(&query);
    if !query.is_empty() && seen.insert(query.clone()) {
        queries.push(query);
    }
}

fn normalize_query(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase()
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

fn detect_language(filename: &str, requested: &[String]) -> String {
    let lower = filename.to_ascii_lowercase();
    if lower.contains(".en.")
        || lower.contains("[en]")
        || lower.contains(".eng.")
        || lower.contains("[eng]")
        || lower.contains("english")
    {
        "eng".to_string()
    } else if lower.contains(".ja.")
        || lower.contains(".ja[")
        || lower.contains(".jp.")
        || lower.contains(".jp[")
        || lower.contains("ja-jp")
        || lower.contains("[ja]")
        || lower.contains("[jp]")
        || lower.contains("japanese")
        || lower.contains("jpsc")
    {
        "jpn".to_string()
    } else if let Some(language) = requested_single_language(requested) {
        language
    } else {
        "eng".to_string()
    }
}

fn requested_language_matches(requested: &[String], language: &str) -> bool {
    requested.is_empty()
        || requested
            .iter()
            .any(|candidate| normalize_lang(candidate) == normalize_lang(language))
}

fn requested_single_language(requested: &[String]) -> Option<String> {
    let mut normalized = requested
        .iter()
        .map(|language| normalize_lang(language).to_string())
        .filter(|language| !language.trim().is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    if normalized.len() == 1 {
        normalized.pop()
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn episode_request() -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "The Apothecary Diaries".to_string(),
            title_aliases: vec!["Kusuriya no Hitorigoto".to_string()],
            title_candidates: vec![],
            year: None,
            season: Some(2),
            episode: Some(23),
            absolute_episode: None,
            external_ids: BTreeMap::new(),
            languages: vec!["eng".to_string()],
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

    #[test]
    fn season_two_queries_include_season_qualified_aliases_before_bare_aliases() {
        let request = episode_request();

        let queries = search_query_candidates(&request);

        assert!(queries
            .iter()
            .any(|query| query == "kusuriya no hitorigoto 2"));
        let qualified = queries
            .iter()
            .position(|query| query == "kusuriya no hitorigoto 2")
            .expect("qualified alias query should exist");
        let bare = queries
            .iter()
            .position(|query| query == "kusuriya no hitorigoto")
            .expect("bare alias query should exist");
        assert!(qualified < bare);
    }

    #[test]
    fn unmarked_jimaku_file_uses_requested_language() {
        let language = detect_language(
            "[NanakoRaws] Kusuriya no Hitorigoto S2 - 23 (NTV 1920x1080 x265 AAC).srt",
            &["eng".to_string()],
        );

        assert_eq!(language, "eng");
    }

    #[test]
    fn explicit_japanese_marker_wins_over_requested_english() {
        let language = detect_language(
            "薬屋のひとりごと.S01E23.WEBRip.Netflix.ja[cc].srt",
            &["eng".to_string()],
        );

        assert_eq!(language, "jpn");
    }
}
