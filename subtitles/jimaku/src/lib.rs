//! Jimaku subtitles, as a WASI Preview 2 component.
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
//! Provider behaviour is unchanged — the same AniList-first entry resolution,
//! the same name-search fallback policy, the same bounded 429 wait budget, the
//! same language detection and match hints. What used to be a hard
//! `FnResult` hard failure is now the SDK's typed [`PluginResult::Err`]; a
//! rate-limited search still returns an empty result set rather than failing.
//!
//! ## No host archive extraction here
//!
//! Jimaku files may be archives (`is_archive`), and this provider has never
//! opened one: it forwards the bytes and lets Scryer's own extraction step deal
//! with the container. There is therefore nothing to route through
//! [`scryer_plugin_pdk::host::archive_extract`], and the plugin does not enable
//! the PDK's `archive-extract` feature.

#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_pdk::sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_pdk::{HttpRequest, HttpResponse, config, http};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION, SubtitleCapabilities,
    SubtitleCommunityEntry, SubtitleDescriptor, SubtitleMatchHint, SubtitleMatchHintKind,
    SubtitlePluginCandidate, SubtitlePluginDownloadRequest, SubtitlePluginDownloadResponse,
    SubtitlePluginSearchRequest, SubtitlePluginSearchResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:subtitle/subtitle-provider@1.1.0",
    // Three packages, three paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/subtitle-v1.1.0"],
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

#[derive(Debug, Clone, Default, Deserialize)]
struct JimakuEntry {
    id: i64,
    anilist_id: Option<i64>,
    /// Romaji name.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    english_name: Option<String>,
    #[serde(default)]
    japanese_name: Option<String>,
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
    /// The host's community entry for this episode; asked in its own numbering.
    CommunityEntry,
    ExternalId,
    NameSearch,
}

impl JimakuEntryMatchKind {
    fn trusts_title_and_episode(self) -> bool {
        matches!(self, Self::CommunityEntry | Self::ExternalId)
    }

    fn rank(self) -> u8 {
        match self {
            Self::CommunityEntry => 2,
            Self::ExternalId => 1,
            Self::NameSearch => 0,
        }
    }

    fn outranks(self, other: Self) -> bool {
        self.rank() > other.rank()
    }
}

impl JimakuMatchedEntry {
    /// The episode number to ask this entry for: the community entry's own
    /// number for the community entry, TVDB numbering (or the absolute number
    /// when TVDB has none) for any other entry.
    fn episode(&self, request: &SubtitlePluginSearchRequest) -> Option<i32> {
        match self.match_kind {
            JimakuEntryMatchKind::CommunityEntry => community_entry(request)
                .map(|entry| entry.episode)
                .or(request.episode.or(request.absolute_episode)),
            _ => request.episode.or(request.absolute_episode),
        }
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

/// One `process` invocation, dispatched by operation.
///
/// This is the whole of the world's request surface: `describe` is owned by
/// the PDK entry macro, and every operational failure is reported in-band
/// through [`PluginResult`], never as a world-level `invocation-error`.
async fn handle_subtitle_command(command: PluginSubtitleCommand) -> PluginSubtitleCommandResult {
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
        // Jimaku is a catalog provider: it serves subtitles that already exist
        // upstream and has no generator. The host reads
        // `SubtitleCapabilities::mode` and never routes a generate here, so
        // this arm answers in-band rather than trapping.
        PluginSubtitleCommand::Generate(_) => {
            PluginSubtitleCommandResult::Generate(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "Jimaku is a catalog subtitle provider and cannot generate \
                                 subtitles"
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
        // the operation whether or not it can serve one. Jimaku cannot: it has
        // no audio decoder and advertises no `sync` capability. Same in-band
        // refusal as `Generate`, for the same reason.
        PluginSubtitleCommand::Sync(_) => {
            PluginSubtitleCommandResult::Sync(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "Jimaku cannot align subtitles".to_string(),
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
    match JimakuConfig::from_host() {
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
    }
}

/// A rate-limited search is still an empty result set, not a failure —
/// `search_subtitles_impl` owns that rule and is unchanged. Every other
/// failure was a hard ABI failure and is now the SDK's typed
/// [`PluginResult::Err`]: same meaning, typed channel.
fn search(request: &SubtitlePluginSearchRequest) -> PluginResult<SubtitlePluginSearchResponse> {
    let config = match JimakuConfig::from_host() {
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
    let reference: JimakuDownloadRef = match serde_json::from_str(&request.provider_file_id) {
        Ok(reference) => reference,
        Err(error) => {
            return PluginResult::Err(plugin_error(format!(
                "Jimaku subtitle reference is not valid: {error}"
            )));
        }
    };
    match download_subtitle_impl(&reference) {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(plugin_error(error)),
    }
}

/// Map this provider's string failures onto the SDK's typed error.
///
/// The classification is the same one `validation_error_response` already
/// applied to the very same messages, so a search failure and a validation
/// failure now agree about what kind of problem they saw.
fn plugin_error(error: String) -> PluginError {
    let (code, retry_after_seconds) = if error.contains("authentication failed") {
        (PluginErrorCode::AuthFailed, None)
    } else if is_rate_limit_error(&error) {
        (
            PluginErrorCode::RateLimited,
            retry_after_from_message(&error),
        )
    } else if error.contains("required") || error.contains("missing") {
        (PluginErrorCode::InvalidConfig, None)
    } else if error.contains("request failed") {
        (PluginErrorCode::UpstreamUnavailable, None)
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

impl JimakuConfig {
    fn from_host() -> Result<Self, String> {
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
                sync: None,
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
        ..Default::default()
    }
}

fn search_subtitles_impl(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let mut api = HostJimakuApi { config };
    search_subtitles_impl_from_result(search_subtitles_inner(config, &mut api, request))
}

fn search_subtitles_impl_from_result(
    result: Result<Vec<SubtitlePluginCandidate>, String>,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    match result {
        Err(error) if is_rate_limit_error(&error) => Ok(Vec::new()),
        result => result,
    }
}

/// The Jimaku API as the search flow sees it: one authenticated GET per API
/// path, answering the response body.
trait JimakuApi {
    fn get(&mut self, path: &str) -> Result<Vec<u8>, String>;
}

struct HostJimakuApi<'a> {
    config: &'a JimakuConfig,
}

impl JimakuApi for HostJimakuApi<'_> {
    fn get(&mut self, path: &str) -> Result<Vec<u8>, String> {
        jimaku_get_bytes(self.config, path)
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(
    api: &mut dyn JimakuApi,
    path: &str,
) -> Result<T, String> {
    let body = api.get(path)?;
    serde_json::from_slice(&body).map_err(|error| format!("Jimaku JSON parse error: {error}"))
}

fn search_subtitles_inner(
    config: &JimakuConfig,
    api: &mut dyn JimakuApi,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let entries = search_entries(config, api, request)?;
    let mut results = Vec::new();
    for matched_entry in entries.into_iter().take(MAX_SEARCH_ENTRY_CANDIDATES) {
        let mut entry_results = search_entry_subtitles(api, request, &matched_entry)?;
        results.append(&mut entry_results);
    }

    Ok(results)
}

/// The anime community entry the host resolved for an episode, if any.
///
/// Jimaku entries are AniList entries, and an AniList entry is usually one
/// cour numbered from its own episode 1 — not the TVDB season Scryer files the
/// episode under. The host reads the title's numbering bridge and names both
/// the entry and the episode's number inside it.
fn community_entry(request: &SubtitlePluginSearchRequest) -> Option<&SubtitleCommunityEntry> {
    request
        .community_entry
        .as_ref()
        .filter(|_| request.media_kind == SubtitleQueryMediaKind::Episode)
}

fn search_entry_subtitles(
    api: &mut dyn JimakuApi,
    request: &SubtitlePluginSearchRequest,
    matched_entry: &JimakuMatchedEntry,
) -> Result<Vec<SubtitlePluginCandidate>, String> {
    let entry = &matched_entry.entry;
    let match_kind = matched_entry.match_kind;
    let episode = matched_entry.episode(request);
    // Whether the files came back filtered to the requested episode. Only
    // then may a trusted entry claim an episode match: its unfiltered fallback
    // is every episode's file.
    let mut episode_filtered = false;
    let files = if request.media_kind == SubtitleQueryMediaKind::Episode && !entry.flags.movie {
        if let Some(episode) = episode {
            let files = entry_files(api, entry.id, Some(episode))?;
            if !files.is_empty() {
                episode_filtered = true;
                files
            } else if match_kind.trusts_title_and_episode() {
                entry_files(api, entry.id, None)?
            } else {
                files
            }
        } else {
            entry_files(api, entry.id, None)?
        }
    } else {
        entry_files(api, entry.id, None)?
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
            episode,
        })
        .map_err(|error| format!("failed to encode Jimaku download ref: {error}"))?;

        let match_hints =
            build_match_hints(request, entry, match_kind, &language, episode_filtered);

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
    episode_filtered: bool,
) -> Vec<SubtitleMatchHint> {
    let mut match_hints = Vec::new();
    if match_kind.trusts_title_and_episode() {
        match_hints.push(SubtitleMatchHint {
            kind: SubtitleMatchHintKind::Title,
            value: None,
        });
        if request.media_kind == SubtitleQueryMediaKind::Episode && episode_filtered {
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
    api: &mut dyn JimakuApi,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<JimakuMatchedEntry>, String> {
    let mut entries = collect_entries(config, api, request)?;
    if let Some(community) = community_entry(request) {
        promote_community_entries(&mut entries, community);
    }
    Ok(entries)
}

fn collect_entries(
    config: &JimakuConfig,
    api: &mut dyn JimakuApi,
    request: &SubtitlePluginSearchRequest,
) -> Result<Vec<JimakuMatchedEntry>, String> {
    let mut entries = Vec::new();
    let mut seen_ids = HashSet::<i64>::new();

    // The community entry names the exact AniList entry this episode belongs
    // to, so it answers first, for every season.
    if let Some(anilist_id) = community_entry(request).and_then(|entry| entry.anilist_id) {
        append_entries(
            get_json(api, &anilist_search_path(&anilist_id.to_string()))?,
            JimakuEntryMatchKind::CommunityEntry,
            &mut entries,
            &mut seen_ids,
        );
        if !entries.is_empty() {
            return Ok(entries);
        }
    }

    let season_number = request.season.unwrap_or(1);
    let should_prefer_season_name_search =
        request.media_kind == SubtitleQueryMediaKind::Episode && season_number > 1;

    if !should_prefer_season_name_search {
        append_external_id_entries(api, request, &mut entries, &mut seen_ids)?;
        if !entries.is_empty() {
            return Ok(entries);
        }
    }

    if should_attempt_name_search(config, request) {
        let queries = search_query_candidates(request);
        let anime_filter = search_query_anime_filter(request);
        for query in &queries {
            append_search_query_entries(api, query, anime_filter, &mut entries, &mut seen_ids)?;
            if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
                return Ok(entries);
            }
        }

        if entries.is_empty() && anime_filter.is_none() {
            for query in &queries {
                append_search_query_entries(api, query, Some(false), &mut entries, &mut seen_ids)?;
                if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
                    return Ok(entries);
                }
            }
        }
    }

    if should_prefer_season_name_search {
        append_external_id_entries(api, request, &mut entries, &mut seen_ids)?;
    }

    Ok(entries)
}

/// Mark the entries that are the host's community entry, and rank them first.
///
/// An entry is the community entry when its AniList id is the community
/// entry's, or — only when either side has no AniList id to compare — when one
/// of its names is one of the community entry's titles. A mismatched AniList id
/// always wins over a matching name: a later cour's titles can carry the bare
/// series name, and that must not pass the first cour off as this one.
fn promote_community_entries(
    entries: &mut [JimakuMatchedEntry],
    community: &SubtitleCommunityEntry,
) {
    for matched in entries.iter_mut() {
        if is_community_entry(&matched.entry, community) {
            matched.match_kind = JimakuEntryMatchKind::CommunityEntry;
        }
    }
    entries.sort_by_key(|matched| std::cmp::Reverse(matched.match_kind.rank()));
}

fn is_community_entry(entry: &JimakuEntry, community: &SubtitleCommunityEntry) -> bool {
    if let (Some(entry_id), Some(community_id)) = (entry.anilist_id, community.anilist_id) {
        return entry_id == community_id;
    }
    let titles = community
        .titles
        .iter()
        .map(|title| title_key(title))
        .filter(|key| !key.is_empty())
        .collect::<HashSet<_>>();
    [&entry.name, &entry.english_name, &entry.japanese_name]
        .into_iter()
        .flatten()
        .any(|name| titles.contains(&title_key(name)))
}

/// Case-, space- and punctuation-blind form of a title, for exact comparison.
fn title_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn should_attempt_name_search(
    config: &JimakuConfig,
    request: &SubtitlePluginSearchRequest,
) -> bool {
    config.enable_name_search_fallback || request.media_kind == SubtitleQueryMediaKind::Movie
}

fn anilist_search_path(anilist_id: &str) -> String {
    format!("entries/search?anilist_id={}", url_encode(anilist_id))
}

/// Entries named by the request's own ids: its AniList ids, then — only when
/// none of those matched — its TMDB id.
fn append_external_id_entries(
    api: &mut dyn JimakuApi,
    request: &SubtitlePluginSearchRequest,
    entries: &mut Vec<JimakuMatchedEntry>,
    seen_ids: &mut HashSet<i64>,
) -> Result<(), String> {
    let found_before = entries.len();
    for id in request
        .external_ids
        .get("anilist")
        .into_iter()
        .flatten()
        .filter(|id| !id.trim().is_empty())
    {
        append_entries(
            get_json(api, &anilist_search_path(id.trim()))?,
            JimakuEntryMatchKind::ExternalId,
            entries,
            seen_ids,
        );
        if entries.len() >= MAX_SEARCH_ENTRY_CANDIDATES {
            return Ok(());
        }
    }
    if entries.len() == found_before
        && let Some(path) = tmdb_search_path(request)
    {
        append_entries(
            get_json(api, &path)?,
            JimakuEntryMatchKind::ExternalId,
            entries,
            seen_ids,
        );
    }
    Ok(())
}

/// Jimaku's `tmdb_id` lookup (`movie:<id>` / `tv:<id>`), for movies and for
/// non-anime series.
///
/// An anime series is left to its AniList ids: a TMDB show spans every cour,
/// while the Jimaku entry carrying that TMDB id is one cour, so trusting it
/// would pair a later season's episode number with the wrong cour's files.
fn tmdb_search_path(request: &SubtitlePluginSearchRequest) -> Option<String> {
    let kind = match request.media_kind {
        SubtitleQueryMediaKind::Movie => "movie",
        SubtitleQueryMediaKind::Episode if request.facet.as_deref() != Some("anime") => "tv",
        SubtitleQueryMediaKind::Episode => return None,
    };
    // The host lists the title's own TMDB id first.
    let id = request
        .external_ids
        .get("tmdb")?
        .iter()
        .map(|id| id.trim())
        .find(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))?;
    Some(format!(
        "entries/search?tmdb_id={}",
        url_encode(&format!("{kind}:{id}"))
    ))
}

fn append_search_query_entries(
    api: &mut dyn JimakuApi,
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
        get_json(api, &path)?,
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
    let mut queries = Vec::new();
    let mut seen_queries = HashSet::new();
    let mut seen_bases = HashSet::new();

    // The community entry's own titles name this cour exactly, so they go
    // first and bare: a season suffix would only blur them.
    for title in community_entry(request)
        .map(|entry| entry.titles.as_slice())
        .unwrap_or_default()
    {
        let normalized = normalize_query(title);
        if !normalized.is_empty() && seen_bases.insert(normalized.clone()) {
            push_query_candidate(&mut queries, &mut seen_queries, normalized);
        }
    }

    let mut bases = Vec::new();
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

    let season = request.season.filter(|season| *season > 1);
    for base in &bases {
        if queries.len() >= MAX_SEARCH_QUERIES {
            break;
        }
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
    api: &mut dyn JimakuApi,
    entry_id: i64,
    episode: Option<i32>,
) -> Result<Vec<JimakuFile>, String> {
    let path = match episode {
        Some(episode) => format!("entries/{entry_id}/files?episode={episode}"),
        None => format!("entries/{entry_id}/files"),
    };
    get_json(api, &path)
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
    get_json(&mut HostJimakuApi { config }, path)
}

fn jimaku_get_bytes(config: &JimakuConfig, path: &str) -> Result<Vec<u8>, String> {
    let url = format!(
        "{}/{}",
        API_BASE.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let response = http_get(&url, Some(config.api_key.as_str()))?;
    if response.status_code() >= 400 {
        return Err(http_error("Jimaku", &response));
    }
    Ok(response.body())
}

/// The provider's own owned copy of one response.
///
/// The header map is a `BTreeMap` now rather than a `HashMap`: that is the
/// shape [`scryer_plugin_pdk::HttpResponse`] hands back, and it is the only
/// type change the migration forced anywhere in this file.
struct JimakuHttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl JimakuHttpResponse {
    fn from_host(response: HttpResponse) -> Self {
        Self {
            status: response.status_code(),
            headers: response.headers().clone(),
            body: response.body(),
        }
    }

    fn status_code(&self) -> u16 {
        self.status
    }

    fn headers(&self) -> &BTreeMap<String, String> {
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
            Ok(JimakuHttpResponse::from_host(response))
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
            title: "Fixture Ledger".to_string(),
            title_aliases: vec!["Fikusucha no Daichou".to_string()],
            title_candidates: vec![],
            year: None,
            season: Some(2),
            episode: Some(23),
            absolute_episode: None,
            community_entry: None,
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
            title: "Fixture Harbor".to_string(),
            title_aliases: vec!["Fikusucha Minato".to_string()],
            title_candidates: vec![],
            year: Some(2024),
            season: None,
            episode: None,
            absolute_episode: None,
            community_entry: None,
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
            "https://jimaku.cc/api/entries/search?query=fixture",
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
                .any(|query| query == "fikusucha no daichou 2")
        );
        let qualified = queries
            .iter()
            .position(|query| query == "fikusucha no daichou 2")
            .expect("qualified alias query should exist");
        let bare = queries
            .iter()
            .position(|query| query == "fikusucha no daichou")
            .expect("bare alias query should exist");
        assert!(qualified < bare);
    }

    #[test]
    fn unmarked_jimaku_file_uses_requested_language() {
        let language = detect_language(
            "[FixtureRaws] Fikusucha no Daichou S2 - 23 (NTV 1920x1080 x265 AAC).srt",
            &["eng".to_string()],
        );

        assert_eq!(language, "eng");
    }

    #[test]
    fn explicit_japanese_marker_wins_over_requested_english() {
        let language = detect_language(
            "架空の帳簿.S01E23.WEBRip.Netflix.ja[cc].srt",
            &["eng".to_string()],
        );

        assert_eq!(language, "jpn");
    }

    #[test]
    fn jpn_marker_wins_over_requested_english() {
        let language = detect_language(
            "[FixtureStudio&Fixture-Sub]Synthetic Sibling Notes.[10][Hi10p_1080p][x264_flac][CHS, JPN].ass",
            &["eng".to_string()],
        );

        assert_eq!(language, "jpn");
    }

    #[test]
    fn chinese_marker_does_not_default_to_requested_english() {
        let language = detect_language(
            "[FixtureStudio&Fixture-Sub]Synthetic Sibling Notes.[10][Hi10p_1080p][x264_flac][CHS].ass",
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
            ..JimakuEntry::default()
        };

        let hints = build_match_hints(
            &request,
            &entry,
            JimakuEntryMatchKind::NameSearch,
            "eng",
            true,
        );

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
            ..JimakuEntry::default()
        };

        let hints = build_match_hints(
            &request,
            &entry,
            JimakuEntryMatchKind::ExternalId,
            "eng",
            true,
        );

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
            ..JimakuEntry::default()
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

    /// Scripted Jimaku API: answers each path from `routes` (`[]` otherwise)
    /// and records every path asked.
    struct ScriptedApi {
        routes: BTreeMap<String, String>,
        calls: Vec<String>,
    }

    impl ScriptedApi {
        fn new(routes: &[(&str, &str)]) -> Self {
            Self {
                routes: routes
                    .iter()
                    .map(|(path, body)| (path.to_string(), body.to_string()))
                    .collect(),
                calls: Vec::new(),
            }
        }
    }

    impl JimakuApi for ScriptedApi {
        fn get(&mut self, path: &str) -> Result<Vec<u8>, String> {
            self.calls.push(path.to_string());
            Ok(self
                .routes
                .get(path)
                .map_or_else(|| b"[]".to_vec(), |body| body.as_bytes().to_vec()))
        }
    }

    fn fallback_config() -> JimakuConfig {
        JimakuConfig {
            api_key: "token".to_string(),
            enable_name_search_fallback: true,
        }
    }

    const SUBTITLE_FILE_SIZE: usize = MIN_SUBTITLE_BYTES + 1;

    fn files_body(names: &[&str]) -> String {
        serde_json::to_string(
            &names
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "name": name,
                        "url": format!("https://jimaku.cc/entry/{name}"),
                        "size": SUBTITLE_FILE_SIZE,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    /// TVDB S01E13, which the community files as episode 1 of its second cour.
    fn second_cour_request() -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            season: Some(1),
            episode: Some(13),
            absolute_episode: Some(13),
            community_entry: Some(SubtitleCommunityEntry {
                season: 2,
                episode: 1,
                anilist_id: Some(9002),
                anidb_id: None,
                mal_id: None,
                titles: vec!["Fixture Ledger Part 2".to_string()],
            }),
            external_ids: BTreeMap::from([("anilist".to_string(), vec!["9002".to_string()])]),
            ..episode_request()
        }
    }

    #[test]
    fn community_entry_is_asked_for_its_own_episode_number() {
        let request = second_cour_request();
        let mut api = ScriptedApi::new(&[
            (
                "entries/search?anilist_id=9002",
                r#"[{"id":77,"anilist_id":9002,"name":"Fikusucha no Daichou Part 2"}]"#,
            ),
            (
                "entries/77/files?episode=1",
                &files_body(&["Fixture Ledger Part 2 - 01.en.srt"]),
            ),
        ]);

        let results = search_subtitles_inner(&fallback_config(), &mut api, &request).unwrap();

        assert_eq!(
            api.calls,
            vec![
                "entries/search?anilist_id=9002",
                "entries/77/files?episode=1"
            ]
        );
        assert_eq!(results.len(), 1);
        assert!(has_hint_kind(
            &results[0].match_hints,
            SubtitleMatchHintKind::SeasonEpisode
        ));
        let reference: JimakuDownloadRef =
            serde_json::from_str(&results[0].provider_file_id).unwrap();
        assert_eq!(reference.episode, Some(1));
    }

    #[test]
    fn community_entry_wins_over_season_two_name_search() {
        // TVDB S02E05 of a show whose second season is its own community entry.
        let mut request = second_cour_request();
        request.season = Some(2);
        request.episode = Some(5);
        request.community_entry.as_mut().unwrap().episode = 5;
        let mut api = ScriptedApi::new(&[
            (
                "entries/search?anilist_id=9002",
                r#"[{"id":77,"anilist_id":9002}]"#,
            ),
            (
                "entries/77/files?episode=5",
                &files_body(&["Part 2 - 05.en.srt"]),
            ),
        ]);

        let results = search_subtitles_inner(&fallback_config(), &mut api, &request).unwrap();

        assert_eq!(results.len(), 1);
        assert!(
            api.calls.iter().all(|path| !path.contains("query=")),
            "the community entry answered, so no name search should run: {:?}",
            api.calls
        );
    }

    #[test]
    fn trusted_entry_without_episode_file_claims_no_episode_match() {
        let request = second_cour_request();
        let mut api = ScriptedApi::new(&[
            (
                "entries/search?anilist_id=9002",
                r#"[{"id":77,"anilist_id":9002}]"#,
            ),
            (
                "entries/77/files",
                &files_body(&["Part 2 - 02.en.srt", "Part 2 - 03.en.srt"]),
            ),
        ]);

        let results = search_subtitles_inner(&fallback_config(), &mut api, &request).unwrap();

        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(has_hint_kind(
                &result.match_hints,
                SubtitleMatchHintKind::Title
            ));
            assert!(!has_hint_kind(
                &result.match_hints,
                SubtitleMatchHintKind::SeasonEpisode
            ));
        }
    }

    #[test]
    fn name_search_entry_matching_the_community_entry_is_promoted() {
        let community = second_cour_request().community_entry.unwrap();
        let mut entries = vec![
            JimakuMatchedEntry {
                entry: JimakuEntry {
                    id: 1,
                    anilist_id: Some(9001),
                    ..JimakuEntry::default()
                },
                match_kind: JimakuEntryMatchKind::NameSearch,
            },
            JimakuMatchedEntry {
                entry: JimakuEntry {
                    id: 2,
                    english_name: Some("Fixture Ledger: Part 2".to_string()),
                    ..JimakuEntry::default()
                },
                match_kind: JimakuEntryMatchKind::NameSearch,
            },
        ];

        promote_community_entries(&mut entries, &community);

        assert_eq!(entries[0].entry.id, 2);
        assert_eq!(entries[0].match_kind, JimakuEntryMatchKind::CommunityEntry);
        assert_eq!(entries[1].match_kind, JimakuEntryMatchKind::NameSearch);
    }

    #[test]
    fn mismatched_anilist_id_blocks_a_community_title_match() {
        let mut community = second_cour_request().community_entry.unwrap();
        community.titles.push("Fixture Ledger".to_string());
        let first_cour = JimakuEntry {
            id: 1,
            anilist_id: Some(9001),
            name: Some("Fixture Ledger".to_string()),
            ..JimakuEntry::default()
        };

        assert!(!is_community_entry(&first_cour, &community));
    }

    #[test]
    fn community_titles_lead_the_name_queries_unqualified() {
        let mut request = second_cour_request();
        request.season = Some(2);

        let queries = search_query_candidates(&request);

        assert_eq!(queries[0], "fixture ledger part 2");
        assert!(
            !queries
                .iter()
                .any(|query| query == "fixture ledger part 2 2")
        );
        assert!(queries.iter().any(|query| query == "fixture ledger 2"));
    }

    #[test]
    fn movie_request_is_matched_by_tmdb_id() {
        let mut request = movie_request();
        request.external_ids = BTreeMap::from([("tmdb".to_string(), vec!["4242".to_string()])]);
        let mut api = ScriptedApi::new(&[
            (
                "entries/search?tmdb_id=movie%3A4242",
                r#"[{"id":88,"flags":{"movie":true}}]"#,
            ),
            ("entries/88/files", &files_body(&["Fixture Harbor.ja.srt"])),
        ]);

        let results = search_subtitles_inner(&fallback_config(), &mut api, &request).unwrap();

        assert_eq!(results.len(), 1);
        assert!(has_hint_kind(
            &results[0].match_hints,
            SubtitleMatchHintKind::Title
        ));
        assert!(api.calls.iter().all(|path| !path.contains("query=")));
    }

    #[test]
    fn anime_series_never_uses_tmdb_lookup() {
        let mut request = episode_request();
        request.external_ids = BTreeMap::from([("tmdb".to_string(), vec!["4242".to_string()])]);
        assert_eq!(tmdb_search_path(&request), None);

        request.facet = Some("series".to_string());
        assert_eq!(
            tmdb_search_path(&request).as_deref(),
            Some("entries/search?tmdb_id=tv%3A4242")
        );
    }
}
