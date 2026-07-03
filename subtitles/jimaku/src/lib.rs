#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginResult,
    ProviderDescriptor, SDK_VERSION, SubtitleCapabilities, SubtitleDescriptor, SubtitleMatchHint,
    SubtitleMatchHintKind, SubtitlePluginCandidate, SubtitlePluginDownloadRequest,
    SubtitlePluginDownloadResponse, SubtitlePluginSearchRequest, SubtitlePluginSearchResponse,
    SubtitlePluginValidateConfigRequest, SubtitlePluginValidateConfigResponse,
    SubtitleProviderMode, SubtitleQueryMediaKind, SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://jimaku.cc/api";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"));
const MIN_SUBTITLE_BYTES: usize = 500;
const DEFAULT_RATE_LIMIT_WAIT_SECONDS: u64 = 1;
const MAX_RATE_LIMIT_TOTAL_WAIT_SECONDS: u64 = 60;
const MAX_SEARCH_ENTRY_CANDIDATES: usize = 5;
const MAX_SEARCH_QUERIES: usize = 12;

#[derive(Clone)]
struct JimakuConfig {
    api_key: String,
    enable_name_search_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct JimakuEntry {
    id: i64,
    anilist_id: Option<i64>,
    #[serde(default)]
    flags: JimakuEntryFlags,
}

#[derive(Debug, Clone)]
struct JimakuMatchedEntry {
    entry: JimakuEntry,
    match_kind: JimakuEntryMatchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JimakuEntryMatchKind {
    ExternalId,
    NameSearch,
}

impl JimakuEntryMatchKind {
    fn trusts_title_and_episode(self) -> bool {
        matches!(self, Self::ExternalId)
    }

    fn outranks(self, other: Self) -> bool {
        matches!((self, other), (Self::ExternalId, Self::NameSearch))
    }
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
    Ok(serde_json::to_string(&PluginResult::Ok(
        SubtitlePluginSearchResponse { results },
    ))?)
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
        })
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "jimaku".to_string(),
        name: "Jimaku".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
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
        role: None,
        options: vec![],
        help_text: None,
    }
}

fn search_subtitles_impl(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    search_subtitles_impl_from_result(search_subtitles_inner(config, request))
}

fn search_subtitles_impl_from_result(
    result: Result<Vec<SubtitlePluginCandidate>, String>,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    match result {
        Err(error) if is_rate_limit_error(&error) => Ok(Vec::new()),
        result => result,
    }
}

fn search_subtitles_inner(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let entries = search_entries(config, request)?;
    let mut results = Vec::new();
    for matched_entry in entries.into_iter().take(MAX_SEARCH_ENTRY_CANDIDATES) {
        let mut entry_results = search_entry_subtitles(
            config,
            request,
            matched_entry.entry,
            matched_entry.match_kind,
        )?;
        results.append(&mut entry_results);
    }

    Ok(results)
}

fn search_entry_subtitles(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
    entry: JimakuEntry,
    match_kind: JimakuEntryMatchKind,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let files = if request.media_kind == SubtitleQueryMediaKind::Episode && !entry.flags.movie {
        if let Some(episode) = request.episode.or(request.absolute_episode) {
            let files = entry_files(config, entry.id, Some(episode))?;
            if files.is_empty() {
                if match_kind.trusts_title_and_episode() {
                    entry_files(config, entry.id, None)?
                } else {
                    files
                }
            } else {
                files
            }
        } else {
            entry_files(config, entry.id, None)?
        }
    } else {
        entry_files(config, entry.id, None)?
    };

    let mut results = Vec::new();
    for file in files {
        if !should_include_search_file(&file, request.include_ai_translated) {
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

        let match_hints = build_match_hints(request, &entry, match_kind, &language);

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

fn build_match_hints(
    request: &SubtitlePluginSearchRequest,
    entry: &JimakuEntry,
    match_kind: JimakuEntryMatchKind,
    language: &str,
) -> Vec<SubtitleMatchHint> {
    let mut match_hints = Vec::new();
    if match_kind.trusts_title_and_episode() {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::Title,
            value: None,
        });
        if request.media_kind == SubtitleQueryMediaKind::Episode && request.episode.is_some() {
            match_hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::SeasonEpisode,
                value: None,
            });
        }
    }
    match_hints.push(SubtitleMatchHint {
        kind: SubtitleMatchHintKind::Language,
        value: Some(language.to_string()),
    });
    if let Some(anilist_id) = entry.anilist_id {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::ExternalId,
            value: Some(format!("anilist:{anilist_id}")),
        });
    }
    match_hints
}

fn search_entries(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<JimakuMatchedEntry>, String> {
    let mut entries = Vec::new();
    let mut seen_ids = HashSet::<i64>::new();

    let season_number = request.season.unwrap_or(1);
    let should_prefer_season_name_search =
        request.media_kind == SubtitleQueryMediaKind::Episode && season_number > 1;

    if !should_prefer_season_name_search {
        append_anilist_entries(config, request, &mut entries, &mut seen_ids)?;
        if !entries.is_empty() {
            return Ok(entries);
        }
    }

    if should_attempt_name_search(config, request) {
        let queries = search_query_candidates(request);
        let anime_filter = search_query_anime_filter(request);
        for query in &queries {
            append_search_query_entries(config, query, anime_filter, &mut entries, &mut seen_ids)?;
            if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
                return Ok(entries);
            }
        }

        if entries.is_empty() && anime_filter.is_none() {
            for query in &queries {
                append_search_query_entries(
                    config,
                    query,
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

fn should_attempt_name_search(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> bool {
    config.enable_name_search_fallback || request.media_kind == SubtitleQueryMediaKind::Movie
}

fn append_anilist_entries(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
    entries: &mut Vec<JimakuMatchedEntry>,
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
        append_entries(
            jimaku_get_json(config, &path)?,
            JimakuEntryMatchKind::ExternalId,
            entries,
            seen_ids,
        );
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
    entries: &mut Vec<JimakuMatchedEntry>,
    seen_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    let path = match anime {
        Some(value) => format!(
            "entries/search?query={}&anime={value}",
            url_encode(query.trim())
        ),
        None => format!("entries/search?query={}", url_encode(query.trim())),
    };
    append_entries(
        jimaku_get_json(config, &path)?,
        JimakuEntryMatchKind::NameSearch,
        entries,
        seen_ids,
    );
    Ok(())
}

fn append_entries(
    found: Vec<JimakuEntry>,
    match_kind: JimakuEntryMatchKind,
    entries: &mut Vec<JimakuMatchedEntry>,
    seen_ids: &mut HashSet<i64>,
) {
    for entry in found {
        if seen_ids.insert(entry.id) {
            entries.push(JimakuMatchedEntry { entry, match_kind });
        } else if let Some(existing) = entries
            .iter_mut()
            .find(|matched| matched.entry.id == entry.id)
            .filter(|matched| match_kind.outranks(matched.match_kind))
        {
            existing.match_kind = match_kind;
        }
    }
}

fn search_query_anime_filter(request: &SubtitlePluginSearchRequest) -> Option<bool> {
    (request.facet.as_deref() == Some("anime")).then_some(true)
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

struct JimakuHttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl JimakuHttpResponse {
    fn from_extism(response: HttpResponse) -> Self {
        Self {
            status: response.status_code(),
            headers: response.headers().clone(),
            body: response.body(),
        }
    }

    fn status_code(&self) -> u16 {
        self.status
    }

    fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    fn body(&self) -> Vec<u8> {
        self.body.clone()
    }
}

fn http_get(url: &str, api_key: Option<&str>) -> Result<JimakuHttpResponse, String> {
    http_get_with(
        url,
        api_key,
        Duration::from_secs(MAX_RATE_LIMIT_TOTAL_WAIT_SECONDS),
        |request| {
            let response = http::request::<Vec<u8>>(request, None)
                .map_err(|error| format!("Jimaku request failed: {error}"))?;
            Ok(JimakuHttpResponse::from_extism(response))
        },
        std::thread::sleep,
    )
}

fn http_get_with<F, S>(
    url: &str,
    api_key: Option<&str>,
    max_rate_limit_wait: Duration,
    mut send: F,
    mut sleep: S,
) -> Result<JimakuHttpResponse, String>
where
    F: FnMut(&HttpRequest) -> Result<JimakuHttpResponse, String>,
    S: FnMut(Duration),
{
    let mut request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("Accept", "application/json")
        .with_header("User-Agent", USER_AGENT);
    if let Some(api_key) = api_key {
        request = request.with_header("Authorization", api_key);
    }

    let mut remaining_rate_limit_wait = max_rate_limit_wait;
    loop {
        let response = send(&request)?;
        if response.status_code() == 429 {
            if remaining_rate_limit_wait.is_zero() {
                return Ok(response);
            }
            let retry_after = Duration::from_secs(
                retry_after_seconds(&response)
                    .unwrap_or(DEFAULT_RATE_LIMIT_WAIT_SECONDS)
                    .max(1),
            );
            let wait_for = std::cmp::min(retry_after, remaining_rate_limit_wait);
            sleep(wait_for);
            remaining_rate_limit_wait = remaining_rate_limit_wait.saturating_sub(wait_for);
            continue;
        }
        return Ok(response);
    }
}

fn http_error(provider: &str, response: &JimakuHttpResponse) -> String {
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

fn is_rate_limit_error(error: &str) -> bool {
    error.contains("rate limited")
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

fn should_include_search_file(file: &JimakuFile, include_ai_translated: bool) -> bool {
    if file.size.unwrap_or(MIN_SUBTITLE_BYTES) < MIN_SUBTITLE_BYTES {
        return false;
    }
    if looks_like_ai_subtitle(&file.name) && !include_ai_translated {
        return false;
    }
    let archive = is_archive(&file.name);
    archive || is_subtitle_file(&file.name)
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
        || lower.contains(".jpn.")
        || lower.contains(".jpn[")
        || lower.contains("ja-jp")
        || lower.contains("[ja]")
        || lower.contains("[jp]")
        || lower.contains("[jpn]")
        || lower.contains("jpn]")
        || lower.contains("jpn,")
        || lower.contains("japanese")
        || lower.contains("jpsc")
    {
        "jpn".to_string()
    } else if lower.contains("[chs")
        || lower.contains("chs]")
        || lower.contains("chs,")
        || lower.contains("[cht")
        || lower.contains("cht]")
        || lower.contains("cht,")
        || lower.contains(".zh.")
        || lower.contains("[zh")
        || lower.contains("chinese")
    {
        "zho".to_string()
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

fn retry_after_seconds(response: &JimakuHttpResponse) -> Option<u64> {
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

    fn movie_request() -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Movie,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "Blue Carbon".to_string(),
            title_aliases: vec!["Aoi Carbon".to_string()],
            title_candidates: vec![],
            year: Some(2024),
            season: None,
            episode: None,
            absolute_episode: None,
            external_ids: BTreeMap::new(),
            languages: vec!["jpn".to_string()],
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

    fn http_response(
        status: u16,
        headers: impl IntoIterator<Item = (&'static str, &'static str)>,
        body: &'static str,
    ) -> JimakuHttpResponse {
        JimakuHttpResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn run_http_get_with_responses(
        responses: Vec<JimakuHttpResponse>,
        max_rate_limit_wait: Duration,
    ) -> (JimakuHttpResponse, usize, Vec<Duration>) {
        let mut responses = VecDeque::from(responses);
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let response = http_get_with(
            "https://jimaku.cc/api/entries/search?query=naruto",
            Some("token"),
            max_rate_limit_wait,
            |_request| {
                attempts += 1;
                responses
                    .pop_front()
                    .ok_or_else(|| "missing test response".to_string())
            },
            |duration| sleeps.push(duration),
        )
        .expect("http_get_with should return a response");
        (response, attempts, sleeps)
    }

    #[test]
    fn season_two_queries_include_season_qualified_aliases_before_bare_aliases() {
        let request = episode_request();

        let queries = search_query_candidates(&request);

        assert!(
            queries
                .iter()
                .any(|query| query == "kusuriya no hitorigoto 2")
        );
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

    #[test]
    fn jpn_marker_wins_over_requested_english() {
        let language = detect_language(
            "[VCB-Studio&Ylbud-Sub]Recently, my sister is unusual.[10][Hi10p_1080p][x264_flac][CHS, JPN].ass",
            &["eng".to_string()],
        );

        assert_eq!(language, "jpn");
    }

    #[test]
    fn chinese_marker_does_not_default_to_requested_english() {
        let language = detect_language(
            "[VCB-Studio&Ylbud-Sub]Recently, my sister is unusual.[10][Hi10p_1080p][x264_flac][CHS].ass",
            &["eng".to_string()],
        );

        assert_eq!(language, "zho");
    }

    #[test]
    fn name_search_entries_do_not_claim_title_or_episode_matches() {
        let request = episode_request();
        let entry = JimakuEntry {
            id: 42,
            anilist_id: Some(123),
            flags: JimakuEntryFlags::default(),
        };

        let hints = build_match_hints(&request, &entry, JimakuEntryMatchKind::NameSearch, "eng");

        assert!(!has_hint_kind(&hints, SubtitleMatchHintKind::Title));
        assert!(!has_hint_kind(&hints, SubtitleMatchHintKind::SeasonEpisode));
        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::Language));
        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::ExternalId));
    }

    #[test]
    fn external_id_entries_can_claim_title_and_episode_matches() {
        let request = episode_request();
        let entry = JimakuEntry {
            id: 42,
            anilist_id: Some(123),
            flags: JimakuEntryFlags::default(),
        };

        let hints = build_match_hints(&request, &entry, JimakuEntryMatchKind::ExternalId, "eng");

        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::Title));
        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::SeasonEpisode));
        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::Language));
        assert!(has_hint_kind(&hints, SubtitleMatchHintKind::ExternalId));
    }

    #[test]
    fn anime_name_search_uses_anime_filter() {
        let mut request = episode_request();
        assert_eq!(search_query_anime_filter(&request), Some(true));

        request.facet = None;
        assert_eq!(search_query_anime_filter(&request), None);
    }

    #[test]
    fn external_id_match_upgrades_name_search_entry() {
        let entry = JimakuEntry {
            id: 42,
            anilist_id: Some(123),
            flags: JimakuEntryFlags::default(),
        };
        let mut entries = Vec::new();
        let mut seen_ids = HashSet::new();

        append_entries(
            vec![entry.clone()],
            JimakuEntryMatchKind::NameSearch,
            &mut entries,
            &mut seen_ids,
        );
        append_entries(
            vec![entry],
            JimakuEntryMatchKind::ExternalId,
            &mut entries,
            &mut seen_ids,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].match_kind, JimakuEntryMatchKind::ExternalId);
    }

    fn has_hint_kind(hints: &[SubtitleMatchHint], kind: SubtitleMatchHintKind) -> bool {
        hints
            .iter()
            .any(|hint| std::mem::discriminant(&hint.kind) == std::mem::discriminant(&kind))
    }

    #[test]
    fn descriptor_only_exposes_public_jimaku_settings() {
        let ProviderDescriptor::Subtitle(descriptor) = descriptor().provider else {
            panic!("jimaku should be a subtitle provider");
        };

        let keys = descriptor
            .config_fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<Vec<_>>();

        assert_eq!(keys, vec!["api_key", "enable_name_search_fallback"]);
    }

    #[test]
    fn descriptor_defaults_name_search_fallback_to_enabled() {
        let ProviderDescriptor::Subtitle(descriptor) = descriptor().provider else {
            panic!("jimaku should be a subtitle provider");
        };

        let fallback = descriptor
            .config_fields
            .iter()
            .find(|field| field.key == "enable_name_search_fallback")
            .expect("enable_name_search_fallback field should exist");

        assert_eq!(fallback.default_value.as_deref(), Some("true"));
    }

    #[test]
    fn episodes_respect_name_search_fallback_setting() {
        let request = episode_request();

        assert!(!should_attempt_name_search(
            &JimakuConfig {
                api_key: "token".to_string(),
                enable_name_search_fallback: false,
            },
            &request,
        ));
        assert!(should_attempt_name_search(
            &JimakuConfig {
                api_key: "token".to_string(),
                enable_name_search_fallback: true,
            },
            &request,
        ));
    }

    #[test]
    fn movies_keep_name_search_even_when_fallback_is_disabled() {
        assert!(should_attempt_name_search(
            &JimakuConfig {
                api_key: "token".to_string(),
                enable_name_search_fallback: false,
            },
            &movie_request(),
        ));
    }

    #[test]
    fn http_get_retries_rate_limit_then_returns_success() {
        let (response, attempts, sleeps) = run_http_get_with_responses(
            vec![
                http_response(429, [("retry-after", "1")], ""),
                http_response(200, [], "[]"),
            ],
            Duration::from_secs(60),
        );

        assert_eq!(response.status_code(), 200);
        assert_eq!(attempts, 2);
        assert_eq!(sleeps, vec![Duration::from_secs(1)]);
    }

    #[test]
    fn http_get_retries_repeated_rate_limits_within_budget() {
        let (response, attempts, sleeps) = run_http_get_with_responses(
            vec![
                http_response(429, [("retry-after", "1")], ""),
                http_response(429, [("retry-after", "1")], ""),
                http_response(200, [], "[]"),
            ],
            Duration::from_secs(60),
        );

        assert_eq!(response.status_code(), 200);
        assert_eq!(attempts, 3);
        assert_eq!(sleeps, vec![Duration::from_secs(1), Duration::from_secs(1)]);
    }

    #[test]
    fn http_get_clamps_rate_limit_sleep_to_remaining_budget() {
        let (response, attempts, sleeps) = run_http_get_with_responses(
            vec![
                http_response(429, [("retry-after", "10")], ""),
                http_response(429, [("retry-after", "10")], ""),
            ],
            Duration::from_secs(3),
        );

        assert_eq!(response.status_code(), 429);
        assert_eq!(attempts, 2);
        assert_eq!(sleeps, vec![Duration::from_secs(3)]);
    }

    #[test]
    fn search_rate_limit_errors_return_empty_results() {
        let results =
            search_subtitles_impl_from_result(Err("Jimaku rate limited — retry after 1s".into()))
                .expect("rate limits should not fail subtitle search");

        assert!(results.is_empty());
    }

    #[test]
    fn search_non_rate_limit_errors_still_fail() {
        let error = search_subtitles_impl_from_result(Err("Jimaku returned HTTP 500: nope".into()))
            .expect_err("non-rate-limit errors should still fail");

        assert_eq!(error, "Jimaku returned HTTP 500: nope");
    }

    #[test]
    fn archive_files_are_always_search_candidates() {
        let file = JimakuFile {
            name: "Show.S01E01.eng.zip".to_string(),
            url: "https://jimaku.cc/file.zip".to_string(),
            size: Some(MIN_SUBTITLE_BYTES + 1),
        };

        assert!(should_include_search_file(&file, false));
    }

    #[test]
    fn ai_named_files_follow_request_flag() {
        let file = JimakuFile {
            name: "Show.S01E01.whisper.eng.srt".to_string(),
            url: "https://jimaku.cc/file.srt".to_string(),
            size: Some(MIN_SUBTITLE_BYTES + 1),
        };

        assert!(!should_include_search_file(&file, false));
        assert!(should_include_search_file(&file, true));
    }

    #[test]
    fn archive_detection_covers_all_supported_download_formats() {
        for suffix in [
            ".zip", ".rar", ".7z", ".tar", ".tar.gz", ".tgz", ".tar.zst", ".tzst", ".tar.xz",
            ".txz", ".gz", ".zst", ".xz",
        ] {
            let filename = format!("Show.S01E01{suffix}");
            assert!(
                is_archive(&filename),
                "{suffix} should be treated as an archive"
            );
            assert!(should_include_search_file(
                &JimakuFile {
                    name: filename,
                    url: "https://jimaku.cc/file".to_string(),
                    size: Some(MIN_SUBTITLE_BYTES + 1),
                },
                false,
            ));
        }
    }
}
