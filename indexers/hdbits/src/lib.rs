//! HDBits (`hdbits.org`) indexer.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Indexers/HDBits` (request
//! generator, parser, settings, both recent-feed fixtures) and against
//! Prowlarr's newer `NzbDrone.Core/Indexers/Definitions/HDBits`, which is the
//! most current first-party integration with the same API. HDBits' own API
//! wiki is member-gated, so where Sonarr and Prowlarr disagree the newer
//! Prowlarr reading wins and the divergence is called out in the README.
//!
//! Shape of the integration:
//!
//! * one JSON endpoint, `POST {base_url}/api/torrents`, authenticated with the
//!   account `username` + `passkey` **inside the JSON body**;
//! * every field name on the wire is lower-case — Sonarr serialises the query
//!   with `CamelCasePropertyNamesContractResolver`
//!   (`NzbDrone.Common/Serializer/Newtonsoft.Json/Json.cs:29`), so `username`,
//!   `passkey`, `search`, `category`, `codec`, `medium`, `origin`, `limit` and
//!   the nested `tvdb`/`imdb` members are all lower-case;
//! * requests are organised as **tiers** (Sonarr's
//!   `IndexerPageableRequestChain`): the id tier runs first and a free-text
//!   tier only runs when the tier before it produced nothing. HDBits answers
//!   HTTP 403 "Rate-limit exceeded. Please try again in 15 minutes." once the
//!   account's query budget is spent, so a search must never fan out where a
//!   fall-through will do;
//! * one page of at most 100 results per call, exactly as Sonarr and Prowlarr
//!   do — see `MAX_PAGE_SIZE`.

use std::collections::{BTreeMap, HashMap};

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
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://hdbits.org";
/// Sonarr's `HDBitsSettings` default: TV + Documentary.
const DEFAULT_CATEGORIES: &str = "2,3";
/// `limit` is documented as 1..=100 and both Sonarr and Prowlarr pin it at 100.
const MAX_PAGE_SIZE: usize = 100;
/// HDBits' own 403 body says "Please try again in 15 minutes."
const FORBIDDEN_RATE_LIMIT_SECONDS: i64 = 900;
/// Sonarr's `HttpIndexerBase.FetchReleases` `minimumBackoff` when a rate limit
/// carries no window of its own.
const RATE_LIMITED_FALLBACK_SECONDS: i64 = 3_600;
/// Sonarr's `HttpIndexerBase.RateLimit` for every indexer.
const REQUEST_INTERVAL_MS: u64 = 2_000;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("scryer-hdbits-indexer/", env!("CARGO_PKG_VERSION"));

/// `type_category` for XXX content, which HDBits leeches and seeds neutrally.
const CATEGORY_XXX: i64 = 7;
/// `type_category` values HDBits half-freeleeches site-wide.
const HALF_LEECH_CATEGORIES: &[i64] = &[2, 3];
/// `type_medium` values HDBits half-freeleeches site-wide (full discs, captures
/// and remuxes).
const HALF_LEECH_MEDIUMS: &[i64] = &[1, 4, 5];
/// `type_origin` for an internal release.
const ORIGIN_INTERNAL: i64 = 1;
/// `type_medium` for a full disc, which never carries a usable scene filename.
const MEDIUM_FULL_DISC: i64 = 1;

// ---------------------------------------------------------------------------
// Published id tables
// ---------------------------------------------------------------------------

/// `HdBitsCategory` (Sonarr `HDBitsSettings.cs:80-98`, identical in Prowlarr).
const CATEGORY_LABELS: &[(i64, &str)] = &[
    (1, "Movie"),
    (2, "TV"),
    (3, "Documentary"),
    (4, "Music"),
    (5, "Sport"),
    (6, "Audio Track"),
    (7, "XXX"),
    (8, "Misc/Demo"),
];

/// The categories a series/anime search is allowed to use.
const SERIES_CATEGORY_IDS: &[i64] = &[2, 3, 5, 8];
/// The categories a movie search is allowed to use.
const MOVIE_CATEGORY_IDS: &[i64] = &[1, 3, 8];

/// `HdBitsCodec` (Sonarr `HDBitsSettings.cs:100-112`).
const CODEC_LABELS: &[(i64, &str)] = &[
    (1, "H.264"),
    (2, "MPEG-2"),
    (3, "VC-1"),
    (4, "XviD"),
    (5, "HEVC"),
];

/// `HdBitsMedium` (Sonarr `HDBitsSettings.cs:114-126`).
const MEDIUM_LABELS: &[(i64, &str)] = &[
    (1, "Blu-ray/HD DVD"),
    (3, "Encode"),
    (4, "Capture"),
    (5, "Remux"),
    (6, "WEB-DL"),
];

/// `HdBitsOrigin` (Prowlarr `HDBitsSettings.cs:87-93`; Sonarr models the same
/// values as a scalar it never fills).
const ORIGIN_LABELS: &[(i64, &str)] = &[(0, "Undefined"), (1, "Internal")];

fn label_for(table: &[(i64, &'static str)], id: i64) -> Option<&'static str> {
    table
        .iter()
        .find(|(value, _)| *value == id)
        .map(|(_, label)| *label)
}

fn options_for(table: &[(i64, &'static str)], ids: &[i64]) -> Vec<ConfigFieldOption> {
    ids.iter()
        .map(|id| ConfigFieldOption {
            value: id.to_string(),
            label: label_for(table, *id).unwrap_or("Unknown").to_string(),
            config_overrides: Default::default(),
        })
        .collect()
}

fn all_ids(table: &[(i64, &'static str)]) -> Vec<i64> {
    table.iter().map(|(id, _)| *id).collect()
}

fn category_descriptors() -> Vec<IndexerCategoryDescriptor> {
    CATEGORY_LABELS
        .iter()
        .map(|(id, label)| {
            let mut facets: Vec<String> = Vec::new();
            if SERIES_CATEGORY_IDS.contains(id) {
                facets.push("series".to_string());
                facets.push("anime".to_string());
            }
            if MOVIE_CATEGORY_IDS.contains(id) {
                facets.push("movie".to_string());
            }
            IndexerCategoryDescriptor {
                value: id.to_string(),
                label: Some((*label).to_string()),
                value_kind: IndexerCategoryValueKind::Numeric,
                facets,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "hdbits".to_string(),
        name: "HDBits Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: "hdbits".to_string(),
            provider_aliases: vec!["hdbits.org".to_string()],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            strategy_plan: Some(scryer_plugin_sdk::IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 2,
            }),
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                // `tvdb` is the only id HDBits accepts for TV and `imdb` the
                // only one it accepts for film. Sending an IMDb id with a TV
                // query is answered with status 9 `ImdbTvNotAllowed`, so the
                // two never mix.
                supported_ids: HashMap::from([
                    ("series".to_string(), vec!["tvdb_id".to_string()]),
                    ("anime".to_string(), vec!["tvdb_id".to_string()]),
                    ("movie".to_string(), vec!["imdb_id".to_string()]),
                ]),
                deduplicates_aliases: false,
                season_param: Some("season".to_string()),
                episode_param: Some("episode".to_string()),
                // HDBits' free-text parameter, which Prowlarr uses for every
                // query that carries no usable id.
                query_param: Some("search".to_string()),
                supported_query_facets: vec![
                    "series".to_string(),
                    "anime".to_string(),
                    "movie".to_string(),
                ],
                search: true,
                // Movie searches only; see `supported_ids`.
                imdb_search: true,
                tvdb_search: true,
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
                    IndexerSearchInput::AirDate,
                    IndexerSearchInput::Limit,
                ],
                supported_external_ids: vec!["tvdb_id".to_string(), "imdb_id".to_string()],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::Numeric],
                    // HDBits files anime under the ordinary TV category; there
                    // is no separate anime category to configure.
                    separate_anime_categories: false,
                    provider_category_metadata: true,
                    categories: category_descriptors(),
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(MAX_PAGE_SIZE as u32),
                    max_page_size: Some(MAX_PAGE_SIZE as u32),
                    // One page per call, as Sonarr's HDBits generator yields
                    // exactly one request per pageable entry.
                    max_pages: Some(1),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: false,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    reports_peers: true,
                    reports_leechers: true,
                    reports_info_hash: true,
                    // The API publishes no magnet URI.
                    reports_magnet_uri: false,
                    reports_volume_factors: true,
                    supports_private_tracker_flags: true,
                    // HDBits publishes no per-torrent seed ratio or seed time,
                    // so the plugin never fills those fields.
                    supports_seed_requirements: false,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: false,
                    subtitles: false,
                    grabs: true,
                    votes: false,
                    // A comment COUNT is reported, never a comment page.
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
        connection_field(
            "base_url",
            "API URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("HDBits site URL"),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            None,
            Some("HDBits account username"),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("HDBits passkey (Profile -> Security -> Passkey)"),
        ),
        tag_field(
            "categories",
            "Categories",
            options_for(CATEGORY_LABELS, SERIES_CATEGORY_IDS),
            Some(DEFAULT_CATEGORIES),
            true,
            "HDBits category IDs searched for series and anime. Comma-separated \
             IDs are still accepted.",
        ),
        tag_field(
            "movie_categories",
            "Movie Categories",
            options_for(CATEGORY_LABELS, MOVIE_CATEGORY_IDS),
            None,
            false,
            "HDBits category IDs searched for movies. Leave empty to skip movie \
             searches entirely.",
        ),
        tag_field(
            "codecs",
            "Codecs",
            options_for(CODEC_LABELS, &all_ids(CODEC_LABELS)),
            None,
            false,
            "Restrict every search to these HDBits codec IDs.",
        ),
        tag_field(
            "mediums",
            "Mediums",
            options_for(MEDIUM_LABELS, &all_ids(MEDIUM_LABELS)),
            None,
            false,
            "Restrict every search to these HDBits medium IDs.",
        ),
        tag_field(
            "origins",
            "Origins",
            options_for(ORIGIN_LABELS, &all_ids(ORIGIN_LABELS)),
            None,
            false,
            "Restrict every search to these HDBits origin IDs (0 undefined, 1 internal).",
        ),
        field(
            "use_filenames",
            "Use Filenames",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some(
                "Report the torrent's scene filename as the release title instead of the \
                 uploader's display name. Never used for XXX content or full discs.",
            ),
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
    let config = HdbitsConfig::from_host()?;
    let limit = result_limit(&request);

    // Sonarr's tier chain: run a tier, and only fall through to the next one
    // when the tier before it produced nothing (`HttpIndexerBase.cs:145-204`
    // breaks out of the tier loop as soon as `releases.Any()`).
    for query in build_query_tiers(&config, &request) {
        let body = post_query(&config, &query).await?;
        let results = parse_response(&config, &body)?;
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
        // HDBits files anime under the same TV categories as everything else.
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

/// A daily series carries the air year in the `season` slot (TheTVDB numbers
/// daily shows by year), so a season number in this window is a year and not a
/// season. Sonarr reaches the same shape through `DailySeasonSearchCriteria`.
fn looks_like_air_year(season: u32) -> bool {
    (1900..=2200).contains(&season)
}

/// `air_date` normalised to HDBits' `yyyy-MM-dd` search term. The host does not
/// fill `context.air_date` today (see the README), so this only fires against a
/// future host that does.
fn air_date_term(request: &SearchRequest) -> Option<String> {
    let raw = request.context.as_ref()?.air_date.as_deref()?.trim();
    let date = raw.split(['T', ' ']).next()?.trim();
    matches_mask(date, "0000-00-00").then(|| date.to_string())
}

/// Prowlarr's `SanitizedSearchTerm` for a free-text HDBits query: every run of
/// non-word characters collapses to a single space
/// (`HDBitsRequestGenerator.cs:30`).
fn sanitize_search_term(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut pending_space = false;
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        } else {
            pending_space = true;
        }
    }
    out
}

/// The request tiers for one search, most specific first. Each entry is one
/// upstream call; the caller stops at the first tier that returns releases.
fn build_query_tiers(config: &HdbitsConfig, request: &SearchRequest) -> Vec<TorrentQuery> {
    let base = |categories: &[i64]| TorrentQuery {
        username: config.username.clone(),
        passkey: config.api_key.clone(),
        category: categories.to_vec(),
        codec: config.codecs.clone(),
        medium: config.mediums.clone(),
        origin: config.origins.clone(),
        limit: Some(MAX_PAGE_SIZE as i64),
        ..TorrentQuery::default()
    };

    if is_recent_request(request) {
        // Sonarr polls the recent feed with an otherwise empty query. Scryer
        // serves every facet from one configured indexer, so the movie list
        // joins the series list for the poll.
        let categories = union_categories(&[&config.categories, &config.movie_categories]);
        if categories.is_empty() {
            return Vec::new();
        }
        return vec![base(&categories)];
    }

    let facet = facet_kind(request);
    let categories = match facet {
        FacetKind::Series => &config.categories,
        FacetKind::Movie => &config.movie_categories,
    };
    // Sonarr's `GetRequest` yields nothing when the category list for the
    // criteria is empty, which is how a TV-only configuration opts out of
    // movie searches.
    if categories.is_empty() {
        return Vec::new();
    }

    let mut tiers: Vec<TorrentQuery> = Vec::new();

    match facet {
        FacetKind::Series => {
            if let Some(tvdb_id) = numeric_id(request, "tvdb_id") {
                if let Some(air_date) = air_date_term(request) {
                    // Sonarr's daily-episode criteria replaces the
                    // season/episode scoping with the air date
                    // (`HDBitsRequestGenerator.cs:75`).
                    let mut query = base(categories);
                    query.tvdb = Some(TvdbQuery {
                        id: Some(tvdb_id),
                        season: None,
                        episode: None,
                    });
                    query.search = Some(air_date);
                    tiers.push(query);
                } else {
                    let mut query = base(categories);
                    query.tvdb = Some(TvdbQuery {
                        id: Some(tvdb_id),
                        season: request.season.map(i64::from),
                        episode: request.episode.map(i64::from),
                    });
                    tiers.push(query);

                    // Sonarr's daily-season criteria searches `"{year}-"`
                    // (`HDBitsRequestGenerator.cs:90`). TheTVDB numbers daily
                    // shows by year, so the precise season query above is
                    // tried first and this only runs when it found nothing.
                    if let Some(season) = request
                        .season
                        .filter(|season| request.episode.is_none() && looks_like_air_year(*season))
                    {
                        let mut query = base(categories);
                        query.tvdb = Some(TvdbQuery {
                            id: Some(tvdb_id),
                            season: None,
                            episode: None,
                        });
                        query.search = Some(format!("{season}-"));
                        tiers.push(query);
                    }
                }
            }
        }
        FacetKind::Movie => {
            if let Some(imdb_id) = numeric_id(request, "imdb_id") {
                let mut query = base(categories);
                query.imdb = Some(ImdbQuery { id: Some(imdb_id) });
                tiers.push(query);
            }
        }
    }

    // Trailing tier: HDBits' free-text `search`. Sonarr never sends one;
    // Prowlarr sends it whenever the criteria carry no usable id
    // (`HDBitsRequestGenerator.cs:28-31,57-60,99-102`).
    if let Some(term) = free_text_term(request) {
        let mut query = base(categories);
        query.search = Some(term);
        tiers.push(query);
    }

    tiers
}

/// The `search` term for the free-text tier.
///
/// Sonarr loops `SearchCriteria.SceneTitles`. Scryer's core never fills
/// `context.scene_titles` and instead dispatches one `freetext_alias` strategy
/// per alias, so the plugin issues exactly one free-text query per call and
/// lets the host own the alias fan-out.
fn free_text_term(request: &SearchRequest) -> Option<String> {
    let sanitized = sanitize_search_term(request.query.trim());
    (!sanitized.is_empty()).then_some(sanitized)
}

/// HDBits' id members are integers. A non-numeric or zero id is no id at all,
/// matching Sonarr's `TryAddSearchParameters` (`TvdbId != 0`) and Prowlarr's
/// `ParseUtil.GetImdbId(...).GetValueOrDefault(0)`.
fn numeric_id(request: &SearchRequest, key: &str) -> Option<i64> {
    let raw = request.ids.get(key)?.trim();
    let digits = raw
        .trim_start_matches("tt")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let value = digits.parse::<i64>().ok()?;
    (value > 0).then_some(value)
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

async fn post_query(config: &HdbitsConfig, query: &TorrentQuery) -> Result<String, Error> {
    StartRateGate::new("hdbits.request-start", 1, REQUEST_INTERVAL_MS)
        .acquire()
        .await
        .map_err(component::deadline_deferred_error)?;

    let body = serde_json::to_vec(query).map_err(|error| {
        typed_error(
            PluginErrorCode::Permanent,
            "HDBits query could not be encoded".to_string(),
            format!("HDBits query serialisation failed: {error}"),
            None,
            None,
        )
    })?;

    let response = component::http(PluginHttpRequest {
        url: api_url(config),
        method: Some("POST".to_string()),
        headers: BTreeMap::from([
            ("Accept".to_string(), "application/json".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ]),
        body,
    })
    .await
    .map_err(|error| {
        deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "HDBits could not be reached".to_string(),
            format!("HDBits request failed: {error:?}"),
        )
    })?;

    classify_response(response.status, &response.headers, &response.body)
}

/// Map one HTTP delivery onto Scryer's typed indexer error lanes.
///
/// HDBits answers an exhausted query budget with **403** and the body
/// "Rate-limit exceeded. Please try again in 15 minutes." — Prowlarr's parser
/// turns exactly that into `RequestLimitReachedException`
/// (`HDBitsParser.cs:30-33`). Credential faults arrive as `status: 5` inside a
/// 200 body instead, so a 403 that does not read like a rate limit is the only
/// case left for an auth verdict.
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
                    "HDBits redirected the API call with HTTP {status} to {location}; the \
                     configured site URL is not the API root"
                ),
            ));
        }
        401 => {
            return Err(auth_failed_error(format!(
                "HDBits rejected the account credentials with HTTP 401: {}",
                body_excerpt(body)
            )));
        }
        403 => {
            let excerpt = body_excerpt(body);
            if looks_like_rate_limit(&excerpt) {
                return Err(rate_limited_error(
                    retry_after_seconds(headers)
                        .or_else(|| retry_minutes_from_body(&excerpt).map(|value| value * 60))
                        .unwrap_or(FORBIDDEN_RATE_LIMIT_SECONDS),
                    format!("HDBits returned HTTP 403: {excerpt}"),
                ));
            }
            return Err(auth_failed_error(format!(
                "HDBits refused the API call with HTTP 403: {excerpt}"
            )));
        }
        429 => {
            return Err(rate_limited_error(
                retry_after_seconds(headers).unwrap_or(RATE_LIMITED_FALLBACK_SECONDS),
                format!("HDBits returned HTTP 429: {}", body_excerpt(body)),
            ));
        }
        _ => {
            return Err(deferred_error(
                IndexerSearchIncompleteReason::UpstreamFailure,
                None,
                format!("HDBits returned HTTP {status}"),
                format!("HDBits returned HTTP {status}: {}", body_excerpt(body)),
            ));
        }
    }

    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            format!(
                "HDBits returned {} bytes, above the {MAX_RESPONSE_BYTES} byte ceiling",
                body.len()
            ),
        ));
    }

    let text = std::str::from_utf8(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("HDBits response was not valid UTF-8: {error}"),
        )
    })?;

    if !is_json_delivery(headers, text) {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            format!(
                "HDBits returned content type {:?} instead of JSON; the site is likely blocked \
                 or behind an interstitial",
                header_value(headers, "content-type").unwrap_or("(absent)")
            ),
        ));
    }

    Ok(text.to_string())
}

fn looks_like_rate_limit(excerpt: &str) -> bool {
    let lowered = excerpt.to_ascii_lowercase();
    lowered.contains("rate-limit")
        || lowered.contains("rate limit")
        || lowered.contains("too many")
        || lowered.contains("try again")
        || lowered.contains("query limit")
}

/// "Please try again in 15 minutes." → 15.
fn retry_minutes_from_body(excerpt: &str) -> Option<i64> {
    let lowered = excerpt.to_ascii_lowercase();
    let index = lowered.find("minute")?;
    let reversed: String = lowered[..index]
        .chars()
        .rev()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    let digits: String = reversed.chars().rev().collect();
    digits.parse::<i64>().ok().filter(|value| *value > 0)
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

fn retry_after_seconds(headers: &BTreeMap<String, String>) -> Option<i64> {
    header_value(headers, "retry-after")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
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
        format!("HDBits setting '{field}' is not usable"),
        detail,
        None,
        None,
    )
}

fn auth_failed_error(detail: String) -> Error {
    typed_error(
        PluginErrorCode::AuthFailed,
        "HDBits rejected the configured 'username' and 'api_key'".to_string(),
        detail,
        None,
        None,
    )
}

fn rate_limited_error(retry_after_seconds: i64, detail: String) -> Error {
    typed_error(
        PluginErrorCode::RateLimited,
        "HDBits query limit reached; searches are paused until it resets".to_string(),
        detail,
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
        "HDBits returned a response Scryer could not read".to_string(),
        detail,
        None,
        Some(IndexerSearchPluginError::InvalidResponse { kind }),
    )
}

/// The documented `status` table (`HDBitsApi.cs:117-129`, identical in Sonarr,
/// Radarr and Prowlarr).
///
/// The 3/6/7/8/9 codes all say the *request* was wrong, which is a permanent
/// fault the host must surface rather than a transient one it should retry.
fn classify_api_status(status: i64, message: &str) -> Error {
    let message = message.trim();
    let detail = if message.is_empty() {
        format!("HDBits API returned status {status}")
    } else {
        format!("HDBits API returned status {status}: {message}")
    };
    match status {
        1 => deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "HDBits reported a general failure".to_string(),
            detail,
        ),
        2 => invalid_config_error(
            "base_url",
            format!("{detail} (HDBits requires an https:// site URL)"),
        ),
        4 => invalid_config_error(
            "username",
            format!("{detail} (both 'username' and 'api_key' must be configured)"),
        ),
        5 => auth_failed_error(detail),
        3 | 6 | 7 | 8 | 9 => typed_error(
            PluginErrorCode::Permanent,
            "HDBits rejected the search request".to_string(),
            detail,
            None,
            None,
        ),
        _ => deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "HDBits reported an unknown API status".to_string(),
            detail,
        ),
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

fn parse_response(config: &HdbitsConfig, body: &str) -> Result<Vec<SearchResult>, Error> {
    let response: HdbitsResponse = serde_json::from_str(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("HDBits JSON parse failed: {error}"),
        )
    })?;

    let status = response
        .status
        .as_ref()
        .and_then(json_i64)
        .unwrap_or_default();
    if status != 0 {
        return Err(classify_api_status(
            status,
            response.message.as_deref().unwrap_or_default(),
        ));
    }

    // Sonarr: "Indexer API call response missing result data".
    let Some(items) = response.data.as_ref().and_then(|data| data.as_array()) else {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::InvalidRoot,
            "HDBits response carried no torrent array in 'data'".to_string(),
        ));
    };

    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let torrent: HdbitsTorrent = serde_json::from_value(item.clone()).map_err(|error| {
            invalid_response_error(
                IndexerSearchInvalidResponseKind::MalformedBody,
                format!("HDBits torrent entry could not be read: {error}"),
            )
        })?;
        if let Some(result) = torrent_to_result(config, torrent) {
            results.push(result);
        }
    }
    Ok(results)
}

/// HDBits' site-wide leech economics, ported from Prowlarr's
/// `HDBitsParser.GetDownloadVolumeFactor`/`GetUploadVolumeFactor`
/// (`HDBitsParser.cs:117-143`). Sonarr models none of this and therefore never
/// tells Scryer that a default (TV + Documentary) HDBits release is half
/// freeleech.
fn volume_factors(freeleech: bool, category: i64, medium: i64, origin: i64) -> (f64, f64) {
    if category == CATEGORY_XXX {
        // 100% neutral leech.
        return (0.0, 0.0);
    }
    if freeleech {
        return (0.0, 1.0);
    }
    if HALF_LEECH_MEDIUMS.contains(&medium)
        || origin == ORIGIN_INTERNAL
        || HALF_LEECH_CATEGORIES.contains(&category)
    {
        return (0.5, 1.0);
    }
    (1.0, 1.0)
}

/// Prowlarr's `GetTitle` (`HDBitsParser.cs:92-98`): the scene filename is a
/// better release title than the uploader's display name, except for XXX
/// content and full discs where HDBits' filenames are not release names.
fn release_title(
    use_filenames: bool,
    name: &str,
    filename: &str,
    category: i64,
    medium: i64,
) -> String {
    let filename = filename.trim();
    if use_filenames
        && category != CATEGORY_XXX
        && medium != MEDIUM_FULL_DISC
        && !filename.is_empty()
    {
        let stripped = strip_torrent_suffix(filename);
        if !stripped.is_empty() {
            return stripped.to_string();
        }
    }
    name.trim().to_string()
}

/// Prowlarr strips `.torrent` case-insensitively.
fn strip_torrent_suffix(filename: &str) -> &str {
    const SUFFIX: &str = ".torrent";
    if filename.len() >= SUFFIX.len()
        && filename.is_char_boundary(filename.len() - SUFFIX.len())
        && filename[filename.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
    {
        return &filename[..filename.len() - SUFFIX.len()];
    }
    filename
}

/// Sonarr's `IsValidRelease` (`HttpIndexerBase.cs:305-320`): an entry with no
/// id or no title is dropped rather than surfaced.
fn torrent_to_result(config: &HdbitsConfig, torrent: HdbitsTorrent) -> Option<SearchResult> {
    let id = torrent.id.as_ref().and_then(json_text)?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return None;
    }

    let name = torrent
        .name
        .as_ref()
        .and_then(json_text)
        .unwrap_or_default();
    let filename = torrent
        .filename
        .as_ref()
        .and_then(json_text)
        .unwrap_or_default();
    let category = torrent
        .type_category
        .as_ref()
        .and_then(json_i64)
        .unwrap_or(0);
    let codec = torrent.type_codec.as_ref().and_then(json_i64).unwrap_or(0);
    let medium = torrent.type_medium.as_ref().and_then(json_i64).unwrap_or(0);
    let origin = torrent.type_origin.as_ref().and_then(json_i64).unwrap_or(0);
    let exclusive = torrent
        .type_exclusive
        .as_ref()
        .and_then(json_i64)
        .unwrap_or(0);

    let title = release_title(config.use_filenames, &name, &filename, category, medium);
    if title.is_empty() {
        return None;
    }

    let seeders = torrent.seeders.as_ref().and_then(json_i64).unwrap_or(0);
    let leechers = torrent.leechers.as_ref().and_then(json_i64).unwrap_or(0);
    let times_completed = torrent.times_completed.as_ref().and_then(json_i64);

    // Sonarr's `GetIndexerFlags` reads `freeleech == "yes"` verbatim.
    let freeleech = torrent
        .freeleech
        .as_ref()
        .and_then(json_text)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("yes"));
    let (download_volume_factor, upload_volume_factor) =
        volume_factors(freeleech, category, medium, origin);
    let is_freeleech =
        download_volume_factor.abs() < f64::EPSILON && upload_volume_factor > f64::EPSILON;

    let mut tags: Vec<String> = Vec::new();
    if origin == ORIGIN_INTERNAL {
        tags.push("internal".to_string());
    }
    if exclusive == 1 {
        tags.push("exclusive".to_string());
    }
    if (download_volume_factor - 0.5).abs() < f64::EPSILON {
        // The fleet's name for a 50% download factor
        // (`indexers/newznab-common/src/lib.rs`).
        tags.push("halfleech".to_string());
    }

    let mut external_ids = HashMap::new();
    if let Some(tvdb_id) = torrent
        .tvdb
        .as_ref()
        .and_then(|tvdb| tvdb.id.as_ref())
        .and_then(json_i64)
        .filter(|value| *value > 0)
    {
        external_ids.insert("tvdb_id".to_string(), tvdb_id.to_string());
    }
    if let Some(imdb_id) = torrent
        .imdb
        .as_ref()
        .and_then(|imdb| imdb.id.as_ref())
        .and_then(json_i64)
        .filter(|value| *value > 0)
    {
        external_ids.insert("imdb_id".to_string(), format!("tt{imdb_id:07}"));
    }

    let mut provider_extra: HashMap<String, serde_json::Value> = HashMap::new();
    // `extra["freeleech"]` (bool) and `extra["tags"]` (array) are the keys the
    // core actually reads; `indexer_flags` alone is stored and never consulted.
    provider_extra.insert(
        "freeleech".to_string(),
        serde_json::Value::from(is_freeleech),
    );
    if !tags.is_empty() {
        provider_extra.insert("tags".to_string(), serde_json::Value::from(tags.clone()));
    }
    provider_extra.insert(
        "type_category".to_string(),
        serde_json::Value::from(category),
    );
    provider_extra.insert("type_codec".to_string(), serde_json::Value::from(codec));
    provider_extra.insert("type_medium".to_string(), serde_json::Value::from(medium));
    provider_extra.insert("type_origin".to_string(), serde_json::Value::from(origin));
    provider_extra.insert(
        "type_exclusive".to_string(),
        serde_json::Value::from(exclusive),
    );
    if let Some(label) = label_for(CATEGORY_LABELS, category) {
        provider_extra.insert("category".to_string(), serde_json::Value::from(label));
    }
    if let Some(label) = label_for(CODEC_LABELS, codec) {
        provider_extra.insert("codec".to_string(), serde_json::Value::from(label));
    }
    if let Some(label) = label_for(MEDIUM_LABELS, medium) {
        provider_extra.insert("medium".to_string(), serde_json::Value::from(label));
    }
    if !name.trim().is_empty() {
        provider_extra.insert("name".to_string(), serde_json::Value::from(name.trim()));
    }
    if !filename.is_empty() {
        provider_extra.insert("filename".to_string(), serde_json::Value::from(filename));
    }
    if let Some(numfiles) = torrent.numfiles.as_ref().and_then(json_i64) {
        provider_extra.insert("numfiles".to_string(), serde_json::Value::from(numfiles));
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
    if let Some(tvdb) = torrent.tvdb.as_ref() {
        if let Some(season) = tvdb.season.as_ref().and_then(json_i64) {
            provider_extra.insert("tvdb_season".to_string(), serde_json::Value::from(season));
        }
        if let Some(episode) = tvdb.episode.as_ref().and_then(json_text) {
            provider_extra.insert("tvdb_episode".to_string(), serde_json::Value::from(episode));
        }
    }
    if let Some(imdb) = torrent.imdb.as_ref() {
        if let Some(value) = imdb.english_title.as_ref().and_then(json_text) {
            provider_extra.insert("imdb_english_title".to_string(), value.into());
        }
        if let Some(value) = imdb.original_title.as_ref().and_then(json_text) {
            provider_extra.insert("imdb_original_title".to_string(), value.into());
        }
        if let Some(value) = imdb.year.as_ref().and_then(json_i64) {
            provider_extra.insert("imdb_year".to_string(), serde_json::Value::from(value));
        }
        if let Some(value) = imdb.rating.as_ref().and_then(json_f64) {
            provider_extra.insert("imdb_rating".to_string(), serde_json::Value::from(value));
        }
        if !imdb.genres.is_empty() {
            provider_extra.insert(
                "imdb_genres".to_string(),
                serde_json::Value::from(imdb.genres.clone()),
            );
        }
    }

    // `utadded` is an exact UTC instant; `added` is the same moment written
    // with an RFC 822 offset the core's RFC 3339 parser cannot read.
    let published_at = torrent
        .utadded
        .as_ref()
        .and_then(json_i64)
        .and_then(unix_to_rfc3339)
        .or_else(|| {
            torrent
                .added
                .as_ref()
                .and_then(json_text)
                .as_deref()
                .and_then(normalize_published_at)
        });

    Some(SearchResult {
        download_url: Some(download_url(config, &id)),
        info_url: Some(info_url(config, &id)),
        // HDBits exposes no separate comment page, only a comment count.
        comment_url: None,
        guid: Some(format!("HDBits-{id}")),
        size_bytes: torrent.size.as_ref().and_then(json_i64),
        published_at,
        grabs: times_completed,
        seeders: Some(seeders),
        peers: Some(seeders + leechers),
        leechers: Some(leechers),
        info_hash_v1: torrent
            .hash
            .as_ref()
            .and_then(json_text)
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty()),
        download_volume_factor: Some(download_volume_factor),
        upload_volume_factor: Some(upload_volume_factor),
        indexer_flags: derive_indexer_flags(
            Some(download_volume_factor),
            Some(upload_volume_factor),
            &tags,
            None,
        ),
        external_ids,
        // The numeric id is the provider's own value; the label is what the
        // core's category/facet contradiction rule can actually read.
        provider_categories: vec![category.to_string()],
        categories: label_for(CATEGORY_LABELS, category)
            .map(|label| vec![label.to_string()])
            .unwrap_or_default(),
        provider_extra,
        ..torrent_result(title, None)
    })
}

fn api_url(config: &HdbitsConfig) -> String {
    format!("{}/api/torrents", config.base_url)
}

/// Sonarr builds both links from the configured base with `HttpUri.CombinePath`
/// so a mirror or reverse-proxy path stays honoured.
fn download_url(config: &HdbitsConfig, id: &str) -> String {
    format!(
        "{}/download.php?id={}&passkey={}",
        config.base_url,
        percent_encode(id),
        percent_encode(&config.api_key)
    )
}

fn info_url(config: &HdbitsConfig, id: &str) -> String {
    format!("{}/details.php?id={}", config.base_url, percent_encode(id))
}

fn percent_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
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
// Dates
// ---------------------------------------------------------------------------

/// Scryer's RSS staleness tracker parses `published_at` with
/// `DateTime::parse_from_rfc3339` only
/// (`crates/scryer-infrastructure-acquisition/src/indexers/search_client.rs`),
/// so a value has to leave this plugin as RFC 3339 — and `utadded` is an exact
/// UTC instant, which makes it the better of the two fields HDBits publishes.
fn unix_to_rfc3339(timestamp: i64) -> Option<String> {
    if timestamp <= 0 {
        return None;
    }
    let days = timestamp.div_euclid(86_400);
    let seconds_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = month_position + if month_position < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

/// `added` is `2015-04-04T20:30:46+0000` — an RFC 822 style offset with no
/// colon, which `parse_from_rfc3339` rejects. HDBits has also been seen using
/// the `YYYY-MM-DD HH:MM:SS` form.
fn normalize_published_at(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (date, rest) = raw.split_once(['T', ' '])?;
    let date = date.trim();
    if !matches_mask(date, "0000-00-00") {
        return None;
    }
    let (time, zone) = split_zone(rest.trim());
    let time = match time.len() {
        5 if matches_mask(time, "00:00") => format!("{time}:00"),
        8 if matches_mask(time, "00:00:00") => time.to_string(),
        _ => return None,
    };
    Some(format!("{date}T{time}{zone}"))
}

/// Split the trailing zone designator off a time and normalise it to RFC 3339.
/// An absent zone is read as UTC, which is what Sonarr's
/// `DateTimeZoneHandling.Utc` does with HDBits' own `+0000` stamps.
fn split_zone(rest: &str) -> (&str, String) {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return (time, "Z".to_string());
    }
    for (index, ch) in rest.char_indices() {
        if (ch == '+' || ch == '-') && index > 0 {
            let (time, zone) = rest.split_at(index);
            let sign = &zone[..1];
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            return match digits.len() {
                4 if digits == "0000" => (time, "Z".to_string()),
                4 => (time, format!("{sign}{}:{}", &digits[..2], &digits[2..])),
                2 if digits == "00" => (time, "Z".to_string()),
                2 => (time, format!("{sign}{digits}:00")),
                _ => (time, "Z".to_string()),
            };
        }
    }
    (rest, "Z".to_string())
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

// ---------------------------------------------------------------------------
// Lenient JSON scalars
// ---------------------------------------------------------------------------

/// HDBits types its JSON loosely: Sonarr ships two fixtures of the same feed,
/// one with `"id": 257142` and one with `"id": "257142"`, and Newtonsoft
/// coerces either. A strict `String`/`i64` mapping rejects the whole array on
/// the first mismatch, so every scalar is read through this enum.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum JsonScalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
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

fn json_f64(value: &JsonScalar) -> Option<f64> {
    match value {
        JsonScalar::Int(value) => Some(*value as f64),
        JsonScalar::Float(value) => Some(*value),
        JsonScalar::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        JsonScalar::Text(text) => text.trim().parse::<f64>().ok(),
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// The `POST /api/torrents` body.
///
/// Every key is lower-case: Sonarr serialises `TorrentQuery` through
/// `Json.ToJson`, whose `CamelCasePropertyNamesContractResolver` lower-cases
/// the first letter of every property.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
struct TorrentQuery {
    username: String,
    passkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    category: Vec<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    codec: Vec<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    medium: Vec<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    origin: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    imdb: Option<ImdbQuery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tvdb: Option<TvdbQuery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
struct TvdbQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    season: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    episode: Option<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
struct ImdbQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HdbitsResponse {
    #[serde(default)]
    status: Option<JsonScalar>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// One entry of the `data` array.
#[derive(Debug, Default, Deserialize)]
struct HdbitsTorrent {
    #[serde(default)]
    id: Option<JsonScalar>,
    #[serde(default)]
    hash: Option<JsonScalar>,
    #[serde(default)]
    name: Option<JsonScalar>,
    #[serde(default)]
    filename: Option<JsonScalar>,
    #[serde(default)]
    size: Option<JsonScalar>,
    #[serde(default)]
    seeders: Option<JsonScalar>,
    #[serde(default)]
    leechers: Option<JsonScalar>,
    #[serde(default)]
    times_completed: Option<JsonScalar>,
    #[serde(default)]
    comments: Option<JsonScalar>,
    #[serde(default)]
    numfiles: Option<JsonScalar>,
    #[serde(default)]
    utadded: Option<JsonScalar>,
    #[serde(default)]
    added: Option<JsonScalar>,
    #[serde(default)]
    freeleech: Option<JsonScalar>,
    #[serde(default)]
    type_category: Option<JsonScalar>,
    #[serde(default)]
    type_codec: Option<JsonScalar>,
    #[serde(default)]
    type_medium: Option<JsonScalar>,
    #[serde(default)]
    type_origin: Option<JsonScalar>,
    #[serde(default)]
    type_exclusive: Option<JsonScalar>,
    #[serde(default)]
    imdb: Option<ImdbInfo>,
    #[serde(default)]
    tvdb: Option<TvdbInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct ImdbInfo {
    #[serde(default)]
    id: Option<JsonScalar>,
    #[serde(default, rename = "englishtitle")]
    english_title: Option<JsonScalar>,
    #[serde(default, rename = "originaltitle")]
    original_title: Option<JsonScalar>,
    #[serde(default)]
    year: Option<JsonScalar>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    rating: Option<JsonScalar>,
}

#[derive(Debug, Default, Deserialize)]
struct TvdbInfo {
    #[serde(default)]
    id: Option<JsonScalar>,
    #[serde(default)]
    season: Option<JsonScalar>,
    #[serde(default)]
    episode: Option<JsonScalar>,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct HdbitsConfig {
    /// Always without a trailing slash.
    base_url: String,
    username: String,
    api_key: String,
    categories: Vec<i64>,
    movie_categories: Vec<i64>,
    codecs: Vec<i64>,
    mediums: Vec<i64>,
    origins: Vec<i64>,
    use_filenames: bool,
}

impl HdbitsConfig {
    fn from_host() -> Result<Self, Error> {
        Self::resolve(HostSettings {
            base_url: config_value("base_url"),
            username: config_value("username"),
            api_key: config_value("api_key"),
            categories: config_value("categories"),
            movie_categories: config_value("movie_categories"),
            codecs: config_value("codecs"),
            mediums: config_value("mediums"),
            origins: config_value("origins"),
            use_filenames: config_value("use_filenames"),
        })
    }

    /// The pure half of configuration resolution, mirroring
    /// `HDBitsSettingsValidator` (`HDBitsSettings.cs:11-20`) plus Prowlarr's
    /// `Username` `NotEmpty` rule: a root URL, a username, a passkey and at
    /// least one category.
    fn resolve(settings: HostSettings) -> Result<Self, Error> {
        let base_url =
            normalize_base_url(settings.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))?;
        let username = settings.username.ok_or_else(|| {
            invalid_config_error(
                "username",
                "HDBits requires an account username".to_string(),
            )
        })?;
        let api_key = settings.api_key.ok_or_else(|| {
            invalid_config_error("api_key", "HDBits requires an account passkey".to_string())
        })?;

        let categories = parse_ids(settings.categories.as_deref().unwrap_or(DEFAULT_CATEGORIES));
        let movie_categories = parse_ids(settings.movie_categories.as_deref().unwrap_or_default());
        if categories.is_empty() && movie_categories.is_empty() {
            return Err(invalid_config_error(
                "categories",
                "either 'Categories' or 'Movie Categories' must contain at least one HDBits \
                 category ID"
                    .to_string(),
            ));
        }

        Ok(Self {
            base_url,
            username,
            api_key,
            categories,
            movie_categories,
            codecs: parse_ids(settings.codecs.as_deref().unwrap_or_default()),
            mediums: parse_ids(settings.mediums.as_deref().unwrap_or_default()),
            origins: parse_ids(settings.origins.as_deref().unwrap_or_default()),
            // Prowlarr's `UseFilenames` default.
            use_filenames: parse_bool(settings.use_filenames.as_deref()).unwrap_or(true),
        })
    }
}

/// The raw configuration values as the host stores them.
#[derive(Debug, Default, Clone)]
struct HostSettings {
    base_url: Option<String>,
    username: Option<String>,
    api_key: Option<String>,
    categories: Option<String>,
    movie_categories: Option<String>,
    codecs: Option<String>,
    mediums: Option<String>,
    origins: Option<String>,
    use_filenames: Option<String>,
}

/// Sonarr's `ValidRootUrl`: non-empty, parseable, `http`/`https`.
fn normalize_base_url(value: &str) -> Result<String, Error> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "base_url",
            "HDBits requires a site URL".to_string(),
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
/// string, keeping the configured order and dropping duplicates.
fn parse_ids(raw: &str) -> Vec<i64> {
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

fn parse_bool(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
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
    options: Vec<ConfigFieldOption>,
    default_value: Option<&str>,
    required: bool,
    help_text: &str,
) -> ConfigFieldDef {
    ConfigFieldDef {
        options,
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
    use scryer_plugin_sdk::{PluginSearchContext, PluginSearchOrigin, PluginSearchSubjectKind};

    /// `src/NzbDrone.Core.Test/Files/Indexers/HdBits/RecentFeedLongIDs.json`.
    const RECENT_FEED_LONG_IDS: &str = r#"{
    "status": 0,
    "data": [
        {
            "id": 257142,
            "hash": "EABC50AEF9F53CEDED84ADF14144D3368E586F3A",
            "leechers": 1,
            "seeders": 46,
            "name": "Supernatural S10E17 1080p WEB-DL DD5.1 H.264-ECI",
            "times_completed": 49,
            "size": 1718009717,
            "utadded": 1428179446,
            "added": "2015-04-04T20:30:46+0000",
            "comments": 0,
            "numfiles": 1,
            "filename": "Supernatural.S10E17.1080p.WEB-DL.DD5.1.H.264-ECI.torrent",
            "freeleech": "no",
            "type_category": 2,
            "type_codec": 1,
            "type_medium": 6,
            "type_origin": 0,
            "username": "abc",
            "owner": 1107944,
            "tvdb": {
                "id": 78901,
                "season": 10,
                "episode": 17
            }
        },
        {
            "id": 257140,
            "hash": "BE3BA5396B9A30544353B55FDD89EDE46C8FB72A",
            "leechers": 0,
            "seeders": 18,
            "name": "Scandal S04E18 1080p WEB-DL DD5.1 H.264-ECI",
            "times_completed": 19,
            "size": 1789106197,
            "utadded": 1428179128,
            "added": "2015-04-04T20:25:28+0000",
            "comments": 0,
            "numfiles": 1,
            "filename": "Scandal.2012.S04E18.1080p.WEB-DL.DD5.1.H.264-ECI.torrent",
            "freeleech": "no",
            "type_category": 2,
            "type_codec": 1,
            "type_medium": 6,
            "type_origin": 0,
            "username": "abc",
            "owner": 1107944,
            "tvdb": {
                "id": 248841,
                "season": 4,
                "episode": 18
            }
        }
    ]
}"#;

    /// `.../RecentFeedStringIDs.json` — the same feed with a string `id`.
    const RECENT_FEED_STRING_IDS: &str = r#"{
    "status": 0,
    "data": [
        {
            "id": "257142",
            "hash": "EABC50AEF9F53CEDED84ADF14144D3368E586F3A",
            "leechers": 1,
            "seeders": 46,
            "name": "Supernatural S10E17 1080p WEB-DL DD5.1 H.264-ECI",
            "times_completed": 49,
            "size": 1718009717,
            "utadded": 1428179446,
            "added": "2015-04-04T20:30:46+0000",
            "comments": 0,
            "numfiles": 1,
            "filename": "Supernatural.S10E17.1080p.WEB-DL.DD5.1.H.264-ECI.torrent",
            "freeleech": "no",
            "type_category": 2,
            "type_codec": 1,
            "type_medium": 6,
            "type_origin": 0,
            "username": "abc",
            "owner": 1107944,
            "tvdb": {
                "id": 78901,
                "season": 10,
                "episode": 17
            }
        },
        {
            "id": "257140",
            "hash": "BE3BA5396B9A30544353B55FDD89EDE46C8FB72A",
            "leechers": 0,
            "seeders": 18,
            "name": "Scandal S04E18 1080p WEB-DL DD5.1 H.264-ECI",
            "times_completed": 19,
            "size": 1789106197,
            "utadded": 1428179128,
            "added": "2015-04-04T20:25:28+0000",
            "comments": 0,
            "numfiles": 1,
            "filename": "Scandal.2012.S04E18.1080p.WEB-DL.DD5.1.H.264-ECI.torrent",
            "freeleech": "no",
            "type_category": 2,
            "type_codec": 1,
            "type_medium": 6,
            "type_origin": 0,
            "username": "abc",
            "owner": 1107944,
            "tvdb": {
                "id": 248841,
                "season": 4,
                "episode": 18
            }
        }
    ]
}"#;

    /// Sonarr's fixture settings: `ApiKey = "fakekey"`, everything else default.
    fn config() -> HdbitsConfig {
        HdbitsConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            username: "somename".to_string(),
            api_key: "fakekey".to_string(),
            categories: vec![2, 3],
            movie_categories: Vec::new(),
            codecs: Vec::new(),
            mediums: Vec::new(),
            origins: Vec::new(),
            // Sonarr reports `name`; see `use_filenames`.
            use_filenames: false,
        }
    }

    fn credentials() -> HostSettings {
        HostSettings {
            username: Some("u".to_string()),
            api_key: Some("k".to_string()),
            ..HostSettings::default()
        }
    }

    fn request() -> SearchRequest {
        SearchRequest {
            limit: 1000,
            ..SearchRequest::default()
        }
    }

    fn context(kind: PluginSearchRequestKind) -> PluginSearchContext {
        PluginSearchContext {
            request_kind: kind,
            search_origin: PluginSearchOrigin::Automatic,
            subject_kind: PluginSearchSubjectKind::Episode,
            ..PluginSearchContext::default()
        }
    }

    fn body(query: &TorrentQuery) -> serde_json::Value {
        serde_json::to_value(query).expect("query serialises")
    }

    fn plugin_error(error: &Error) -> PluginError {
        error
            .downcast_ref::<StructuredPluginError>()
            .expect("error should be a structured plugin error")
            .plugin_error()
            .clone()
    }

    fn error_code(error: &Error) -> PluginErrorCode {
        plugin_error(error).code
    }

    fn details(error: &Error) -> IndexerSearchPluginError {
        match plugin_error(error).details {
            Some(PluginErrorDetails::IndexerSearch(details)) => details,
            other => panic!("expected indexer-search details, got {other:?}"),
        }
    }

    // -- H1: both fixture shapes -------------------------------------------

    #[test]
    fn parses_sonarrs_long_id_recent_feed() {
        let results = parse_response(&config(), RECENT_FEED_LONG_IDS).expect("feed parses");
        assert_eq!(results.len(), 2);

        let first = &results[0];
        assert_eq!(first.guid.as_deref(), Some("HDBits-257142"));
        assert_eq!(
            first.title,
            "Supernatural S10E17 1080p WEB-DL DD5.1 H.264-ECI"
        );
        assert_eq!(
            first.download_url.as_deref(),
            Some("https://hdbits.org/download.php?id=257142&passkey=fakekey")
        );
        assert_eq!(
            first.info_url.as_deref(),
            Some("https://hdbits.org/details.php?id=257142")
        );
        assert_eq!(first.published_at.as_deref(), Some("2015-04-04T20:30:46Z"));
        assert_eq!(first.size_bytes, Some(1_718_009_717));
        // Sonarr asserts the upper-case form; Scryer normalises info hashes to
        // lower case (`normalize_indexer_info_hash`).
        assert_eq!(
            first.info_hash_v1.as_deref(),
            Some("eabc50aef9f53ceded84adf14144d3368e586f3a")
        );
        assert_eq!(first.magnet_url, None);
        assert_eq!(first.comment_url, None);
        assert_eq!(first.seeders, Some(46));
        assert_eq!(first.peers, Some(47));
        assert_eq!(first.leechers, Some(1));
        assert_eq!(first.grabs, Some(49));
        assert_eq!(
            first.external_ids.get("tvdb_id").map(String::as_str),
            Some("78901")
        );
        assert_eq!(first.protocol, Some(IndexerProtocol::Torrent));
        assert_eq!(first.source_kind, Some(IndexerSourceKind::Torrent));
    }

    #[test]
    fn parses_sonarrs_string_id_recent_feed_identically() {
        let long = parse_response(&config(), RECENT_FEED_LONG_IDS).expect("feed parses");
        let string = parse_response(&config(), RECENT_FEED_STRING_IDS).expect("feed parses");
        assert_eq!(long.len(), string.len());
        for (long, string) in long.iter().zip(string.iter()) {
            assert_eq!(long.guid, string.guid);
            assert_eq!(long.title, string.title);
            assert_eq!(long.download_url, string.download_url);
            assert_eq!(long.info_url, string.info_url);
            assert_eq!(long.published_at, string.published_at);
            assert_eq!(long.info_hash_v1, string.info_hash_v1);
            assert_eq!(long.external_ids, string.external_ids);
        }
    }

    #[test]
    fn parses_the_second_fixture_entry() {
        let results = parse_response(&config(), RECENT_FEED_LONG_IDS).expect("feed parses");
        let second = &results[1];
        assert_eq!(second.guid.as_deref(), Some("HDBits-257140"));
        assert_eq!(second.title, "Scandal S04E18 1080p WEB-DL DD5.1 H.264-ECI");
        assert_eq!(second.published_at.as_deref(), Some("2015-04-04T20:25:28Z"));
        assert_eq!(second.size_bytes, Some(1_789_106_197));
        assert_eq!(second.seeders, Some(18));
        assert_eq!(second.peers, Some(18));
        assert_eq!(
            second.external_ids.get("tvdb_id").map(String::as_str),
            Some("248841")
        );
    }

    #[test]
    fn tolerates_every_scalar_arriving_as_a_string() {
        let body = r#"{
            "status": "0",
            "data": [{
                "id": "1",
                "hash": "AA",
                "name": "Some Release",
                "size": "123",
                "seeders": "3",
                "leechers": "4",
                "times_completed": "5",
                "numfiles": "6",
                "comments": "7",
                "utadded": "1428179446",
                "freeleech": "yes",
                "type_category": "2",
                "type_codec": "1",
                "type_medium": "6",
                "type_origin": "1",
                "type_exclusive": "1",
                "tvdb": {"id": "78901", "season": "10", "episode": "17"},
                "imdb": {"id": "460681", "year": "2005", "rating": "8.4"}
            }]
        }"#;
        let results = parse_response(&config(), body).expect("feed parses");
        assert_eq!(results.len(), 1);
        let first = &results[0];
        assert_eq!(first.size_bytes, Some(123));
        assert_eq!(first.seeders, Some(3));
        assert_eq!(first.peers, Some(7));
        assert_eq!(first.grabs, Some(5));
        assert_eq!(first.download_volume_factor, Some(0.0));
        assert_eq!(
            first.external_ids.get("imdb_id").map(String::as_str),
            Some("tt0460681")
        );
        assert_eq!(
            first.provider_extra.get("imdb_year"),
            Some(&serde_json::Value::from(2005))
        );
        assert_eq!(
            first.provider_extra.get("tvdb_episode"),
            Some(&serde_json::Value::from("17"))
        );
    }

    #[test]
    fn releases_without_an_id_or_a_title_are_dropped() {
        let body = r#"{"status":0,"data":[
            {"hash":"AA","name":"No id"},
            {"id":2,"name":"   "},
            {"id":3,"name":"Kept"}
        ]}"#;
        let results = parse_response(&config(), body).expect("feed parses");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Kept");
    }

    // -- H2: request shapes -------------------------------------------------

    #[test]
    fn the_recent_poll_sends_an_otherwise_empty_query() {
        let tiers = build_query_tiers(&config(), &request());
        assert_eq!(tiers.len(), 1);
        assert_eq!(
            body(&tiers[0]),
            serde_json::json!({
                "username": "somename",
                "passkey": "fakekey",
                "category": [2, 3],
                "limit": 100
            })
        );
    }

    #[test]
    fn every_wire_key_is_lower_case() {
        let mut config = config();
        config.codecs = vec![1, 5];
        config.mediums = vec![3, 6];
        config.origins = vec![1];
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.season = Some(10);
        request.episode = Some(17);

        let tiers = build_query_tiers(&config, &request);
        let payload = body(&tiers[0]);
        for key in payload.as_object().expect("object body").keys() {
            assert_eq!(key.as_str(), key.to_ascii_lowercase().as_str(), "key {key}");
        }
        assert_eq!(
            payload,
            serde_json::json!({
                "username": "somename",
                "passkey": "fakekey",
                "category": [2, 3],
                "codec": [1, 5],
                "medium": [3, 6],
                "origin": [1],
                "tvdb": {"id": 78901, "season": 10, "episode": 17},
                "limit": 100
            })
        );
    }

    #[test]
    fn a_season_search_omits_the_episode_member() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.season = Some(10);
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(
            body(&tiers[0])["tvdb"],
            serde_json::json!({"id": 78901, "season": 10})
        );
    }

    #[test]
    fn an_id_only_request_never_issues_a_free_text_call() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.season = Some(1);
        request.episode = Some(3);
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].search.is_none());
    }

    #[test]
    fn an_id_and_text_request_puts_the_id_tier_first() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.query = "Supernatural".to_string();
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 2);
        assert!(tiers[0].tvdb.is_some());
        assert_eq!(tiers[0].search, None);
        assert_eq!(tiers[1].tvdb, None);
        assert_eq!(tiers[1].search.as_deref(), Some("Supernatural"));
    }

    #[test]
    fn an_interactive_free_text_search_issues_one_sanitised_query() {
        let mut request = request();
        request.query = "Marvel's Agents of S.H.I.E.L.D.".to_string();
        request.context = Some(context(PluginSearchRequestKind::Search));
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(
            tiers[0].search.as_deref(),
            Some("Marvel s Agents of S H I E L D")
        );
    }

    #[test]
    fn a_zero_or_junk_tvdb_id_is_not_an_id() {
        let mut request = request();
        request.ids.insert("tvdb_id".to_string(), "0".to_string());
        request.query = "Some Show".to_string();
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].tvdb, None);
        assert_eq!(tiers[0].search.as_deref(), Some("Some Show"));
    }

    #[test]
    fn an_air_date_replaces_the_season_and_episode_scoping() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "289574".to_string());
        request.season = Some(2023);
        request.episode = Some(3);
        request.context = Some(PluginSearchContext {
            air_date: Some("2023-01-03".to_string()),
            ..context(PluginSearchRequestKind::Search)
        });
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(body(&tiers[0])["tvdb"], serde_json::json!({"id": 289574}));
        assert_eq!(tiers[0].search.as_deref(), Some("2023-01-03"));
    }

    #[test]
    fn a_daily_season_search_falls_back_to_the_year_prefix() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "289574".to_string());
        request.season = Some(2023);
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 2);
        assert_eq!(
            body(&tiers[0])["tvdb"],
            serde_json::json!({"id": 289574, "season": 2023})
        );
        assert_eq!(tiers[1].search.as_deref(), Some("2023-"));
        assert_eq!(body(&tiers[1])["tvdb"], serde_json::json!({"id": 289574}));
    }

    #[test]
    fn an_ordinary_season_number_never_produces_a_year_tier() {
        let mut request = request();
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.season = Some(10);
        assert_eq!(build_query_tiers(&config(), &request).len(), 1);
    }

    #[test]
    fn an_anime_absolute_episode_searches_the_series_unscoped() {
        let mut request = request();
        request.facet = Some("anime".to_string());
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.absolute_episode = Some(112);
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        // HDBits carries no absolute numbering, so the series query is the
        // most specific request it can serve; the host's `ids_sxex` strategy
        // supplies the season/episode form separately.
        assert_eq!(body(&tiers[0])["tvdb"], serde_json::json!({"id": 78901}));
        assert_eq!(body(&tiers[0])["category"], serde_json::json!([2, 3]));
    }

    #[test]
    fn an_anime_search_uses_the_series_categories() {
        let mut request = request();
        request.facet = Some("anime".to_string());
        request
            .ids
            .insert("tvdb_id".to_string(), "78901".to_string());
        request.season = Some(1);
        request.episode = Some(2);
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(body(&tiers[0])["category"], serde_json::json!([2, 3]));
    }

    #[test]
    fn a_movie_search_needs_movie_categories_and_uses_the_imdb_member() {
        let mut request = request();
        request.facet = Some("movie".to_string());
        request
            .ids
            .insert("imdb_id".to_string(), "tt0076759".to_string());

        assert!(build_query_tiers(&config(), &request).is_empty());

        let mut config = config();
        config.movie_categories = vec![1];
        let tiers = build_query_tiers(&config, &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(
            body(&tiers[0]),
            serde_json::json!({
                "username": "somename",
                "passkey": "fakekey",
                "category": [1],
                "imdb": {"id": 76759},
                "limit": 100
            })
        );
    }

    #[test]
    fn a_series_search_never_sends_an_imdb_id() {
        let mut request = request();
        request.facet = Some("series".to_string());
        request
            .ids
            .insert("imdb_id".to_string(), "tt0076759".to_string());
        request.query = "Star Wars".to_string();
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].imdb, None);
        assert_eq!(tiers[0].search.as_deref(), Some("Star Wars"));
    }

    #[test]
    fn the_recent_poll_joins_the_series_and_movie_categories() {
        let mut config = config();
        config.movie_categories = vec![1, 3];
        let mut request = request();
        request.context = Some(context(PluginSearchRequestKind::Recent));
        let tiers = build_query_tiers(&config, &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(body(&tiers[0])["category"], serde_json::json!([2, 3, 1]));
    }

    #[test]
    fn an_explicit_recent_context_ignores_leftover_criteria() {
        let mut request = request();
        request.query = "Supernatural".to_string();
        request.context = Some(context(PluginSearchRequestKind::Recent));
        let tiers = build_query_tiers(&config(), &request);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].search, None);
    }

    #[test]
    fn a_facet_with_no_configured_categories_issues_no_requests() {
        let mut config = config();
        config.categories = Vec::new();
        config.movie_categories = vec![1];
        let mut request = request();
        request.query = "Some Show".to_string();
        assert!(build_query_tiers(&config, &request).is_empty());
    }

    #[test]
    fn the_host_limit_of_one_thousand_is_capped_at_one_page() {
        assert_eq!(result_limit(&request()), MAX_PAGE_SIZE);
        let mut request = request();
        request.limit = 25;
        assert_eq!(result_limit(&request), 25);
        request.limit = 0;
        assert_eq!(result_limit(&request), MAX_PAGE_SIZE);
    }

    // -- H3: delivery and API status classification -------------------------

    #[test]
    fn an_unauthorised_response_reports_an_auth_failure() {
        let error = classify_response(401, &BTreeMap::new(), b"nope").unwrap_err();
        assert_eq!(error_code(&error), PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_forbidden_rate_limit_defers_for_the_window_it_names() {
        let error = classify_response(
            403,
            &BTreeMap::new(),
            b"<h1>Error</h1><p>Rate-limit exceeded. Please try again in 15 minutes.</p>",
        )
        .unwrap_err();
        let plugin_error = plugin_error(&error);
        assert_eq!(plugin_error.code, PluginErrorCode::RateLimited);
        assert_eq!(plugin_error.retry_after_seconds, Some(900));
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::RateLimited,
                ..
            }
        ));
    }

    #[test]
    fn a_forbidden_response_that_is_not_a_rate_limit_is_an_auth_failure() {
        let error = classify_response(403, &BTreeMap::new(), b"Access denied").unwrap_err();
        assert_eq!(error_code(&error), PluginErrorCode::AuthFailed);
    }

    #[test]
    fn a_retry_after_header_wins_over_the_default_window() {
        let headers = BTreeMap::from([("Retry-After".to_string(), "42".to_string())]);
        let error = plugin_error(&classify_response(429, &headers, b"slow down").unwrap_err());
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(42));
    }

    #[test]
    fn a_429_without_a_header_uses_the_one_hour_floor() {
        let error = plugin_error(&classify_response(429, &BTreeMap::new(), b"").unwrap_err());
        assert_eq!(
            error.retry_after_seconds,
            Some(RATE_LIMITED_FALLBACK_SECONDS)
        );
    }

    #[test]
    fn a_redirect_blames_the_base_url_and_names_the_location() {
        let headers = BTreeMap::from([(
            "Location".to_string(),
            "https://hdbits.org/login.php".to_string(),
        )]);
        let error = plugin_error(&classify_response(302, &headers, b"").unwrap_err());
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("base_url"));
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("https://hdbits.org/login.php")
        );
    }

    #[test]
    fn a_server_error_defers_on_upstream_failure() {
        let error = classify_response(503, &BTreeMap::new(), b"down").unwrap_err();
        assert_eq!(error_code(&error), PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::UpstreamFailure,
                ..
            }
        ));
    }

    #[test]
    fn an_html_body_is_an_unexpected_content_type() {
        let headers = BTreeMap::from([("Content-Type".to_string(), "text/html".to_string())]);
        let error = classify_response(200, &headers, b"<html></html>").unwrap_err();
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
            }
        ));
    }

    #[test]
    fn an_unlabelled_json_body_is_still_accepted() {
        let body = classify_response(200, &BTreeMap::new(), b"{\"status\":0,\"data\":[]}")
            .expect("json body accepted");
        assert!(body.starts_with('{'));
    }

    #[test]
    fn an_unparseable_body_is_a_malformed_body() {
        let error = parse_response(&config(), "not json").unwrap_err();
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse {
                kind: IndexerSearchInvalidResponseKind::MalformedBody
            }
        ));
    }

    #[test]
    fn a_missing_data_array_is_an_invalid_root() {
        for body in [r#"{"status":0}"#, r#"{"status":0,"data":"nope"}"#] {
            let error = parse_response(&config(), body).unwrap_err();
            assert!(
                matches!(
                    details(&error),
                    IndexerSearchPluginError::InvalidResponse {
                        kind: IndexerSearchInvalidResponseKind::InvalidRoot
                    }
                ),
                "{body}"
            );
        }
    }

    /// Sonarr's `should_warn_on_wrong_passkey` fixture returns
    /// `{status: 5, message: "Invalid authentication credentials"}` and merely
    /// warns; Scryer's typed error names the setting instead.
    #[test]
    fn a_wrong_passkey_is_a_typed_auth_failure() {
        let body = r#"{"status":5,"message":"Invalid authentication credentials"}"#;
        let error = plugin_error(&parse_response(&config(), body).unwrap_err());
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"));
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("Invalid authentication credentials")
        );
    }

    #[test]
    fn the_documented_api_status_table_is_classified() {
        let cases: &[(i64, PluginErrorCode)] = &[
            (1, PluginErrorCode::UpstreamUnavailable),
            (2, PluginErrorCode::InvalidConfig),
            (3, PluginErrorCode::Permanent),
            (4, PluginErrorCode::InvalidConfig),
            (5, PluginErrorCode::AuthFailed),
            (6, PluginErrorCode::Permanent),
            (7, PluginErrorCode::Permanent),
            (8, PluginErrorCode::Permanent),
            (9, PluginErrorCode::Permanent),
            (99, PluginErrorCode::UpstreamUnavailable),
        ];
        for (status, expected) in cases {
            let error = classify_api_status(*status, "boom");
            assert_eq!(error_code(&error), *expected, "status {status}");
        }
    }

    #[test]
    fn an_ssl_required_status_blames_the_base_url() {
        let error = plugin_error(&classify_api_status(2, ""));
        assert!(error.public_message.contains("base_url"));
        assert!(error.details.is_none());
    }

    #[test]
    fn missing_auth_data_blames_the_credentials_and_never_defers() {
        let error = plugin_error(&classify_api_status(4, "auth data missing"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("username"));
        assert!(error.details.is_none());
    }

    // -- H4: publish dates ---------------------------------------------------

    #[test]
    fn utadded_is_preferred_and_emitted_as_rfc_3339_utc() {
        assert_eq!(
            unix_to_rfc3339(1_428_179_446).as_deref(),
            Some("2015-04-04T20:30:46Z")
        );
        assert_eq!(unix_to_rfc3339(0), None);
        assert_eq!(unix_to_rfc3339(-1), None);
    }

    #[test]
    fn the_added_field_is_normalised_when_utadded_is_absent() {
        let body =
            r#"{"status":0,"data":[{"id":1,"name":"X","added":"2015-04-04T20:30:46+0000"}]}"#;
        let results = parse_response(&config(), body).expect("feed parses");
        assert_eq!(
            results[0].published_at.as_deref(),
            Some("2015-04-04T20:30:46Z")
        );
    }

    #[test]
    fn added_offsets_become_rfc_3339_offsets() {
        assert_eq!(
            normalize_published_at("2015-04-04T20:30:46+0000").as_deref(),
            Some("2015-04-04T20:30:46Z")
        );
        assert_eq!(
            normalize_published_at("2015-04-04T20:30:46+0200").as_deref(),
            Some("2015-04-04T20:30:46+02:00")
        );
        assert_eq!(
            normalize_published_at("2015-04-04T20:30:46-0500").as_deref(),
            Some("2015-04-04T20:30:46-05:00")
        );
        assert_eq!(
            normalize_published_at("2015-04-04 20:30:46").as_deref(),
            Some("2015-04-04T20:30:46Z")
        );
        assert_eq!(
            normalize_published_at("2015-04-04T20:30:46Z").as_deref(),
            Some("2015-04-04T20:30:46Z")
        );
        assert_eq!(
            normalize_published_at("2015-04-04 20:30").as_deref(),
            Some("2015-04-04T20:30:00Z")
        );
        assert_eq!(normalize_published_at("nonsense"), None);
        assert_eq!(normalize_published_at(""), None);
        assert_eq!(normalize_published_at("2015-04-04"), None);
    }

    // -- M3: result metadata --------------------------------------------------

    #[test]
    fn a_freeleech_release_reports_the_key_the_core_reads() {
        let body = r#"{"status":0,"data":[{"id":1,"name":"X","freeleech":"yes","type_category":2,"type_medium":6}]}"#;
        let result = &parse_response(&config(), body).expect("feed parses")[0];
        assert_eq!(result.download_volume_factor, Some(0.0));
        assert_eq!(result.upload_volume_factor, Some(1.0));
        assert!(result.indexer_flags.iter().any(|flag| flag == "freeleech"));
        assert_eq!(
            result.provider_extra.get("freeleech"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn hdbits_half_leech_economics_are_reported() {
        // TV (2) and Documentary (3) are half freeleech site-wide, as are full
        // discs (1), captures (4), remuxes (5) and internal releases.
        assert_eq!(volume_factors(false, 2, 6, 0), (0.5, 1.0));
        assert_eq!(volume_factors(false, 3, 6, 0), (0.5, 1.0));
        assert_eq!(volume_factors(false, 1, 1, 0), (0.5, 1.0));
        assert_eq!(volume_factors(false, 1, 4, 0), (0.5, 1.0));
        assert_eq!(volume_factors(false, 1, 5, 0), (0.5, 1.0));
        assert_eq!(volume_factors(false, 1, 6, 1), (0.5, 1.0));
        assert_eq!(volume_factors(false, 1, 6, 0), (1.0, 1.0));
        assert_eq!(volume_factors(true, 1, 6, 0), (0.0, 1.0));
        // XXX is neutral leech in both directions.
        assert_eq!(volume_factors(false, 7, 6, 0), (0.0, 0.0));
        assert_eq!(volume_factors(true, 7, 6, 0), (0.0, 0.0));
    }

    #[test]
    fn a_half_leech_release_is_tagged_but_not_called_freeleech() {
        let result = &parse_response(&config(), RECENT_FEED_LONG_IDS).expect("feed parses")[0];
        assert_eq!(result.download_volume_factor, Some(0.5));
        assert_eq!(
            result.provider_extra.get("freeleech"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            result.provider_extra.get("tags"),
            Some(&serde_json::json!(["halfleech"]))
        );
        assert!(result.indexer_flags.iter().any(|flag| flag == "halfleech"));
        assert!(!result.indexer_flags.iter().any(|flag| flag == "freeleech"));
    }

    #[test]
    fn internal_and_exclusive_releases_are_tagged() {
        let body = r#"{"status":0,"data":[{"id":1,"name":"X","type_category":1,"type_medium":6,"type_origin":1,"type_exclusive":1}]}"#;
        let result = &parse_response(&config(), body).expect("feed parses")[0];
        assert_eq!(
            result.provider_extra.get("tags"),
            Some(&serde_json::json!(["internal", "exclusive", "halfleech"]))
        );
        assert!(result.indexer_flags.iter().any(|flag| flag == "internal"));
        assert!(result.indexer_flags.iter().any(|flag| flag == "exclusive"));
    }

    #[test]
    fn xxx_content_is_reported_as_neutral_leech() {
        let body = r#"{"status":0,"data":[{"id":1,"name":"X","type_category":7,"type_medium":6}]}"#;
        let result = &parse_response(&config(), body).expect("feed parses")[0];
        assert_eq!(result.download_volume_factor, Some(0.0));
        assert_eq!(result.upload_volume_factor, Some(0.0));
        // A neutral-leech release is not a freeleech release for scoring.
        assert_eq!(
            result.provider_extra.get("freeleech"),
            Some(&serde_json::Value::Bool(false))
        );
        assert!(
            result
                .indexer_flags
                .iter()
                .any(|flag| flag == "neutral_upload")
        );
    }

    #[test]
    fn categories_carry_both_the_provider_id_and_a_readable_label() {
        let result = &parse_response(&config(), RECENT_FEED_LONG_IDS).expect("feed parses")[0];
        assert_eq!(result.provider_categories, vec!["2".to_string()]);
        assert_eq!(result.categories, vec!["TV".to_string()]);
        assert_eq!(
            result.provider_extra.get("category"),
            Some(&serde_json::Value::from("TV"))
        );
        assert_eq!(
            result.provider_extra.get("medium"),
            Some(&serde_json::Value::from("WEB-DL"))
        );
        assert_eq!(
            result.provider_extra.get("codec"),
            Some(&serde_json::Value::from("H.264"))
        );
        assert_eq!(
            result.provider_extra.get("numfiles"),
            Some(&serde_json::Value::from(1))
        );
    }

    #[test]
    fn imdb_metadata_is_reported_in_provider_extra() {
        let body = r#"{"status":0,"data":[{"id":1,"name":"X","imdb":{"id":460681,"englishtitle":"Supernatural","originaltitle":"Supernatural","year":2005,"genres":["Drama","Fantasy"],"rating":8.4}}]}"#;
        let result = &parse_response(&config(), body).expect("feed parses")[0];
        assert_eq!(
            result.provider_extra.get("imdb_english_title"),
            Some(&serde_json::Value::from("Supernatural"))
        );
        assert_eq!(
            result.provider_extra.get("imdb_genres"),
            Some(&serde_json::json!(["Drama", "Fantasy"]))
        );
        assert_eq!(
            result.provider_extra.get("imdb_rating"),
            Some(&serde_json::json!(8.4))
        );
        assert_eq!(
            result.external_ids.get("imdb_id").map(String::as_str),
            Some("tt0460681")
        );
    }

    // -- titles ---------------------------------------------------------------

    #[test]
    fn the_filename_is_preferred_as_the_release_title_by_default() {
        let mut config = config();
        config.use_filenames = true;
        let result = &parse_response(&config, RECENT_FEED_LONG_IDS).expect("feed parses")[0];
        assert_eq!(
            result.title,
            "Supernatural.S10E17.1080p.WEB-DL.DD5.1.H.264-ECI"
        );
        assert_eq!(
            result.provider_extra.get("name"),
            Some(&serde_json::Value::from(
                "Supernatural S10E17 1080p WEB-DL DD5.1 H.264-ECI"
            ))
        );
    }

    #[test]
    fn xxx_content_and_full_discs_keep_the_display_name() {
        assert_eq!(
            release_title(true, "Display Name", "release.name.torrent", 7, 6),
            "Display Name"
        );
        assert_eq!(
            release_title(true, "Display Name", "release.name.torrent", 2, 1),
            "Display Name"
        );
        assert_eq!(
            release_title(true, "Display Name", "release.name.torrent", 2, 6),
            "release.name"
        );
        assert_eq!(
            release_title(true, "Display Name", "release.name.TORRENT", 2, 6),
            "release.name"
        );
        assert_eq!(
            release_title(true, "Display Name", "  ", 2, 6),
            "Display Name"
        );
        assert_eq!(
            release_title(false, "Display Name", "release.name.torrent", 2, 6),
            "Display Name"
        );
        assert_eq!(
            release_title(true, "Display Name", ".torrent", 2, 6),
            "Display Name"
        );
    }

    // -- configuration --------------------------------------------------------

    #[test]
    fn the_base_url_keeps_its_path_and_loses_its_trailing_slash() {
        let resolved = HdbitsConfig::resolve(HostSettings {
            base_url: Some("https://mirror.example/hdb/".to_string()),
            ..credentials()
        })
        .expect("config resolves");
        assert_eq!(resolved.base_url, "https://mirror.example/hdb");
        assert_eq!(
            api_url(&resolved),
            "https://mirror.example/hdb/api/torrents"
        );
        assert_eq!(
            download_url(&resolved, "7"),
            "https://mirror.example/hdb/download.php?id=7&passkey=k"
        );
        assert_eq!(
            info_url(&resolved, "7"),
            "https://mirror.example/hdb/details.php?id=7"
        );
    }

    #[test]
    fn an_unusable_base_url_is_invalid_config() {
        for bad in ["", "   ", "not a url", "ftp://hdbits.org"] {
            let error = HdbitsConfig::resolve(HostSettings {
                base_url: Some(bad.to_string()),
                ..credentials()
            })
            .unwrap_err();
            assert_eq!(error_code(&error), PluginErrorCode::InvalidConfig, "{bad}");
        }
    }

    #[test]
    fn credentials_and_ids_are_percent_encoded_in_the_download_url() {
        let mut config = config();
        config.api_key = "pass key&x".to_string();
        assert_eq!(
            download_url(&config, "7"),
            "https://hdbits.org/download.php?id=7&passkey=pass+key%26x"
        );
    }

    #[test]
    fn missing_credentials_are_invalid_config_not_temporary() {
        let error = HdbitsConfig::resolve(HostSettings {
            api_key: Some("k".to_string()),
            ..HostSettings::default()
        })
        .unwrap_err();
        assert_eq!(error_code(&error), PluginErrorCode::InvalidConfig);
        assert!(plugin_error(&error).public_message.contains("username"));

        let error = HdbitsConfig::resolve(HostSettings {
            username: Some("u".to_string()),
            ..HostSettings::default()
        })
        .unwrap_err();
        assert!(plugin_error(&error).public_message.contains("api_key"));
    }

    #[test]
    fn an_empty_category_configuration_is_rejected() {
        let error = HdbitsConfig::resolve(HostSettings {
            categories: Some("   ".to_string()),
            movie_categories: Some(String::new()),
            ..credentials()
        })
        .unwrap_err();
        assert_eq!(error_code(&error), PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn the_legacy_comma_separated_id_lists_still_parse() {
        assert_eq!(parse_ids("2,3"), vec![2, 3]);
        assert_eq!(parse_ids(" 2 ; 3 \n 5 "), vec![2, 3, 5]);
        assert_eq!(parse_ids("2,2,3"), vec![2, 3]);
        assert_eq!(parse_ids("abc"), Vec::<i64>::new());

        let resolved = HdbitsConfig::resolve(HostSettings {
            categories: Some("2,3,5".to_string()),
            codecs: Some("1,5".to_string()),
            mediums: Some("3;6".to_string()),
            origins: Some("1".to_string()),
            ..credentials()
        })
        .expect("config resolves");
        assert_eq!(resolved.categories, vec![2, 3, 5]);
        assert_eq!(resolved.codecs, vec![1, 5]);
        assert_eq!(resolved.mediums, vec![3, 6]);
        assert_eq!(resolved.origins, vec![1]);
    }

    #[test]
    fn use_filenames_defaults_to_on_and_accepts_the_usual_spellings() {
        let resolved = HdbitsConfig::resolve(credentials()).expect("config resolves");
        assert!(resolved.use_filenames);
        assert_eq!(parse_bool(Some("false")), Some(false));
        assert_eq!(parse_bool(Some("0")), Some(false));
        assert_eq!(parse_bool(Some("YES")), Some(true));
        assert_eq!(parse_bool(Some("maybe")), None);
        assert_eq!(parse_bool(None), None);
    }

    #[test]
    fn the_defaults_match_sonarrs_settings() {
        let fields = config_fields();
        let by_key = |key: &str| {
            fields
                .iter()
                .find(|field| field.key == key)
                .unwrap_or_else(|| panic!("field {key}"))
                .clone()
        };
        assert_eq!(
            by_key("base_url").default_value.as_deref(),
            Some(DEFAULT_BASE_URL)
        );
        assert_eq!(by_key("categories").default_value.as_deref(), Some("2,3"));
        assert!(by_key("categories").required);
        assert!(by_key("username").required);
        assert!(by_key("api_key").required);
        assert_eq!(by_key("api_key").field_type, ConfigFieldType::Password);
        assert_eq!(by_key("movie_categories").default_value, None);
        assert_eq!(by_key("codecs").default_value, None);
        assert_eq!(by_key("mediums").default_value, None);
        assert_eq!(
            by_key("use_filenames").default_value.as_deref(),
            Some("true")
        );
        assert_eq!(
            by_key("minimum_seeders").default_value.as_deref(),
            Some("1")
        );
    }

    #[test]
    fn the_pick_lists_carry_the_published_id_tables() {
        let fields = config_fields();
        let options = |key: &str| {
            fields
                .iter()
                .find(|field| field.key == key)
                .expect("field")
                .options
                .iter()
                .map(|option| (option.value.clone(), option.label.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            options("codecs"),
            vec![
                ("1".to_string(), "H.264".to_string()),
                ("2".to_string(), "MPEG-2".to_string()),
                ("3".to_string(), "VC-1".to_string()),
                ("4".to_string(), "XviD".to_string()),
                ("5".to_string(), "HEVC".to_string()),
            ]
        );
        assert_eq!(
            options("mediums"),
            vec![
                ("1".to_string(), "Blu-ray/HD DVD".to_string()),
                ("3".to_string(), "Encode".to_string()),
                ("4".to_string(), "Capture".to_string()),
                ("5".to_string(), "Remux".to_string()),
                ("6".to_string(), "WEB-DL".to_string()),
            ]
        );
        assert_eq!(
            options("origins"),
            vec![
                ("0".to_string(), "Undefined".to_string()),
                ("1".to_string(), "Internal".to_string()),
            ]
        );
        assert_eq!(
            options("categories"),
            vec![
                ("2".to_string(), "TV".to_string()),
                ("3".to_string(), "Documentary".to_string()),
                ("5".to_string(), "Sport".to_string()),
                ("8".to_string(), "Misc/Demo".to_string()),
            ]
        );
        assert_eq!(
            options("movie_categories"),
            vec![
                ("1".to_string(), "Movie".to_string()),
                ("3".to_string(), "Documentary".to_string()),
                ("8".to_string(), "Misc/Demo".to_string()),
            ]
        );
        assert!(
            fields
                .iter()
                .all(|field| field.key != "categories" || field.field_type == ConfigFieldType::Tag)
        );
    }

    // -- descriptor honesty ---------------------------------------------------

    fn indexer_descriptor() -> IndexerDescriptor {
        match build_descriptor().provider {
            ProviderDescriptor::Indexer(descriptor) => descriptor,
            other => panic!("expected an indexer descriptor, got {other:?}"),
        }
    }

    #[test]
    fn the_descriptor_reports_the_real_api_ceiling_and_torrent_features() {
        let descriptor = indexer_descriptor();
        let limits = descriptor.capabilities.limits.expect("limits");
        assert_eq!(limits.page_size, Some(100));
        assert_eq!(limits.max_page_size, Some(100));
        assert_eq!(limits.max_pages, Some(1));

        let torrent = descriptor.capabilities.torrent.expect("torrent caps");
        assert!(torrent.reports_info_hash);
        assert!(!torrent.reports_magnet_uri);
        assert!(torrent.reports_volume_factors);
        // HDBits publishes no per-torrent seed ratio or seed time.
        assert!(!torrent.supports_seed_requirements);

        let features = descriptor
            .capabilities
            .response_features
            .expect("response features");
        // A comment count is not a comment URL.
        assert!(!features.comments);
        assert!(features.grabs);
    }

    #[test]
    fn the_descriptor_declares_only_the_ids_each_facet_can_query() {
        let descriptor = indexer_descriptor();
        let ids = &descriptor.capabilities.supported_ids;
        assert_eq!(ids.get("series"), Some(&vec!["tvdb_id".to_string()]));
        assert_eq!(ids.get("anime"), Some(&vec!["tvdb_id".to_string()]));
        assert_eq!(ids.get("movie"), Some(&vec!["imdb_id".to_string()]));
        assert!(descriptor.capabilities.tvdb_search);
        assert!(descriptor.capabilities.imdb_search);
        assert!(!descriptor.capabilities.anidb_search);
        assert_eq!(
            descriptor.capabilities.query_param.as_deref(),
            Some("search")
        );
        assert_eq!(descriptor.rate_limit_seconds, Some(2));
    }

    #[test]
    fn the_declared_category_table_matches_the_published_ids() {
        let descriptor = indexer_descriptor();
        let model = descriptor
            .capabilities
            .category_model
            .expect("category model");
        assert_eq!(model.categories.len(), CATEGORY_LABELS.len());
        let movie = model
            .categories
            .iter()
            .find(|category| category.value == "1")
            .expect("movie category");
        assert_eq!(movie.label.as_deref(), Some("Movie"));
        assert_eq!(movie.facets, vec!["movie".to_string()]);
        let tv = model
            .categories
            .iter()
            .find(|category| category.value == "2")
            .expect("tv category");
        assert_eq!(tv.facets, vec!["series".to_string(), "anime".to_string()]);
        // HDBits has no separate anime category to configure.
        assert!(!model.separate_anime_categories);
    }

    #[test]
    fn duplicate_releases_are_dropped_once_only() {
        let body = r#"{"status":0,"data":[
            {"id":1,"name":"A"},
            {"id":1,"name":"A"},
            {"id":2,"name":"B"}
        ]}"#;
        let results = dedupe_results(parse_response(&config(), body).expect("feed parses"));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn a_rate_limit_window_is_read_out_of_the_body_text() {
        assert_eq!(retry_minutes_from_body("try again in 15 minutes"), Some(15));
        assert_eq!(retry_minutes_from_body("try again in 5 minute"), Some(5));
        assert_eq!(retry_minutes_from_body("no window here"), None);
        assert!(looks_like_rate_limit("Rate-limit exceeded."));
        assert!(!looks_like_rate_limit("Access denied"));
    }
}
