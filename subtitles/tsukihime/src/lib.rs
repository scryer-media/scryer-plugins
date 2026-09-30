//! Tsukihime subtitles, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:subtitle/subtitle-provider@1.1.0`: two
//! exports carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`,
//! `process` exchanges a `PluginCommandRequest` for a
//! `PluginCommandResponse`, and `process` is an `async func` on this world
//! revision), plus two imports: the shared `scryer:host/services@1.0.0` door
//! that every non-archive family world uses for config, plugin state, HTTP,
//! and host-owned archive extraction, and the family-neutral typed
//! `scryer:runtime/host@1.0.0` surface reached through
//! `scryer_plugin_pdk::runtime`.
//!
//! ## What the migration changed
//!
//! The previous artifact was a plain `cdylib` with four exported
//! entry points (`scryer_describe`, `scryer_validate_config`,
//! `scryer_subtitle_search`, `scryer_subtitle_download`) whose host services
//! arrived through the core-module `scryer:host/v1` pointer ABI. A component
//! has no exported linear memory for a host to slice, so both halves move onto
//! the canonical ABI: the four entry points collapse into one `process` export
//! dispatching the SDK's [`PluginSubtitleCommand`], and host services cross as
//! postcard `list<u8>` values through [`scryer_plugin_pdk::host`].
//!
//! Provider behaviour is unchanged — the same endpoints, the same
//! plugin-owned rate-limit windows in host state, the same candidate and match
//! hints.
//!
//! ## XZ is the host's job now
//!
//! Tsukihime serves its subtitle attachments XZ-compressed. This plugin used
//! to carry its own `lzma-rs` decoder; it now asks the host to open the
//! container through [`scryer_plugin_pdk::host::archive_extract`], which
//! delegates to the installed archive extractor. That removes a second
//! decompressor from the fleet, and it means a Scryer with no archive
//! extractor installed answers in-band (`Unsupported`) rather than this plugin
//! silently shipping its own limits.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_pdk::sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_pdk::{
    HttpRequest, PluginArchiveExtractRequest, config,
    host::{HostCallError, archive_extract},
    http, var,
};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor,
    PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleCommunityEntry, SubtitleDescriptor, SubtitleMatchHint,
    SubtitleMatchHintKind, SubtitlePluginCandidate, SubtitlePluginDownloadRequest,
    SubtitlePluginDownloadResponse, SubtitlePluginSearchRequest, SubtitlePluginSearchResponse,
    SubtitlePluginValidateConfigRequest, SubtitlePluginValidateConfigResponse,
    SubtitleProviderMode, SubtitleQueryMediaKind, SubtitleValidateConfigStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:subtitle/subtitle-provider@1.1.0",
    // Three packages, three paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it. One canonical
    // copy of each, no `deps/` duplicates and no symlinks to keep in sync.
    path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/subtitle-v1.1.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_subtitle_component_main!(
    descriptor = build_descriptor,
    handler = handle_subtitle_command,
);

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
        // Tsukihime is a catalog provider: it serves subtitles that already
        // exist upstream and has no generator. The host reads
        // `SubtitleCapabilities::mode` and never routes a generate here, so
        // this arm answers in-band rather than trapping.
        PluginSubtitleCommand::Generate(_) => {
            PluginSubtitleCommandResult::Generate(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "Tsukihime is a catalog subtitle provider and cannot generate \
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
        // migrated off the Preview 1 transport, so every subtitle provider now
        // sees the operation whether or not it can serve one. Tsukihime cannot:
        // it has no audio decoder and advertises no `sync` capability. Same
        // in-band refusal as `Generate`, for the same reason.
        PluginSubtitleCommand::Sync(_) => {
            PluginSubtitleCommandResult::Sync(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "Tsukihime is a catalog subtitle provider and cannot align \
                                 subtitles"
                    .to_string(),
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
    let config = TsukihimeConfig::from_host();
    match get_json::<Value>(&config, "stats") {
        Ok(_) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
        Err(error) => validation_error_response(error),
    }
}

/// A rate-limited search is an empty result set, not a failure.
///
/// The plugin owns its own upstream windows, so hitting one means "nothing
/// more this minute" and the host keeps whatever other providers returned.
/// Every other failure is a real one. Both halves are the pre-migration
/// behaviour; only the channel changed, from a hard ABI fault to the SDK's
/// typed [`PluginResult`].
fn search(request: &SubtitlePluginSearchRequest) -> PluginResult<SubtitlePluginSearchResponse> {
    match subtitle_search_impl(request) {
        Ok(results) => PluginResult::Ok(SubtitlePluginSearchResponse { results }),
        Err(TsukihimeError::RateLimited(_)) => {
            PluginResult::Ok(SubtitlePluginSearchResponse::default())
        }
        Err(error) => PluginResult::Err(plugin_error(error)),
    }
}

fn download(
    request: &SubtitlePluginDownloadRequest,
) -> PluginResult<SubtitlePluginDownloadResponse> {
    let reference: TsukihimeDownloadRef = match serde_json::from_str(&request.provider_file_id) {
        Ok(reference) => reference,
        Err(error) => {
            return PluginResult::Err(plugin_error(TsukihimeError::Message(format!(
                "Tsukihime subtitle reference is not valid: {error}"
            ))));
        }
    };
    match subtitle_download_impl(&reference) {
        Ok(response) => PluginResult::Ok(response),
        Err(error) => PluginResult::Err(plugin_error(error)),
    }
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
    let config = TsukihimeConfig::from_host();
    let limit = config.limit_for_request(DEFAULT_MAX_RESULTS);
    let mut results = Vec::new();
    let mut detail_budget = config.max_detail_fetches;
    let mut fetched = HashSet::new();

    let mut fetch = |path: &str| get_json::<Value>(&config, path);
    let mut consume = |rung: TorrentRung| -> Result<bool, TsukihimeError> {
        for summary in rung.summaries {
            if !include_torrent_summary(&summary, &config, request) || !fetched.insert(summary.id) {
                continue;
            }
            if detail_budget == 0 {
                return Ok(true);
            }
            detail_budget -= 1;
            let detail =
                match get_json::<TorrentDetail>(&config, &format!("torrents/{}", summary.id)) {
                    Ok(detail) => detail,
                    Err(TsukihimeError::RateLimited(_)) => return Ok(true),
                    Err(error) => return Err(error),
                };
            append_detail_candidates(&mut results, &config, request, detail, rung.episode_page)?;
        }
        Ok(!results.is_empty() || detail_budget == 0)
    };
    run_search_ladder(request, limit, &mut fetch, &mut consume)?;

    Ok(results)
}

/// How many `search/torrents` queries one subtitle search may spend.
///
/// Title search has its own, much smaller upstream budget, so the ladder never
/// walks every alias.
const MAX_TITLE_QUERIES: usize = 3;

/// One rung of the search ladder: torrents worth a detail fetch.
struct TorrentRung {
    summaries: Vec<TorrentSummary>,
    /// The episode number the `animes/{id}/episodes/{n}` page was asked for,
    /// when that page is where the torrents came from. The page already
    /// filters by episode, so every torrent on it is evidence for that
    /// episode.
    episode_page: Option<i64>,
}

type Fetch<'a> = dyn FnMut(&str) -> Result<Value, TsukihimeError> + 'a;
type Consume<'a> = dyn FnMut(TorrentRung) -> Result<bool, TsukihimeError> + 'a;

/// Walk the torrent sources, cheapest and most precise first, until one
/// yields subtitles.
///
/// The ladder is: the resolved anime's episode page, then the anime's full
/// listing, then title search (community entry titles first). Tsukihime only
/// fills `episode_no` for some titles, so an empty episode page usually means
/// "unnumbered", not "absent"; and a title's own torrents often carry no
/// cached subtitles while a season pack filed under a sibling entry does. So
/// every non-empty rung is handed to `consume`, which fetches details and
/// answers whether the search is done (it found candidates or spent its
/// detail budget). The detail budget lives in `consume` and is shared by all
/// rungs, so falling back never costs more detail requests than one rung
/// could.
fn run_search_ladder(
    request: &SubtitlePluginSearchRequest,
    limit: usize,
    fetch: &mut Fetch<'_>,
    consume: &mut Consume<'_>,
) -> Result<(), TsukihimeError> {
    if let Some(resolved) = resolve_anime(request, fetch)? {
        let anime_id = resolved.anime.id;
        let episode = if resolved.via_community {
            community_episode(request)
        } else {
            requested_episode_for(request, Some(&resolved.anime))
        };
        if let Some(episode) = episode {
            let page = fetch_page(fetch, &format!("animes/{anime_id}/episodes/{episode}"))?;
            if !page.results.is_empty()
                && consume(TorrentRung {
                    summaries: page.results,
                    episode_page: Some(episode),
                })?
            {
                return Ok(());
            }
        }
        let page = fetch_page(fetch, &format!("animes/{anime_id}?limit={limit}&offset=0"))?;
        // The listing omits each torrent's anime; it is the resolved one.
        let summaries = select_for_episode(page.results, |_| {
            episode_target(request, Some(&resolved.anime))
        });
        if !summaries.is_empty()
            && consume(TorrentRung {
                summaries,
                episode_page: None,
            })?
        {
            return Ok(());
        }
    }

    for query in title_queries(request) {
        let page = fetch_page(
            fetch,
            &format!(
                "search/torrents?q={}&limit={limit}&offset=0",
                url_encode(&query)
            ),
        )?;
        let summaries = select_for_episode(page.results, |torrent| {
            episode_target(request, torrent.anime.as_ref())
        });
        if !summaries.is_empty()
            && consume(TorrentRung {
                summaries,
                episode_page: None,
            })?
        {
            return Ok(());
        }
    }
    Ok(())
}

/// A torrent list, where a 404 is simply an empty rung of the ladder.
fn fetch_page(fetch: &mut Fetch<'_>, path: &str) -> Result<TorrentPage, TsukihimeError> {
    match fetch_typed(fetch, path) {
        Err(TsukihimeError::NotFound) => Ok(TorrentPage {
            results: Vec::new(),
        }),
        other => other,
    }
}

fn fetch_typed<T: for<'de> Deserialize<'de>>(
    fetch: &mut Fetch<'_>,
    path: &str,
) -> Result<T, TsukihimeError> {
    serde_json::from_value(fetch(path)?)
        .map_err(|error| TsukihimeError::Message(format!("Tsukihime JSON parse error: {error}")))
}

/// Keep the torrents that can hold the requested episode, likeliest first.
///
/// A listing or title search can hold hundreds of torrents and only
/// `max_detail_fetches` of them are opened, so the choice decides what the
/// search sees. A torrent whose name (or Tsukihime number) names one other
/// episode cannot hold this one and is not worth a detail request. Torrents
/// that name this episode under any numbering lead; torrents whose episode is
/// unknown (batches, season packs, unnumbered titles) follow. The sort is
/// stable, so upstream order holds within each group. A request without an
/// episode keeps every torrent in upstream order.
fn select_for_episode(
    summaries: Vec<TorrentSummary>,
    target_for: impl Fn(&TorrentSummary) -> Option<EpisodeTarget>,
) -> Vec<TorrentSummary> {
    let mut ranked: Vec<(u8, TorrentSummary)> = summaries
        .into_iter()
        .filter_map(|torrent| {
            let Some(target) = target_for(&torrent) else {
                return Some((1, torrent));
            };
            let named = parse_file_episode(&torrent.name).or(torrent.episode_no.map(|episode| {
                ParsedEpisode {
                    season: None,
                    episode,
                }
            }));
            match named {
                Some(named) if target.may_be(named) => Some((0, torrent)),
                Some(_) => None,
                None => Some((1, torrent)),
            }
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, torrent)| torrent).collect()
}

struct ResolvedAnime {
    anime: Anime,
    /// Resolved from the community entry's own ids, so its episodes carry
    /// the community entry's numbering.
    via_community: bool,
}

/// Resolve the Tsukihime anime for a request.
///
/// Tsukihime's anime rows are AniDB/AniList/MAL entries, which is exactly
/// what a community entry names, so its ids are asked first. The request's
/// own external ids come after, as they did before community entries existed.
fn resolve_anime(
    request: &SubtitlePluginSearchRequest,
    fetch: &mut Fetch<'_>,
) -> Result<Option<ResolvedAnime>, TsukihimeError> {
    let mut lookups: Vec<(&str, String, bool)> = Vec::new();
    if let Some(community) = community_entry(request) {
        for (endpoint, id) in [
            ("anidb", community.anidb_id),
            ("anilist", community.anilist_id),
            ("mal", community.mal_id),
        ] {
            if let Some(id) = id.filter(|id| *id > 0) {
                lookups.push((endpoint, id.to_string(), true));
            }
        }
    }
    for (keys, endpoint) in [
        (["anidb_id", "anidb"].as_slice(), "anidb"),
        (["anilist_id", "anilist"].as_slice(), "anilist"),
        (["mal_id", "mal"].as_slice(), "mal"),
    ] {
        if let Some(id) = first_external_id(request, keys) {
            lookups.push((endpoint, id, false));
        }
    }

    for (endpoint, id, via_community) in lookups {
        let path = format!("animes/{endpoint}/{}", url_encode(&id));
        match fetch_typed::<Anime>(fetch, &path) {
            Ok(anime) => {
                return Ok(Some(ResolvedAnime {
                    anime,
                    via_community,
                }));
            }
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

/// The anime community entry the host resolved for an episode, if any.
///
/// A community entry is usually one cour numbered from its own episode 1,
/// which is not the TVDB season and episode the rest of the request carries.
fn community_entry(request: &SubtitlePluginSearchRequest) -> Option<&SubtitleCommunityEntry> {
    request
        .community_entry
        .as_ref()
        .filter(|_| request.media_kind == SubtitleQueryMediaKind::Episode)
}

fn community_episode(request: &SubtitlePluginSearchRequest) -> Option<i64> {
    community_entry(request)
        .map(|entry| i64::from(entry.episode))
        .filter(|episode| *episode > 0)
}

/// The episode number a torrent of `anime` would use for the request.
///
/// A torrent of the community entry's own anime numbers by that entry, so it
/// is asked for the community episode. A torrent of some other anime (a title
/// search can return the previous cour, say) and every request without a
/// community entry use the request's TVDB episode, then its absolute number.
/// An anime with no id to compare is taken to be the community entry.
fn requested_episode_for(
    request: &SubtitlePluginSearchRequest,
    anime: Option<&Anime>,
) -> Option<i64> {
    if request.media_kind != SubtitleQueryMediaKind::Episode {
        return None;
    }
    if let Some(community) = community_entry(request)
        && anime.is_none_or(|anime| anime_matches_community(anime, community))
        && let Some(episode) = community_episode(request)
    {
        return Some(episode);
    }
    request
        .episode
        .or(request.absolute_episode)
        .map(i64::from)
        .filter(|episode| *episode > 0)
}

fn anime_matches_community(anime: &Anime, community: &SubtitleCommunityEntry) -> bool {
    let pairs = [
        (anime.anidb, community.anidb_id),
        (anime.anilist, community.anilist_id),
        (anime.mal, community.mal_id),
    ];
    pairs.iter().all(|pair| match pair {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => true,
    })
}

/// How the requested episode can appear in the releases of one anime.
struct EpisodeTarget {
    /// The number a bare `Show - 12` in this anime's releases carries: the
    /// community episode for the community entry's own anime, the TVDB
    /// episode otherwise.
    episode: Option<i64>,
    /// `SxxEyy` pairs that name this episode: TVDB's own, and the community
    /// entry's season and episode for the community entry's anime.
    season_episodes: Vec<(i64, i64)>,
    /// Every number a bare episode could carry under some numbering (TVDB,
    /// absolute, community). Enough to keep a torrent in the running, never
    /// enough to claim the episode.
    plausible: Vec<i64>,
    absolute: Option<i64>,
}

impl EpisodeTarget {
    /// Whether a parsed name positively is the requested episode.
    fn is(&self, named: ParsedEpisode) -> bool {
        match named.season {
            Some(season) => self.season_episodes.contains(&(season, named.episode)),
            None => self.episode == Some(named.episode),
        }
    }

    /// Whether a parsed name could be the requested episode under any
    /// numbering a release group might use.
    fn may_be(&self, named: ParsedEpisode) -> bool {
        self.is(named) || (named.season.is_none() && self.plausible.contains(&named.episode))
    }
}

fn episode_target(
    request: &SubtitlePluginSearchRequest,
    anime: Option<&Anime>,
) -> Option<EpisodeTarget> {
    if request.media_kind != SubtitleQueryMediaKind::Episode {
        return None;
    }
    let positive = |value: Option<i32>| value.map(i64::from).filter(|value| *value > 0);
    let tvdb_episode = positive(request.episode);
    let absolute = positive(request.absolute_episode);
    let community = community_entry(request)
        .filter(|community| anime.is_none_or(|anime| anime_matches_community(anime, community)));

    let mut season_episodes = Vec::new();
    if let (Some(season), Some(episode)) = (positive(request.season), tvdb_episode) {
        season_episodes.push((season, episode));
    }
    if let Some(community) = community
        && let (Some(season), Some(episode)) = (
            positive(Some(community.season)),
            positive(Some(community.episode)),
        )
    {
        season_episodes.push((season, episode));
    }
    let plausible = [tvdb_episode, absolute, community_episode(request)]
        .into_iter()
        .flatten()
        .collect();
    let target = EpisodeTarget {
        episode: requested_episode_for(request, anime),
        season_episodes,
        plausible,
        absolute,
    };
    (target.episode.is_some() || target.absolute.is_some()).then_some(target)
}

/// Title-search queries, in the order they are tried.
///
/// The community entry's own titles name this cour exactly, so they lead;
/// the request's title candidates, title and aliases follow. Duplicates and
/// one-character queries are dropped, and at most [`MAX_TITLE_QUERIES`] are
/// kept.
fn title_queries(request: &SubtitlePluginSearchRequest) -> Vec<String> {
    let community_titles = community_entry(request)
        .map(|entry| entry.titles.as_slice())
        .unwrap_or_default();
    let mut queries: Vec<String> = Vec::new();
    for title in community_titles
        .iter()
        .chain(request.title_candidates.iter())
        .chain(std::iter::once(&request.title))
        .chain(request.title_aliases.iter())
    {
        let title = title.trim();
        if title.chars().count() < 2
            || queries
                .iter()
                .any(|query| query.eq_ignore_ascii_case(title))
        {
            continue;
        }
        queries.push(title.to_string());
        if queries.len() == MAX_TITLE_QUERIES {
            break;
        }
    }
    queries
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
    episode_page: Option<i64>,
) -> Result<(), TsukihimeError> {
    if !include_torrent_summary(&detail.summary, config, request) {
        return Ok(());
    }

    for file in &detail.files {
        let file_hints = detail_match_hints(request, &detail, episode_page, &file.filename);
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
            let mut match_hints = file_hints.clone();
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

/// Match hints for one file of a torrent.
///
/// The host reads a `SeasonEpisode` hint as a full season-and-episode match,
/// so one is emitted only on positive evidence that this file is the requested
/// episode: the torrent came off the anime's episode page, Tsukihime numbered
/// the torrent as that episode, or the file's own name carries it. A season
/// pack is judged file by file, and a file whose number cannot be read gets no
/// episode claim at all; the candidate is still returned and simply ranks
/// below the ones that prove their episode.
fn detail_match_hints(
    request: &SubtitlePluginSearchRequest,
    detail: &TorrentDetail,
    episode_page: Option<i64>,
    filename: &str,
) -> Vec<SubtitleMatchHint> {
    let mut hints = vec![SubtitleMatchHint {
        kind: SubtitleMatchHintKind::Title,
        value: None,
    }];

    if let Some(target) = episode_target(request, detail.summary.anime.as_ref()) {
        let named = parse_file_episode(filename);
        let vouched = episode_page.or_else(|| {
            target
                .episode
                .filter(|episode| detail.summary.episode_no == Some(*episode))
        });
        let matched = match (vouched, named) {
            // The file names this episode itself.
            (_, Some(named)) if target.is(named) => Some(named.episode),
            // A multi-file torrent vouches for the episode as a whole; a file
            // in it that names another episode is not that episode.
            (Some(_), Some(named)) if detail.files.len() > 1 && !target.may_be(named) => None,
            (Some(episode), _) => Some(episode),
            (None, _) => None,
        };
        if let Some(episode) = matched {
            hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::SeasonEpisode,
                value: Some(episode.to_string()),
            });
        }
        if let Some(absolute) = target.absolute
            && named.is_some_and(|named| named.season.is_none() && named.episode == absolute)
        {
            hints.push(SubtitleMatchHint {
                kind: SubtitleMatchHintKind::AbsoluteEpisode,
                value: Some(absolute.to_string()),
            });
        }
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

/// An episode number read out of a release or file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedEpisode {
    /// Present only for the `SxxEyy` shape.
    season: Option<i64>,
    episode: i64,
}

impl ParsedEpisode {
    fn bare(episode: i64) -> Self {
        Self {
            season: None,
            episode,
        }
    }
}

/// Read a single episode out of a release or file name.
///
/// Tolerant of the common anime naming shapes: `S01E12`, `E12` / `EP12` /
/// `Episode 12`, `Show - 12`, and a bare `Show 12 [1080p]` before a bracketed
/// tag. Folders and the extension are ignored. Anything that looks like a
/// range (`01-12`, `01 ~ 12`), a multi-episode file (`S01E01E02`), or a
/// `Part 2` / `Season 2` label is not an episode and yields `None`, as does a
/// name with no number at all.
fn parse_file_episode(filename: &str) -> Option<ParsedEpisode> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let stem = match base.rsplit_once('.') {
        Some((stem, extension))
            if (1..=4).contains(&extension.len())
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
                && extension.bytes().any(|byte| byte.is_ascii_alphabetic()) =>
        {
            stem
        }
        _ => base,
    };
    let name: Vec<u8> = stem
        .bytes()
        .map(|byte| match byte {
            b'_' => b' ',
            byte => byte.to_ascii_lowercase(),
        })
        .collect();

    season_episode_number(&name)
        .or_else(|| dash_episode_number(&name))
        .or_else(|| marked_episode_number(&name))
        .or_else(|| bare_episode_number(&name))
        .flatten()
}

/// The digits starting at `start`, as `(value, end)`, capped at four digits.
fn digits_at(name: &[u8], start: usize) -> Option<(i64, usize)> {
    let end = start
        + name[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    if end == start || end - start > 4 {
        return None;
    }
    let value = std::str::from_utf8(&name[start..end]).ok()?.parse().ok()?;
    Some((value, end))
}

fn boundary_before(name: &[u8], index: usize) -> bool {
    index == 0 || !name[index - 1].is_ascii_alphanumeric()
}

/// Whether the number that ended at `end` is a whole episode number: not a
/// range, a decimal, a resolution, or the start of a longer token. A `v2`
/// revision suffix is allowed.
fn episode_number_ends(name: &[u8], mut end: usize) -> bool {
    if name.get(end) == Some(&b'v') && name.get(end + 1).is_some_and(u8::is_ascii_digit) {
        end += 1;
        while name.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    match name.get(end) {
        None => return true,
        Some(byte) if byte.is_ascii_alphanumeric() => return false,
        // `12.5` is a recap, not episode 12; `S01E07.1080p` is still 7.
        Some(b'.') if name.get(end + 1).is_some_and(u8::is_ascii_digit) => {
            if name[end + 1..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count()
                < 3
            {
                return false;
            }
        }
        Some(_) => {}
    }
    let mut rest = name[end..].iter().copied().skip_while(|byte| *byte == b' ');
    match rest.next() {
        Some(b'-' | b'~' | b'&' | b'+' | b',') => {
            // `01-12`, `01 ~ 12`, `01 & 02`: a range or a pair, not one episode.
            let after = rest.find(|byte| *byte != b' ' && *byte != b'e');
            !after.is_some_and(|byte| byte.is_ascii_digit())
        }
        _ => true,
    }
}

/// Outer `Option`: whether the shape was found. Inner: the episode, or
/// `None` when the shape was found but names more than one episode.
fn season_episode_number(name: &[u8]) -> Option<Option<ParsedEpisode>> {
    for start in 0..name.len() {
        if name[start] != b's' || !boundary_before(name, start) {
            continue;
        }
        let Some((season, season_end)) = digits_at(name, start + 1) else {
            continue;
        };
        if name.get(season_end) != Some(&b'e') {
            continue;
        }
        let Some((episode, end)) = digits_at(name, season_end + 1) else {
            continue;
        };
        if name.get(end) == Some(&b'e') && name.get(end + 1).is_some_and(u8::is_ascii_digit) {
            return Some(None);
        }
        return Some(episode_number_ends(name, end).then_some(ParsedEpisode {
            season: Some(season),
            episode,
        }));
    }
    None
}

fn dash_episode_number(name: &[u8]) -> Option<Option<ParsedEpisode>> {
    for start in 0..name.len().saturating_sub(2) {
        if &name[start..start + 3] != b" - " {
            continue;
        }
        let Some((episode, end)) = digits_at(name, start + 3) else {
            continue;
        };
        if !episode_number_ends(name, end) {
            // `Show - 01-12` is a range; stop rather than read a later number.
            return Some(None);
        }
        return Some(Some(ParsedEpisode::bare(episode)));
    }
    None
}

/// `E12`, `EP 12`, `Episode 12`. A marker that does not end a whole number
/// (a CRC like `[E1CAB75D]`) is skipped, not taken as a verdict.
fn marked_episode_number(name: &[u8]) -> Option<Option<ParsedEpisode>> {
    for marker in [&b"episode"[..], b"ep", b"e"] {
        for start in 0..name.len() {
            if !name[start..].starts_with(marker) || !boundary_before(name, start) {
                continue;
            }
            let mut digits = start + marker.len();
            if marker.len() > 1 {
                while name.get(digits) == Some(&b' ') || name.get(digits) == Some(&b'.') {
                    digits += 1;
                }
            }
            let Some((episode, end)) = digits_at(name, digits) else {
                continue;
            };
            if episode_number_ends(name, end) {
                return Some(Some(ParsedEpisode::bare(episode)));
            }
        }
    }
    None
}

fn bare_episode_number(name: &[u8]) -> Option<Option<ParsedEpisode>> {
    const LABELS: [&str; 7] = ["part", "season", "cour", "vol", "volume", "s", "no"];
    for start in 1..name.len() {
        if name[start - 1] != b' ' || !name[start].is_ascii_digit() {
            continue;
        }
        let Some((episode, end)) = digits_at(name, start) else {
            continue;
        };
        // `Show 2024 [1080p]` names a year, not an episode.
        if (1900..=2099).contains(&episode) {
            continue;
        }
        let tagged = name[end..]
            .iter()
            .find(|byte| **byte != b' ')
            .is_some_and(|byte| matches!(byte, b'[' | b'('));
        if name.get(end) != Some(&b' ') || !tagged {
            continue;
        }
        let previous_word = name[..start - 1]
            .rsplit(|byte| *byte == b' ')
            .next()
            .unwrap_or_default();
        let previous_word = previous_word.strip_suffix(b".").unwrap_or(previous_word);
        if LABELS.iter().any(|label| previous_word == label.as_bytes()) {
            continue;
        }
        return Some(Some(ParsedEpisode::bare(episode)));
    }
    None
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
    let content = extract_xz_subtitle(reference, response.body)?;
    Ok(SubtitlePluginDownloadResponse {
        content_base64: BASE64.encode(content),
        format: reference.format.clone(),
        filename: Some(reference.filename.clone()),
        content_type: subtitle_content_type(&reference.format).map(str::to_string),
    })
}

/// Unwrap one XZ-compressed subtitle attachment through the host.
///
/// Tsukihime stores exactly one subtitle per `.xz` attachment, so the response
/// carries exactly one member; anything else means the upstream shape changed
/// and is reported rather than guessed at. The extractor's own expansion
/// limits and path safety apply, which is the point of routing this through
/// the host instead of decompressing here.
fn extract_xz_subtitle(
    reference: &TsukihimeDownloadRef,
    compressed: Vec<u8>,
) -> Result<Vec<u8>, TsukihimeError> {
    let extracted = archive_extract(PluginArchiveExtractRequest {
        content: compressed,
        format: "xz".to_string(),
        filename: Some(reference.filename.clone()),
        password: None,
    })
    .map_err(|error| match error {
        HostCallError::Service(error) => TsukihimeError::Plugin(error),
        error => TsukihimeError::Message(format!("Scryer host archive extraction failed: {error}")),
    })?;

    let mut files = extracted.files.into_iter();
    let Some(file) = files.next() else {
        return Err(TsukihimeError::Message(
            "Tsukihime subtitle archive contained no files".to_string(),
        ));
    };
    if files.next().is_some() {
        return Err(TsukihimeError::Message(
            "Tsukihime subtitle archive contained more than one file".to_string(),
        ));
    }
    Ok(file.content)
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

/// The MIME type for a subtitle format Tsukihime advertises.
///
/// The host's extraction service returns members, not media types, so the
/// provider keeps naming the content type from the reference it built during
/// search — exactly as it did before the migration.
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
        TsukihimeError::Plugin(_) => SubtitleValidateConfigStatus::Unsupported,
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
        TsukihimeError::Plugin(error) => return error,
    };
    PluginError {
        code,
        public_message: error.to_string(),
        debug_message: None,
        retry_after_seconds,
        details: None,
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
    headers: &BTreeMap<String, String>,
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

fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    header_value(headers, "Retry-After").and_then(|value| value.trim().parse::<i64>().ok())
}

fn retry_after_from_remaining_headers(headers: &BTreeMap<String, String>, now: u64) -> Option<i64> {
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

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
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
    Plugin(PluginError),
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
            Self::Plugin(error) => formatter.write_str(&error.public_message),
        }
    }
}

struct TsukihimeHttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
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
    fn from_host() -> Self {
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
        ..Default::default()
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
            community_entry: None,
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

        append_detail_candidates(&mut results, &config, &request, fixture_detail(), None)
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
    fn numbered_torrent_claims_its_episode_with_the_number() {
        let mut results = Vec::new();
        let config = test_config();
        append_detail_candidates(
            &mut results,
            &config,
            &episode_request(2, 12),
            fixture_detail(),
            None,
        )
        .expect("candidate mapping succeeds");

        assert_eq!(season_episode_values(&results[0].match_hints), vec!["12"]);
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
        let mut headers = BTreeMap::new();
        headers.insert("x-ratelimit-remaining".to_string(), "0".to_string());
        headers.insert("X-RateLimit-Reset".to_string(), "12".to_string());
        headers.insert("retry-after".to_string(), "9".to_string());

        assert_eq!(retry_after_seconds(&headers), Some(9));
        assert_eq!(retry_after_from_remaining_headers(&headers, 100), Some(12));
    }

    fn test_config() -> TsukihimeConfig {
        TsukihimeConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            max_results: DEFAULT_MAX_RESULTS,
            max_detail_fetches: DEFAULT_MAX_DETAIL_FETCHES,
            include_adult: false,
        }
    }

    fn episode_request(season: i32, episode: i32) -> SubtitlePluginSearchRequest {
        SubtitlePluginSearchRequest {
            media_kind: SubtitleQueryMediaKind::Episode,
            community_entry: None,
            facet: Some("anime".to_string()),
            file_hash: None,
            imdb_id: None,
            series_imdb_id: None,
            title: "Show".to_string(),
            title_aliases: vec![],
            title_candidates: vec![],
            year: None,
            season: Some(season),
            episode: Some(episode),
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
        }
    }

    /// TVDB files the episode as S01E12; the community splits the season in
    /// two, and the episode is the second entry's episode 1.
    fn second_cour_request() -> SubtitlePluginSearchRequest {
        let mut request = episode_request(1, 12);
        request
            .external_ids
            .insert("anilist".to_string(), vec!["111".to_string()]);
        request.community_entry = Some(SubtitleCommunityEntry {
            season: 2,
            episode: 1,
            anilist_id: Some(222),
            anidb_id: Some(333),
            mal_id: None,
            titles: vec!["Show Part 2".to_string()],
        });
        request
    }

    fn torrent_json(id: i64, name: &str, episode_no: Option<i64>, anilist: i64) -> Value {
        serde_json::json!({
            "id": id,
            "state": "completed",
            "name": name,
            "sublangs": ["en"],
            "episode_no": episode_no,
            "anime": {"id": 9, "anilist": anilist},
        })
    }

    fn page(torrents: Vec<Value>) -> Value {
        serde_json::json!({"total": torrents.len(), "results": torrents})
    }

    /// A fake Tsukihime API: known paths answer, everything else is a 404,
    /// and every path asked for is recorded in order. A rung counts as having
    /// produced subtitles when it holds one of the `yields` torrents.
    struct FakeApi {
        routes: HashMap<String, Value>,
        yields: HashSet<i64>,
        calls: Vec<String>,
        rungs: Vec<(Vec<i64>, Option<i64>)>,
    }

    impl FakeApi {
        fn new(routes: &[(&str, Value)], yields: &[i64]) -> Self {
            Self {
                routes: routes
                    .iter()
                    .map(|(path, value)| (path.to_string(), value.clone()))
                    .collect(),
                yields: yields.iter().copied().collect(),
                calls: Vec::new(),
                rungs: Vec::new(),
            }
        }

        fn run(&mut self, request: &SubtitlePluginSearchRequest) {
            let routes = &self.routes;
            let yields = &self.yields;
            let calls = &mut self.calls;
            let rungs = &mut self.rungs;
            let mut fetch = |path: &str| {
                calls.push(path.to_string());
                routes.get(path).cloned().ok_or(TsukihimeError::NotFound)
            };
            let mut consume = |rung: TorrentRung| {
                let ids: Vec<i64> = rung.summaries.iter().map(|torrent| torrent.id).collect();
                let done = ids.iter().any(|id| yields.contains(id));
                rungs.push((ids, rung.episode_page));
                Ok(done)
            };
            run_search_ladder(request, 50, &mut fetch, &mut consume).expect("ladder succeeds");
        }
    }

    fn season_episode_values(hints: &[SubtitleMatchHint]) -> Vec<String> {
        hints
            .iter()
            .filter(|hint| hint.kind == SubtitleMatchHintKind::SeasonEpisode)
            .map(|hint| hint.value.clone().unwrap_or_default())
            .collect()
    }

    fn community_anime() -> (&'static str, Value) {
        (
            "animes/anidb/333",
            serde_json::json!({"id": 9, "anidb": 333, "anilist": 222}),
        )
    }

    #[test]
    fn community_entry_ids_and_episode_win_over_tvdb_numbering() {
        let mut api = FakeApi::new(
            &[
                community_anime(),
                (
                    "animes/9/episodes/1",
                    page(vec![torrent_json(
                        1,
                        "[Group] Show Part 2 - 01",
                        Some(1),
                        222,
                    )]),
                ),
            ],
            &[1],
        );

        api.run(&second_cour_request());

        assert_eq!(api.calls, vec!["animes/anidb/333", "animes/9/episodes/1"]);
        assert_eq!(api.rungs, vec![(vec![1], Some(1))]);
    }

    #[test]
    fn unknown_community_ids_fall_back_to_external_ids_and_tvdb_episode() {
        let mut api = FakeApi::new(
            &[
                (
                    "animes/anilist/111",
                    serde_json::json!({"id": 5, "anilist": 111}),
                ),
                (
                    "animes/5/episodes/12",
                    page(vec![torrent_json(2, "[Group] Show - 12", Some(12), 111)]),
                ),
            ],
            &[2],
        );

        api.run(&second_cour_request());

        assert_eq!(
            api.calls,
            vec![
                "animes/anidb/333",
                "animes/anilist/222",
                "animes/anilist/111",
                "animes/5/episodes/12",
            ]
        );
        assert_eq!(api.rungs, vec![(vec![2], Some(12))]);
    }

    #[test]
    fn empty_episode_page_falls_back_to_the_listing_keeping_only_possible_torrents() {
        let mut api = FakeApi::new(
            &[
                community_anime(),
                ("animes/9/episodes/1", page(vec![])),
                (
                    "animes/9?limit=50&offset=0",
                    page(vec![
                        torrent_json(10, "[Group] Show Part 2 - 03 [1080p]", None, 222),
                        torrent_json(11, "[Group] Show Part 2 [Batch 1080p]", None, 222),
                        torrent_json(12, "[Group] Show Part 2 - 01 [1080p]", None, 222),
                        torrent_json(13, "[Group] Show - S01E12 [1080p]", None, 222),
                        torrent_json(14, "[Group] Show - 12 [1080p]", None, 222),
                        torrent_json(15, "[Group] Show - S01E13 [1080p]", None, 222),
                    ]),
                ),
            ],
            &[12],
        );

        api.run(&second_cour_request());

        assert_eq!(
            api.calls,
            vec![
                "animes/anidb/333",
                "animes/9/episodes/1",
                "animes/9?limit=50&offset=0",
            ]
        );
        assert_eq!(api.rungs, vec![(vec![12, 13, 14, 11], None)]);
    }

    #[test]
    fn listing_without_subtitles_falls_back_to_title_search_led_by_community_titles() {
        let mut api = FakeApi::new(
            &[
                community_anime(),
                ("animes/9/episodes/1", page(vec![])),
                (
                    "animes/9?limit=50&offset=0",
                    page(vec![torrent_json(
                        11,
                        "[Group] Show Part 2 [Batch]",
                        None,
                        222,
                    )]),
                ),
                (
                    "search/torrents?q=Show%20Part%202&limit=50&offset=0",
                    page(vec![torrent_json(
                        30,
                        "[Group] Show - S01E05 [1080p]",
                        None,
                        222,
                    )]),
                ),
                (
                    "search/torrents?q=Show&limit=50&offset=0",
                    page(vec![torrent_json(20, "[Group] Show [Season 1]", None, 444)]),
                ),
            ],
            &[20],
        );

        api.run(&second_cour_request());

        assert_eq!(
            api.calls,
            vec![
                "animes/anidb/333",
                "animes/9/episodes/1",
                "animes/9?limit=50&offset=0",
                "search/torrents?q=Show%20Part%202&limit=50&offset=0",
                "search/torrents?q=Show&limit=50&offset=0",
            ]
        );
        // The S01E05 single episode is never worth a detail request.
        assert_eq!(api.rungs, vec![(vec![11], None), (vec![20], None)]);
    }

    #[test]
    fn title_search_spends_at_most_three_queries() {
        let mut request = episode_request(1, 12);
        request.title_candidates = vec!["Alpha".to_string(), "alpha".to_string()];
        request.title_aliases = vec!["Beta".to_string(), "Gamma".to_string(), "Delta".to_string()];
        let mut api = FakeApi::new(&[], &[]);

        api.run(&request);

        assert!(api.rungs.is_empty());
        assert_eq!(
            api.calls,
            vec![
                "search/torrents?q=Alpha&limit=50&offset=0",
                "search/torrents?q=Show&limit=50&offset=0",
                "search/torrents?q=Beta&limit=50&offset=0",
            ]
        );
    }

    #[test]
    fn torrent_of_another_anime_uses_tvdb_numbering() {
        let request = second_cour_request();
        let community_anime = Anime {
            id: 9,
            anilist: Some(222),
            mal: None,
            anidb: None,
        };
        let other_anime = Anime {
            id: 4,
            anilist: Some(444),
            mal: None,
            anidb: None,
        };
        assert_eq!(
            requested_episode_for(&request, Some(&community_anime)),
            Some(1)
        );
        assert_eq!(requested_episode_for(&request, None), Some(1));
        assert_eq!(
            requested_episode_for(&request, Some(&other_anime)),
            Some(12)
        );
        assert_eq!(
            requested_episode_for(&episode_request(1, 12), None),
            Some(12)
        );
    }

    #[test]
    fn file_episode_parser_reads_common_shapes() {
        for (name, expected) in [
            ("Show - S01E12.mkv", Some(12)),
            ("[Group] Show S02E03 [1080p WEB].mkv", Some(3)),
            ("Show.S01E07.1080p.WEB.mkv", Some(7)),
            ("[Group] Show - 12 [1080p].mkv", Some(12)),
            ("[Group] Show - 12v2 [1080p][ABCD1234].mkv", Some(12)),
            ("[Group] Show - 05 - The Subtitle [1080p].mkv", Some(5)),
            ("[Group]_Show_-_08_[720p].mkv", Some(8)),
            ("Pack Folder/Show - 01.mkv", Some(1)),
            ("Show E12 [1080p].mkv", Some(12)),
            ("Show EP 04 (1080p).mkv", Some(4)),
            ("Show Episode 9.mkv", Some(9)),
            ("[Group] Show 12 [1080p].mkv", Some(12)),
            ("[Group] Show 2024 [1080p] [Batch].mkv", None),
            ("[Group] Show (2024) 07 [1080p].mkv", Some(7)),
            ("[Group] Show Part 2 [1080p].mkv", None),
            ("[Group] Show Season 2 [1080p].mkv", None),
            ("[Group] Show - 01-12 [Batch].mkv", None),
            ("[Group] Show - 01 ~ 12 [Batch]", None),
            ("Show - S01E01E02.mkv", None),
            ("[Group] Show [BD 1080p AV1 OPUS].mkv", None),
            ("[Group] Show - NCOP [1080p].mkv", None),
            ("Show - 12.5 [1080p].mkv", None),
            ("[Group] Show Vol. 2 [BD].mkv", None),
            ("[Group] Show - 03 [1080p][E1CAB75D].mkv", Some(3)),
            ("[Group] Show [E1CAB75D] E07.mkv", Some(7)),
            ("[Group] Show - S01E22v2 [1080p]", Some(22)),
        ] {
            assert_eq!(
                parse_file_episode(name).map(|parsed| parsed.episode),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn file_episode_parser_keeps_the_season_only_for_season_episode_names() {
        assert_eq!(
            parse_file_episode("Show - S02E01.mkv"),
            Some(ParsedEpisode {
                season: Some(2),
                episode: 1
            })
        );
        assert_eq!(
            parse_file_episode("Show - 01.mkv"),
            Some(ParsedEpisode::bare(1))
        );
    }

    fn season_pack_detail(files: &[&str], episode_no: Option<i64>) -> TorrentDetail {
        let files = files
            .iter()
            .enumerate()
            .map(|(index, filename)| {
                serde_json::json!({
                    "id": 100 + index as i64,
                    "filename": filename,
                    "attachments": [{
                        "id": 500 + index as i64,
                        "type": 1,
                        "info": {"codec": "ASS", "lang": "en", "cached": 1, "tracknum": 3},
                    }],
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "id": 42,
            "state": "completed",
            "name": "[Group] Show [Season 1] [BD 1080p]",
            "sublangs": ["en"],
            "episode_no": episode_no,
            "anime": {"id": 9, "anilist": 222},
            "files": files,
        }))
        .expect("season pack parses")
    }

    fn hints_by_file(
        request: &SubtitlePluginSearchRequest,
        detail: TorrentDetail,
        episode_page: Option<i64>,
    ) -> Vec<Vec<String>> {
        let mut results = Vec::new();
        append_detail_candidates(&mut results, &test_config(), request, detail, episode_page)
            .expect("candidate mapping succeeds");
        results
            .iter()
            .map(|candidate| season_episode_values(&candidate.match_hints))
            .collect()
    }

    #[test]
    fn season_pack_claims_only_the_file_that_names_the_episode() {
        let detail = season_pack_detail(
            &["Show - S01E01.mkv", "Show - S01E12.mkv", "Show - NCED.mkv"],
            None,
        );

        let hints = hints_by_file(&episode_request(1, 12), detail, None);

        assert_eq!(hints, vec![vec![], vec!["12".to_string()], vec![]]);
    }

    #[test]
    fn sibling_entry_season_pack_claims_the_tvdb_numbered_file() {
        // A pack filed under the previous cour's entry, numbered the TVDB way.
        let mut detail = season_pack_detail(&["Show - S01E01.mkv", "Show - S01E12.mkv"], None);
        detail.summary.anime = Some(Anime {
            id: 4,
            anilist: Some(444),
            mal: None,
            anidb: None,
        });

        let hints = hints_by_file(&second_cour_request(), detail, None);

        assert_eq!(hints, vec![vec![], vec!["12".to_string()]]);
    }

    #[test]
    fn community_season_episode_name_claims_the_episode() {
        let detail = season_pack_detail(&["Show - S02E01.mkv", "Show - S01E01.mkv"], None);

        let hints = hints_by_file(&second_cour_request(), detail, None);

        assert_eq!(hints, vec![vec!["1".to_string()], vec![]]);
    }

    #[test]
    fn community_request_claims_the_file_numbered_by_the_community_entry() {
        let detail = season_pack_detail(&["Show Part 2 - 01.mkv", "Show Part 2 - 12.mkv"], None);

        let hints = hints_by_file(&second_cour_request(), detail, None);

        assert_eq!(hints, vec![vec!["1".to_string()], vec![]]);
    }

    #[test]
    fn episode_page_torrent_claims_its_files_unless_a_file_names_another_episode() {
        let single = season_pack_detail(&["Show Part 2 [1080p].mkv"], None);
        assert_eq!(
            hints_by_file(&second_cour_request(), single, Some(1)),
            vec![vec!["1".to_string()]]
        );

        let multi = season_pack_detail(&["Show - 01.mkv", "Show - 02.mkv"], None);
        assert_eq!(
            hints_by_file(&second_cour_request(), multi, Some(1)),
            vec![vec!["1".to_string()], vec![]]
        );
    }

    #[test]
    fn absolute_episode_hint_follows_the_file_number() {
        let mut request = episode_request(2, 3);
        request.absolute_episode = Some(15);
        let detail = season_pack_detail(&["Show - 15.mkv", "Show - 03.mkv"], None);

        let mut results = Vec::new();
        append_detail_candidates(&mut results, &test_config(), &request, detail, None)
            .expect("candidate mapping succeeds");

        let absolute = |hints: &[SubtitleMatchHint]| {
            hints
                .iter()
                .filter(|hint| hint.kind == SubtitleMatchHintKind::AbsoluteEpisode)
                .filter_map(|hint| hint.value.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(absolute(&results[0].match_hints), vec!["15"]);
        assert!(season_episode_values(&results[0].match_hints).is_empty());
        assert!(absolute(&results[1].match_hints).is_empty());
        assert_eq!(season_episode_values(&results[1].match_hints), vec!["3"]);
    }
}
