//! FileList (`filelist.io`) indexer.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Indexers/FileList` (request
//! generator, parser, settings, fixture) and against FileList's own published
//! `api.php` documentation. Where the two disagree the documentation wins and
//! the divergence is called out in the plugin README.
//!
//! Shape of the integration:
//!
//! * one JSON endpoint, `GET {base_url}/api.php`, authenticated with the
//!   account `username` + `passkey`;
//! * `action=latest-torrents` for the recent/RSS poll and
//!   `action=search-torrents` for everything else;
//! * requests are organised as **tiers** (Sonarr's `IndexerPageableRequestChain`):
//!   the IMDb tier runs first and the name tier only runs when the tier before
//!   it produced nothing. FileList bills a documented 150 calls per hour per
//!   account, so a search must never fan out where a fall-through will do.

use std::collections::{BTreeMap, HashMap};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::component::{self, StartRateGate, structured_plugin_error};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    IndexerCapabilities as Capabilities, IndexerCategoryDescriptor, IndexerCategoryModel,
    IndexerCategoryValueKind, IndexerDescriptor, IndexerFeedMode, IndexerLimitCapabilities,
    IndexerProtocol, IndexerResponseFeatures, IndexerSearchIncompleteReason, IndexerSearchInput,
    IndexerSearchInvalidResponseKind, IndexerSearchPluginError, IndexerSourceKind,
    IndexerTorrentCapabilities, PluginDescriptor, PluginError, PluginErrorCode, PluginErrorDetails,
    PluginSearchRequest as SearchRequest, PluginSearchRequestKind,
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult, ProviderDescriptor,
    SDK_VERSION, derive_indexer_flags, torrent_result,
};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://filelist.io";
/// Sonarr's `FileListSettings` default: TV SD, TV HD, TV 4K.
const DEFAULT_CATEGORIES: &str = "23,21,27";
/// `latest-torrents` accepts `limit` in 1..=100 and `search-torrents` never
/// returns more than one page, so 100 is the true ceiling.
const MAX_PAGE_SIZE: usize = 100;
/// FileList documents a hard budget of 150 API calls per hour per account.
const HOURLY_API_BUDGET: u32 = 150;
/// FileList's 429 carries no `Retry-After`, and the budget it protects is
/// hourly, so an hour is the honest fallback window (it is also Sonarr's
/// `minimumBackoff` in `HttpIndexerBase.FetchReleases`).
const RATE_LIMITED_FALLBACK_SECONDS: i64 = 3_600;
/// Sonarr's `HttpIndexerBase.RateLimit` for every indexer.
const REQUEST_INTERVAL_MS: u64 = 2_000;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("scryer-filelist-indexer/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Category table
// ---------------------------------------------------------------------------

/// FileList's published category ids. Labels pair the English name Sonarr and
/// Radarr show with FileList's own Romanian name so an operator can match the
/// option against the site.
const CATEGORY_LABELS: &[(i64, &str)] = &[
    (1, "Movies SD (Filme SD)"),
    (2, "Movies DVD (Filme DVD)"),
    (3, "Movies DVD-RO (Filme DVD-RO)"),
    (4, "Movies HD (Filme HD)"),
    (5, "FLAC"),
    (6, "Movies 4K (Filme 4K)"),
    (7, "XXX"),
    (8, "Applications (Programe)"),
    (9, "Games PC (Jocuri PC)"),
    (10, "Games Console (Jocuri Console)"),
    (11, "Audio"),
    (12, "Music Video (Videoclip)"),
    (13, "Sport"),
    (14, "TV"),
    (15, "Animation (Desene)"),
    (16, "Documentaries (Docs)"),
    (17, "Linux"),
    (18, "Misc (Diverse)"),
    (19, "Movies HD-RO (Filme HD-RO)"),
    (20, "Movies Blu-Ray (Filme Blu-Ray)"),
    (21, "TV HD (Seriale HD)"),
    (22, "Mobile"),
    (23, "TV SD (Seriale SD)"),
    (24, "Anime"),
    (25, "Movies 3D (Filme 3D)"),
    (26, "Movies 4K Blu-Ray (Filme 4K Blu-Ray)"),
    (27, "TV 4K (Seriale 4K)"),
    (28, "RO Dubbed"),
];

/// The set Sonarr's `FileListCategories` offers, in Sonarr's order.
const SERIES_CATEGORY_IDS: &[i64] = &[24, 15, 27, 21, 23, 13, 28];
/// The set Radarr's `FileListCategories` offers, in Radarr's order.
const MOVIE_CATEGORY_IDS: &[i64] = &[24, 15, 1, 2, 3, 4, 19, 6, 20, 26, 25, 28, 7];

fn category_label(id: i64) -> &'static str {
    CATEGORY_LABELS
        .iter()
        .find(|(value, _)| *value == id)
        .map(|(_, label)| *label)
        .unwrap_or("Unknown")
}

fn category_options(ids: &[i64]) -> Vec<ConfigFieldOption> {
    ids.iter()
        .map(|id| ConfigFieldOption {
            value: id.to_string(),
            label: category_label(*id).to_string(),
            config_overrides: Default::default(),
        })
        .collect()
}

fn category_descriptors() -> Vec<IndexerCategoryDescriptor> {
    let mut descriptors: Vec<IndexerCategoryDescriptor> = Vec::new();
    for (id, facet) in SERIES_CATEGORY_IDS
        .iter()
        .map(|id| (*id, "series"))
        .chain(MOVIE_CATEGORY_IDS.iter().map(|id| (*id, "movie")))
    {
        if let Some(existing) = descriptors
            .iter_mut()
            .find(|descriptor| descriptor.value == id.to_string())
        {
            if !existing.facets.iter().any(|value| value == facet) {
                existing.facets.push(facet.to_string());
            }
            continue;
        }
        let mut facets = vec![facet.to_string()];
        // FileList files anime under 24/15 and Sonarr searches those with the
        // anime criteria, so both carry the anime facet as well.
        if id == 24 || id == 15 {
            facets.push("anime".to_string());
        }
        descriptors.push(IndexerCategoryDescriptor {
            value: id.to_string(),
            label: Some(category_label(id).to_string()),
            value_kind: IndexerCategoryValueKind::Numeric,
            facets,
        });
    }
    descriptors
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "filelist".to_string(),
        name: "FileList Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "filelist".to_string(),
            provider_aliases: vec!["filelist.io".to_string()],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            strategy_plan: Some(scryer_plugin_sdk::IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 4,
            }),
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                // FileList's only id search is `type=imdb`; Sonarr searches
                // series and anime with it and Radarr searches movies with it.
                supported_ids: HashMap::from([
                    ("series".to_string(), vec!["imdb_id".to_string()]),
                    ("anime".to_string(), vec!["imdb_id".to_string()]),
                    ("movie".to_string(), vec!["imdb_id".to_string()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                query_param: Some("query".to_string()),
                supported_query_facets: vec![
                    "series".to_string(),
                    "anime".to_string(),
                    "movie".to_string(),
                ],
                search: true,
                imdb_search: true,
                tvdb_search: false,
                anidb_search: false,
                rss: true,
                protocols: vec![IndexerProtocol::Torrent],
                feed_modes: vec![
                    IndexerFeedMode::Recent,
                    IndexerFeedMode::Rss,
                    IndexerFeedMode::AutomaticSearch,
                    IndexerFeedMode::InteractiveSearch,
                ],
                search_inputs: vec![
                    IndexerSearchInput::TextQuery,
                    IndexerSearchInput::TitleQuery,
                    IndexerSearchInput::IdQuery,
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::AbsoluteEpisode,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["imdb_id".to_string()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    categories: category_descriptors(),
                }),
                limits: Some(IndexerLimitCapabilities {
                    // `latest-torrents` caps `limit` at 100 and
                    // `search-torrents` is a single unpaged response.
                    page_size: Some(MAX_PAGE_SIZE as u32),
                    max_page_size: Some(MAX_PAGE_SIZE as u32),
                    max_pages: Some(1),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: false,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_leechers: true,
                    // The API returns neither an info hash nor a magnet URI.
                    reports_info_hash: false,
                    reports_magnet_uri: false,
                    // `freeleech`/`doubleup` are reported per torrent.
                    reports_volume_factors: true,
                    supports_private_tracker_flags: true,
                    // FileList publishes no per-torrent seed ratio or seed time
                    // requirement, so the plugin never fills those fields.
                    supports_seed_requirements: false,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: false,
                    subtitles: false,
                    grabs: true,
                    votes: false,
                    // The API reports a comment COUNT, never a comment URL —
                    // Sonarr's fixture asserts `CommentUrl` is null.
                    comments: false,
                    info_url: true,
                    guid: true,
                    raw_provider_metadata: true,
                    password_hint: false,
                    protection_hint: false,
                }),
            },
            scoring_policies: vec![],
            config_fields: config_fields(),
            allowed_hosts: vec![],
            rate_limit_seconds: Some(2),
        }),
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            None,
            Some("FileList account username"),
        ),
        field(
            "passkey",
            "Passkey",
            ConfigFieldType::Password,
            true,
            None,
            Some("FileList account passkey (Profile -> Security)"),
        ),
        connection_field(
            "base_url",
            "API URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("FileList site URL"),
        ),
        tag_field(
            "categories",
            "Categories",
            SERIES_CATEGORY_IDS,
            Some(DEFAULT_CATEGORIES),
            true,
            "FileList category IDs searched for series. Comma-separated IDs are \
             still accepted.",
        ),
        tag_field(
            "anime_categories",
            "Anime Categories",
            SERIES_CATEGORY_IDS,
            None,
            false,
            "FileList category IDs searched for anime. Leave empty to skip anime \
             searches, as Sonarr does.",
        ),
        tag_field(
            "movie_categories",
            "Movie Categories",
            MOVIE_CATEGORY_IDS,
            None,
            false,
            "FileList category IDs searched for movies. Leave empty to skip movie \
             searches.",
        ),
        field(
            "minimum_seeders",
            "Minimum Seeders",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some("Minimum seeders preference for host-side release decisions"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

async fn search(request: SearchRequest) -> FnResult<SearchResponse> {
    let config = FileListConfig::from_host()?;
    let limit = result_limit(&request);

    // Sonarr's tier chain: run a tier, and only fall through to the next one
    // when the tier before it produced nothing (`HttpIndexerBase.FetchReleases`
    // breaks out of the tier loop as soon as `releases.Any()`).
    for url in build_request_tiers(&config, &request) {
        let body = fetch_json(&url).await?;
        let results = parse_torrents(&config, &body)?;
        if !results.is_empty() {
            return Ok(SearchResponse {
                results: dedupe_results(results).into_iter().take(limit).collect(),
                ..Default::default()
            });
        }
    }

    Ok(SearchResponse::default())
}

fn result_limit(request: &SearchRequest) -> usize {
    if request.limit == 0 {
        MAX_PAGE_SIZE
    } else {
        request.limit.min(MAX_PAGE_SIZE)
    }
}

/// Which configured category list a request searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FacetKind {
    Series,
    Anime,
    Movie,
}

fn facet_kind(request: &SearchRequest) -> FacetKind {
    match request
        .facet
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("movie") => FacetKind::Movie,
        Some("anime") => FacetKind::Anime,
        // An absolute episode number is only ever an anime request, and Sonarr
        // routes it to `AnimeCategories` regardless of how it was labelled.
        _ if request.absolute_episode.is_some() => FacetKind::Anime,
        _ => FacetKind::Series,
    }
}

/// True for the recent/RSS poll: either the host said so, or the request
/// carries no criteria at all.
fn is_recent_request(request: &SearchRequest) -> bool {
    if request
        .context
        .as_ref()
        .is_some_and(|context| matches!(context.request_kind, PluginSearchRequestKind::Recent))
    {
        return true;
    }
    request.query.trim().is_empty()
        && request.ids.is_empty()
        && request.season.is_none()
        && request.episode.is_none()
        && request.absolute_episode.is_none()
}

/// The request tiers for one search, most specific first. Each entry is one
/// upstream call; the caller stops at the first tier that returns releases.
fn build_request_tiers(config: &FileListConfig, request: &SearchRequest) -> Vec<String> {
    if is_recent_request(request) {
        // Sonarr concatenates Categories + AnimeCategories for the recent feed.
        // Scryer serves every facet from one configured indexer, so the movie
        // list joins them.
        let categories = union_categories(&[
            &config.categories,
            &config.anime_categories,
            &config.movie_categories,
        ]);
        if categories.is_empty() {
            return Vec::new();
        }
        return vec![request_url(
            config,
            "latest-torrents",
            &categories,
            &format!("&limit={}", result_limit(request)),
        )];
    }

    let categories = match facet_kind(request) {
        FacetKind::Series => &config.categories,
        FacetKind::Anime => &config.anime_categories,
        FacetKind::Movie => &config.movie_categories,
    };
    // Sonarr's `GetRequest` yields nothing when the category list for the
    // criteria is empty, which is how an anime-only or TV-only configuration
    // opts out of the other facets.
    if categories.is_empty() {
        return Vec::new();
    }

    let anime = facet_kind(request) == FacetKind::Anime;
    let season_episode = season_episode_params(request);
    let absolute = request
        .absolute_episode
        .map(|absolute| format!("&season=0&episode={absolute}"));

    let mut tiers = Vec::new();

    // Tier 0/1: `type=imdb`.
    if let Some(imdb_id) = imdb_query(request) {
        for suffix in scoped_suffixes(anime, absolute.as_deref(), &season_episode) {
            tiers.push(request_url(
                config,
                "search-torrents",
                categories,
                &format!("&type=imdb&query={}{suffix}", urlencoding::encode(&imdb_id)),
            ));
        }
    }

    // Trailing tiers: `type=name`.
    let name_query = name_query(request);
    if let Some(name_query) = name_query {
        for suffix in scoped_suffixes(anime, absolute.as_deref(), &season_episode) {
            tiers.push(request_url(
                config,
                "search-torrents",
                categories,
                &format!(
                    "&type=name&query={}{suffix}",
                    urlencoding::encode(&name_query)
                ),
            ));
        }
    }

    tiers
}

/// The per-tier season/episode scoping.
///
/// Sonarr splits an anime episode search into an absolute-numbered tier
/// (`season=0&episode={absolute}`) followed by a season/episode tier, because
/// FileList carries anime under both numbering schemes. Everything else is a
/// single scoping.
fn scoped_suffixes(anime: bool, absolute: Option<&str>, season_episode: &str) -> Vec<String> {
    match (anime, absolute) {
        (true, Some(absolute)) => {
            let mut suffixes = vec![absolute.to_string()];
            if !season_episode.is_empty() {
                suffixes.push(season_episode.to_string());
            }
            suffixes
        }
        _ => vec![season_episode.to_string()],
    }
}

fn season_episode_params(request: &SearchRequest) -> String {
    match (request.season, request.episode) {
        (Some(season), Some(episode)) => format!("&season={season}&episode={episode}"),
        (Some(season), None) => format!("&season={season}"),
        // Sonarr never builds this shape, but FileList accepts `episode`
        // independently and dropping it would widen the search for no reason.
        (None, Some(episode)) => format!("&episode={episode}"),
        (None, None) => String::new(),
    }
}

/// FileList accepts `tt4719744` or `4719744`; the id is forwarded as supplied.
fn imdb_query(request: &SearchRequest) -> Option<String> {
    request
        .ids
        .get("imdb_id")
        .map(|value| value.trim())
        .filter(|value| {
            !value.is_empty()
                && value
                    .trim_start_matches("tt")
                    .chars()
                    .any(|ch| ch.is_ascii_digit())
        })
        .map(str::to_string)
}

/// The `type=name` search term.
///
/// Sonarr loops over `SearchCriteria.SceneTitles`. Scryer's core never fills
/// `context.scene_titles` and instead dispatches one `freetext_alias` strategy
/// per alias, so the plugin issues exactly one name query per call and lets the
/// host own the alias fan-out. Radarr's movie generator appends the release
/// year when it has no IMDb id, which is reproduced here.
fn name_query(request: &SearchRequest) -> Option<String> {
    let query = request.query.trim();
    if query.is_empty() {
        return None;
    }
    if facet_kind(request) != FacetKind::Movie {
        return Some(query.to_string());
    }
    let year = request.context.as_ref().and_then(|context| context.year);
    match year {
        Some(year) if !query.contains(&year.to_string()) => Some(format!("{query} {year}")),
        _ => Some(query.to_string()),
    }
}

/// `{base}/api.php?action=…&category=…{params}&username=…&passkey=…`.
///
/// The `action`/`category`/params prefix is byte-identical to Sonarr's
/// `FileListRequestGenerator.GetRequest`; the credentials are appended because
/// FileList documents `username`/`passkey` as query parameters (Sonarr relies
/// on .NET's Basic challenge-response, which the Scryer host does not perform).
fn request_url(config: &FileListConfig, action: &str, categories: &[i64], params: &str) -> String {
    let categories = categories
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}/api.php?action={action}&category={categories}{params}&username={}&passkey={}",
        config.base_url,
        urlencoding::encode(&config.username),
        urlencoding::encode(&config.passkey),
    )
}

fn union_categories(lists: &[&Vec<i64>]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for list in lists {
        for value in list.iter() {
            if !out.contains(value) {
                out.push(*value);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Transport and delivery classification
// ---------------------------------------------------------------------------

async fn fetch_json(url: &str) -> Result<String, Error> {
    StartRateGate::new("filelist.request-start", 1, REQUEST_INTERVAL_MS)
        .acquire()
        .await
        .map_err(component::deadline_deferred_error)?;

    let response = component::http(PluginHttpRequest {
        url: url.to_string(),
        method: Some("GET".to_string()),
        headers: request_headers(url),
        body: Vec::new(),
    })
    .await
    .map_err(|error| {
        deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "FileList could not be reached".to_string(),
            format!("FileList request failed: {error:?}"),
        )
    })?;

    classify_response(response.status, &response.headers, &response.body)
}

fn request_headers(url: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        ("Accept".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
    ]);
    // Belt and braces: FileList documents query-parameter credentials, while
    // Sonarr authenticates the same call with HTTP Basic. Sending both costs
    // nothing and keeps the plugin working against either front end.
    if let Some(credentials) = basic_credentials(url) {
        headers.insert(
            "Authorization".to_string(),
            format!("Basic {}", STANDARD.encode(credentials)),
        );
    }
    headers
}

fn basic_credentials(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let mut username = None;
    let mut passkey = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "username" => username = Some(urlencoding::decode(value).ok()?.into_owned()),
            "passkey" => passkey = Some(urlencoding::decode(value).ok()?.into_owned()),
            _ => {}
        }
    }
    Some(format!("{}:{}", username?, passkey?))
}

/// Map one HTTP delivery onto Scryer's typed indexer error lanes.
///
/// FileList's documented status codes: 400 invalid search/filter parameters,
/// 401 empty username/passkey, 403 invalid credentials or too many failed
/// authentications, 429 hourly budget exhausted, 503 unavailable.
fn classify_response(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<String, Error> {
    match status {
        200 => {}
        300..=399 => {
            let location = header_value(headers, "location").unwrap_or("(no Location header)");
            return Err(invalid_config_error(
                "base_url",
                format!(
                    "FileList redirected the API call with HTTP {status} to {location}; the \
                     configured site URL is not the API root"
                ),
            ));
        }
        400 => {
            return Err(invalid_config_error(
                "categories",
                format!(
                    "FileList rejected the search parameters with HTTP 400: {}",
                    body_excerpt(body)
                ),
            ));
        }
        401 | 403 => {
            return Err(auth_failed_error(format!(
                "FileList rejected the account credentials with HTTP {status}: {}",
                body_excerpt(body)
            )));
        }
        429 => {
            return Err(rate_limited_error(retry_after_seconds(headers)));
        }
        _ => {
            return Err(deferred_error(
                IndexerSearchIncompleteReason::UpstreamFailure,
                None,
                format!("FileList returned HTTP {status}"),
                format!("FileList returned HTTP {status}: {}", body_excerpt(body)),
            ));
        }
    }

    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            format!(
                "FileList returned {} bytes, above the {MAX_RESPONSE_BYTES} byte ceiling",
                body.len()
            ),
        ));
    }

    let text = std::str::from_utf8(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("FileList response was not valid UTF-8: {error}"),
        )
    })?;

    // Sonarr's parser throws when the content type is not JSON. An HTML body is
    // the Cloudflare/interstitial case the addendum calls out.
    if !is_json_delivery(headers, text) {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            format!(
                "FileList returned content type {:?} instead of JSON; the site is likely blocked \
                 or behind an interstitial",
                header_value(headers, "content-type").unwrap_or("(absent)")
            ),
        ));
    }

    Ok(text.to_string())
}

/// A JSON delivery is one the server labelled JSON, or — when no content type
/// survived a proxy — one whose body actually starts with a JSON root.
fn is_json_delivery(headers: &BTreeMap<String, String>, body: &str) -> bool {
    match header_value(headers, "content-type") {
        Some(content_type) => content_type
            .split(';')
            .next()
            .map(str::trim)
            .is_some_and(|value| value.ends_with("/json") || value.ends_with("+json")),
        None => body.trim_start().starts_with(['[', '{']),
    }
}

fn retry_after_seconds(headers: &BTreeMap<String, String>) -> i64 {
    header_value(headers, "retry-after")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(RATE_LIMITED_FALLBACK_SECONDS)
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn body_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.chars().count() <= 400 {
        return trimmed.to_string();
    }
    trimmed.chars().take(400).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

fn typed_error(
    code: PluginErrorCode,
    public_message: String,
    debug_message: String,
    retry_after_seconds: Option<i64>,
    details: Option<IndexerSearchPluginError>,
) -> Error {
    structured_plugin_error(PluginError {
        code,
        public_message,
        debug_message: Some(debug_message),
        retry_after_seconds,
        details: details.map(PluginErrorDetails::IndexerSearch),
    })
}

/// A configuration fault. Deliberately NOT a deferred error: the host must
/// surface it rather than cool the indexer down for a typo.
fn invalid_config_error(field: &str, detail: String) -> Error {
    typed_error(
        PluginErrorCode::InvalidConfig,
        format!("FileList setting '{field}' is not usable"),
        detail,
        None,
        None,
    )
}

fn auth_failed_error(detail: String) -> Error {
    typed_error(
        PluginErrorCode::AuthFailed,
        "FileList rejected the configured 'username' and 'passkey'".to_string(),
        detail,
        None,
        None,
    )
}

fn rate_limited_error(retry_after_seconds: i64) -> Error {
    typed_error(
        PluginErrorCode::RateLimited,
        format!("FileList hourly API budget of {HOURLY_API_BUDGET} requests is exhausted"),
        format!("FileList returned HTTP 429; retrying after {retry_after_seconds}s"),
        Some(retry_after_seconds),
        Some(IndexerSearchPluginError::Deferred {
            reason: IndexerSearchIncompleteReason::RateLimited,
            retry_after_seconds: Some(retry_after_seconds),
        }),
    )
}

fn deferred_error(
    reason: IndexerSearchIncompleteReason,
    retry_after_seconds: Option<i64>,
    public_message: String,
    debug_message: String,
) -> Error {
    let code = if reason == IndexerSearchIncompleteReason::RateLimited {
        PluginErrorCode::RateLimited
    } else {
        PluginErrorCode::UpstreamUnavailable
    };
    typed_error(
        code,
        public_message,
        debug_message,
        retry_after_seconds,
        Some(IndexerSearchPluginError::Deferred {
            reason,
            retry_after_seconds,
        }),
    )
}

fn invalid_response_error(kind: IndexerSearchInvalidResponseKind, detail: String) -> Error {
    typed_error(
        PluginErrorCode::UpstreamUnavailable,
        "FileList returned a response Scryer could not read".to_string(),
        detail,
        None,
        Some(IndexerSearchPluginError::InvalidResponse { kind }),
    )
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

fn parse_torrents(config: &FileListConfig, body: &str) -> Result<Vec<SearchResult>, Error> {
    let root: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("FileList JSON parse failed: {error}"),
        )
    })?;

    // The API answers faults with `{"error": "…"}` even on a 200.
    if let Some(message) = root.get("error").and_then(value_text) {
        return Err(classify_api_error(&message));
    }

    let Some(items) = root.as_array() else {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::InvalidRoot,
            "FileList response root was not a torrent array".to_string(),
        ));
    };

    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let torrent: FileListTorrent = serde_json::from_value(item.clone()).map_err(|error| {
            invalid_response_error(
                IndexerSearchInvalidResponseKind::MalformedBody,
                format!("FileList torrent entry could not be read: {error}"),
            )
        })?;
        if let Some(result) = torrent_to_result(config, torrent) {
            results.push(result);
        }
    }
    Ok(results)
}

fn classify_api_error(message: &str) -> Error {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("passkey")
        || lowered.contains("username")
        || lowered.contains("credential")
        || lowered.contains("authenticat")
    {
        return auth_failed_error(format!("FileList API error: {message}"));
    }
    if lowered.contains("rate limit")
        || lowered.contains("too many")
        || lowered.contains("limit reached")
    {
        return rate_limited_error(RATE_LIMITED_FALLBACK_SECONDS);
    }
    if lowered.contains("categor") || lowered.contains("invalid") || lowered.contains("parameter") {
        return invalid_config_error("categories", format!("FileList API error: {message}"));
    }
    deferred_error(
        IndexerSearchIncompleteReason::UpstreamFailure,
        None,
        "FileList reported an API error".to_string(),
        format!("FileList API error: {message}"),
    )
}

/// Sonarr's `IsValidRelease`: a release with no title or no download URL is
/// dropped rather than surfaced.
fn torrent_to_result(config: &FileListConfig, torrent: FileListTorrent) -> Option<SearchResult> {
    let id = torrent.id.as_ref().and_then(json_text)?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let title = torrent.name.as_ref().and_then(json_text)?;
    let title = title.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let seeders = torrent.seeders.as_ref().and_then(json_i64).unwrap_or(0);
    let leechers = torrent.leechers.as_ref().and_then(json_i64).unwrap_or(0);
    let times_completed = torrent.times_completed.as_ref().and_then(json_i64);
    let freeleech = torrent.freeleech.as_ref().is_some_and(json_flag);
    let doubleup = torrent.doubleup.as_ref().is_some_and(json_flag);
    let internal = torrent.internal.as_ref().is_some_and(json_flag);
    let moderated = torrent.moderated.as_ref().is_some_and(json_flag);

    // The fleet's flag vocabulary (see `newznab-common`): the volume factors
    // carry the leech economics and `tags` carries the qualitative labels.
    let download_volume_factor = if freeleech { 0.0 } else { 1.0 };
    let upload_volume_factor = if doubleup { 2.0 } else { 1.0 };
    let mut tags: Vec<String> = Vec::new();
    if internal {
        tags.push("internal".to_string());
    }
    if doubleup {
        tags.push("doubleupload".to_string());
    }

    let mut external_ids = HashMap::new();
    if let Some(imdb_id) = normalize_imdb(torrent.imdb.as_ref().and_then(json_text).as_deref()) {
        external_ids.insert("imdb_id".to_string(), imdb_id);
    }

    let category = torrent
        .category
        .as_ref()
        .and_then(json_text)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let mut provider_extra = HashMap::new();
    // `freeleech` and `tags` are the keys Scryer's rule engine reads
    // (`crates/scryer-rules/src/release.rs`), so they are reported as data and
    // not only as flag strings.
    provider_extra.insert("freeleech".to_string(), serde_json::Value::from(freeleech));
    provider_extra.insert("doubleup".to_string(), serde_json::Value::from(doubleup));
    provider_extra.insert("internal".to_string(), serde_json::Value::from(internal));
    provider_extra.insert("moderated".to_string(), serde_json::Value::from(moderated));
    if !tags.is_empty() {
        provider_extra.insert("tags".to_string(), serde_json::Value::from(tags.clone()));
    }
    if let Some(files) = torrent.files.as_ref().and_then(json_i64) {
        provider_extra.insert("files".to_string(), serde_json::Value::from(files));
    }
    if let Some(comments) = torrent.comments.as_ref().and_then(json_i64) {
        provider_extra.insert("comments".to_string(), serde_json::Value::from(comments));
    }
    if let Some(times_completed) = times_completed {
        provider_extra.insert(
            "times_completed".to_string(),
            serde_json::Value::from(times_completed),
        );
    }
    if let Some(description) = torrent.small_description.as_ref().and_then(json_text) {
        provider_extra.insert(
            "small_description".to_string(),
            serde_json::Value::from(description),
        );
    }
    if let Some(category) = category.as_deref() {
        provider_extra.insert("category".to_string(), serde_json::Value::from(category));
    }

    let categories = category
        .clone()
        .map(|value| vec![value])
        .unwrap_or_default();

    Some(SearchResult {
        download_url: Some(download_url(config, &id)),
        info_url: Some(info_url(config, &id)),
        // FileList exposes no separate comment page; Sonarr's fixture asserts
        // an empty `CommentUrl`.
        comment_url: None,
        guid: Some(format!("FileList-{id}")),
        size_bytes: torrent.size.as_ref().and_then(json_i64),
        published_at: torrent
            .upload_date
            .as_ref()
            .and_then(json_text)
            .as_deref()
            .and_then(normalize_published_at),
        grabs: times_completed,
        seeders: Some(seeders),
        peers: Some(seeders + leechers),
        leechers: Some(leechers),
        download_volume_factor: Some(download_volume_factor),
        upload_volume_factor: Some(upload_volume_factor),
        indexer_flags: derive_indexer_flags(
            Some(download_volume_factor),
            Some(upload_volume_factor),
            &tags,
            None,
        ),
        external_ids,
        categories: categories.clone(),
        provider_categories: categories,
        provider_extra,
        ..torrent_result(title, None)
    })
}

/// Sonarr builds the download URL from the configured base rather than trusting
/// the API's `download_link`, so a mirror or reverse proxy stays honoured.
fn download_url(config: &FileListConfig, id: &str) -> String {
    format!(
        "{}/download.php?id={}&passkey={}",
        config.base_url,
        urlencoding::encode(id),
        urlencoding::encode(&config.passkey),
    )
}

fn info_url(config: &FileListConfig, id: &str) -> String {
    format!(
        "{}/details.php?id={}",
        config.base_url,
        urlencoding::encode(id)
    )
}

/// Sonarr: `tt{imdbId:D7}` from the digits after `tt`, dropped when zero.
fn normalize_imdb(value: Option<&str>) -> Option<String> {
    let digits = value?
        .trim()
        .trim_start_matches("tt")
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() || digits.chars().all(|ch| ch == '0') {
        return None;
    }
    Some(format!("tt{:0>7}", digits.trim_start_matches('0')))
}

/// FileList reports `upload_date` as `YYYY-MM-DD HH:MM:SS` with no zone.
///
/// Scryer's RSS staleness tracker parses `published_at` with
/// `DateTime::parse_from_rfc3339` only
/// (`crates/scryer-infrastructure-acquisition/src/indexers/search_client.rs`),
/// so the value has to leave this plugin as RFC 3339 UTC.
fn normalize_published_at(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Already an RFC 3339 instant: pass it through untouched.
    if raw.len() >= 20
        && raw.as_bytes().get(10) == Some(&b'T')
        && (raw.ends_with('Z') || raw[19..].contains('+') || raw[19..].contains('-'))
    {
        return Some(raw.to_string());
    }

    let (date, time) = raw.split_once([' ', 'T'])?;
    let date = date.trim();
    let time = time.trim();
    if !matches_mask(date, "0000-00-00") {
        return None;
    }
    let time = match time.len() {
        5 if matches_mask(time, "00:00") => format!("{time}:00"),
        8 if matches_mask(time, "00:00:00") => time.to_string(),
        _ => return None,
    };
    Some(format!("{date}T{time}Z"))
}

/// `0` in the mask means "one ASCII digit"; every other byte must match.
fn matches_mask(value: &str, mask: &str) -> bool {
    if value.len() != mask.len() || !value.is_ascii() {
        return false;
    }
    value.bytes().zip(mask.bytes()).all(|(byte, mask)| {
        if mask == b'0' {
            byte.is_ascii_digit()
        } else {
            byte == mask
        }
    })
}

fn dedupe_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for result in results {
        let key = result.guid.clone().unwrap_or_else(|| result.title.clone());
        if seen.iter().any(|existing| existing == &key) {
            continue;
        }
        seen.push(key);
        out.push(result);
    }
    out
}

// ---------------------------------------------------------------------------
// Lenient JSON scalars
// ---------------------------------------------------------------------------

/// FileList types its JSON loosely: `id` is a number, `freeleech`/`internal`/
/// `moderated`/`doubleup` are `0`/`1` integers, and mirrors have been seen
/// returning the same fields as strings. Sonarr's Newtonsoft coerces all of
/// these, so the plugin must too — a strict `bool`/`String` mapping rejects the
/// entire array on the first real payload.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum JsonScalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// Text of a raw JSON value, used only to read the API's `error` member.
fn value_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn json_text(value: &JsonScalar) -> Option<String> {
    match value {
        JsonScalar::Text(text) => Some(text.clone()),
        JsonScalar::Int(value) => Some(value.to_string()),
        JsonScalar::Float(value) => Some(value.to_string()),
        JsonScalar::Bool(value) => Some(value.to_string()),
    }
}

fn json_i64(value: &JsonScalar) -> Option<i64> {
    match value {
        JsonScalar::Int(value) => Some(*value),
        JsonScalar::Float(value) => Some(*value as i64),
        JsonScalar::Bool(value) => Some(i64::from(*value)),
        JsonScalar::Text(text) => text.trim().parse::<i64>().ok(),
    }
}

fn json_flag(value: &JsonScalar) -> bool {
    match value {
        JsonScalar::Bool(value) => *value,
        JsonScalar::Int(value) => *value != 0,
        JsonScalar::Float(value) => *value != 0.0,
        JsonScalar::Text(text) => matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    }
}

/// One entry of the `api.php` torrent array.
#[derive(Debug, Default, Deserialize)]
struct FileListTorrent {
    #[serde(default)]
    id: Option<JsonScalar>,
    #[serde(default)]
    name: Option<JsonScalar>,
    #[serde(default)]
    size: Option<JsonScalar>,
    #[serde(default)]
    leechers: Option<JsonScalar>,
    #[serde(default)]
    seeders: Option<JsonScalar>,
    #[serde(default)]
    times_completed: Option<JsonScalar>,
    #[serde(default)]
    comments: Option<JsonScalar>,
    #[serde(default)]
    files: Option<JsonScalar>,
    #[serde(default)]
    imdb: Option<JsonScalar>,
    #[serde(default)]
    internal: Option<JsonScalar>,
    #[serde(default)]
    moderated: Option<JsonScalar>,
    #[serde(default)]
    freeleech: Option<JsonScalar>,
    #[serde(default)]
    doubleup: Option<JsonScalar>,
    #[serde(default)]
    category: Option<JsonScalar>,
    #[serde(default)]
    small_description: Option<JsonScalar>,
    #[serde(default)]
    upload_date: Option<JsonScalar>,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FileListConfig {
    /// Always without a trailing slash.
    base_url: String,
    username: String,
    passkey: String,
    categories: Vec<i64>,
    anime_categories: Vec<i64>,
    movie_categories: Vec<i64>,
}

impl FileListConfig {
    fn from_host() -> Result<Self, Error> {
        Self::resolve(
            config_value("base_url"),
            config_value("username"),
            config_value("passkey"),
            config_value("categories"),
            config_value("anime_categories"),
            config_value("movie_categories"),
        )
    }

    /// The pure half of configuration resolution, mirroring
    /// `FileListSettingsValidator`: a root URL, a username, a passkey, and at
    /// least one category list.
    fn resolve(
        base_url: Option<String>,
        username: Option<String>,
        passkey: Option<String>,
        categories: Option<String>,
        anime_categories: Option<String>,
        movie_categories: Option<String>,
    ) -> Result<Self, Error> {
        let base_url = normalize_base_url(base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))?;
        let username = username.ok_or_else(|| {
            invalid_config_error(
                "username",
                "FileList requires an account username".to_string(),
            )
        })?;
        let passkey = passkey.ok_or_else(|| {
            invalid_config_error(
                "passkey",
                "FileList requires an account passkey".to_string(),
            )
        })?;

        let categories = parse_categories(categories.as_deref().unwrap_or(DEFAULT_CATEGORIES));
        let anime_categories = parse_categories(anime_categories.as_deref().unwrap_or_default());
        let movie_categories = parse_categories(movie_categories.as_deref().unwrap_or_default());
        if categories.is_empty() && anime_categories.is_empty() && movie_categories.is_empty() {
            return Err(invalid_config_error(
                "categories",
                "either 'Categories', 'Anime Categories' or 'Movie Categories' must contain at \
                 least one FileList category ID"
                    .to_string(),
            ));
        }

        Ok(Self {
            base_url,
            username,
            passkey,
            categories,
            anime_categories,
            movie_categories,
        })
    }
}

/// Sonarr's `ValidRootUrl`: non-empty, parseable, `http`/`https`.
fn normalize_base_url(value: &str) -> Result<String, Error> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "base_url",
            "FileList requires a site URL".to_string(),
        ));
    }
    let parsed = url::Url::parse(trimmed).map_err(|error| {
        invalid_config_error(
            "base_url",
            format!("'{trimmed}' is not a valid URL: {error}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid_config_error(
            "base_url",
            format!("'{trimmed}' must be an http(s) URL with a host"),
        ));
    }
    Ok(trimmed.to_string())
}

/// Accepts both the tag form the UI writes and the legacy comma-separated
/// string, and keeps the configured order while dropping duplicates.
fn parse_categories(raw: &str) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for part in raw.split([',', ';', '\n', ' ', '\t']) {
        let Ok(value) = part.trim().parse::<i64>() else {
            continue;
        };
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Config field helpers
// ---------------------------------------------------------------------------

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        role: None,
        host_binding: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
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
            default_value,
            help_text,
        )
    }
}

fn tag_field(
    key: &str,
    label: &str,
    option_ids: &[i64],
    default_value: Option<&str>,
    required: bool,
    help_text: &str,
) -> ConfigFieldDef {
    ConfigFieldDef {
        options: category_options(option_ids),
        ..field(
            key,
            label,
            ConfigFieldType::Tag,
            required,
            default_value,
            Some(help_text),
        )
    }
}

scryer_indexer_component_main!(descriptor = build_descriptor, search = search,);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_pdk::component::StructuredPluginError;
    use scryer_plugin_sdk::PluginSearchContext;

    /// Sonarr's `Files/Indexers/FileList/RecentFeed.json`, verbatim.
    const RECENT_FEED: &str = r#"[
  {
    "id": 1234,
    "name": "Mankind.Divided.2019.S01E01.1080p.WEB-DL",
    "imdb": "tt1232322",
    "freeleech": 0,
    "upload_date": "2019-01-22 22:20:19",
    "download_link": "https://filelist.io/download.php?id=1234&passkey=somepass",
    "size": 830512414,
    "internal": 0,
    "moderated": 1,
    "category": "Seriale HD",
    "seeders": 12,
    "leechers": 2,
    "times_completed": 11,
    "comments": 0,
    "files": 3,
    "small_description": "Much anticipated show about (redacted)"
  },
  {
    "id": 1235,
    "name": "Mankind.Divided.2019.S01E02.1080p.WEB-DL",
    "imdb": "tt9999999",
    "freeleech": 0,
    "upload_date": "2019-01-22 22:19:37",
    "download_link": "https://filelist.io/download.php?id=1235&passkey=somepass",
    "size": 473149881,
    "internal": 0,
    "moderated": 1,
    "category": "Seriale HD",
    "seeders": 9,
    "leechers": 1,
    "times_completed": 8,
    "comments": 0,
    "files": 3,
    "small_description": "(redacted) finds a way to unify two of the most insignificant factions"
  }
]"#;

    fn test_config() -> FileListConfig {
        FileListConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            username: "someuser".to_string(),
            passkey: "somepass".to_string(),
            categories: vec![23, 21, 27],
            anime_categories: vec![24],
            movie_categories: vec![4, 6],
        }
    }

    fn request() -> SearchRequest {
        SearchRequest {
            limit: 1000,
            ..SearchRequest::default()
        }
    }

    fn json_headers() -> BTreeMap<String, String> {
        BTreeMap::from([(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )])
    }

    fn plugin_error(error: &Error) -> PluginError {
        error
            .downcast_ref::<StructuredPluginError>()
            .expect("error should be a structured plugin error")
            .plugin_error()
            .clone()
    }

    fn details(error: &Error) -> IndexerSearchPluginError {
        match plugin_error(error).details {
            Some(PluginErrorDetails::IndexerSearch(details)) => details,
            other => panic!("expected indexer-search details, got {other:?}"),
        }
    }

    // -- H1: the real API payload deserializes ------------------------------

    #[test]
    fn parses_sonarrs_recent_feed_fixture() {
        let results = parse_torrents(&test_config(), RECENT_FEED).expect("fixture should parse");
        assert_eq!(results.len(), 2);

        let first = &results[0];
        assert_eq!(first.title, "Mankind.Divided.2019.S01E01.1080p.WEB-DL");
        assert_eq!(
            first.download_url.as_deref(),
            Some("https://filelist.io/download.php?id=1234&passkey=somepass")
        );
        assert_eq!(
            first.info_url.as_deref(),
            Some("https://filelist.io/details.php?id=1234")
        );
        assert_eq!(first.comment_url, None);
        assert_eq!(first.guid.as_deref(), Some("FileList-1234"));
        assert_eq!(first.published_at.as_deref(), Some("2019-01-22T22:20:19Z"));
        assert_eq!(first.size_bytes, Some(830_512_414));
        assert_eq!(first.info_hash_v1, None);
        assert_eq!(first.magnet_url, None);
        assert_eq!(first.seeders, Some(12));
        assert_eq!(first.leechers, Some(2));
        assert_eq!(first.peers, Some(14));
        assert_eq!(first.grabs, Some(11));
        assert_eq!(
            first.external_ids.get("imdb_id").map(String::as_str),
            Some("tt1232322")
        );
        assert_eq!(first.source_kind, Some(IndexerSourceKind::Torrent));
        assert_eq!(first.protocol, Some(IndexerProtocol::Torrent));
    }

    #[test]
    fn parses_the_second_fixture_entry() {
        let results = parse_torrents(&test_config(), RECENT_FEED).expect("fixture should parse");
        let second = &results[1];
        assert_eq!(second.title, "Mankind.Divided.2019.S01E02.1080p.WEB-DL");
        assert_eq!(
            second.download_url.as_deref(),
            Some("https://filelist.io/download.php?id=1235&passkey=somepass")
        );
        assert_eq!(second.published_at.as_deref(), Some("2019-01-22T22:19:37Z"));
        assert_eq!(second.size_bytes, Some(473_149_881));
        assert_eq!(second.seeders, Some(9));
        assert_eq!(second.peers, Some(10));
        assert_eq!(
            second.external_ids.get("imdb_id").map(String::as_str),
            Some("tt9999999")
        );
    }

    #[test]
    fn tolerates_string_typed_ids_and_flags() {
        let body = r#"[{"id":"77","name":"A","size":"12","seeders":"3","leechers":"1",
            "freeleech":"1","internal":"true","doubleup":"0","moderated":"1",
            "times_completed":"4","upload_date":"2024-05-06 07:08:09"}]"#;
        let results = parse_torrents(&test_config(), body).expect("string payload should parse");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].guid.as_deref(), Some("FileList-77"));
        assert_eq!(results[0].size_bytes, Some(12));
        assert_eq!(results[0].seeders, Some(3));
        assert_eq!(results[0].download_volume_factor, Some(0.0));
        assert_eq!(
            results[0].published_at.as_deref(),
            Some("2024-05-06T07:08:09Z")
        );
    }

    #[test]
    fn tolerates_boolean_typed_flags() {
        let body = r#"[{"id":1,"name":"A","freeleech":true,"internal":true,"doubleup":true}]"#;
        let results = parse_torrents(&test_config(), body).expect("boolean payload should parse");
        assert_eq!(results[0].download_volume_factor, Some(0.0));
        assert_eq!(results[0].upload_volume_factor, Some(2.0));
        assert!(
            results[0]
                .indexer_flags
                .iter()
                .any(|flag| flag == "internal")
        );
    }

    // -- M1: flags, factors and category metadata ---------------------------

    #[test]
    fn freeleech_sets_the_flag_the_volume_factor_and_the_rule_engine_key() {
        let body = r#"[{"id":1,"name":"A","freeleech":1,"category":"Seriale HD"}]"#;
        let results = parse_torrents(&test_config(), body).expect("payload should parse");
        let result = &results[0];
        assert!(result.indexer_flags.iter().any(|flag| flag == "freeleech"));
        assert_eq!(result.download_volume_factor, Some(0.0));
        assert_eq!(
            result.provider_extra.get("freeleech"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn non_freeleech_reports_a_full_download_volume_factor() {
        let body = r#"[{"id":1,"name":"A","freeleech":0}]"#;
        let results = parse_torrents(&test_config(), body).expect("payload should parse");
        assert_eq!(results[0].download_volume_factor, Some(1.0));
        assert_eq!(results[0].upload_volume_factor, Some(1.0));
        assert!(results[0].indexer_flags.is_empty());
        assert_eq!(
            results[0].provider_extra.get("freeleech"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn doubleup_doubles_the_upload_volume_factor_and_tags_the_release() {
        let body = r#"[{"id":1,"name":"A","doubleup":1}]"#;
        let results = parse_torrents(&test_config(), body).expect("payload should parse");
        assert_eq!(results[0].upload_volume_factor, Some(2.0));
        assert!(
            results[0]
                .indexer_flags
                .iter()
                .any(|flag| flag == "doubleupload")
        );
        assert_eq!(
            results[0].provider_extra.get("tags"),
            Some(&serde_json::json!(["doubleupload"]))
        );
    }

    #[test]
    fn internal_is_reported_as_a_flag_and_a_tag() {
        let body = r#"[{"id":1,"name":"A","internal":1}]"#;
        let results = parse_torrents(&test_config(), body).expect("payload should parse");
        assert!(
            results[0]
                .indexer_flags
                .iter()
                .any(|flag| flag == "internal")
        );
        assert_eq!(
            results[0].provider_extra.get("tags"),
            Some(&serde_json::json!(["internal"]))
        );
    }

    #[test]
    fn the_provider_category_is_reported_on_both_category_fields() {
        let results = parse_torrents(&test_config(), RECENT_FEED).expect("fixture should parse");
        assert_eq!(results[0].categories, vec!["Seriale HD".to_string()]);
        assert_eq!(
            results[0].provider_categories,
            vec!["Seriale HD".to_string()]
        );
        assert_eq!(
            results[0].provider_extra.get("category"),
            Some(&serde_json::Value::from("Seriale HD"))
        );
    }

    #[test]
    fn descriptive_counters_land_in_provider_extra() {
        let results = parse_torrents(&test_config(), RECENT_FEED).expect("fixture should parse");
        let extra = &results[0].provider_extra;
        assert_eq!(extra.get("files"), Some(&serde_json::Value::from(3)));
        assert_eq!(extra.get("comments"), Some(&serde_json::Value::from(0)));
        assert_eq!(
            extra.get("times_completed"),
            Some(&serde_json::Value::from(11))
        );
        assert_eq!(extra.get("moderated"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(
            extra.get("small_description"),
            Some(&serde_json::Value::from(
                "Much anticipated show about (redacted)"
            ))
        );
        // The API's own `download_link` carries the passkey and is never
        // duplicated into the host's extra map.
        assert!(extra.get("download_link").is_none());
    }

    #[test]
    fn releases_without_a_title_or_an_id_are_dropped() {
        let body = r#"[{"id":1,"name":""},{"name":"No id"},{"id":2,"name":"Keep"}]"#;
        let results = parse_torrents(&test_config(), body).expect("payload should parse");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Keep");
    }

    // -- H4: publish dates ---------------------------------------------------

    #[test]
    fn upload_dates_become_rfc_3339_utc() {
        assert_eq!(
            normalize_published_at("2019-01-22 22:20:19").as_deref(),
            Some("2019-01-22T22:20:19Z")
        );
        assert_eq!(
            normalize_published_at("2019-01-22T22:20:19").as_deref(),
            Some("2019-01-22T22:20:19Z")
        );
        assert_eq!(
            normalize_published_at("2019-01-22 22:20").as_deref(),
            Some("2019-01-22T22:20:00Z")
        );
    }

    #[test]
    fn rfc_3339_upload_dates_pass_through() {
        assert_eq!(
            normalize_published_at("2019-01-22T22:20:19Z").as_deref(),
            Some("2019-01-22T22:20:19Z")
        );
        assert_eq!(
            normalize_published_at("2019-01-22T22:20:19+02:00").as_deref(),
            Some("2019-01-22T22:20:19+02:00")
        );
    }

    #[test]
    fn unreadable_upload_dates_are_dropped_rather_than_guessed() {
        assert_eq!(normalize_published_at(""), None);
        assert_eq!(normalize_published_at("yesterday"), None);
        assert_eq!(normalize_published_at("2019-1-2 3:4:5"), None);
        assert_eq!(normalize_published_at("22/01/2019 22:20:19"), None);
    }

    // -- IMDb normalisation --------------------------------------------------

    #[test]
    fn imdb_ids_are_padded_the_way_sonarr_pads_them() {
        assert_eq!(
            normalize_imdb(Some("tt1232322")).as_deref(),
            Some("tt1232322")
        );
        assert_eq!(normalize_imdb(Some("1234")).as_deref(), Some("tt0001234"));
        assert_eq!(
            normalize_imdb(Some("tt0001234")).as_deref(),
            Some("tt0001234")
        );
    }

    #[test]
    fn empty_and_zero_imdb_ids_are_dropped() {
        assert_eq!(normalize_imdb(None), None);
        assert_eq!(normalize_imdb(Some("")), None);
        assert_eq!(normalize_imdb(Some("tt")), None);
        assert_eq!(normalize_imdb(Some("tt0000000")), None);
    }

    // -- H2: request tiers ---------------------------------------------------

    #[test]
    fn the_recent_feed_uses_latest_torrents_over_every_configured_category() {
        let tiers = build_request_tiers(&test_config(), &request());
        assert_eq!(
            tiers,
            vec![
                "https://filelist.io/api.php?action=latest-torrents&category=23,21,27,24,4,6\
                 &limit=100&username=someuser&passkey=somepass"
                    .to_string()
            ]
        );
    }

    #[test]
    fn an_explicit_recent_context_still_polls_the_latest_feed() {
        let mut req = request();
        req.context = Some(PluginSearchContext {
            request_kind: PluginSearchRequestKind::Recent,
            ..PluginSearchContext::default()
        });
        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].contains("action=latest-torrents"));
    }

    #[test]
    fn a_series_search_puts_the_imdb_tier_ahead_of_the_name_tier() {
        let mut req = request();
        req.facet = Some("series".to_string());
        req.query = "Mankind Divided".to_string();
        req.ids
            .insert("imdb_id".to_string(), "tt1232322".to_string());
        req.season = Some(1);
        req.episode = Some(1);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(
            tiers,
            vec![
                "https://filelist.io/api.php?action=search-torrents&category=23,21,27\
                 &type=imdb&query=tt1232322&season=1&episode=1&username=someuser&passkey=somepass"
                    .to_string(),
                "https://filelist.io/api.php?action=search-torrents&category=23,21,27\
                 &type=name&query=Mankind%20Divided&season=1&episode=1&username=someuser\
                 &passkey=somepass"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn a_season_search_omits_the_episode_parameter() {
        let mut req = request();
        req.facet = Some("series".to_string());
        req.ids
            .insert("imdb_id".to_string(), "tt1232322".to_string());
        req.season = Some(3);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(
            tiers[0].ends_with(
                "&type=imdb&query=tt1232322&season=3&username=someuser&passkey=somepass"
            )
        );
    }

    #[test]
    fn an_id_only_request_never_issues_a_name_call() {
        let mut req = request();
        req.facet = Some("series".to_string());
        req.ids
            .insert("imdb_id".to_string(), "tt1232322".to_string());
        req.season = Some(1);
        req.episode = Some(2);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].contains("type=imdb"));
    }

    #[test]
    fn an_anime_episode_search_tiers_absolute_numbering_before_season_episode() {
        let mut req = request();
        req.facet = Some("anime".to_string());
        req.query = "Show".to_string();
        req.ids
            .insert("imdb_id".to_string(), "tt5555555".to_string());
        req.season = Some(2);
        req.episode = Some(9);
        req.absolute_episode = Some(33);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 4);
        assert!(tiers[0].contains("category=24"));
        assert!(tiers[0].contains("&type=imdb&query=tt5555555&season=0&episode=33"));
        assert!(tiers[1].contains("&type=imdb&query=tt5555555&season=2&episode=9"));
        assert!(tiers[2].contains("&type=name&query=Show&season=0&episode=33"));
        assert!(tiers[3].contains("&type=name&query=Show&season=2&episode=9"));
    }

    #[test]
    fn an_absolute_episode_without_a_facet_still_uses_the_anime_categories() {
        let mut req = request();
        req.query = "Show".to_string();
        req.absolute_episode = Some(12);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].contains("category=24"));
        assert!(tiers[0].contains("&type=name&query=Show&season=0&episode=12"));
    }

    #[test]
    fn an_anime_season_search_tiers_imdb_then_name() {
        let mut req = request();
        req.facet = Some("anime".to_string());
        req.query = "Show".to_string();
        req.ids
            .insert("imdb_id".to_string(), "tt5555555".to_string());
        req.season = Some(2);

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 2);
        assert!(tiers[0].contains("&type=imdb&query=tt5555555&season=2"));
        assert!(tiers[1].contains("&type=name&query=Show&season=2"));
    }

    #[test]
    fn interactive_free_text_issues_a_single_name_request() {
        let mut req = request();
        req.query = "Mankind Divided 1080p".to_string();

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(
            tiers,
            vec![
                "https://filelist.io/api.php?action=search-torrents&category=23,21,27\
                 &type=name&query=Mankind%20Divided%201080p&username=someuser&passkey=somepass"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_movie_search_uses_the_movie_categories_and_appends_the_year() {
        let mut req = request();
        req.facet = Some("movie".to_string());
        req.query = "Arrival".to_string();
        req.context = Some(PluginSearchContext {
            year: Some(2016),
            ..PluginSearchContext::default()
        });

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].contains("category=4,6"));
        assert!(tiers[0].contains("&type=name&query=Arrival%202016"));
    }

    #[test]
    fn a_movie_query_that_already_carries_the_year_is_left_alone() {
        let mut req = request();
        req.facet = Some("movie".to_string());
        req.query = "Arrival 2016".to_string();
        req.context = Some(PluginSearchContext {
            year: Some(2016),
            ..PluginSearchContext::default()
        });

        let tiers = build_request_tiers(&test_config(), &req);
        assert!(tiers[0].contains("&type=name&query=Arrival%202016&"));
    }

    #[test]
    fn a_facet_with_no_configured_categories_issues_no_requests() {
        let mut config = test_config();
        config.anime_categories.clear();
        let mut req = request();
        req.facet = Some("anime".to_string());
        req.query = "Show".to_string();

        assert!(build_request_tiers(&config, &req).is_empty());
    }

    #[test]
    fn a_blank_imdb_id_does_not_produce_an_imdb_tier() {
        let mut req = request();
        req.facet = Some("series".to_string());
        req.query = "Show".to_string();
        req.ids.insert("imdb_id".to_string(), "  ".to_string());

        let tiers = build_request_tiers(&test_config(), &req);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].contains("type=name"));
    }

    #[test]
    fn credentials_are_percent_encoded_in_the_request_url() {
        let mut config = test_config();
        config.username = "user name".to_string();
        config.passkey = "pass&key".to_string();
        let tiers = build_request_tiers(&config, &request());
        assert!(tiers[0].ends_with("&username=user%20name&passkey=pass%26key"));
    }

    #[test]
    fn basic_credentials_are_recovered_from_the_request_url() {
        let url = request_url(&test_config(), "latest-torrents", &[21], "");
        assert_eq!(
            basic_credentials(&url).as_deref(),
            Some("someuser:somepass")
        );
    }

    #[test]
    fn the_result_limit_is_capped_at_the_api_page_size() {
        let mut req = request();
        req.limit = 1000;
        assert_eq!(result_limit(&req), MAX_PAGE_SIZE);
        req.limit = 25;
        assert_eq!(result_limit(&req), 25);
        req.limit = 0;
        assert_eq!(result_limit(&req), MAX_PAGE_SIZE);
    }

    // -- H3: delivery classification ----------------------------------------

    #[test]
    fn a_successful_json_delivery_returns_the_body() {
        let body = classify_response(200, &json_headers(), RECENT_FEED.as_bytes())
            .expect("a JSON 200 should be accepted");
        assert!(body.starts_with('['));
    }

    #[test]
    fn an_unauthorised_response_reports_an_auth_failure() {
        let error = classify_response(
            401,
            &json_headers(),
            br#"{"error":"Username and passkey cannot be empty."}"#,
        )
        .expect_err("401 should fail");
        assert_eq!(plugin_error(&error).code, PluginErrorCode::AuthFailed);
        assert!(plugin_error(&error).details.is_none());
    }

    #[test]
    fn a_forbidden_response_reports_an_auth_failure() {
        let error = classify_response(403, &json_headers(), br#"{"error":"Invalid passkey."}"#)
            .expect_err("403 should fail");
        assert_eq!(plugin_error(&error).code, PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_rate_limited_response_defers_with_the_retry_after_header() {
        let mut headers = json_headers();
        headers.insert("Retry-After".to_string(), "120".to_string());
        let error = classify_response(429, &headers, b"").expect_err("429 should fail");
        assert_eq!(plugin_error(&error).code, PluginErrorCode::RateLimited);
        assert_eq!(plugin_error(&error).retry_after_seconds, Some(120));
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::RateLimited,
                retry_after_seconds: Some(120),
            }
        ));
    }

    #[test]
    fn a_rate_limited_response_without_a_header_defers_for_the_hourly_window() {
        let error = classify_response(429, &json_headers(), b"").expect_err("429 should fail");
        assert_eq!(
            plugin_error(&error).retry_after_seconds,
            Some(RATE_LIMITED_FALLBACK_SECONDS)
        );
    }

    #[test]
    fn a_server_error_defers_as_an_upstream_failure() {
        for status in [500, 502, 503, 504] {
            let error =
                classify_response(status, &json_headers(), b"").expect_err("5xx should fail");
            assert_eq!(
                plugin_error(&error).code,
                PluginErrorCode::UpstreamUnavailable
            );
            assert!(matches!(
                details(&error),
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::UpstreamFailure,
                    ..
                }
            ));
        }
    }

    #[test]
    fn a_redirect_reports_the_base_url_as_unusable() {
        let mut headers = json_headers();
        headers.insert(
            "Location".to_string(),
            "https://filelist.io/login.php".to_string(),
        );
        let error = classify_response(302, &headers, b"").expect_err("3xx should fail");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("base_url"));
        assert!(error.debug_message.unwrap().contains("login.php"));
    }

    #[test]
    fn a_bad_request_reports_the_category_setting() {
        let error = classify_response(400, &json_headers(), br#"{"error":"Invalid category."}"#)
            .expect_err("400 should fail");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("categories"));
    }

    #[test]
    fn an_html_body_reports_an_unexpected_content_type() {
        let headers = BTreeMap::from([(
            "content-type".to_string(),
            "text/html; charset=UTF-8".to_string(),
        )]);
        let error = classify_response(200, &headers, b"<html>Just a moment...</html>")
            .expect_err("HTML should fail");
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
            }
        ));
    }

    #[test]
    fn a_missing_content_type_falls_back_to_sniffing_the_body() {
        let body = classify_response(200, &BTreeMap::new(), b"[]")
            .expect("a JSON body without a content type should be accepted");
        assert_eq!(body, "[]");

        let error = classify_response(200, &BTreeMap::new(), b"<html></html>")
            .expect_err("an HTML body without a content type should fail");
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
            }
        ));
    }

    #[test]
    fn malformed_json_reports_a_malformed_body() {
        let error = parse_torrents(&test_config(), "{not json").expect_err("garbage should fail");
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::MalformedBody
            }
        ));
    }

    #[test]
    fn a_non_array_root_reports_an_invalid_root() {
        let error =
            parse_torrents(&test_config(), r#"{"total":0}"#).expect_err("object root should fail");
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::InvalidRoot
            }
        ));
    }

    #[test]
    fn an_api_error_about_the_passkey_reports_an_auth_failure() {
        let error = parse_torrents(&test_config(), r#"{"error":"Invalid passkey!"}"#)
            .expect_err("an API error should fail");
        assert_eq!(plugin_error(&error).code, PluginErrorCode::AuthFailed);
    }

    #[test]
    fn an_api_error_about_the_rate_limit_defers() {
        let error = parse_torrents(
            &test_config(),
            r#"{"error":"Rate limit reached, try again later."}"#,
        )
        .expect_err("an API error should fail");
        assert_eq!(plugin_error(&error).code, PluginErrorCode::RateLimited);
    }

    #[test]
    fn an_unrecognised_api_error_defers_as_an_upstream_failure() {
        let error = parse_torrents(&test_config(), r#"{"error":"Boom."}"#)
            .expect_err("an API error should fail");
        assert_eq!(
            plugin_error(&error).code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    #[test]
    fn an_empty_result_array_is_not_an_error() {
        assert!(
            parse_torrents(&test_config(), "[]")
                .expect("an empty array should parse")
                .is_empty()
        );
    }

    // -- Configuration -------------------------------------------------------

    #[test]
    fn configuration_requires_at_least_one_category_list() {
        let error = FileListConfig::resolve(
            None,
            Some("u".to_string()),
            Some("p".to_string()),
            Some("".to_string()),
            None,
            None,
        )
        .expect_err("no categories should fail");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("categories"));
    }

    #[test]
    fn an_anime_only_configuration_is_accepted() {
        let config = FileListConfig::resolve(
            None,
            Some("u".to_string()),
            Some("p".to_string()),
            Some("".to_string()),
            Some("24".to_string()),
            None,
        )
        .expect("an anime-only configuration should be accepted");
        assert!(config.categories.is_empty());
        assert_eq!(config.anime_categories, vec![24]);
    }

    #[test]
    fn missing_credentials_are_reported_by_field() {
        let error = FileListConfig::resolve(None, None, Some("p".to_string()), None, None, None)
            .expect_err("a missing username should fail");
        assert!(plugin_error(&error).public_message.contains("username"));

        let error = FileListConfig::resolve(None, Some("u".to_string()), None, None, None, None)
            .expect_err("a missing passkey should fail");
        assert!(plugin_error(&error).public_message.contains("passkey"));
    }

    #[test]
    fn the_base_url_must_be_an_http_root() {
        for value in ["", "filelist.io", "ftp://filelist.io", "not a url"] {
            let error = FileListConfig::resolve(
                Some(value.to_string()),
                Some("u".to_string()),
                Some("p".to_string()),
                None,
                None,
                None,
            )
            .expect_err("a bad base URL should fail");
            let error = plugin_error(&error);
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
            assert!(error.public_message.contains("base_url"));
        }
    }

    #[test]
    fn a_base_url_keeps_its_path_and_loses_its_trailing_slash() {
        let config = FileListConfig::resolve(
            Some("https://mirror.example/fl/".to_string()),
            Some("u".to_string()),
            Some("p".to_string()),
            None,
            None,
            None,
        )
        .expect("a mirror with a path should be accepted");
        assert_eq!(config.base_url, "https://mirror.example/fl");
        assert_eq!(
            download_url(&config, "9"),
            "https://mirror.example/fl/download.php?id=9&passkey=p"
        );
        assert_eq!(
            info_url(&config, "9"),
            "https://mirror.example/fl/details.php?id=9"
        );
    }

    #[test]
    fn category_lists_accept_the_legacy_csv_form_and_drop_duplicates() {
        assert_eq!(parse_categories("23,21,27"), vec![23, 21, 27]);
        assert_eq!(parse_categories(" 23 ; 21 \n 27 "), vec![23, 21, 27]);
        assert_eq!(parse_categories("23,23,21"), vec![23, 21]);
        assert_eq!(parse_categories("not-a-number"), Vec::<i64>::new());
    }

    // -- M2/M3: descriptor honesty -------------------------------------------

    fn indexer_descriptor() -> IndexerDescriptor {
        match build_descriptor().provider {
            ProviderDescriptor::Indexer(descriptor) => descriptor,
            _ => panic!("filelist must describe itself as an indexer"),
        }
    }

    #[test]
    fn the_descriptor_reports_the_real_api_ceiling_and_torrent_features() {
        let descriptor = indexer_descriptor();
        let limits = descriptor.capabilities.limits.expect("limits");
        assert_eq!(limits.page_size, Some(100));
        assert_eq!(limits.max_page_size, Some(100));

        let torrent = descriptor.capabilities.torrent.expect("torrent caps");
        assert!(!torrent.reports_info_hash);
        assert!(!torrent.reports_magnet_uri);
        assert!(!torrent.supports_seed_requirements);
        assert!(torrent.reports_volume_factors);
        assert!(torrent.supports_private_tracker_flags);

        let features = descriptor
            .capabilities
            .response_features
            .expect("response features");
        assert!(!features.comments);
        assert!(features.grabs);
    }

    #[test]
    fn the_descriptor_claims_imdb_search_for_every_facet_it_serves() {
        let capabilities = indexer_descriptor().capabilities;
        assert!(capabilities.imdb_search);
        assert!(!capabilities.tvdb_search);
        assert!(!capabilities.anidb_search);
        for facet in ["series", "anime", "movie"] {
            assert_eq!(
                capabilities.supported_ids.get(facet).map(Vec::as_slice),
                Some(["imdb_id".to_string()].as_slice()),
                "{facet} should be searchable by IMDb id"
            );
            assert!(
                capabilities
                    .supported_query_facets
                    .contains(&facet.to_string())
            );
        }
        assert_eq!(capabilities.query_param.as_deref(), Some("query"));
        assert_eq!(capabilities.season_param.as_deref(), Some("season"));
        assert_eq!(capabilities.episode_param.as_deref(), Some("episode"));
    }

    #[test]
    fn the_category_fields_keep_their_keys_and_defaults_and_gain_options() {
        let fields = config_fields();
        let by_key = |key: &str| {
            fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("{key} must stay a config field"))
                .clone()
        };

        let categories = by_key("categories");
        assert_eq!(categories.field_type, ConfigFieldType::Tag);
        assert_eq!(
            categories.default_value.as_deref(),
            Some(DEFAULT_CATEGORIES)
        );
        let values: Vec<&str> = categories
            .options
            .iter()
            .map(|option| option.value.as_str())
            .collect();
        assert_eq!(values, vec!["24", "15", "27", "21", "23", "13", "28"]);

        let anime = by_key("anime_categories");
        assert_eq!(anime.field_type, ConfigFieldType::Tag);
        assert!(anime.default_value.is_none());
        assert!(!anime.required);

        let movies = by_key("movie_categories");
        assert_eq!(movies.field_type, ConfigFieldType::Tag);
        assert!(movies.options.iter().any(|option| option.value == "4"));

        // Existing keys are a public contract.
        for key in ["username", "passkey", "base_url", "minimum_seeders"] {
            let _ = by_key(key);
        }
        assert_eq!(by_key("passkey").field_type, ConfigFieldType::Password);
        assert_eq!(
            by_key("base_url").role,
            Some(ConfigFieldRole::ConnectionUrl)
        );
        assert_eq!(
            by_key("minimum_seeders").default_value.as_deref(),
            Some("1")
        );
    }

    #[test]
    fn the_declared_category_metadata_matches_the_published_table() {
        let model = indexer_descriptor()
            .capabilities
            .category_model
            .expect("category model");
        assert!(model.separate_anime_categories);
        let seriale_hd = model
            .categories
            .iter()
            .find(|descriptor| descriptor.value == "21")
            .expect("TV HD must be declared");
        assert_eq!(seriale_hd.label.as_deref(), Some("TV HD (Seriale HD)"));
        assert_eq!(seriale_hd.value_kind, IndexerCategoryValueKind::Numeric);
        assert_eq!(seriale_hd.facets, vec!["series".to_string()]);

        let anime = model
            .categories
            .iter()
            .find(|descriptor| descriptor.value == "24")
            .expect("Anime must be declared");
        assert!(anime.facets.contains(&"anime".to_string()));
        assert!(anime.facets.contains(&"movie".to_string()));
    }

    // -- Dedupe --------------------------------------------------------------

    #[test]
    fn duplicate_guids_are_collapsed_keeping_the_first() {
        let results = vec![
            SearchResult {
                guid: Some("FileList-1".to_string()),
                ..torrent_result("first", None)
            },
            SearchResult {
                guid: Some("FileList-1".to_string()),
                ..torrent_result("second", None)
            },
            SearchResult {
                guid: Some("FileList-2".to_string()),
                ..torrent_result("third", None)
            },
        ];
        let deduped = dedupe_results(results);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].title, "first");
        assert_eq!(deduped[1].title, "third");
    }
}
