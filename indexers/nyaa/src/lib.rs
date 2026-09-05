//! Nyaa (`nyaa.si`) RSS indexer.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Indexers/Nyaa` (`Nyaa.cs`,
//! `NyaaRequestGenerator.cs`, `NyaaSettings.cs`), its `RssParser` /
//! `TorrentRssParser`, the `NyaaFixture`/`NyaaRequestGeneratorFixture` tests
//! and the `Files/Indexers/Nyaa/Nyaa2021.xml` fixture — and, for the wire
//! contract, against the site's own source (`nyaadevs/nyaa`,
//! `nyaa/views/main.py`, `nyaa/search.py`, `nyaa/templates/rss.xml`) plus a
//! live read of `https://nyaa.si/?page=rss`.
//!
//! Shape of the integration:
//!
//! * Nyaa renders **any** search as RSS when `page=rss` is present, so the
//!   recent poll and a term search are the same endpoint with and without a
//!   search term. There is no API key and no login.
//! * The site accepts two spellings for every search parameter — the modern
//!   `q`/`c`/`f`/`p` and the legacy nyaa.se `term`/`cats`/`filter`/`page` —
//!   because `main.py` reads them through `chain_get(req_args, 'q', 'term')`
//!   and friends. Sonarr sends the legacy spelling; it still works, so the
//!   plugin keeps it (and keeps `additional_params`' published default) rather
//!   than churning a config contract for no behavioural gain.
//! * The fetch, the delivery classification, the XML parse and the result
//!   assembly are done in-plugin rather than through
//!   `rss-indexer-common::execute_rss_urls`: that helper cannot report a typed
//!   error (every failure becomes `Temporary`), it post-filters parsed releases
//!   against the request — which Sonarr never does, and which empties a
//!   `movie`-faceted Nyaa search outright — and it cannot see the `nyaa:`
//!   namespace metadata this feed carries. See the README and the
//!   reconciliation report.

use std::collections::{BTreeMap, HashMap};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use scryer_plugin_pdk::component::{self, LogLevel, StartRateGate, structured_plugin_error};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, IndexerCapabilities as Capabilities,
    IndexerCategoryDescriptor, IndexerCategoryModel, IndexerCategoryValueKind, IndexerDescriptor,
    IndexerFeedMode, IndexerLimitCapabilities, IndexerProtocol, IndexerResponseFeatures,
    IndexerSearchIncompleteReason, IndexerSearchInput, IndexerSearchInvalidResponseKind,
    IndexerSearchPluginError, IndexerSourceKind, IndexerStrategyPlanCapability,
    IndexerTorrentCapabilities, PluginDescriptor, PluginError, PluginErrorCode, PluginErrorDetails,
    PluginSearchRequest as SearchRequest, PluginSearchRequestKind,
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult,
    PluginSearchSubjectKind, ProviderDescriptor, SDK_VERSION, torrent_result,
};

const PROVIDER_ID: &str = "nyaa";
const USER_AGENT: &str = concat!("scryer-nyaa-indexer/", env!("CARGO_PKG_VERSION"));
const DEFAULT_BASE_URL: &str = "https://nyaa.si";
/// `NyaaSettings.AdditionalParameters` (`NyaaSettings.cs`): category `1_0`
/// (all anime) with the "no remakes" quality filter.
const DEFAULT_ADDITIONAL_PARAMS: &str = "&cats=1_0&filter=1";
/// Sonarr's `HttpIndexerBase.RateLimit` for every indexer.
const REQUEST_INTERVAL_MS: u64 = 2_000;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Sonarr's `minimumBackoff` when a rate-limited response carries no
/// `Retry-After` (`HttpIndexerBase.FetchReleases`).
const RATE_LIMITED_FALLBACK_SECONDS: i64 = 3_600;
/// `DEFAULT_PER_PAGE` in `nyaa/search.py`; the RSS branch slices the result set
/// to the same `per_page`. A deployment can override it with `RESULTS_PER_PAGE`.
const FEED_PAGE_SIZE: u32 = 75;
/// A season number this large is a daily series' year, not a season. Sonarr
/// answers `DailySeasonSearchCriteria` with an empty request chain
/// (`NyaaRequestGenerator.cs`), so no season term is built for one.
const DAILY_SEASON_FLOOR: u32 = 1_900;

// ---------------------------------------------------------------------------
// Category table
// ---------------------------------------------------------------------------

/// Nyaa's published category ids and names, from `NYAA_CATEGORIES` in the
/// site's `db_create.py`: six main categories, each with sub-categories
/// numbered in declaration order. `1_0` is "all anime" and is the plugin's
/// default.
///
/// The RSS item carries both halves — `nyaa:categoryId` (`1_3`) and
/// `nyaa:category` (`Anime - Non-English-translated`) — so both are reported.
///
/// Only the anime tree carries Scryer facets: Nyaa files anime films inside it
/// rather than in a separate movie tree, which is why a `movie`-faceted search
/// is legitimate here.
const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("1_0", "Anime", &["anime", "movie"]),
    ("1_1", "Anime - Anime Music Video", &["anime"]),
    ("1_2", "Anime - English-translated", &["anime", "movie"]),
    ("1_3", "Anime - Non-English-translated", &["anime", "movie"]),
    ("1_4", "Anime - Raw", &["anime", "movie"]),
    ("2_0", "Audio", &[]),
    ("2_1", "Audio - Lossless", &[]),
    ("2_2", "Audio - Lossy", &[]),
    ("3_0", "Literature", &[]),
    ("3_1", "Literature - English-translated", &[]),
    ("3_2", "Literature - Non-English-translated", &[]),
    ("3_3", "Literature - Raw", &[]),
    ("4_0", "Live Action", &[]),
    ("4_1", "Live Action - English-translated", &[]),
    ("4_2", "Live Action - Idol/Promotional Video", &[]),
    ("4_3", "Live Action - Non-English-translated", &[]),
    ("4_4", "Live Action - Raw", &[]),
    ("5_0", "Pictures", &[]),
    ("5_1", "Pictures - Graphics", &[]),
    ("5_2", "Pictures - Photos", &[]),
    ("6_0", "Software", &[]),
    ("6_1", "Software - Applications", &[]),
    ("6_2", "Software - Games", &[]),
];

fn category_descriptors() -> Vec<IndexerCategoryDescriptor> {
    CATEGORIES
        .iter()
        .map(|(id, name, facets)| IndexerCategoryDescriptor {
            value: (*id).to_string(),
            label: Some((*name).to_string()),
            value_kind: IndexerCategoryValueKind::String,
            facets: facets.iter().map(|facet| (*facet).to_string()).collect(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "Nyaa Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_ID.to_string(),
            provider_aliases: vec![],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            // Every upstream call queues behind one 2 s start gate, so running
            // the host's strategies concurrently buys nothing and only makes
            // the pacing harder to read.
            strategy_plan: Some(IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 1,
            }),
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                // Nyaa has no id lookup of any kind: the only search input is
                // the free-text term.
                supported_ids: HashMap::new(),
                deduplicates_aliases: false,
                // The season and episode numbers are folded into the term;
                // there is no season/episode query parameter.
                season_param: None,
                episode_param: None,
                // Load-bearing: the host only dispatches a text strategy when
                // `query_param` is set (`search_client.rs::build_strategies`).
                // Name it as the plugin actually sends it.
                query_param: Some("term".to_string()),
                supported_query_facets: vec!["movie".to_string(), "anime".to_string()],
                search: true,
                imdb_search: false,
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
                    IndexerSearchInput::Season,
                    IndexerSearchInput::Episode,
                    IndexerSearchInput::AbsoluteEpisode,
                    IndexerSearchInput::SpecialEpisodeTitle,
                    IndexerSearchInput::Limit,
                ],
                // Nothing in the feed identifies a series or a film.
                supported_external_ids: vec![],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::String],
                    // Nyaa is an anime site: its anime tree is the whole
                    // catalogue, not a sub-tree of a TV one.
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    categories: category_descriptors(),
                }),
                limits: Some(IndexerLimitCapabilities {
                    page_size: Some(FEED_PAGE_SIZE),
                    max_page_size: Some(FEED_PAGE_SIZE),
                    // Nyaa pages with `&p=N`, but Sonarr never pages this
                    // indexer and the host asks for 1000 results on every
                    // search — honouring that literally would mean 14 paced
                    // requests per term. One page per term, as Sonarr does.
                    max_pages: Some(1),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: false,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    reports_seeders: true,
                    // `nyaa:leechers` + `nyaa:seeders`, Sonarr's
                    // `CalculatePeersAsSum`.
                    reports_peers: true,
                    reports_leechers: true,
                    reports_info_hash: true,
                    // The feed's `<link>` is a `.torrent` URL. Nyaa can emit
                    // magnets instead (`&m` in `additional_params`), and the
                    // plugin reports one when it sees one, but the default and
                    // Sonarr-equivalent configuration carries none.
                    reports_magnet_uri: false,
                    // Public tracker: no freeleech, no volume factors.
                    reports_volume_factors: false,
                    supports_private_tracker_flags: false,
                    // Public tracker: no hit-and-run rule to report.
                    supports_seed_requirements: false,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: false,
                    subtitles: false,
                    // `nyaa:downloads`.
                    grabs: true,
                    votes: false,
                    // `nyaa:comments` is a count, not a comment page URL, and
                    // the feed has no `<comments>` element — Sonarr's fixture
                    // asserts an empty `CommentUrl`.
                    comments: false,
                    // `<guid>` is the `/view/<id>` page.
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
            "Website URL",
            true,
            Some(DEFAULT_BASE_URL),
            Some("Nyaa website URL, for example https://nyaa.si"),
        ),
        field(
            "anime_standard_format_search",
            "Anime Standard Format Search",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some(
                "Also search Nyaa with SxxExx and season-pack terms. Nyaa releases are usually \
                 numbered absolutely, so this is off by default; without it, season searches and \
                 episode searches that carry no absolute number make no request at all.",
            ),
        ),
        field(
            "additional_params",
            "Additional Parameters",
            ConfigFieldType::String,
            false,
            Some(DEFAULT_ADDITIONAL_PARAMS),
            Some(
                "Extra query parameters appended to the Nyaa RSS request, each starting with '&'. \
                 Category: 'cats' (or 'c') with an id such as 1_0 (all anime), 1_2 \
                 (English-translated), 1_3 (non-English-translated), 1_4 (raw). Quality filter: \
                 'filter' (or 'f') 0 none, 1 no remakes, 2 trusted only, 3 completed only. Also \
                 useful: '&u=<uploader>', '&s=seeders&o=desc', '&m=1' for magnet links.",
            ),
        ),
        field(
            "minimum_seeders",
            "Minimum Seeders",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some(
                "Minimum seeders preference for host-side release decisions. Scryer applies it; \
                 the plugin never withholds a release itself.",
            ),
        ),
        field(
            "user_agent",
            "User Agent",
            ConfigFieldType::String,
            false,
            Some(USER_AGENT),
            Some("Optional custom User-Agent header"),
        ),
        field(
            "cookie",
            "Cookie Header",
            ConfigFieldType::Password,
            false,
            None,
            Some(
                "Optional raw Cookie header. Nyaa's RSS feed needs no authentication; this exists \
                 for mirrors behind a proxy or a hand-supplied Cloudflare clearance cookie.",
            ),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some("Optional username for HTTP basic auth (reverse proxies in front of a mirror)"),
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            Some("Optional password for HTTP basic auth"),
        ),
        field(
            "additional_headers",
            "Additional Headers",
            ConfigFieldType::Multiline,
            false,
            None,
            Some("Optional extra headers, one per line, formatted as Header-Name: value"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

async fn search(request: SearchRequest) -> FnResult<SearchResponse> {
    let config = NyaaConfig::from_host()?;
    let urls = nyaa_urls(
        &config.base_url,
        &config.additional_params,
        &request,
        config.anime_standard_format_search,
    );

    // Sonarr answers a search criteria it cannot express with an empty
    // `IndexerPageableRequestChain` rather than a broad request. Do the same,
    // and without spending an upstream call.
    if urls.is_empty() {
        return Ok(SearchResponse::default());
    }

    let mut results = Vec::new();
    for url in &urls {
        let body = fetch_feed(&config, url).await?;
        results.extend(parse_feed(&body, url)?);
    }

    let mut results = dedupe_results(results);
    if let Some(limit) = result_limit(&request) {
        results.truncate(limit);
    }

    Ok(SearchResponse {
        results,
        ..Default::default()
    })
}

/// `limit == 0` means "plugin default", which here is "one page of whatever the
/// feed returned".
///
/// The adapter hard-codes `limit: 1000` on every search
/// (`crates/scryer-plugins/src/indexer_adapter.rs`), far above the 75 items a
/// Nyaa page carries; it is honoured rather than ignored so a host that ever
/// sends a smaller value is respected.
fn result_limit(request: &SearchRequest) -> Option<usize> {
    (request.limit > 0).then_some(request.limit)
}

/// True for the recent/RSS poll: either the host said so, or the request
/// carries no criteria at all.
fn is_recent_request(request: &SearchRequest) -> bool {
    if let Some(context) = request.context.as_ref() {
        return matches!(context.request_kind, PluginSearchRequestKind::Recent);
    }
    request.query.trim().is_empty()
        && request.ids.is_empty()
        && request.season.is_none()
        && request.episode.is_none()
        && request.absolute_episode.is_none()
}

/// Build the request URLs for one host search call.
///
/// `{base}/?page=rss{additional_params}[&term=…]` — Sonarr's
/// `NyaaRequestGenerator.GetPagedRequests`.
fn nyaa_urls(
    base_url: &str,
    additional_params: &str,
    req: &SearchRequest,
    anime_standard_format_search: bool,
) -> Vec<String> {
    let root = base_url.trim().trim_end_matches('/');

    if is_recent_request(req) {
        return vec![format!("{root}/?page=rss{additional_params}")];
    }

    let terms = nyaa_terms(req, anime_standard_format_search);
    if terms.is_empty() {
        return Vec::new();
    }

    // Nyaa reads the search term as `chain_get(req_args, 'q', 'term')`, so a
    // `q=` left in `additional_params` would silently win over the term this
    // plugin is issuing. Drop the term members from the operator's parameters
    // while a term of our own is in play; on a bare poll (above) they are left
    // alone, because there they are a deliberate standing filter.
    let params = strip_term_params(additional_params);
    let base = format!("{root}/?page=rss{params}");

    terms
        .into_iter()
        .map(|term| format!("{base}&term={term}"))
        .collect()
}

/// The search terms Sonarr's `NyaaRequestGenerator` would build, mapped onto
/// Scryer's single request shape.
///
/// | Sonarr criteria | terms |
/// |---|---|
/// | `AnimeEpisodeSearchCriteria` | `{t}+{abs}`, and `{t}+{abs:00}` when `abs < 10` |
/// | + `AnimeStandardFormatSearch` | also `{t}+s{ss}e{ee}` |
/// | `SingleEpisodeSearchCriteria` | `{t}+s{ss}e{ee}`, **only** with the option |
/// | `SeasonSearchCriteria` / `AnimeSeasonSearchCriteria` | `{t}+s{ss}`, **only** with the option |
/// | `SpecialEpisodeSearchCriteria` | `{t}+{episode title}` |
/// | `Daily*SearchCriteria` | none |
///
/// Two deliberate additions over Sonarr, both of which only ever fire where
/// Sonarr would have issued nothing:
///
/// * a free-text/title request with no season, episode or absolute number is
///   sent as the bare term — that is the interactive "search Nyaa for this"
///   case, and an anime-movie search (`facet: movie`), neither of which Sonarr
///   models for this indexer;
/// * the special-episode term is built from `context.episode_title`.
///
/// One thing is deliberately **not** done: the request is never fanned out over
/// `tagged_aliases`. Sonarr loops `SceneTitles` inside the request generator;
/// Scryer's host runs its own alias/id strategy tiers and calls the plugin once
/// per title, so looping here would multiply every search by the alias count.
fn nyaa_terms(req: &SearchRequest, anime_standard_format_search: bool) -> Vec<String> {
    let Some(title) = search_title(req) else {
        return Vec::new();
    };
    let prepared = prepare_query(&title);
    if prepared.is_empty() {
        return Vec::new();
    }

    // Sonarr's `SpecialEpisodeSearchCriteria`: "<clean series title> <clean
    // episode title>", cleaned by `SearchCriteriaBase.GetCleanSceneTitle`.
    if matches!(subject_kind(req), Some(PluginSearchSubjectKind::Special))
        && let Some(episode_title) = req
            .context
            .as_ref()
            .and_then(|context| context.episode_title.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        let series = clean_scene_title(&title);
        let episode = clean_scene_title(episode_title);
        if !series.is_empty() && !episode.is_empty() {
            return vec![format!("{series}+{episode}")];
        }
    }

    let mut terms = Vec::new();

    if let Some(absolute_episode) = req.absolute_episode.filter(|episode| *episode > 0) {
        terms.push(format!("{prepared}+{absolute_episode}"));
        if absolute_episode < 10 {
            terms.push(format!("{prepared}+{absolute_episode:02}"));
        }
    }

    if anime_standard_format_search {
        match (season_number(req), req.episode) {
            (Some(season), Some(episode)) if episode > 0 => {
                terms.push(format!("{prepared}+s{season:02}e{episode:02}"));
            }
            (Some(season), _) => {
                terms.push(format!("{prepared}+s{season:02}"));
            }
            _ => {}
        }
    }

    if terms.is_empty() {
        // Sonarr issues nothing for a season/episode search without the
        // standard-format option, because Nyaa numbers releases absolutely and
        // an `SxxExx` term would not match. Reproduce that rather than
        // re-fetching the whole series under a broad term.
        if req.season.is_some() || req.episode.is_some() || req.absolute_episode.is_some() {
            return Vec::new();
        }
        terms.push(prepared);
    }

    dedupe_strings(terms)
}

/// A season Sonarr would build an `s{ss}` term for: present, non-zero, and not
/// a daily series' year.
fn season_number(req: &SearchRequest) -> Option<u32> {
    req.season
        .filter(|season| *season > 0 && *season < DAILY_SEASON_FLOOR)
}

fn subject_kind(req: &SearchRequest) -> Option<PluginSearchSubjectKind> {
    req.context.as_ref().map(|context| context.subject_kind)
}

/// The single title this call searches for.
///
/// Sonarr uses `searchCriteria.SceneTitles`. `PluginSearchContext.scene_titles`
/// has no writer anywhere in the core (verified on `release-0.19.8` and
/// `release-NEXT`), so it is read first — it is the right field the moment the
/// host fills it — and `query` is the value that actually arrives today. A
/// tagged alias is used only when the host sent no query at all.
fn search_title(req: &SearchRequest) -> Option<String> {
    if let Some(scene_title) = req
        .context
        .as_ref()
        .and_then(|context| context.scene_titles.first())
        .map(|title| title.trim())
        .filter(|title| !title.is_empty())
    {
        return Some(scene_title.to_string());
    }
    let query = req.query.trim();
    if !query.is_empty() {
        return Some(query.to_string());
    }
    req.tagged_aliases
        .iter()
        .map(|alias| alias.name.trim())
        .find(|name| !name.is_empty())
        .map(str::to_string)
}

/// Sonarr's `PrepareQuery` is `query.Replace(' ', '+')` and nothing else, so a
/// title containing `&`, `#`, `=` or `%` corrupts the URL and a non-ASCII title
/// is sent as raw bytes.
///
/// Here each whitespace-separated word is percent-encoded and the words are
/// joined with `+`, which Werkzeug decodes back to a space. The result is
/// byte-identical to Sonarr's for an ASCII alphanumeric title — every case in
/// `NyaaRequestGeneratorFixture` — and correct for the ones Sonarr breaks.
fn prepare_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(percent_encode)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("+")
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// `SearchCriteriaBase.GetCleanSceneTitle`, which is what Sonarr's
/// special-episode queries are built from: drop a leading "The ", `&` becomes
/// "and", apostrophes and periods are removed, every other non-word run becomes
/// a single `+`.
///
/// Sonarr also strips diacritics; that needs Unicode normalisation tables, so
/// it is not reproduced — an accented character is a word character here and
/// survives into the term, which Nyaa's full-text search handles.
fn clean_scene_title(title: &str) -> String {
    let trimmed = title.trim();
    let without_the = trimmed
        .strip_prefix("The ")
        .or_else(|| trimmed.strip_prefix("the "))
        .or_else(|| trimmed.strip_prefix("THE "))
        .unwrap_or(trimmed);

    let mut out = String::with_capacity(without_the.len());
    let mut pending_separator = false;
    for character in without_the.chars() {
        match character {
            '&' => {
                if pending_separator && !out.is_empty() {
                    out.push('+');
                }
                pending_separator = false;
                out.push_str("and");
            }
            // `SpecialCharacter`: apostrophes (straight, back-tick, acute and
            // both curly forms) and periods are removed outright, not turned
            // into a separator.
            '\'' | '.' | '\u{0060}' | '\u{00B4}' | '\u{2018}' | '\u{2019}' => {}
            _ if character.is_alphanumeric() || character == '_' => {
                if pending_separator && !out.is_empty() {
                    out.push('+');
                }
                pending_separator = false;
                out.push(character);
            }
            _ => pending_separator = true,
        }
    }
    percent_encode_term(&out)
}

/// Percent-encode a `+`-separated term without touching the separators.
fn percent_encode_term(term: &str) -> String {
    term.split('+')
        .map(percent_encode)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("+")
}

/// Remove any `q`/`term` member from an `additional_params` string, preserving
/// the rest verbatim.
fn strip_term_params(additional_params: &str) -> String {
    additional_params
        .split('&')
        .filter(|member| {
            let key = member.split('=').next().unwrap_or_default().trim();
            !(key.eq_ignore_ascii_case("q") || key.eq_ignore_ascii_case("term"))
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for value in values {
        if !seen.iter().any(|existing| existing == &value) {
            seen.push(value);
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Transport and delivery classification
// ---------------------------------------------------------------------------

async fn fetch_feed(config: &NyaaConfig, url: &str) -> Result<String, Error> {
    StartRateGate::new(
        format!("{PROVIDER_ID}.request-start"),
        1,
        REQUEST_INTERVAL_MS,
    )
    .acquire()
    .await
    .map_err(component::deadline_deferred_error)?;

    component::log(
        LogLevel::Debug,
        format!("http_trace plugin={PROVIDER_ID} method=GET url={url}"),
    );

    let response = component::http(PluginHttpRequest {
        url: url.to_string(),
        method: Some("GET".to_string()),
        headers: config.request_headers(),
        body: Vec::new(),
    })
    .await
    .map_err(|error| {
        deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "Nyaa could not be reached".to_string(),
            format!("Nyaa request to {url} failed: {error:?}"),
        )
    })?;

    component::log(
        LogLevel::Debug,
        format!(
            "http_trace_response plugin={PROVIDER_ID} status={} url={url}",
            response.status
        ),
    );

    classify_response(response.status, &response.headers, &response.body)
}

/// Map one HTTP delivery onto Scryer's typed indexer error lanes.
///
/// What Nyaa actually does, from its own `nyaa/views/main.py` and
/// `nyaa/search.py` plus the deployment in front of it:
///
/// * a malformed `c=`/`f=` value, or one that names no real category, is
///   `flask.abort(400)` — that is a settings fault in `additional_params`, not
///   an upstream outage, so it must not cool the indexer down;
/// * the site sits behind a CDN that answers a challenge or a block with an
///   HTML page, sometimes under a 200 and sometimes under a 403/503;
/// * a wrong `base_url` gives a 404, an HTML page, or a redirect (the host's
///   plugin HTTP does not follow redirects, so a 3xx arrives as a 3xx).
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
                    "Nyaa redirected the RSS request with HTTP {status} to {location}; the \
                     configured 'base_url' is not this site's root (the host does not follow \
                     redirects)"
                ),
            ));
        }
        400 => {
            return Err(invalid_config_error(
                "additional_params",
                format!(
                    "Nyaa rejected the request with HTTP 400. It answers 400 for a category or \
                     quality filter it does not recognise: check 'additional_params' — the \
                     category must be an id such as 1_0 and the filter must be 0, 1, 2 or 3. \
                     Response: {}",
                    body_excerpt(body)
                ),
            ));
        }
        401 | 403 => {
            if is_html_delivery(headers, body) {
                return Err(invalid_response_error(
                    IndexerSearchInvalidResponseKind::UnexpectedContentType,
                    format!(
                        "Nyaa answered HTTP {status} with an HTML page: the site is likely \
                         blocking Scryer (a CDN challenge) rather than rejecting a credential — \
                         Nyaa's RSS feed needs none: {}",
                        body_excerpt(body)
                    ),
                ));
            }
            return Err(auth_failed_error(format!(
                "Nyaa refused the RSS request with HTTP {status}. Nyaa's own feed needs no \
                 credentials, so this is a proxy or mirror in front of it rejecting the \
                 configured 'username'/'password' or 'cookie': {}",
                body_excerpt(body)
            )));
        }
        429 => return Err(rate_limited_error(retry_after_seconds(headers))),
        _ => {
            return Err(deferred_error(
                IndexerSearchIncompleteReason::UpstreamFailure,
                None,
                format!("Nyaa returned HTTP {status}"),
                format!("Nyaa returned HTTP {status}: {}", body_excerpt(body)),
            ));
        }
    }

    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            format!(
                "Nyaa returned {} bytes, above the {MAX_RESPONSE_BYTES} byte ceiling",
                body.len()
            ),
        ));
    }

    let text = std::str::from_utf8(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("Nyaa feed was not valid UTF-8: {error}"),
        )
    })?;

    if is_html_delivery(headers, body) {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            format!(
                "Nyaa returned content type {:?} instead of RSS: the site is likely blocking \
                 Scryer, or 'base_url' does not point at a Nyaa instance: {}",
                header_value(headers, "content-type").unwrap_or("(absent)"),
                body_excerpt(body)
            ),
        ));
    }

    Ok(text.to_string())
}

/// An HTML delivery is one the server labelled `text/html`, or — when the
/// content type is absent or a lying `text/plain` — one whose body opens with a
/// doctype or an `<html>` root.
///
/// Nyaa serves its feed as `application/xml` (`render_rss` in
/// `nyaa/views/main.py`), so a `text/html` body is never the feed.
fn is_html_delivery(headers: &BTreeMap<String, String>, body: &[u8]) -> bool {
    if let Some(content_type) = header_value(headers, "content-type")
        && content_type.to_ascii_lowercase().contains("text/html")
    {
        return true;
    }
    let head = String::from_utf8_lossy(&body[..body.len().min(512)])
        .trim_start()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
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
        format!("Nyaa setting '{field}' is not usable"),
        detail,
        None,
        None,
    )
}

fn auth_failed_error(detail: String) -> Error {
    typed_error(
        PluginErrorCode::AuthFailed,
        "Nyaa refused the configured credentials".to_string(),
        detail,
        None,
        None,
    )
}

fn rate_limited_error(retry_after_seconds: i64) -> Error {
    typed_error(
        PluginErrorCode::RateLimited,
        "Nyaa is rate limiting Scryer".to_string(),
        format!("Nyaa returned HTTP 429; retrying after {retry_after_seconds}s"),
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
        "Nyaa returned a response Scryer could not read".to_string(),
        detail,
        None,
        Some(IndexerSearchPluginError::InvalidResponse { kind }),
    )
}

// ---------------------------------------------------------------------------
// Feed parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct FeedItem {
    title: Option<String>,
    link: Option<String>,
    guid: Option<String>,
    published_at: Option<String>,
    comment_url: Option<String>,
    enclosure_url: Option<String>,
    enclosure_length: Option<i64>,
    size: Option<String>,
    info_hash: Option<String>,
    seeders: Option<String>,
    leechers: Option<String>,
    downloads: Option<String>,
    category: Option<String>,
    category_id: Option<String>,
    comment_count: Option<String>,
    trusted: Option<String>,
    remake: Option<String>,
}

/// Parse one RSS 2.0 document into releases.
///
/// Sonarr's `RssParser.GetItems` walks `rss > channel > item`; a document
/// without a `channel` yields nothing. Here a document with no `channel` at all
/// is reported as `InvalidResponse(InvalidRoot)` rather than silently returning
/// zero releases, because an empty feed and a wrong endpoint are different
/// operator problems. An **empty** `channel` is a legitimate quiet feed.
fn parse_feed(body: &str, request_url: &str) -> Result<Vec<SearchResult>, Error> {
    let mut reader = Reader::from_str(body);
    // Text is NOT trimmed at the reader: quick-xml splits a text node at every
    // entity reference, and trimming each piece would eat the spaces around an
    // `&amp;`. The assembled values are trimmed once in `build_result`.
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut items: Vec<FeedItem> = Vec::new();
    let mut item = FeedItem::default();
    let mut in_item = false;
    let mut saw_channel = false;
    let mut current_tag: Option<ElementName> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref event)) => {
                let name = ElementName::parse(event.name().as_ref());
                match name.local.as_str() {
                    "channel" if !name.prefixed => saw_channel = true,
                    "item" if !name.prefixed => {
                        in_item = true;
                        item = FeedItem::default();
                        current_tag = None;
                    }
                    "enclosure" if in_item => {
                        parse_enclosure(event, &mut item);
                        current_tag = None;
                    }
                    _ if in_item => current_tag = Some(name),
                    _ => current_tag = None,
                }
            }
            Ok(Event::Empty(ref event)) if in_item => {
                if ElementName::parse(event.name().as_ref()).local == "enclosure" {
                    parse_enclosure(event, &mut item);
                }
            }
            Ok(Event::Text(text)) if in_item => {
                apply_text(&mut item, current_tag.as_ref(), text.as_ref());
            }
            Ok(Event::CData(text)) if in_item => {
                apply_text(&mut item, current_tag.as_ref(), text.as_ref());
            }
            // `&amp;`, `&#39;`, … arrive as their own event, and Sonarr's
            // `GetTitle` runs `WebUtility.HtmlDecode` over the result.
            Ok(Event::GeneralRef(ref reference)) if in_item => {
                if let Some(decoded) = decode_reference(reference.as_ref()) {
                    apply_text(&mut item, current_tag.as_ref(), &decoded);
                }
            }
            Ok(Event::End(ref event)) => {
                let name = ElementName::parse(event.name().as_ref());
                if name.local == "item" && !name.prefixed {
                    in_item = false;
                    current_tag = None;
                    items.push(std::mem::take(&mut item));
                } else if current_tag.as_ref().map(|tag| &tag.local) == Some(&name.local) {
                    current_tag = None;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(invalid_response_error(
                    IndexerSearchInvalidResponseKind::MalformedBody,
                    format!(
                        "Nyaa feed is not well-formed XML at byte {}: {error}",
                        reader.buffer_position()
                    ),
                ));
            }
        }

        buf.clear();
    }

    if !saw_channel {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::InvalidRoot,
            "Nyaa response has no RSS <channel> element; the configured 'base_url' is not a Nyaa \
             instance"
                .to_string(),
        ));
    }

    Ok(items
        .into_iter()
        .filter_map(|item| build_result(item, request_url))
        .collect())
}

/// An element name split into its local part and whether it carried a
/// namespace prefix.
///
/// The distinction matters here, and only here: Sonarr reads `size`,
/// `infoHash`, `seeders` and `leechers` with `FindDecendants`, which matches a
/// **local name in any namespace** — that is how it reaches `nyaa:size`. It
/// reads `title`, `link`, `guid`, `pubDate` and `comments` with
/// `item.Element(name)`, which matches only the **no-namespace** element. So
/// `nyaa:comments` (a comment *count*) is not Sonarr's `<comments>` (a comment
/// *URL*), and the fixture's assertion that `CommentUrl` is empty depends on
/// exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ElementName {
    local: String,
    prefixed: bool,
}

impl ElementName {
    fn parse(name: &str) -> Self {
        match name.rsplit_once(':') {
            Some((_, local)) => Self {
                local: local.to_ascii_lowercase(),
                prefixed: true,
            },
            None => Self {
                local: name.to_ascii_lowercase(),
                prefixed: false,
            },
        }
    }
}

/// Append one text (or resolved entity) fragment to the element it belongs to.
fn apply_text(item: &mut FeedItem, current_tag: Option<&ElementName>, value: &str) {
    if value.is_empty() {
        return;
    }
    let Some(tag) = current_tag else {
        return;
    };
    match (tag.local.as_str(), tag.prefixed) {
        ("title", false) => merge_text(&mut item.title, value),
        ("link", false) => merge_text(&mut item.link, value),
        ("guid", false) => merge_text(&mut item.guid, value),
        ("pubdate", false) => merge_text(&mut item.published_at, value),
        ("comments", false) => merge_text(&mut item.comment_url, value),
        ("comments", true) => merge_text(&mut item.comment_count, value),
        // Sonarr's namespace-agnostic reads.
        ("size", _) => merge_text(&mut item.size, value),
        ("infohash", _) => merge_text(&mut item.info_hash, value),
        ("seeders", _) => merge_text(&mut item.seeders, value),
        ("leechers", _) => merge_text(&mut item.leechers, value),
        // Nyaa's own extras, which Sonarr discards.
        ("downloads", true) => merge_text(&mut item.downloads, value),
        ("categoryid", true) => merge_text(&mut item.category_id, value),
        ("category", _) => merge_text(&mut item.category, value),
        ("trusted", true) => merge_text(&mut item.trusted, value),
        ("remake", true) => merge_text(&mut item.remake, value),
        _ => {}
    }
}

fn merge_text(slot: &mut Option<String>, value: &str) {
    match slot {
        Some(existing) => existing.push_str(value),
        None => *slot = Some(value.to_string()),
    }
}

fn parse_enclosure(event: &BytesStart<'_>, item: &mut FeedItem) {
    for attr in event.attributes().flatten() {
        let value = attr.value.to_string();
        match attr.key.as_ref() {
            "url" => item.enclosure_url = Some(value),
            "length" => item.enclosure_length = value.replace(',', "").parse::<i64>().ok(),
            _ => {}
        }
    }
}

/// Resolve one `&…;` reference, given its content (the bytes between `&` and
/// `;`). This is Sonarr's `WebUtility.HtmlDecode` reduced to what an RSS feed
/// can legally carry: the five XML entities, the HTML `&nbsp;`, and numeric
/// character references in decimal or hex.
///
/// An unknown reference is put back verbatim rather than dropped.
fn decode_reference(entity: &str) -> Option<String> {
    match entity {
        "amp" => return Some("&".to_string()),
        "lt" => return Some("<".to_string()),
        "gt" => return Some(">".to_string()),
        "quot" => return Some("\"".to_string()),
        "apos" => return Some("'".to_string()),
        "nbsp" => return Some(" ".to_string()),
        _ => {}
    }
    let decoded = entity.strip_prefix('#').and_then(|digits| {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        char::from_u32(code).map(|value| value.to_string())
    });
    Some(decoded.unwrap_or_else(|| format!("&{entity};")))
}

/// Sonarr's `IsValidRelease` (`HttpIndexerBase.cs`): a release with no title or
/// no download URL is dropped rather than surfaced.
fn build_result(item: FeedItem, request_url: &str) -> Option<SearchResult> {
    let title = item.title.as_deref().map(str::trim).unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    // `TorrentRssParser` prefers a torrent enclosure and falls back to `<link>`;
    // the Nyaa feed only ever carries `<link>`.
    let download_url = item
        .enclosure_url
        .as_deref()
        .or(item.link.as_deref())
        .and_then(|value| resolve_url(request_url, value))?;
    // Nyaa emits magnet links instead of `.torrent` URLs when the request
    // carries `&m` (`use_magnet_links` in `nyaa/views/main.py`).
    let magnet_url = download_url
        .starts_with("magnet:?")
        .then(|| download_url.clone());

    // `UseGuidInfoUrl = true`: the `<guid>` is the `/view/<id>` page.
    let info_url = item.guid.as_deref().and_then(|value| {
        let resolved = resolve_url(request_url, value)?;
        (!resolved.starts_with("magnet:?")).then_some(resolved)
    });

    let size_bytes = item
        .enclosure_length
        .filter(|value| *value > 0)
        .or_else(|| item.size.as_deref().and_then(parse_size));

    let seeders = item.seeders.as_deref().and_then(parse_count);
    let leechers = item.leechers.as_deref().and_then(parse_count);
    // `CalculatePeersAsSum = true` with `PeersElementName = "leechers"`.
    let peers = match (seeders, leechers) {
        (Some(seeders), Some(leechers)) => Some(seeders + leechers),
        (None, Some(leechers)) => Some(leechers),
        _ => None,
    };

    let info_hash_v1 = item
        .info_hash
        .as_deref()
        .map(str::trim)
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|value| value.to_ascii_lowercase());

    let category = item
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let category_id = item
        .category_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut provider_categories = Vec::new();
    if let Some(category_id) = category_id {
        provider_categories.push(category_id.to_string());
    }
    if let Some(category) = category {
        provider_categories.push(category.to_string());
    }

    // Nyaa has no freeleech, so `extra["freeleech"]` is deliberately never
    // emitted: the core reads it as an `Option<bool>` and an absent key means
    // "unknown" while `false` would assert "not freeleech". What Nyaa does
    // publish is a per-release trust signal.
    let mut indexer_flags = Vec::new();
    if is_yes(item.trusted.as_deref()) {
        indexer_flags.push("trusted".to_string());
    }
    if is_yes(item.remake.as_deref()) {
        indexer_flags.push("remake".to_string());
    }

    let mut provider_extra = HashMap::new();
    provider_extra.insert(
        "feed_source".to_string(),
        serde_json::Value::from(PROVIDER_ID),
    );
    if let Some(category) = category {
        provider_extra.insert("category".to_string(), serde_json::Value::from(category));
    }
    if let Some(category_id) = category_id {
        provider_extra.insert(
            "category_id".to_string(),
            serde_json::Value::from(category_id),
        );
    }
    if let Some(comments) = item.comment_count.as_deref().and_then(parse_count) {
        provider_extra.insert("comments".to_string(), serde_json::Value::from(comments));
    }
    if !indexer_flags.is_empty() {
        // `result.indexer_flags` reaches the host's `extra` but is not read
        // back when a candidate is reused (`search_client.rs` restores `tags`,
        // not `indexer_flags`), so the same values are published under the key
        // that survives the round trip.
        provider_extra.insert(
            "tags".to_string(),
            serde_json::Value::from(indexer_flags.clone()),
        );
    }

    Some(SearchResult {
        link: Some(download_url.clone()),
        info_url,
        // Sonarr's `GetCommentUrl` reads the no-namespace `<comments>`
        // element. Nyaa's feed has none — `nyaa:comments` is a *count* — so
        // this is `None` for every real item, exactly as `NyaaFixture`
        // asserts; a mirror that does emit one is honoured.
        comment_url: item
            .comment_url
            .as_deref()
            .and_then(|value| resolve_url(request_url, value)),
        guid: item
            .guid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| Some(download_url.clone())),
        size_bytes,
        published_at: item
            .published_at
            .as_deref()
            .and_then(rfc2822_to_rfc3339_utc),
        seeders,
        peers,
        leechers,
        info_hash_v1,
        magnet_url,
        grabs: item.downloads.as_deref().and_then(parse_count),
        categories: category.map(str::to_string).into_iter().collect(),
        provider_categories,
        indexer_flags,
        provider_extra,
        ..torrent_result(title, Some(download_url))
    })
}

fn is_yes(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("yes") | Some("true") | Some("1")
    )
}

fn parse_count(value: &str) -> Option<i64> {
    let trimmed = value.trim().replace(',', "");
    trimmed.parse::<i64>().ok().filter(|count| *count >= 0)
}

fn resolve_url(request_url: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("magnet:?")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return Some(trimmed.to_string());
    }
    url::Url::parse(request_url)
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|url| url.to_string())
}

/// Dedupe by guid, then by download URL.
///
/// Sonarr has no cross-request dedupe for Nyaa at all: the anime-episode search
/// issues `{title}+9` and `{title}+09` in the same tier and merges both result
/// sets, so a release matching both terms is reported twice.
fn dedupe_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for result in results {
        let key = result
            .guid
            .clone()
            .or_else(|| result.download_url.clone())
            .unwrap_or_else(|| result.title.clone());
        if seen
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&key))
        {
            continue;
        }
        seen.push(key);
        out.push(result);
    }
    out
}

// ---------------------------------------------------------------------------
// Size
// ---------------------------------------------------------------------------

/// Sonarr's `RssParser.ParseSize(value, defaultToBinaryPrefix: true)`, which is
/// what `SizeElementName = "size"` runs over `nyaa:size` ("609.6 MiB").
///
/// Regex, for reference:
/// `(?<value>(?<!\.\d*)(?:\d+,)*\d+(?:\.\d{1,3})?)\W?(?<unit>[KMG]i?B)(?![\w/])`
/// with the whole string short-circuiting to `long.Parse` when it is all
/// digits. Rust's `regex` crate has no look-around, so this is the same grammar
/// written as a leftmost-match scanner.
fn parse_size(value: &str) -> Option<i64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse::<i64>().ok();
    }

    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }
        // `(?<!\.\d*)`: a value may not begin inside a decimal fraction.
        if start > 0 && (bytes[start - 1] == b'.' || bytes[start - 1].is_ascii_digit()) {
            continue;
        }
        let Some((number, after_value)) = match_number(bytes, start) else {
            continue;
        };
        // `\W?`: at most one non-word character between value and unit.
        let mut cursor = after_value;
        if cursor < bytes.len() && !is_word_byte(bytes[cursor]) {
            cursor += 1;
        }
        let Some((power, after_unit)) = match_unit(bytes, cursor) else {
            continue;
        };
        // `(?![\w/])`: "1.5 GB/s" is a rate, not a size.
        if after_unit < bytes.len()
            && (is_word_byte(bytes[after_unit]) || bytes[after_unit] == b'/')
        {
            continue;
        }
        // `defaultToBinaryPrefix: true` — KB/MB/GB are 1024-based here too.
        return Some((number * 1024_f64.powi(power)).round() as i64);
    }
    None
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `(?:\d+,)*\d+(?:\.\d{1,3})?`
fn match_number(bytes: &[u8], start: usize) -> Option<(f64, usize)> {
    let mut cursor = start;
    let mut digits = String::new();
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_digit() {
            digits.push(bytes[cursor] as char);
            cursor += 1;
        } else if bytes[cursor] == b','
            && cursor + 1 < bytes.len()
            && bytes[cursor + 1].is_ascii_digit()
        {
            cursor += 1;
        } else {
            break;
        }
    }
    if digits.is_empty() {
        return None;
    }
    if cursor < bytes.len() && bytes[cursor] == b'.' {
        let fraction_start = cursor + 1;
        let mut fraction_end = fraction_start;
        while fraction_end < bytes.len()
            && fraction_end - fraction_start < 3
            && bytes[fraction_end].is_ascii_digit()
        {
            fraction_end += 1;
        }
        // Only a 1..=3 digit fraction is part of the number, and only when the
        // digit run really ends there (a 4th digit means this is not a size).
        if fraction_end > fraction_start
            && !(fraction_end < bytes.len() && bytes[fraction_end].is_ascii_digit())
        {
            digits.push('.');
            digits.push_str(std::str::from_utf8(&bytes[fraction_start..fraction_end]).ok()?);
            cursor = fraction_end;
        }
    }
    digits.parse::<f64>().ok().map(|value| (value, cursor))
}

/// `[KMG]i?B`, returning the power of the prefix and the offset just past the
/// unit.
fn match_unit(bytes: &[u8], start: usize) -> Option<(i32, usize)> {
    let power = match bytes.get(start)?.to_ascii_lowercase() {
        b'k' => 1,
        b'm' => 2,
        b'g' => 3,
        _ => return None,
    };
    let binary = bytes.get(start + 1)?.eq_ignore_ascii_case(&b'i');
    let unit_end = if binary { start + 3 } else { start + 2 };
    if !bytes
        .get(unit_end.checked_sub(1)?)?
        .eq_ignore_ascii_case(&b'b')
    {
        return None;
    }
    Some((power, unit_end))
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// RSS `pubDate` is RFC 2822; Nyaa emits `Tue, 24 Aug 2021 22:18:46 -0000`.
///
/// The adapter passes `published_at` through as a raw string
/// (`crates/scryer-plugins/src/indexer_adapter.rs`) and the RSS staleness
/// tracker parses it with `DateTime::parse_from_rfc3339` **only**
/// (`crates/scryer-infrastructure-acquisition/src/indexers/search_client.rs`),
/// verified on both `release-0.19.8` and `release-NEXT`. A raw `pubDate` is
/// therefore silently dropped, so it is normalised to RFC 3339 UTC here.
fn rfc2822_to_rfc3339_utc(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Some feeds emit an ISO instant instead; pass a valid one straight on.
    if raw.len() >= 20 && raw.as_bytes().get(10) == Some(&b'T') {
        return Some(raw.to_string());
    }

    // Drop the optional `Tue, ` day-of-week prefix.
    let rest = match raw.split_once(',') {
        Some((_, tail)) => tail.trim(),
        None => raw,
    };

    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year_text = parts.next()?;
    let year: i64 = year_text.parse().ok()?;
    // RFC 822 two-digit years, per RFC 2822 §4.3.
    let year = match year_text.len() {
        2 if year >= 50 => 1900 + year,
        2 => 2000 + year,
        _ => year,
    };

    let mut time_parts = parts.next()?.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = match time_parts.next() {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let offset_seconds = parts.next().and_then(zone_offset_seconds).unwrap_or(0);

    let timestamp = days_from_civil(year, month, day) * 86_400
        + hour * 3_600
        + minute * 60
        // A leap second folds onto :59 rather than rolling the minute over.
        + second.min(59)
        - offset_seconds;
    unix_to_rfc3339(timestamp)
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lowered = name.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|month| lowered.starts_with(month))
        .map(|index| index as i64 + 1)
}

/// `+HHMM` / `-HHMM` (Nyaa emits `-0000`, which RFC 2822 defines as UTC with an
/// unknown local zone), plus the obsolete alphabetic zones RFC 2822 §4.3 keeps
/// alive. Anything unrecognised is UTC, which is what RFC 2822 mandates for an
/// unknown alphabetic zone.
fn zone_offset_seconds(zone: &str) -> Option<i64> {
    let zone = zone.trim();
    if let Some(rest) = zone.strip_prefix(['+', '-']) {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        if rest.len() != 4 || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let hours: i64 = rest[..2].parse().ok()?;
        let minutes: i64 = rest[2..].parse().ok()?;
        return Some(sign * (hours * 3_600 + minutes * 60));
    }
    let hours = match zone.to_ascii_uppercase().as_str() {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "EDT" => -4,
        "EST" | "CDT" => -5,
        "CST" | "MDT" => -6,
        "MST" | "PDT" => -7,
        "PST" => -8,
        _ => return None,
    };
    Some(hours * 3_600)
}

fn unix_to_rfc3339(timestamp: i64) -> Option<String> {
    let days = timestamp.div_euclid(86_400);
    let seconds_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Howard Hinnant's `days_from_civil`, the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct NyaaConfig {
    base_url: String,
    additional_params: String,
    anime_standard_format_search: bool,
    user_agent: String,
    cookie: Option<String>,
    username: Option<String>,
    password: Option<String>,
    additional_headers: String,
}

impl NyaaConfig {
    fn from_host() -> Result<Self, Error> {
        Ok(Self {
            base_url: validate_base_url(config_value("base_url").as_deref().unwrap_or_default())?,
            additional_params: validate_additional_params(
                config_value("additional_params")
                    .as_deref()
                    .unwrap_or(DEFAULT_ADDITIONAL_PARAMS),
            )?,
            anime_standard_format_search: config_bool("anime_standard_format_search"),
            user_agent: config_value("user_agent").unwrap_or_else(|| USER_AGENT.to_string()),
            cookie: config_value("cookie"),
            username: config_value("username"),
            password: config_value("password"),
            additional_headers: config_value("additional_headers").unwrap_or_default(),
        })
    }

    fn request_headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            // Deliberately excludes `text/html`: Sonarr's `RssParser.PreProcess`
            // only reports "responded with html content" when the request did
            // not ask for HTML, and the same rule is what makes a CDN challenge
            // page distinguishable here.
            (
                "Accept".to_string(),
                "application/rss+xml, application/xml, text/xml;q=0.9".to_string(),
            ),
            ("User-Agent".to_string(), self.user_agent.clone()),
            ("Accept-Language".to_string(), "en-US,en;q=0.9".to_string()),
        ]);
        if let Some(cookie) = self.cookie.as_deref() {
            headers.insert("Cookie".to_string(), cookie.to_string());
        }
        if let Some(username) = self.username.as_deref() {
            let password = self.password.as_deref().unwrap_or_default();
            let encoded = STANDARD.encode(format!("{username}:{password}"));
            headers.insert("Authorization".to_string(), format!("Basic {encoded}"));
        }
        for line in self.additional_headers.lines() {
            let Some((name, value)) = line.trim().split_once(':') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() && !value.is_empty() {
                headers.insert(name.to_string(), value.to_string());
            }
        }
        headers
    }
}

/// `NyaaSettingsValidator`'s `RuleFor(c => c.BaseUrl).ValidRootUrl()`
/// (`NyaaSettings.cs`), as a typed configuration error rather than an untyped
/// failure.
fn validate_base_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "base_url",
            format!("Nyaa needs a website URL, for example {DEFAULT_BASE_URL}"),
        ));
    }
    let parsed = url::Url::parse(trimmed).map_err(|error| {
        invalid_config_error(
            "base_url",
            format!("'{trimmed}' is not a valid URL: {error}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        return Err(invalid_config_error(
            "base_url",
            format!("'{trimmed}' must be an http(s) URL with a host"),
        ));
    }
    if !parsed.query().unwrap_or_default().is_empty()
        || !parsed.fragment().unwrap_or_default().is_empty()
    {
        return Err(invalid_config_error(
            "base_url",
            format!(
                "'{trimmed}' must be the site root, not a search URL — put category and filter \
                 settings in 'additional_params' instead"
            ),
        ));
    }
    Ok(trimmed.to_string())
}

/// `NyaaSettingsValidator`'s
/// `RuleFor(c => c.AdditionalParameters).Matches("(&[a-z]+=[a-z0-9_]+)*", IgnoreCase)`.
///
/// That rule is **vacuous** in Sonarr: FluentValidation's `Matches` is an
/// unanchored `Regex.IsMatch`, and `(…)*` matches the empty string at position
/// 0, so every value passes — including `cats=1_0` with no leading `&`, which
/// then produces `…?page=rsscats=1_0` and a request for the unfiltered front
/// page.
///
/// So the shape is enforced here rather than the character class: every member
/// must start with `&` and carry a `[A-Za-z][A-Za-z0-9_]*` key, optionally with
/// a value. The value is **not** restricted to `[a-z0-9_]`, because Sonarr's
/// class would reject working parameters the site documents — `&u=Erai-raws`
/// filters by uploader and contains a hyphen.
fn validate_additional_params(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if !trimmed.starts_with('&') {
        return Err(invalid_config_error(
            "additional_params",
            format!(
                "'{trimmed}' must start with '&' — each parameter is appended to the RSS URL, so \
                 the value looks like '{DEFAULT_ADDITIONAL_PARAMS}'"
            ),
        ));
    }
    for member in trimmed.split('&').skip(1) {
        if member.is_empty() {
            return Err(invalid_config_error(
                "additional_params",
                format!("'{trimmed}' has an empty parameter (two '&' in a row, or a trailing '&')"),
            ));
        }
        let (key, value) = match member.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (member, None),
        };
        let key_is_valid = key.starts_with(|c: char| c.is_ascii_alphabetic())
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !key_is_valid {
            return Err(invalid_config_error(
                "additional_params",
                format!("'{member}' in 'additional_params' is not a 'name=value' parameter"),
            ));
        }
        if let Some(value) = value
            && value
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '#' | '?' | '&'))
        {
            return Err(invalid_config_error(
                "additional_params",
                format!(
                    "the value of '{key}' in 'additional_params' contains a character that cannot \
                     appear in a URL query ('{value}')"
                ),
            ));
        }
    }
    Ok(trimmed.to_string())
}

fn config_value(key: &str) -> Option<String> {
    component::config_get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_bool(key: &str) -> bool {
    config_value(key).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
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

scryer_indexer_component_main!(descriptor = build_descriptor, search = search,);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_pdk::component::StructuredPluginError;
    use scryer_plugin_sdk::{
        PluginSearchContext, PluginSearchOrigin, PluginSearchQueryKind, TaggedAlias,
    };

    /// Sonarr's `Files/Indexers/Nyaa/Nyaa2021.xml`, verbatim.
    const RECENT_FEED: &str = r#"<rss xmlns:atom="http://www.w3.org/2005/Atom"
  xmlns:nyaa="https://nyaa.si/xmlns/nyaa" version="2.0">
  <channel>
    <title>Nyaa - Home - Torrent File RSS</title>
    <description>RSS Feed for Home</description>
    <link>https://nyaa.si/</link>
    <atom:link href="https://nyaa.si/?page=rss" rel="self" type="application/rss+xml"/>
    <item>
      <title>[Foxy-Subs] Mahouka Koukou no Yuutousei - 08 [720p] [3194D881].mkv</title>
      <link>https://nyaa.si/download/1424896.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/1424896</guid>
      <pubDate>Tue, 24 Aug 2021 22:18:46 -0000</pubDate>
      <nyaa:seeders>4</nyaa:seeders>
      <nyaa:leechers>3</nyaa:leechers>
      <nyaa:downloads>2</nyaa:downloads>
      <nyaa:infoHash>e8ca5e20eca876339f41c3d9e95ea66c1d7caaee</nyaa:infoHash>
      <nyaa:categoryId>1_3</nyaa:categoryId>
      <nyaa:category>Anime - Non-English-translated</nyaa:category>
      <nyaa:size>609.6 MiB</nyaa:size>
      <nyaa:comments>0</nyaa:comments>
      <nyaa:trusted>No</nyaa:trusted>
      <nyaa:remake>No</nyaa:remake>
      <description>
        <![CDATA[ <a href="https://nyaa.si/view/1424896">#1424896 | [Foxy-Subs] Mahouka Koukou no Yuutousei - 08 [720p] [3194D881].mkv</a> | 609.6 MiB | Anime - Non-English-translated | E8CA5E20ECA876339F41C3D9E95EA66C1D7CAAEE ]]>
      </description>
    </item>
    <item>
      <title>Macross Zero (BDRip 1920x1080p x265 HEVC TrueHD, FLAC 5.1+2.0)[sxales]</title>
      <link>https://nyaa.si/download/1424895.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/1424895</guid>
      <pubDate>Tue, 24 Aug 2021 22:03:11 -0000</pubDate>
      <nyaa:seeders>23</nyaa:seeders>
      <nyaa:leechers>32</nyaa:leechers>
      <nyaa:downloads>17</nyaa:downloads>
      <nyaa:infoHash>26f37f26d5b3475b41a98dc575fabfa6f8d32a76</nyaa:infoHash>
      <nyaa:categoryId>1_2</nyaa:categoryId>
      <nyaa:category>Anime - English-translated</nyaa:category>
      <nyaa:size>5.7 GiB</nyaa:size>
      <nyaa:comments>2</nyaa:comments>
      <nyaa:trusted>No</nyaa:trusted>
      <nyaa:remake>No</nyaa:remake>
      <description>
        <![CDATA[ <a href="https://nyaa.si/view/1424895">#1424895 | Macross Zero (BDRip 1920x1080p x265 HEVC TrueHD, FLAC 5.1+2.0)[sxales]</a> | 5.7 GiB | Anime - English-translated | 26F37F26D5B3475B41A98DC575FABFA6F8D32A76 ]]>
      </description>
    </item>
    <item>
      <title>Fumetsu no Anata e - 19 [WEBDL 1080p] Ukr DVO</title>
      <link>https://nyaa.si/download/1424887.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/1424887</guid>
      <pubDate>Tue, 24 Aug 2021 21:23:06 -0000</pubDate>
      <nyaa:seeders>5</nyaa:seeders>
      <nyaa:leechers>4</nyaa:leechers>
      <nyaa:downloads>4</nyaa:downloads>
      <nyaa:infoHash>3e4300e24b39983802162877755aab4380bd137a</nyaa:infoHash>
      <nyaa:categoryId>1_3</nyaa:categoryId>
      <nyaa:category>Anime - Non-English-translated</nyaa:category>
      <nyaa:size>1.4 GiB</nyaa:size>
      <nyaa:comments>0</nyaa:comments>
      <nyaa:trusted>No</nyaa:trusted>
      <nyaa:remake>No</nyaa:remake>
      <description>
        <![CDATA[ <a href="https://nyaa.si/view/1424887">#1424887 | Fumetsu no Anata e - 19 [WEBDL 1080p] Ukr DVO</a> | 1.4 GiB | Anime - Non-English-translated | 3E4300E24B39983802162877755AAB4380BD137A ]]>
      </description>
    </item>
  </channel>
</rss>"#;

    const FEED_URL: &str = "https://nyaa.si/?page=rss&cats=1_0&filter=1";

    fn search_context(
        request_kind: PluginSearchRequestKind,
        subject_kind: PluginSearchSubjectKind,
    ) -> PluginSearchContext {
        PluginSearchContext {
            request_kind,
            search_origin: match request_kind {
                PluginSearchRequestKind::Recent => PluginSearchOrigin::Rss,
                PluginSearchRequestKind::Search => PluginSearchOrigin::Automatic,
            },
            subject_kind,
            query_kind: PluginSearchQueryKind::Title,
            ..PluginSearchContext::default()
        }
    }

    /// The typed error a plugin function returned, as the host's bridge
    /// recovers it (`component.rs::to_plugin_result`).
    fn structured(error: &Error) -> PluginError {
        error
            .downcast_ref::<StructuredPluginError>()
            .expect("error should be a structured plugin error")
            .plugin_error()
            .clone()
    }

    // -----------------------------------------------------------------------
    // Descriptor
    // -----------------------------------------------------------------------

    #[test]
    fn descriptor_is_id_free_and_movie_anime_text_capable_for_scryer_dispatch() {
        let descriptor = build_descriptor();
        assert_eq!(descriptor.sdk_version, SDK_VERSION);
        assert_eq!(descriptor.sdk_constraint, current_sdk_constraint());

        let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
            panic!("expected indexer descriptor");
        };

        assert!(indexer.capabilities.supported_ids.is_empty());
        assert!(indexer.capabilities.supported_external_ids.is_empty());
        // The host only dispatches a text strategy when `query_param` is set.
        assert_eq!(indexer.capabilities.query_param.as_deref(), Some("term"));
        assert_eq!(
            indexer.capabilities.supported_query_facets,
            vec!["movie".to_string(), "anime".to_string()]
        );
        assert!(
            indexer
                .capabilities
                .search_inputs
                .contains(&IndexerSearchInput::TextQuery)
        );
    }

    #[test]
    fn the_descriptor_claims_only_what_the_feed_carries() {
        let ProviderDescriptor::Indexer(indexer) = build_descriptor().provider else {
            panic!("expected indexer descriptor");
        };
        let torrent = indexer.capabilities.torrent.expect("torrent capabilities");

        assert!(torrent.reports_seeders);
        assert!(torrent.reports_peers);
        assert!(torrent.reports_leechers);
        // `nyaa:infoHash` is in every item.
        assert!(torrent.reports_info_hash);
        // The `<link>` is a `.torrent` URL unless the operator asks for magnets.
        assert!(!torrent.reports_magnet_uri);
        // Public tracker: no freeleech, no private flags, no hit-and-run rule.
        assert!(!torrent.reports_volume_factors);
        assert!(!torrent.supports_private_tracker_flags);
        assert!(!torrent.supports_seed_requirements);

        let features = indexer
            .capabilities
            .response_features
            .expect("response features");
        assert!(features.grabs, "nyaa:downloads");
        assert!(features.info_url, "the guid is the /view/ page");
        assert!(features.guid);
        assert!(!features.comments, "nyaa:comments is a count, not a URL");
        assert!(!features.languages);

        // Nyaa folds the season and episode into the search term.
        assert!(indexer.capabilities.season_param.is_none());
        assert!(indexer.capabilities.episode_param.is_none());
    }

    #[test]
    fn the_descriptor_declares_one_unpaged_page_of_seventy_five() {
        let ProviderDescriptor::Indexer(indexer) = build_descriptor().provider else {
            panic!("expected indexer descriptor");
        };
        let limits = indexer.capabilities.limits.expect("limits");
        assert_eq!(limits.page_size, Some(75));
        assert_eq!(limits.max_page_size, Some(75));
        assert_eq!(limits.max_pages, Some(1));
        assert_eq!(limits.rate_limit_hint_seconds, Some(2));
        assert_eq!(indexer.rate_limit_seconds, Some(2));
        assert_eq!(
            indexer
                .strategy_plan
                .expect("strategy plan")
                .max_parallel_strategies,
            1
        );
    }

    #[test]
    fn the_declared_category_table_is_nyaas_published_ids() {
        let ProviderDescriptor::Indexer(indexer) = build_descriptor().provider else {
            panic!("expected indexer descriptor");
        };
        let model = indexer.capabilities.category_model.expect("category model");
        assert!(model.separate_anime_categories);
        assert_eq!(model.categories.len(), 23);

        let anime = model
            .categories
            .iter()
            .find(|category| category.value == "1_0")
            .expect("1_0");
        assert_eq!(anime.label.as_deref(), Some("Anime"));
        assert_eq!(anime.facets, vec!["anime".to_string(), "movie".to_string()]);

        let raw = model
            .categories
            .iter()
            .find(|category| category.value == "1_4")
            .expect("1_4");
        assert_eq!(raw.label.as_deref(), Some("Anime - Raw"));

        // Nothing outside the anime tree carries a Scryer facet.
        assert!(
            model
                .categories
                .iter()
                .filter(|category| !category.value.starts_with("1_"))
                .all(|category| category.facets.is_empty())
        );
    }

    #[test]
    fn the_config_field_keys_are_the_published_contract() {
        let keys: Vec<String> = config_fields().into_iter().map(|field| field.key).collect();
        assert_eq!(
            keys,
            vec![
                "base_url",
                "anime_standard_format_search",
                "additional_params",
                "minimum_seeders",
                "user_agent",
                "cookie",
                "username",
                "password",
                "additional_headers",
            ]
        );

        let user_agent = config_fields()
            .into_iter()
            .find(|field| field.key == "user_agent")
            .expect("user_agent");
        // The shared descriptor builder advertised a hard-coded
        // "Scryer Nyaa Indexer/0.1" that never tracked the crate version.
        assert_eq!(user_agent.default_value.as_deref(), Some(USER_AGENT));
        assert!(USER_AGENT.starts_with("scryer-nyaa-indexer/"));

        let additional_params = config_fields()
            .into_iter()
            .find(|field| field.key == "additional_params")
            .expect("additional_params");
        assert_eq!(
            additional_params.default_value.as_deref(),
            Some(DEFAULT_ADDITIONAL_PARAMS)
        );
    }

    // -----------------------------------------------------------------------
    // Request building — Sonarr's NyaaRequestGeneratorFixture
    // -----------------------------------------------------------------------

    #[test]
    fn movie_freetext_search_uses_nyaa_term_query() {
        let req = SearchRequest {
            query: "JUJUTSU KAISEN 0".to_string(),
            facet: Some("movie".to_string()),
            ..SearchRequest::default()
        };

        let urls = nyaa_urls("https://nyaa.si/", DEFAULT_ADDITIONAL_PARAMS, &req, false);

        assert_eq!(
            urls,
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=JUJUTSU+KAISEN+0"]
        );
    }

    #[test]
    fn anime_absolute_episode_search_matches_sonarr_terms() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            absolute_episode: Some(9),
            ..SearchRequest::default()
        };

        let urls = nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false);

        assert_eq!(
            urls,
            vec![
                "https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+9",
                "https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+09",
            ]
        );
    }

    #[test]
    fn anime_standard_format_search_adds_season_episode_term() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            absolute_episode: Some(9),
            season: Some(1),
            episode: Some(9),
            ..SearchRequest::default()
        };

        let urls = nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, true);

        assert_eq!(
            urls,
            vec![
                "https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+9",
                "https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+09",
                "https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+s01e09",
            ]
        );
    }

    #[test]
    fn anime_standard_format_search_adds_season_pack_term() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            season: Some(3),
            ..SearchRequest::default()
        };

        let urls = nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, true);

        assert_eq!(
            urls,
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+s03"]
        );
    }

    /// `NyaaRequestGeneratorFixture.should_not_search_season`: without the
    /// standard-format option a season search issues no request at all.
    #[test]
    fn a_season_search_without_the_standard_format_option_issues_no_request() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            season: Some(1),
            ..SearchRequest::default()
        };

        assert!(nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false).is_empty());
    }

    #[test]
    fn a_single_episode_search_needs_the_standard_format_option() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            season: Some(2),
            episode: Some(4),
            ..SearchRequest::default()
        };

        assert!(nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false).is_empty());
        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, true),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden+s02e04"]
        );
    }

    /// Sonarr answers `DailySeasonSearchCriteria` with an empty chain; a daily
    /// series' "season" is its year.
    #[test]
    fn a_daily_season_number_never_becomes_a_season_term() {
        let req = SearchRequest {
            query: "Some Daily Show".to_string(),
            season: Some(2014),
            ..SearchRequest::default()
        };

        assert!(nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, true).is_empty());
    }

    #[test]
    fn a_recent_poll_asks_for_the_bare_feed() {
        let req = SearchRequest {
            context: Some(search_context(
                PluginSearchRequestKind::Recent,
                PluginSearchSubjectKind::Unknown,
            )),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si/", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1"]
        );
    }

    /// A `Recent` request kind wins even when stale criteria are attached, and
    /// the operator's standing `q=` filter is left alone on a poll.
    #[test]
    fn a_recent_poll_keeps_a_standing_term_in_the_additional_parameters() {
        let req = SearchRequest {
            query: "ignored".to_string(),
            context: Some(search_context(
                PluginSearchRequestKind::Recent,
                PluginSearchSubjectKind::Title,
            )),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", "&c=1_2&q=Erai", &req, false),
            vec!["https://nyaa.si/?page=rss&c=1_2&q=Erai"]
        );
    }

    /// Nyaa reads the term as `chain_get(req_args, 'q', 'term')`, so a `q=` in
    /// `additional_params` would silently win over the plugin's own term.
    #[test]
    fn a_search_term_is_never_shadowed_by_a_q_in_the_additional_parameters() {
        let req = SearchRequest {
            query: "Bleach".to_string(),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", "&c=1_2&q=Erai&f=0", &req, false),
            vec!["https://nyaa.si/?page=rss&c=1_2&f=0&term=Bleach"]
        );
        assert_eq!(
            nyaa_urls("https://nyaa.si", "&term=Erai&f=0", &req, false),
            vec!["https://nyaa.si/?page=rss&f=0&term=Bleach"]
        );
    }

    /// Sonarr's `SpecialEpisodeSearchCriteria` searches
    /// "<clean series title> <clean episode title>".
    #[test]
    fn a_special_episode_searches_the_episode_title() {
        let req = SearchRequest {
            query: "Gintama".to_string(),
            facet: Some("special".to_string()),
            context: Some(PluginSearchContext {
                episode_title: Some("The Perfect Christmas Eve".to_string()),
                ..search_context(
                    PluginSearchRequestKind::Search,
                    PluginSearchSubjectKind::Special,
                )
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Gintama+Perfect+Christmas+Eve"]
        );
    }

    #[test]
    fn a_special_episode_without_an_episode_title_falls_back_to_the_series_term() {
        let req = SearchRequest {
            query: "Gintama".to_string(),
            facet: Some("special".to_string()),
            context: Some(search_context(
                PluginSearchRequestKind::Search,
                PluginSearchSubjectKind::Special,
            )),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Gintama"]
        );
    }

    #[test]
    fn the_clean_scene_title_matches_sonarrs_rule() {
        assert_eq!(
            clean_scene_title("The Perfect Christmas Eve"),
            "Perfect+Christmas+Eve"
        );
        assert_eq!(clean_scene_title("Tom & Jerry"), "Tom+and+Jerry");
        assert_eq!(
            clean_scene_title("Marvel's Agents of S.H.I.E.L.D."),
            "Marvels+Agents+of+SHIELD"
        );
        assert_eq!(clean_scene_title("  spaced   out  "), "spaced+out");
    }

    /// The host runs its own alias tiers and calls the plugin once per title;
    /// looping the aliases here would multiply every search by the alias count.
    #[test]
    fn tagged_aliases_do_not_fan_the_request_out() {
        let req = SearchRequest {
            query: "Naruto Shippuuden".to_string(),
            tagged_aliases: vec![
                TaggedAlias {
                    name: "Naruto Shippuden".to_string(),
                    language: "en".to_string(),
                },
                TaggedAlias {
                    name: "ナルト 疾風伝".to_string(),
                    language: "ja".to_string(),
                },
            ],
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden"]
        );
    }

    #[test]
    fn a_tagged_alias_is_used_only_when_the_host_sent_no_query() {
        let req = SearchRequest {
            tagged_aliases: vec![TaggedAlias {
                name: "Naruto Shippuuden".to_string(),
                language: "en".to_string(),
            }],
            context: Some(search_context(
                PluginSearchRequestKind::Search,
                PluginSearchSubjectKind::Title,
            )),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Naruto+Shippuuden"]
        );
    }

    /// `context.scene_titles` has no writer in the core today, but it is the
    /// field Sonarr searches with, so it wins when the host ever fills it.
    #[test]
    fn a_scene_title_from_the_context_wins_over_the_query() {
        let req = SearchRequest {
            query: "Attack on Titan".to_string(),
            context: Some(PluginSearchContext {
                scene_titles: vec!["Shingeki no Kyojin".to_string()],
                ..search_context(
                    PluginSearchRequestKind::Search,
                    PluginSearchSubjectKind::Title,
                )
            }),
            ..SearchRequest::default()
        };

        assert_eq!(
            nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false),
            vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Shingeki+no+Kyojin"]
        );
    }

    #[test]
    fn a_search_with_no_usable_title_issues_no_request() {
        let req = SearchRequest {
            context: Some(search_context(
                PluginSearchRequestKind::Search,
                PluginSearchSubjectKind::Title,
            )),
            ..SearchRequest::default()
        };

        assert!(nyaa_urls("https://nyaa.si", DEFAULT_ADDITIONAL_PARAMS, &req, false).is_empty());
    }

    /// Sonarr's `PrepareQuery` only swaps spaces for `+`, so `&`, `#` and a
    /// non-ASCII title corrupt the URL.
    #[test]
    fn a_term_is_percent_encoded_without_losing_sonarrs_plus_separator() {
        assert_eq!(prepare_query("Naruto Shippuuden"), "Naruto+Shippuuden");
        assert_eq!(prepare_query("Fate/stay night"), "Fate%2Fstay+night");
        assert_eq!(prepare_query("Tom & Jerry"), "Tom+%26+Jerry");
        assert_eq!(prepare_query("K-On!"), "K-On%21");
        assert_eq!(prepare_query("ナルト"), "%E3%83%8A%E3%83%AB%E3%83%88");
        assert_eq!(prepare_query("   "), "");
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_not_doubled() {
        let req = SearchRequest {
            query: "Bleach".to_string(),
            ..SearchRequest::default()
        };
        for base in ["https://nyaa.si", "https://nyaa.si/", "https://nyaa.si///"] {
            assert_eq!(
                nyaa_urls(base, DEFAULT_ADDITIONAL_PARAMS, &req, false),
                vec!["https://nyaa.si/?page=rss&cats=1_0&filter=1&term=Bleach"],
                "base {base}"
            );
        }
    }

    #[test]
    fn an_empty_additional_parameters_value_still_builds_a_feed_url() {
        let req = SearchRequest {
            query: "Bleach".to_string(),
            ..SearchRequest::default()
        };
        assert_eq!(
            nyaa_urls("https://nyaa.si", "", &req, false),
            vec!["https://nyaa.si/?page=rss&term=Bleach"]
        );
    }

    // -----------------------------------------------------------------------
    // Parsing — Sonarr's NyaaFixture
    // -----------------------------------------------------------------------

    #[test]
    fn should_parse_2021_recent_feed_from_nyaa() {
        let results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        assert_eq!(results.len(), 3);

        let first = &results[0];
        assert_eq!(
            first.title,
            "[Foxy-Subs] Mahouka Koukou no Yuutousei - 08 [720p] [3194D881].mkv"
        );
        assert_eq!(first.source_kind, Some(IndexerSourceKind::Torrent));
        assert_eq!(first.protocol, Some(IndexerProtocol::Torrent));
        assert_eq!(
            first.download_url.as_deref(),
            Some("https://nyaa.si/download/1424896.torrent")
        );
        assert_eq!(
            first.info_url.as_deref(),
            Some("https://nyaa.si/view/1424896")
        );
        assert_eq!(first.comment_url, None);
        assert_eq!(first.published_at.as_deref(), Some("2021-08-24T22:18:46Z"));
        // 609.6 MiB, binary prefix.
        assert_eq!(first.size_bytes, Some(639_211_930));
        assert_eq!(first.magnet_url, None);
        assert_eq!(first.seeders, Some(4));
        // `CalculatePeersAsSum`: leechers + seeders.
        assert_eq!(first.peers, Some(3 + 4));
        assert_eq!(first.leechers, Some(3));

        // Scryer-specific, beyond what Sonarr keeps.
        assert_eq!(
            first.info_hash_v1.as_deref(),
            Some("e8ca5e20eca876339f41c3d9e95ea66c1d7caaee")
        );
        assert_eq!(first.grabs, Some(2));
        assert_eq!(first.guid.as_deref(), Some("https://nyaa.si/view/1424896"));
        assert_eq!(
            first.categories,
            vec!["Anime - Non-English-translated".to_string()]
        );
        assert_eq!(
            first.provider_categories,
            vec![
                "1_3".to_string(),
                "Anime - Non-English-translated".to_string()
            ]
        );
        assert_eq!(
            first.provider_extra.get("category_id"),
            Some(&serde_json::Value::from("1_3"))
        );
        assert_eq!(
            first.provider_extra.get("comments"),
            Some(&serde_json::Value::from(0))
        );
        assert!(first.indexer_flags.is_empty());
        assert!(!first.provider_extra.contains_key("tags"));
        assert!(!first.provider_extra.contains_key("freeleech"));
        // Public tracker: nothing to clamp a seeding profile against.
        assert_eq!(first.minimum_seed_ratio, None);
        assert_eq!(first.minimum_seed_time_minutes, None);
    }

    #[test]
    fn the_remaining_fixture_entries_parse_too() {
        let results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");

        let second = &results[1];
        assert_eq!(
            second.title,
            "Macross Zero (BDRip 1920x1080p x265 HEVC TrueHD, FLAC 5.1+2.0)[sxales]"
        );
        assert_eq!(second.published_at.as_deref(), Some("2021-08-24T22:03:11Z"));
        // 5.7 GiB
        assert_eq!(second.size_bytes, Some(6_120_328_397));
        assert_eq!(second.seeders, Some(23));
        assert_eq!(second.peers, Some(32 + 23));
        assert_eq!(second.grabs, Some(17));
        assert_eq!(
            second.provider_extra.get("comments"),
            Some(&serde_json::Value::from(2))
        );

        let third = &results[2];
        assert_eq!(third.title, "Fumetsu no Anata e - 19 [WEBDL 1080p] Ukr DVO");
        assert_eq!(third.published_at.as_deref(), Some("2021-08-24T21:23:06Z"));
        // 1.4 GiB
        assert_eq!(third.size_bytes, Some(1_503_238_554));
        assert_eq!(third.seeders, Some(5));
        assert_eq!(third.peers, Some(4 + 5));
    }

    /// `nyaa:comments` is a count. Sonarr's `GetCommentUrl` reads
    /// `item.Element("comments")`, which is namespace-exact, and the fixture
    /// asserts `CommentUrl` is empty — so the count must never be mistaken for
    /// a URL.
    #[test]
    fn the_nyaa_comment_count_is_not_read_as_a_comment_url() {
        let results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        assert!(results.iter().all(|result| result.comment_url.is_none()));

        // A feed that really does carry a no-namespace <comments> URL is read.
        let feed = RECENT_FEED.replace(
            "<nyaa:comments>0</nyaa:comments>",
            "<nyaa:comments>0</nyaa:comments><comments>https://nyaa.si/view/1424896#comments</comments>",
        );
        let results = parse_feed(&feed, FEED_URL).expect("feed parses");
        assert_eq!(
            results[0].comment_url.as_deref(),
            Some("https://nyaa.si/view/1424896#comments")
        );
        // …and the count is still reported separately.
        assert_eq!(
            results[0].provider_extra.get("comments"),
            Some(&serde_json::Value::from(0))
        );
    }

    #[test]
    fn the_trust_flags_are_reported_when_the_feed_sets_them() {
        let feed = RECENT_FEED
            .replace(
                "<nyaa:trusted>No</nyaa:trusted>",
                "<nyaa:trusted>Yes</nyaa:trusted>",
            )
            .replace(
                "<nyaa:remake>No</nyaa:remake>",
                "<nyaa:remake>Yes</nyaa:remake>",
            );
        let results = parse_feed(&feed, FEED_URL).expect("feed parses");

        assert_eq!(
            results[0].indexer_flags,
            vec!["trusted".to_string(), "remake".to_string()]
        );
        // `indexer_flags` reaches the host's `extra` but is not restored when a
        // candidate is reused; `tags` is.
        assert_eq!(
            results[0].provider_extra.get("tags"),
            Some(&serde_json::Value::from(vec![
                "trusted".to_string(),
                "remake".to_string()
            ]))
        );
    }

    #[test]
    fn a_magnet_link_feed_is_reported_as_a_magnet() {
        let feed = RECENT_FEED.replace(
            "<link>https://nyaa.si/download/1424896.torrent</link>",
            "<link>magnet:?xt=urn:btih:e8ca5e20eca876339f41c3d9e95ea66c1d7caaee&amp;dn=Mahouka</link>",
        );
        let results = parse_feed(&feed, FEED_URL).expect("feed parses");

        assert_eq!(
            results[0].magnet_url.as_deref(),
            Some("magnet:?xt=urn:btih:e8ca5e20eca876339f41c3d9e95ea66c1d7caaee&dn=Mahouka")
        );
        assert_eq!(
            results[0].info_url.as_deref(),
            Some("https://nyaa.si/view/1424896"),
            "the guid stays the details page"
        );
    }

    #[test]
    fn a_release_without_a_title_or_a_link_is_dropped() {
        let feed = r#"<rss version="2.0"><channel>
            <item><link>https://nyaa.si/download/1.torrent</link></item>
            <item><title>No link at all</title></item>
            <item><title>Good</title><link>https://nyaa.si/download/2.torrent</link></item>
        </channel></rss>"#;
        let results = parse_feed(feed, FEED_URL).expect("feed parses");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good");
    }

    #[test]
    fn html_entities_in_a_title_are_decoded() {
        let feed = r#"<rss version="2.0"><channel><item>
            <title>Rosemary&#39;s Baby &amp; Friends</title>
            <link>https://nyaa.si/download/1.torrent</link>
        </item></channel></rss>"#;
        let results = parse_feed(feed, FEED_URL).expect("feed parses");
        assert_eq!(results[0].title, "Rosemary's Baby & Friends");
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_request_url() {
        let feed = r#"<rss version="2.0"><channel><item>
            <title>Relative</title>
            <link>/download/1424896.torrent</link>
            <guid>/view/1424896</guid>
        </item></channel></rss>"#;
        let results = parse_feed(feed, FEED_URL).expect("feed parses");
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("https://nyaa.si/download/1424896.torrent")
        );
        assert_eq!(
            results[0].info_url.as_deref(),
            Some("https://nyaa.si/view/1424896")
        );
    }

    #[test]
    fn an_unparseable_pub_date_keeps_the_release() {
        let feed = r#"<rss version="2.0"><channel><item>
            <title>Bad date</title>
            <link>https://nyaa.si/download/1.torrent</link>
            <pubDate>not a date</pubDate>
        </item></channel></rss>"#;
        let results = parse_feed(feed, FEED_URL).expect("feed parses");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].published_at, None);
    }

    #[test]
    fn a_short_or_non_hex_info_hash_is_not_reported() {
        let feed = RECENT_FEED.replace(
            "<nyaa:infoHash>e8ca5e20eca876339f41c3d9e95ea66c1d7caaee</nyaa:infoHash>",
            "<nyaa:infoHash>not-a-hash</nyaa:infoHash>",
        );
        let results = parse_feed(&feed, FEED_URL).expect("feed parses");
        assert_eq!(results[0].info_hash_v1, None);
    }

    #[test]
    fn an_uppercase_info_hash_is_lower_cased() {
        let feed = RECENT_FEED.replace(
            "e8ca5e20eca876339f41c3d9e95ea66c1d7caaee",
            "E8CA5E20ECA876339F41C3D9E95EA66C1D7CAAEE",
        );
        let results = parse_feed(&feed, FEED_URL).expect("feed parses");
        assert_eq!(
            results[0].info_hash_v1.as_deref(),
            Some("e8ca5e20eca876339f41c3d9e95ea66c1d7caaee")
        );
    }

    #[test]
    fn an_empty_channel_is_a_quiet_feed_not_an_error() {
        let results = parse_feed(
            r#"<rss version="2.0"><channel><title>Nyaa</title></channel></rss>"#,
            FEED_URL,
        )
        .expect("feed parses");
        assert!(results.is_empty());
    }

    #[test]
    fn a_document_with_no_channel_is_an_invalid_root() {
        let error = parse_feed(r#"<html><body>nope</body></html>"#, FEED_URL)
            .expect_err("no channel is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            structured.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::InvalidRoot
                }
            ))
        ));
    }

    #[test]
    fn a_malformed_document_is_a_malformed_body() {
        let error = parse_feed(r#"<rss><channel><item><title>oops</channel>"#, FEED_URL)
            .expect_err("malformed XML is an error");
        let structured = structured(&error);
        assert!(matches!(
            structured.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::MalformedBody
                }
            ))
        ));
    }

    // -----------------------------------------------------------------------
    // No post-filtering
    // -----------------------------------------------------------------------

    /// The finding that motivated dropping `rss-indexer-common`:
    /// `execute_rss_urls` runs `filter_results`, whose `category_matches`
    /// compares the request facet against the item's categories. Every Nyaa
    /// category starts with "Anime", so a `movie`-faceted search dropped
    /// **every** result.
    #[test]
    fn a_movie_facet_does_not_drop_anime_categorised_releases() {
        let results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        let request = SearchRequest {
            query: "Macross Zero".to_string(),
            facet: Some("movie".to_string()),
            ..SearchRequest::default()
        };
        // Nothing in the plugin narrows the parsed set beyond dedupe and limit.
        let kept = dedupe_results(results);
        assert_eq!(kept.len(), 3);
        assert!(result_limit(&request).is_none());
    }

    /// `title_matches` in the shared filter required the query tokens to appear
    /// in the release title. Nyaa titles are routinely romanised differently or
    /// carry the Japanese title, and the term already went to Nyaa's own search.
    #[test]
    fn a_release_whose_title_does_not_contain_the_query_is_kept() {
        let results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        assert!(
            results
                .iter()
                .any(|result| !result.title.contains("Shingeki")),
            "the plugin never matches the query against the title"
        );
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn the_host_limit_is_honoured_and_zero_means_everything() {
        let mut results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        let request = SearchRequest {
            limit: 2,
            ..SearchRequest::default()
        };
        if let Some(limit) = result_limit(&request) {
            results.truncate(limit);
        }
        assert_eq!(results.len(), 2);

        assert_eq!(result_limit(&SearchRequest::default()), None);
        assert_eq!(
            result_limit(&SearchRequest {
                limit: 1000,
                ..SearchRequest::default()
            }),
            Some(1000)
        );
    }

    #[test]
    fn duplicate_entries_are_deduped_by_guid() {
        let mut results = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        let duplicate = parse_feed(RECENT_FEED, FEED_URL).expect("feed parses");
        results.extend(duplicate);
        assert_eq!(results.len(), 6);
        assert_eq!(dedupe_results(results).len(), 3);
    }

    // -----------------------------------------------------------------------
    // Delivery classification
    // -----------------------------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn an_ok_xml_delivery_is_returned_verbatim() {
        let body = classify_response(
            200,
            &headers(&[("Content-Type", "application/xml")]),
            RECENT_FEED.as_bytes(),
        )
        .expect("ok delivery");
        assert_eq!(body, RECENT_FEED);
    }

    #[test]
    fn a_redirect_is_a_typed_config_error_naming_the_base_url() {
        let error = classify_response(302, &headers(&[("Location", "https://nyaa.si/login")]), b"")
            .expect_err("3xx is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::InvalidConfig);
        assert!(structured.public_message.contains("base_url"));
        assert!(
            structured
                .debug_message
                .as_deref()
                .unwrap_or_default()
                .contains("https://nyaa.si/login")
        );
        assert!(structured.details.is_none());
    }

    /// Nyaa answers `flask.abort(400)` for a category or filter value it does
    /// not recognise (`nyaa/search.py`), which is a settings fault.
    #[test]
    fn a_bad_request_names_the_additional_parameters() {
        let error =
            classify_response(400, &headers(&[]), b"Bad Request").expect_err("400 is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::InvalidConfig);
        assert!(structured.public_message.contains("additional_params"));
        assert!(structured.details.is_none());
    }

    #[test]
    fn a_rate_limited_delivery_defers_with_the_retry_after_header() {
        let error = classify_response(429, &headers(&[("Retry-After", "120")]), b"slow down")
            .expect_err("429 is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::RateLimited);
        assert_eq!(structured.retry_after_seconds, Some(120));
        assert!(matches!(
            structured.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::RateLimited,
                    retry_after_seconds: Some(120)
                }
            ))
        ));
    }

    #[test]
    fn a_rate_limited_delivery_without_a_header_uses_sonarrs_hour() {
        let error =
            classify_response(429, &headers(&[]), b"slow down").expect_err("429 is an error");
        assert_eq!(
            structured(&error).retry_after_seconds,
            Some(RATE_LIMITED_FALLBACK_SECONDS)
        );
    }

    #[test]
    fn a_challenge_page_is_an_unexpected_content_type() {
        for (status, content_type) in [
            (403u16, "text/html"),
            (503, "text/html"),
            (200, "text/html"),
        ] {
            let error = classify_response(
                status,
                &headers(&[("Content-Type", content_type)]),
                b"<!DOCTYPE html><html><body>Just a moment...</body></html>",
            )
            .expect_err("html is an error");
            let structured = structured(&error);
            if status == 503 {
                // 503 is an upstream failure with an HTML body; the status wins.
                assert_eq!(structured.code, PluginErrorCode::UpstreamUnavailable);
                continue;
            }
            assert!(matches!(
                structured.details,
                Some(PluginErrorDetails::IndexerSearch(
                    IndexerSearchPluginError::InvalidResponse {
                        kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
                    }
                )),
            ));
        }
    }

    #[test]
    fn an_html_body_without_a_content_type_is_still_detected() {
        let error = classify_response(
            200,
            &headers(&[]),
            b"<html><head><title>Attention Required</title></head></html>",
        )
        .expect_err("html is an error");
        assert!(matches!(
            structured(&error).details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
                }
            ))
        ));
    }

    #[test]
    fn a_plain_403_is_an_auth_failure_from_a_proxy() {
        let error = classify_response(403, &headers(&[("Content-Type", "text/plain")]), b"denied")
            .expect_err("403 is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::AuthFailed);
        assert!(structured.details.is_none());
    }

    #[test]
    fn a_server_error_defers_as_an_upstream_failure() {
        let error = classify_response(500, &headers(&[]), b"boom").expect_err("500 is an error");
        let structured = structured(&error);
        assert_eq!(structured.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            structured.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::UpstreamFailure,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn an_oversized_body_is_a_truncated_body() {
        let body = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        let error = classify_response(200, &headers(&[]), &body).expect_err("too big");
        assert!(matches!(
            structured(&error).details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::TruncatedBody
                }
            ))
        ));
    }

    #[test]
    fn a_non_utf8_body_is_a_malformed_body() {
        let error =
            classify_response(200, &headers(&[]), &[0xff, 0xfe, 0xfd]).expect_err("invalid utf-8");
        assert!(matches!(
            structured(&error).details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::MalformedBody
                }
            ))
        ));
    }

    #[test]
    fn the_request_asks_for_xml_with_a_versioned_user_agent() {
        let config = NyaaConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            additional_params: DEFAULT_ADDITIONAL_PARAMS.to_string(),
            anime_standard_format_search: false,
            user_agent: USER_AGENT.to_string(),
            cookie: None,
            username: None,
            password: None,
            additional_headers: String::new(),
        };
        let headers = config.request_headers();
        let accept = headers.get("Accept").expect("Accept");
        assert!(accept.contains("application/rss+xml"));
        assert!(!accept.contains("text/html"));
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some(USER_AGENT)
        );
        assert!(!headers.contains_key("Authorization"));
        assert!(!headers.contains_key("Cookie"));
    }

    #[test]
    fn basic_auth_cookie_and_extra_headers_are_applied() {
        let config = NyaaConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            additional_params: String::new(),
            anime_standard_format_search: false,
            user_agent: "custom/1".to_string(),
            cookie: Some("cf_clearance=abc".to_string()),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            additional_headers: "X-Extra: yes\nbroken line\n".to_string(),
        };
        let headers = config.request_headers();
        assert_eq!(
            headers.get("Cookie").map(String::as_str),
            Some("cf_clearance=abc")
        );
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Basic dXNlcjpwYXNz")
        );
        assert_eq!(headers.get("X-Extra").map(String::as_str), Some("yes"));
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("custom/1")
        );
    }

    // -----------------------------------------------------------------------
    // Settings validation
    // -----------------------------------------------------------------------

    #[test]
    fn a_missing_or_unusable_base_url_is_a_typed_config_error() {
        for value in [
            "",
            "   ",
            "not a url",
            "ftp://nyaa.si",
            "https://",
            "//nyaa.si",
        ] {
            let error = validate_base_url(value).expect_err("bad base url");
            let structured = structured(&error);
            assert_eq!(
                structured.code,
                PluginErrorCode::InvalidConfig,
                "value {value:?}"
            );
            assert!(structured.public_message.contains("base_url"));
            assert!(structured.details.is_none());
        }
    }

    #[test]
    fn a_search_url_is_rejected_as_the_base_url() {
        let error = validate_base_url("https://nyaa.si/?page=rss&c=1_0")
            .expect_err("a search URL is not the root");
        assert_eq!(structured(&error).code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn a_root_url_with_a_path_is_accepted_for_mirrors() {
        assert_eq!(
            validate_base_url("https://mirror.example/nyaa/").expect("valid"),
            "https://mirror.example/nyaa/"
        );
        assert_eq!(
            validate_base_url(" https://nyaa.si ").expect("valid"),
            "https://nyaa.si"
        );
    }

    #[test]
    fn the_published_additional_parameters_default_validates() {
        assert_eq!(
            validate_additional_params(DEFAULT_ADDITIONAL_PARAMS).expect("valid"),
            DEFAULT_ADDITIONAL_PARAMS
        );
        for value in [
            "",
            "&c=1_2&f=0",
            "&cats=1_0&filter=1&s=seeders&o=desc",
            // Sonarr's own character class would reject the hyphen here, and
            // the site documents this parameter.
            "&u=Erai-raws",
            "&m=1",
            "&m",
        ] {
            assert!(
                validate_additional_params(value).is_ok(),
                "expected {value:?} to validate"
            );
        }
    }

    /// Sonarr's `Matches("(&[a-z]+=[a-z0-9_]+)*")` is unanchored, so it accepts
    /// everything — including a value with no leading `&`, which produces
    /// `…?page=rsscats=1_0` and silently returns the unfiltered front page.
    #[test]
    fn a_malformed_additional_parameters_value_is_a_typed_config_error() {
        for value in [
            "cats=1_0&filter=1",
            "?cats=1_0",
            "&cats=1_0&&filter=1",
            "&=1_0",
            "&1cats=1_0",
            "&cats=1 0",
            "&cats=1_0#top",
        ] {
            let error = validate_additional_params(value).expect_err("expected a rejection");
            let structured = structured(&error);
            assert_eq!(
                structured.code,
                PluginErrorCode::InvalidConfig,
                "value {value:?}"
            );
            assert!(structured.public_message.contains("additional_params"));
        }
    }

    // -----------------------------------------------------------------------
    // Size and dates
    // -----------------------------------------------------------------------

    #[test]
    fn the_size_grammar_matches_sonarrs_parse_size() {
        assert_eq!(parse_size("609.6 MiB"), Some(639_211_930));
        assert_eq!(parse_size("5.7 GiB"), Some(6_120_328_397));
        assert_eq!(parse_size("1.4 GiB"), Some(1_503_238_554));
        // `defaultToBinaryPrefix: true`: an unprefixed unit is 1024-based too.
        assert_eq!(parse_size("1 GB"), Some(1_073_741_824));
        assert_eq!(parse_size("1,024 KiB"), Some(1_048_576));
        assert_eq!(parse_size("639211930"), Some(639_211_930));
        // `(?![\w/])`: a rate is not a size.
        assert_eq!(parse_size("1.5 GB/s"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("unknown"), None);
    }

    #[test]
    fn a_fraction_tail_never_starts_a_size() {
        // `(?<!\.\d*)`: the `5` of `0.5` must not be read as `5 GB`.
        assert_eq!(parse_size("0.5 GiB"), Some(536_870_912));
    }

    #[test]
    fn rfc_2822_pub_dates_become_rfc_3339_utc() {
        let cases = [
            ("Tue, 24 Aug 2021 22:18:46 -0000", "2021-08-24T22:18:46Z"),
            ("Tue, 24 Aug 2021 22:18:46 +0000", "2021-08-24T22:18:46Z"),
            ("24 Aug 2021 22:18:46 -0000", "2021-08-24T22:18:46Z"),
            ("Mon, 12 May 2014 19:06:34 GMT", "2014-05-12T19:06:34Z"),
            ("Tue, 24 Aug 2021 22:18:46 +0200", "2021-08-24T20:18:46Z"),
            ("Tue, 24 Aug 2021 22:18:46 -0530", "2021-08-25T03:48:46Z"),
            ("Sat, 29 Feb 2020 12:00 -0000", "2020-02-29T12:00:00Z"),
            ("Fri, 31 Dec 1999 23:59:59 -0000", "1999-12-31T23:59:59Z"),
            ("Tue, 24 Aug 99 22:18:46 -0000", "1999-08-24T22:18:46Z"),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                rfc2822_to_rfc3339_utc(raw).as_deref(),
                Some(expected),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_rfc_3339_pub_date_is_passed_through() {
        assert_eq!(
            rfc2822_to_rfc3339_utc("2021-08-24T22:18:46Z").as_deref(),
            Some("2021-08-24T22:18:46Z")
        );
    }

    #[test]
    fn an_unparseable_pub_date_is_none_rather_than_a_value_the_core_drops() {
        for raw in ["", "not a date", "Tue, 99 Xxx 2021 22:18:46 -0000"] {
            assert_eq!(rfc2822_to_rfc3339_utc(raw), None, "{raw}");
        }
    }

    #[test]
    fn the_civil_calendar_round_trips() {
        for days in [-25_000i64, -1, 0, 1, 10_000, 20_000, 30_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }

    // -----------------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------------

    #[test]
    fn a_recent_request_is_recognised_from_the_context_or_from_empty_criteria() {
        assert!(is_recent_request(&SearchRequest::default()));
        assert!(is_recent_request(&SearchRequest {
            query: "ignored".to_string(),
            context: Some(search_context(
                PluginSearchRequestKind::Recent,
                PluginSearchSubjectKind::Unknown
            )),
            ..SearchRequest::default()
        }));
        assert!(!is_recent_request(&SearchRequest {
            query: "Bleach".to_string(),
            ..SearchRequest::default()
        }));
        assert!(!is_recent_request(&SearchRequest {
            absolute_episode: Some(9),
            ..SearchRequest::default()
        }));
    }

    #[test]
    fn the_term_list_is_deduped() {
        // `abs = 10` produces `+10` twice under Sonarr's two format strings for
        // values below 10 only, but a caller-supplied duplicate must not fan out.
        assert_eq!(
            dedupe_strings(vec!["a".into(), "a".into(), "b".into()]),
            vec!["a", "b"]
        );
    }
}
