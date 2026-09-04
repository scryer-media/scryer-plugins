//! TorrentLeech (`torrentleech.org`) RSS indexer.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Indexers/Torrentleech`
//! (`Torrentleech.cs`, `TorrentleechRequestGenerator.cs`,
//! `TorrentleechSettings.cs`), Sonarr's `RssParser`/`TorrentRssParser`, its
//! `TorrentleechFixture.cs` and the `Files/Indexers/Torrentleech/Torrentleech.xml`
//! fixture, plus Prowlarr's current Cardigann definition
//! (`definitions/v11/torrentleech.yml`) for the site's live domain list,
//! category ids, request delay and hit-and-run rule, and live reads of
//! `rss.torrentleech.org` performed during the review.
//!
//! Shape of the integration:
//!
//! * TorrentLeech publishes **one** personalised RSS feed per account at
//!   `https://rss.torrentleech.org/{RSSKEY}` (and a 24-hour variant at
//!   `https://rss24h.torrentleech.org/{RSSKEY}`). The endpoint takes no query,
//!   season or episode parameter, so the feed is a recent list and nothing else
//!   — exactly why Sonarr declares `SupportsSearch => false` and answers every
//!   search-criteria overload with an empty request chain
//!   (`TorrentleechRequestGenerator.cs:20-53`).
//! * The plugin therefore serves the recent/RSS poll only. A request that
//!   carries search criteria is answered with an empty response **without**
//!   spending an upstream call, which is Sonarr's behaviour and also protects a
//!   Cloudflare-fronted private tracker from pointless traffic.
//! * The fetch, the delivery classification, the XML parse and the result
//!   assembly are all done in-plugin rather than through
//!   `rss-indexer-common::execute_rss_urls`, because that helper cannot report a
//!   typed error (every failure becomes `Temporary`), it post-filters parsed
//!   releases against the request (which Sonarr never does), it never reads
//!   `<comments>` (so `comment_url` was always unset) and it cannot see the
//!   sentinel item TorrentLeech returns for an invalid RSS key. See the README
//!   and the reconciliation report.

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
    PluginSearchResponse as SearchResponse, PluginSearchResult as SearchResult, ProviderDescriptor,
    SDK_VERSION, torrent_result,
};

const PROVIDER_ID: &str = "torrentleech";
const USER_AGENT: &str = concat!("scryer-torrentleech-indexer/", env!("CARGO_PKG_VERSION"));
/// Sonarr's `TorrentleechSettings` default. Kept verbatim as the published
/// config default (config contract); the scheme is upgraded at request time
/// because TorrentLeech is HSTS-preloaded and plain HTTP no longer works — see
/// [`upgrade_torrentleech_scheme`].
const DEFAULT_BASE_URL: &str = "http://rss.torrentleech.org";
/// Prowlarr's `requestDelay: 4.1` for TorrentLeech (Prowlarr issue #13796),
/// rounded up to whole seconds because the descriptor's hint is an integer.
/// Sonarr uses its fleet-wide 2 s `HttpIndexerBase.RateLimit`.
const REQUEST_INTERVAL_MS: u64 = 5_000;
const RATE_LIMIT_HINT_SECONDS: i64 = 5;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Sonarr's `minimumBackoff` when a rate-limited response carries no
/// `Retry-After` (`HttpIndexerBase.FetchReleases`).
const RATE_LIMITED_FALLBACK_SECONDS: i64 = 3_600;

/// TorrentLeech's published hit-and-run rule, from Prowlarr's definition
/// (`minimumratio: 1.0`, `minimumseedtime: 864000` seconds = 10 days). The
/// definition notes the 10 days applies to registered users and is shorter for
/// upgraded ones, so this pair is the conservative floor.
const SITE_MINIMUM_SEED_RATIO: f64 = 1.0;
const SITE_MINIMUM_SEED_TIME_MINUTES: i64 = 14_400;

/// The registrable domains TorrentLeech serves from, taken from Prowlarr's
/// `links`/`legacylinks`. Every one of them answered with
/// `Strict-Transport-Security: max-age=15768000; includeSubdomains; preload`
/// when probed on 2026-09-02, so an `http://` URL for any of them is upgraded
/// rather than sent (see [`upgrade_torrentleech_scheme`]).
const TORRENTLEECH_DOMAINS: &[&str] = &[
    "torrentleech.org",
    "torrentleech.cc",
    "torrentleech.me",
    "tleechreload.org",
    "tlgetin.cc",
];

// ---------------------------------------------------------------------------
// Category table
// ---------------------------------------------------------------------------

/// TorrentLeech's category ids and the names the RSS `<category>` element
/// carries.
///
/// The ids are Prowlarr's current `categorymappings`
/// (`definitions/v11/torrentleech.yml`, updated 2026-08-31); the display names
/// are the site's own leaf names as published by FlexGet's TorrentLeech
/// component (`flexget/components/sites/sites/torrentleech.py`), which is what
/// the feed actually emits — the fixture's `Episodes HD` is id 32.
///
/// The facet column follows Prowlarr's newznab mapping: `Movies*` → `movie`,
/// `TV*` → `series`, `Anime` → `anime`, everything else has no Scryer facet.
const CATEGORIES: &[(i64, &str, &str)] = &[
    // Movies
    (8, "Cam", "movie"),
    (9, "TS/TC", "movie"),
    (11, "DVDRip/DVDScreener", "movie"),
    (37, "WEBRip", "movie"),
    (43, "HDRip", "movie"),
    (14, "BlurayRip", "movie"),
    (12, "DVD-R", "movie"),
    (13, "Bluray", "movie"),
    (41, "4KUpscaled", "movie"),
    (47, "Real4K", "movie"),
    (15, "Boxsets", "movie"),
    (29, "Documentaries", "movie"),
    (36, "Movies Foreign", "movie"),
    // TV
    (26, "Episodes", "series"),
    (32, "Episodes HD", "series"),
    (27, "TV Boxsets", "series"),
    (44, "TV Foreign", "series"),
    (35, "Cartoons", "series"),
    (34, "Anime", "anime"),
    // Games. Prowlarr's descriptions for 20/21 are "Games PS2"/"Games Mac"
    // while both map to `Console/PS3`; 21 is PS3 and the second description is
    // a copy-paste slip in the definition, so it is named PS3 here.
    (17, "Games PC", ""),
    (42, "Games Mac", ""),
    (18, "Games XBOX", ""),
    (19, "Games XBOX360", ""),
    (40, "Games XBOXONE", ""),
    (20, "Games PS2", ""),
    (21, "Games PS3", ""),
    (39, "Games PS4", ""),
    (49, "Games PS5", ""),
    (22, "Games PSP", ""),
    (28, "Games Wii", ""),
    (30, "Games Nintendo DS", ""),
    (48, "Games Nintendo Switch", ""),
    // Applications
    (23, "PC ISO", ""),
    (24, "PC Mac", ""),
    (25, "PC Mobile", ""),
    (33, "PC 0-day", ""),
    (38, "Education", ""),
    // Books
    (45, "Books EBooks", ""),
    (46, "Books Comics", ""),
    // Audio
    (31, "Audio", ""),
    (16, "Music videos", ""),
];

/// Names the site has used that resolve unambiguously to one id but are not the
/// display name in [`CATEGORIES`].
const CATEGORY_ALIASES: &[(&str, i64)] = &[
    ("TS", 9),
    ("TV Episodes", 26),
    ("TV Episodes HD", 32),
    ("TV Anime", 34),
    ("TV Cartoons", 35),
    ("Movies Boxsets", 15),
    ("EBooks", 45),
    ("Comics", 46),
];

/// Names that map to more than one id, so no id is reported for them.
/// TorrentLeech has both a Movies (36) and a TV (44) "Foreign" category.
const AMBIGUOUS_CATEGORY_NAMES: &[&str] = &["Foreign"];

/// Resolve an RSS `<category>` name to TorrentLeech's numeric id, or `None`
/// when the name is ambiguous or is one the site has since retired (the 2014
/// fixture's `HD`, for example).
fn category_id_for_name(name: &str) -> Option<i64> {
    let name = name.trim();
    if AMBIGUOUS_CATEGORY_NAMES
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
    {
        return None;
    }
    CATEGORIES
        .iter()
        .find(|(_, label, _)| label.eq_ignore_ascii_case(name))
        .map(|(id, _, _)| *id)
        .or_else(|| {
            CATEGORY_ALIASES
                .iter()
                .find(|(label, _)| label.eq_ignore_ascii_case(name))
                .map(|(_, id)| *id)
        })
}

fn category_descriptors() -> Vec<IndexerCategoryDescriptor> {
    CATEGORIES
        .iter()
        .map(|(id, name, facet)| IndexerCategoryDescriptor {
            value: (*name).to_string(),
            label: Some(format!("{name} (TorrentLeech category {id})")),
            value_kind: IndexerCategoryValueKind::String,
            facets: if facet.is_empty() {
                Vec::new()
            } else {
                vec![(*facet).to_string()]
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_ID.to_string(),
        name: "TorrentLeech Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_ID.to_string(),
            provider_aliases: vec!["torrent-leech".to_string()],
            provider_profiles: vec![],
            search_semantics_version: Some(2),
            // One feed, one request: there is nothing to fan out over.
            strategy_plan: Some(IndexerStrategyPlanCapability {
                version: 1,
                max_parallel_strategies: 1,
            }),
            source_kind: IndexerSourceKind::Torrent,
            capabilities: Capabilities {
                // The feed takes no query, id, season or episode parameter.
                supported_ids: HashMap::new(),
                deduplicates_aliases: false,
                season_param: None,
                episode_param: None,
                query_param: None,
                supported_query_facets: vec![],
                // Sonarr: `SupportsSearch => false`.
                search: false,
                imdb_search: false,
                tvdb_search: false,
                anidb_search: false,
                rss: true,
                protocols: vec![IndexerProtocol::Torrent],
                feed_modes: vec![IndexerFeedMode::Recent, IndexerFeedMode::Rss],
                search_inputs: vec![IndexerSearchInput::Limit],
                // Nothing in the feed identifies a series or a film.
                supported_external_ids: vec![],
                category_model: Some(IndexerCategoryModel {
                    value_kinds: vec![IndexerCategoryValueKind::String],
                    // TorrentLeech files anime under its own `Anime` category
                    // (id 34) rather than inside the TV tree.
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    categories: category_descriptors(),
                }),
                limits: Some(IndexerLimitCapabilities {
                    // TorrentLeech's RSS endpoint returns a fixed recent window
                    // whose length it does not publish, and takes no paging
                    // parameter, so the honest answer is "one page of unknown
                    // length". The shipped descriptor claimed 200.
                    page_size: None,
                    max_page_size: None,
                    max_pages: Some(1),
                    rate_limit_hint_seconds: Some(RATE_LIMIT_HINT_SECONDS as u32),
                    api_quota_supported: false,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    // Seeders/leechers come out of `<description>`
                    // ("Seeders: 1 - Leechers: 7"), and peers is their sum —
                    // which is what Sonarr's fixture asserts.
                    reports_seeders: true,
                    reports_peers: true,
                    reports_leechers: true,
                    // The feed carries neither, and the shipped descriptor
                    // claimed both. Sonarr's fixture asserts `InfoHash` and
                    // `MagnetUrl` are null.
                    reports_info_hash: false,
                    reports_magnet_uri: false,
                    // The RSS feed carries no freeleech/download-multiplier
                    // field; the site's JSON browse API does.
                    reports_volume_factors: false,
                    // TorrentLeech is a private tracker.
                    supports_private_tracker_flags: true,
                    // The plugin reports TorrentLeech's site-wide hit-and-run
                    // minimums on every release (see `SITE_MINIMUM_*`).
                    supports_seed_requirements: true,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: false,
                    subtitles: false,
                    grabs: false,
                    votes: false,
                    // `<comments>` is a real per-release URL in this feed.
                    comments: true,
                    // `<guid>` is the torrent's details page, which is what
                    // `UseGuidInfoUrl = true` makes the info URL.
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
            rate_limit_seconds: Some(RATE_LIMIT_HINT_SECONDS),
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
            Some(
                "TorrentLeech RSS host. The feed URL is this value with your RSS key appended: \
                 https://rss.torrentleech.org/RSSKEY. Use https://rss24h.torrentleech.org for the \
                 24-hour feed. Plain http:// is upgraded to https:// automatically — TorrentLeech \
                 is HSTS-preloaded and refuses http.",
            ),
        ),
        field(
            "api_key",
            "RSS Key",
            ConfigFieldType::Password,
            true,
            None,
            Some(
                "TorrentLeech RSS key — the 20-character token from the RSS link on your profile \
                 page, not the whole URL.",
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
                "Optional raw Cookie header. The RSS feed authenticates with the key in the URL; a \
                 cookie is only needed when a Cloudflare clearance cookie has to be supplied by \
                 hand.",
            ),
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            Some("Optional username for HTTP basic auth (reverse proxies in front of the feed)"),
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
    let config = TorrentLeechConfig::from_host()?;

    // Sonarr's `TorrentleechRequestGenerator` answers every `GetSearchRequests`
    // overload with an empty `IndexerPageableRequestChain`
    // (`TorrentleechRequestGenerator.cs:20-53`): the RSS endpoint has no query
    // parameter, so a search cannot be narrowed and issuing it would just
    // re-fetch the recent list. Answer empty without spending the upstream call.
    if !is_recent_request(&request) {
        return Ok(SearchResponse::default());
    }

    let body = fetch_feed(&config).await?;
    let items = parse_feed(&body)?;
    let mut results = build_results(&items, &config);
    if results.is_empty()
        && let Some(error) = detect_error_item(&items)
    {
        return Err(error);
    }
    results = dedupe_results(results);
    if let Some(limit) = result_limit(&request) {
        results.truncate(limit);
    }

    Ok(SearchResponse {
        results,
        ..Default::default()
    })
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

/// The feed is a single unpaged document of unknown length, so there is no page
/// size to clamp against. `limit == 0` means "plugin default", which here is
/// "everything the feed returned".
///
/// The adapter hard-codes `limit: 1000` on every search
/// (`crates/scryer-plugins/src/indexer_adapter.rs`, verified on `release-0.19.8`
/// and `release-NEXT`), far above any real feed length; it is honoured rather
/// than ignored so a host that ever sends a smaller value is respected.
fn result_limit(request: &SearchRequest) -> Option<usize> {
    (request.limit > 0).then_some(request.limit)
}

// ---------------------------------------------------------------------------
// Transport and delivery classification
// ---------------------------------------------------------------------------

async fn fetch_feed(config: &TorrentLeechConfig) -> Result<String, Error> {
    StartRateGate::new(
        format!("{PROVIDER_ID}.request-start"),
        1,
        REQUEST_INTERVAL_MS,
    )
    .acquire()
    .await
    .map_err(component::deadline_deferred_error)?;

    let logged_url = redact_key(&config.feed_url, &config.api_key);
    component::log(
        LogLevel::Debug,
        format!("http_trace plugin={PROVIDER_ID} method=GET url={logged_url}"),
    );

    let response = component::http(PluginHttpRequest {
        url: config.feed_url.clone(),
        method: Some("GET".to_string()),
        headers: config.request_headers(),
        body: Vec::new(),
    })
    .await
    .map_err(|error| {
        deferred_error(
            IndexerSearchIncompleteReason::UpstreamFailure,
            None,
            "TorrentLeech could not be reached".to_string(),
            format!("TorrentLeech request to {logged_url} failed: {error:?}"),
        )
    })?;

    component::log(
        LogLevel::Debug,
        format!(
            "http_trace_response plugin={PROVIDER_ID} status={} url={logged_url}",
            response.status
        ),
    );

    classify_response(response.status, &response.headers, &response.body)
}

/// Map one HTTP delivery onto Scryer's typed indexer error lanes.
///
/// What TorrentLeech's RSS host actually does, measured on 2026-09-02:
///
/// * `http://rss.torrentleech.org/…` → **403 with a Cloudflare HTML body**
///   (the site is HSTS-preloaded and refuses plaintext). The plugin upgrades the
///   scheme before it gets here, so this arm is a backstop.
/// * a wrong RSS key → **HTTP 200 `application/rss+xml`** carrying a
///   well-formed feed whose single item is `An error has occured!` /
///   `Your RSS key is invalid.` — handled after the parse, in
///   [`detect_error_item`], not here.
/// * a path that is not `/{RSSKEY}` (a trailing slash, an extra category
///   segment, an `/rss/` prefix) → **nginx 404 with an HTML body**.
/// * `rss.torrentleech.cc`/`.me` and the `tleechreload.org` mirrors answer the
///   `/{RSSKEY}` path with the **site's HTML**, not a feed: only
///   `rss.torrentleech.org` and `rss24h.torrentleech.org` serve RSS.
///
/// This is Sonarr's `RssParser.PreProcess` distinction ("Indexer responded with
/// html content. Site is likely blocked or unavailable.") expressed as typed
/// errors instead of one exception class.
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
                    "TorrentLeech redirected the feed with HTTP {status} to {location}; the \
                     configured 'base_url' is not the RSS host (the host does not follow \
                     redirects). Use https://rss.torrentleech.org."
                ),
            ));
        }
        404 => {
            return Err(invalid_config_error(
                "base_url",
                format!(
                    "TorrentLeech answered HTTP 404: the feed URL must be exactly \
                     'https://rss.torrentleech.org/RSSKEY' with no trailing slash and no extra \
                     path segments. Body: {}",
                    body_excerpt(body)
                ),
            ));
        }
        401 | 403 => {
            if is_html_delivery(headers, body) {
                return Err(invalid_response_error(
                    IndexerSearchInvalidResponseKind::UnexpectedContentType,
                    format!(
                        "TorrentLeech answered HTTP {status} with an HTML page: the site is likely \
                         blocking Scryer (Cloudflare), or 'base_url' is an http:// URL that \
                         TorrentLeech refuses: {}",
                        body_excerpt(body)
                    ),
                ));
            }
            return Err(auth_failed_error(format!(
                "TorrentLeech rejected the request with HTTP {status}: {}",
                body_excerpt(body)
            )));
        }
        429 => return Err(rate_limited_error(retry_after_seconds(headers))),
        _ => {
            return Err(deferred_error(
                IndexerSearchIncompleteReason::UpstreamFailure,
                None,
                format!("TorrentLeech returned HTTP {status}"),
                format!(
                    "TorrentLeech returned HTTP {status}: {}",
                    body_excerpt(body)
                ),
            ));
        }
    }

    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            format!(
                "TorrentLeech returned {} bytes, above the {MAX_RESPONSE_BYTES} byte ceiling",
                body.len()
            ),
        ));
    }

    let text = std::str::from_utf8(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("TorrentLeech feed was not valid UTF-8: {error}"),
        )
    })?;

    if is_html_delivery(headers, body) {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            format!(
                "TorrentLeech returned content type {:?} instead of RSS: the site is likely \
                 blocking Scryer (Cloudflare), or 'base_url' names a mirror that does not serve \
                 RSS — only rss.torrentleech.org and rss24h.torrentleech.org do: {}",
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

/// The RSS key is a **path segment**, in the feed URL and again in every
/// download URL (`/rss/download/{id}/{RSSKEY}/{name}.torrent`), so it must never
/// reach a log or a persisted guid verbatim.
fn redact_key(url: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return url.to_string();
    }
    url.split('/')
        .map(|segment| {
            if segment == api_key {
                "REDACTED"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
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
        format!("TorrentLeech setting '{field}' is not usable"),
        detail,
        None,
        None,
    )
}

fn auth_failed_error(detail: String) -> Error {
    typed_error(
        PluginErrorCode::AuthFailed,
        "TorrentLeech rejected the configured 'api_key' (RSS key)".to_string(),
        detail,
        None,
        None,
    )
}

fn rate_limited_error(retry_after_seconds: i64) -> Error {
    typed_error(
        PluginErrorCode::RateLimited,
        "TorrentLeech is rate limiting Scryer".to_string(),
        format!("TorrentLeech returned HTTP 429; retrying after {retry_after_seconds}s"),
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
        "TorrentLeech returned a response Scryer could not read".to_string(),
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
    comments: Option<String>,
    description: Option<String>,
    published_at: Option<String>,
    categories: Vec<String>,
    /// The `<category>` currently being assembled; flushed into `categories`
    /// when the element closes, so several `<category>` elements stay separate.
    category_buffer: Option<String>,
    enclosure_url: Option<String>,
    enclosure_length: Option<i64>,
}

/// Parse one RSS 2.0 document into feed items.
///
/// Sonarr's `RssParser.GetItems` walks `rss > channel > item`; a document
/// without a `channel` yields nothing. Here a document with no `channel` at all
/// is reported as `InvalidResponse(InvalidRoot)` rather than silently returning
/// zero releases, because an empty feed and a wrong endpoint are different
/// operator problems. An **empty** `channel` is a legitimate quiet feed.
fn parse_feed(body: &str) -> Result<Vec<FeedItem>, Error> {
    let mut reader = Reader::from_str(body);
    // Text is NOT trimmed at the reader: quick-xml splits a text node at every
    // entity reference (`Rosemary&#39;s` is three events), and trimming each
    // piece would silently eat the spaces around an `&amp;`. Whitespace between
    // elements is ignored because `current_tag` is `None` there, and the
    // assembled values are trimmed once in `build_result`.
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut items: Vec<FeedItem> = Vec::new();
    let mut item = FeedItem::default();
    let mut in_item = false;
    let mut saw_channel = false;
    let mut current_tag: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref event)) => {
                let local = local_name(event.name().as_ref());
                match local.as_str() {
                    "channel" => saw_channel = true,
                    "item" => {
                        in_item = true;
                        item = FeedItem::default();
                        current_tag = None;
                    }
                    "enclosure" if in_item => {
                        parse_enclosure(event, &mut item);
                        current_tag = None;
                    }
                    _ if in_item => current_tag = Some(local),
                    _ => current_tag = None,
                }
            }
            Ok(Event::Empty(ref event)) if in_item => {
                if local_name(event.name().as_ref()) == "enclosure" {
                    parse_enclosure(event, &mut item);
                }
            }
            Ok(Event::Text(text)) if in_item => {
                apply_text(&mut item, current_tag.as_deref(), text.as_ref());
            }
            Ok(Event::CData(text)) if in_item => {
                apply_text(&mut item, current_tag.as_deref(), text.as_ref());
            }
            // `&amp;`, `&#39;`, … arrive as their own event, and Sonarr's
            // `GetTitle` runs `WebUtility.HtmlDecode` over the result.
            Ok(Event::GeneralRef(ref reference)) if in_item => {
                if let Some(decoded) = decode_reference(reference.as_ref()) {
                    apply_text(&mut item, current_tag.as_deref(), &decoded);
                }
            }
            Ok(Event::End(ref event)) => {
                let local = local_name(event.name().as_ref());
                if local == "item" {
                    in_item = false;
                    current_tag = None;
                    items.push(std::mem::take(&mut item));
                } else if current_tag.as_deref() == Some(local.as_str()) {
                    if local == "category"
                        && let Some(value) = item.category_buffer.take()
                        && !value.trim().is_empty()
                    {
                        item.categories.push(value.trim().to_string());
                    }
                    current_tag = None;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(invalid_response_error(
                    IndexerSearchInvalidResponseKind::MalformedBody,
                    format!(
                        "TorrentLeech feed is not well-formed XML at byte {}: {error}",
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
            "TorrentLeech response has no RSS <channel> element; the configured 'base_url' is not \
             the RSS host"
                .to_string(),
        ));
    }

    Ok(items)
}

fn build_results(items: &[FeedItem], config: &TorrentLeechConfig) -> Vec<SearchResult> {
    items
        .iter()
        .filter_map(|item| build_result(item, config))
        .collect()
}

/// TorrentLeech answers a bad RSS key with **HTTP 200 `application/rss+xml`**
/// and a well-formed feed whose only item is
///
/// ```xml
/// <item><title>An error has occured!</title><link></link>
///   <description><![CDATA[Your RSS key is invalid.]]></description></item>
/// ```
///
/// (measured against `https://rss.torrentleech.org/<20 zeroes>` on 2026-09-02).
/// Sonarr parses that item, finds no download URL, drops it in `IsValidRelease`
/// and reports "0 releases" — so an operator with a revoked key sees an indexer
/// that quietly returns nothing, for ever. Scryer says what happened instead.
///
/// Called only when nothing buildable came out of the feed, so a sentinel that
/// ever appeared alongside real releases could never cost a release.
fn detect_error_item(items: &[FeedItem]) -> Option<Error> {
    let sentinel = items.iter().find(|item| {
        let title = item
            .title
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let has_link = item
            .link
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        !has_link && (title.contains("error has occured") || title.contains("error has occurred"))
    })?;

    let detail = sentinel
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("no detail supplied");

    if detail.to_ascii_lowercase().contains("rss key") {
        return Some(auth_failed_error(format!(
            "TorrentLeech answered the feed with its error item: \"{detail}\". Copy the RSS key \
             again from the RSS link on your TorrentLeech profile page."
        )));
    }

    Some(deferred_error(
        IndexerSearchIncompleteReason::UpstreamFailure,
        None,
        "TorrentLeech reported an error instead of a feed".to_string(),
        format!("TorrentLeech answered the feed with its error item: \"{detail}\""),
    ))
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

/// Append one text (or resolved entity) fragment to the element it belongs to.
///
/// Fragments are appended verbatim; each assembled value is trimmed once in
/// [`build_result`].
fn apply_text(item: &mut FeedItem, current_tag: Option<&str>, value: &str) {
    if value.is_empty() {
        return;
    }
    match current_tag.unwrap_or_default() {
        "title" => merge_text(&mut item.title, value),
        "link" => merge_text(&mut item.link, value),
        "guid" => merge_text(&mut item.guid, value),
        "comments" => merge_text(&mut item.comments, value),
        "description" => merge_text(&mut item.description, value),
        "pubDate" => merge_text(&mut item.published_at, value),
        "category" => merge_text(&mut item.category_buffer, value),
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
/// An unknown reference is put back verbatim rather than dropped, so a feed
/// containing a bare `&` (`AT&T`) survives instead of losing characters.
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

/// Sonarr's `IsValidRelease` (`HttpIndexerBase.cs:311-325`): a release with no
/// title or no download URL is dropped rather than surfaced.
fn build_result(item: &FeedItem, config: &TorrentLeechConfig) -> Option<SearchResult> {
    let title = item.title.as_deref().map(str::trim).unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    // `TorrentRssParser` prefers a torrent enclosure and falls back to `<link>`;
    // the TorrentLeech feed only ever carries `<link>`.
    let download_url = item
        .enclosure_url
        .as_deref()
        .or(item.link.as_deref())
        .and_then(|value| resolve_url(&config.feed_url, value))?;
    let magnet_url = download_url
        .starts_with("magnet:?")
        .then(|| download_url.clone());

    // `UseGuidInfoUrl = true` (`Torrentleech.cs:26`): the info URL is the
    // `<guid>`, which for this feed is the torrent's details page.
    let guid_url = item
        .guid
        .as_deref()
        .and_then(|value| resolve_url(&config.feed_url, value));
    // `RssParser.GetCommentUrl` reads `<comments>`; `rss-common` never did, so
    // `comment_url` used to be unset for every TorrentLeech release.
    let comment_url = item
        .comments
        .as_deref()
        .and_then(|value| resolve_url(&config.feed_url, value));

    let description = item
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();

    // Sonarr's `TorrentRssParser.GetSeeders`/`GetPeers` with
    // `ParseSeedersInDescription = true` (`Torrentleech.cs:26`).
    let seeders = parse_labelled_count(description, "seeder");
    let leechers = parse_labelled_count(description, "leecher");
    let explicit_peers = parse_labelled_count(description, "peer");
    let seeders = seeders.or_else(|| match (explicit_peers, leechers) {
        (Some(peers), Some(leechers)) => Some(peers - leechers),
        _ => None,
    });
    let peers = explicit_peers.or_else(|| match (seeders, leechers) {
        (Some(seeders), Some(leechers)) => Some(seeders + leechers),
        _ => None,
    });

    // The feed carries no size today, so this is almost always `None` — which
    // is where Scryer deliberately differs from Sonarr's `Size = 0`. It is
    // still parsed so a feed revision that adds `Size: 1.37 GB` is picked up.
    let size_bytes = item
        .enclosure_length
        .filter(|value| *value > 0)
        .or_else(|| parse_size_in_description(description));

    let mut categories = item.categories.clone();
    if categories.is_empty()
        && let Some(category) = parse_category_in_description(description)
    {
        categories.push(category);
    }

    let mut provider_extra = HashMap::new();
    provider_extra.insert(
        "feed_source".to_string(),
        serde_json::Value::from(PROVIDER_ID),
    );
    if !description.is_empty() {
        provider_extra.insert(
            "description".to_string(),
            serde_json::Value::from(description),
        );
    }
    if let Some(category) = categories.first() {
        provider_extra.insert(
            "category".to_string(),
            serde_json::Value::from(category.as_str()),
        );
        if let Some(id) = category_id_for_name(category) {
            provider_extra.insert("category_id".to_string(), serde_json::Value::from(id));
        }
    }
    if let Some(size) = size_bytes {
        provider_extra.insert("reported_size".to_string(), serde_json::Value::from(size));
    }

    Some(SearchResult {
        link: Some(download_url.clone()),
        info_url: guid_url.clone(),
        comment_url,
        // The `<guid>` is the details page: stable, unique per torrent and free
        // of the RSS key. Sonarr fills a missing guid with `Guid.NewGuid()`, a
        // value that changes on every poll; the download URL with the key
        // stripped is used instead so the same release keeps its identity.
        guid: Some(guid_url.unwrap_or_else(|| redact_key(&download_url, &config.api_key))),
        size_bytes,
        published_at: item
            .published_at
            .as_deref()
            .and_then(rfc2822_to_rfc3339_utc),
        seeders,
        peers,
        leechers,
        // TorrentLeech's published hit-and-run rule (Prowlarr's `minimumratio`
        // / `minimumseedtime`). Scryer's seeding gate treats these as a floor
        // under a seeding profile that honours tracker minimums, so a release
        // grabbed here is not removed before the tracker is satisfied.
        minimum_seed_ratio: Some(SITE_MINIMUM_SEED_RATIO),
        minimum_seed_time_minutes: Some(SITE_MINIMUM_SEED_TIME_MINUTES),
        magnet_url,
        categories: categories.clone(),
        provider_categories: categories,
        provider_extra,
        ..torrent_result(title, Some(download_url))
    })
}

/// Sonarr's `RssParser.ParseUrl`: an absolute URL is kept, a relative one is
/// resolved against the request URL.
fn resolve_url(feed_url: &str, value: &str) -> Option<String> {
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
    url::Url::parse(feed_url)
        .ok()
        .and_then(|base| base.join(trimmed).ok())
        .map(|url| url.to_string())
}

fn dedupe_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for result in results {
        let key = result.guid.clone().unwrap_or_else(|| result.title.clone());
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
// Description parsing
// ---------------------------------------------------------------------------

/// Sonarr's `ParseSeedersRegex` / `ParseLeechersRegex` / `ParsePeersRegex`
/// (`TorrentRssParser.cs:174-176`), which are all the same shape:
///
/// ```text
/// (Seeder)s?:\s+(?<value>\d+)|(?<value>\d+)\s+(seeder)s?
/// ```
///
/// with `RegexOptions.IgnoreCase`. .NET tries every start position left to
/// right and, at each position, the left alternative first — which is exactly
/// what this scanner does. TorrentLeech's description is
/// `Category: Episodes HD - Seeders: 1 - Leechers: 7`, so the labelled form is
/// the one that fires.
fn parse_labelled_count(text: &str, label: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if let Some(value) = match_labelled_then_value(bytes, start, label) {
            return Some(value);
        }
        if let Some(value) = match_value_then_label(bytes, start, label) {
            return Some(value);
        }
    }
    None
}

/// `(Seeder)s?:\s+(?<value>\d+)`
fn match_labelled_then_value(bytes: &[u8], start: usize, label: &str) -> Option<i64> {
    let mut cursor = match_ascii_ignore_case(bytes, start, label)?;
    if bytes
        .get(cursor)
        .is_some_and(|byte| byte.eq_ignore_ascii_case(&b's'))
    {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b':') {
        return None;
    }
    cursor += 1;
    let whitespace_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    if cursor == whitespace_start {
        return None;
    }
    read_digits(bytes, cursor).map(|(value, _)| value)
}

/// `(?<value>\d+)\s+(seeder)s?`
fn match_value_then_label(bytes: &[u8], start: usize, label: &str) -> Option<i64> {
    let (value, mut cursor) = read_digits(bytes, start)?;
    let whitespace_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    if cursor == whitespace_start {
        return None;
    }
    match_ascii_ignore_case(bytes, cursor, label)?;
    Some(value)
}

/// Match `needle` at `start`, case-insensitively, returning the offset just
/// past it.
fn match_ascii_ignore_case(bytes: &[u8], start: usize, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    let end = start.checked_add(needle.len())?;
    let slice = bytes.get(start..end)?;
    slice
        .iter()
        .zip(needle)
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
        .then_some(end)
}

fn read_digits(bytes: &[u8], start: usize) -> Option<(i64, usize)> {
    let mut cursor = start;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..cursor])
        .ok()?
        .parse::<i64>()
        .ok()
        .map(|value| (value, cursor))
}

/// Sonarr's `RssParser.ParseSize(value, defaultToBinaryPrefix: true)`.
///
/// Regex, for reference:
/// `(?<value>(?<!\.\d*)(?:\d+,)*\d+(?:\.\d{1,3})?)\W?(?<unit>[KMG]i?B)(?![\w/])`
/// with the whole string short-circuiting to `long.Parse` when it is all
/// digits. Rust's `regex` crate has no look-around, so this is the same grammar
/// written as a leftmost-match scanner.
///
/// Sonarr does **not** set `ParseSizeInDescription` for TorrentLeech (the feed
/// carries no size), so this is dormant against today's feed and exists so a
/// feed revision that adds one is read. `Category: 4KUpscaled - Seeders: 5`
/// does not match: `[KMG]i?B` needs the `B`.
fn parse_size_in_description(description: &str) -> Option<i64> {
    let text = description.trim();
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
        let Some((value, after_value)) = match_number(bytes, start) else {
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
        return Some((value * 1024_f64.powi(power)).round() as i64);
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

/// TorrentLeech repeats the category inside `<description>`
/// (`Category: Episodes HD - Seeders: 1 - Leechers: 7`). The `<category>`
/// element is the primary source; this is the fallback for a feed variant that
/// omits it. Sonarr throws the whole description away.
fn parse_category_in_description(description: &str) -> Option<String> {
    let text = description.trim();
    let lowered = text.to_ascii_lowercase();
    let offset = lowered.find("category:")?;
    let tail = &text[offset + "category:".len()..];
    let end = tail
        .find(" - ")
        .or_else(|| tail.to_ascii_lowercase().find("size:"))
        .unwrap_or(tail.len());
    let candidate = tail[..end].trim().trim_end_matches([';', ',', '-']).trim();
    (!candidate.is_empty()).then(|| candidate.to_string())
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// RSS `pubDate` is RFC 2822 (`Mon, 12 May 2014 19:15:28 +0000`).
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

    // Drop the optional `Mon, ` day-of-week prefix.
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

/// `+HHMM` / `-HHMM`, plus the obsolete alphabetic zones RFC 2822 §4.3 keeps
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
struct TorrentLeechConfig {
    feed_url: String,
    api_key: String,
    user_agent: String,
    cookie: Option<String>,
    username: Option<String>,
    password: Option<String>,
    additional_headers: String,
}

impl TorrentLeechConfig {
    fn from_host() -> Result<Self, Error> {
        let configured = config_value("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let base_url = validate_base_url(&configured)?;
        if configured.starts_with("http://") && base_url.starts_with("https://") {
            component::log(
                LogLevel::Info,
                format!(
                    "plugin={PROVIDER_ID} upgraded the configured base_url from http:// to \
                     https:// — TorrentLeech is HSTS-preloaded and answers plain http with 403"
                ),
            );
        }
        let api_key = validate_api_key(config_value("api_key").as_deref().unwrap_or_default())?;
        Ok(Self {
            feed_url: build_feed_url(&base_url, &api_key),
            api_key,
            user_agent: config_value("user_agent").unwrap_or_else(|| USER_AGENT.to_string()),
            cookie: config_value("cookie"),
            username: config_value("username"),
            password: config_value("password"),
            additional_headers: config_value("additional_headers").unwrap_or_default(),
        })
    }

    fn request_headers(&self) -> BTreeMap<String, String> {
        // Sonarr requests `HttpAccept.Rss`
        // (`TorrentleechRequestGenerator.cs:58`), and `RssParser.PreProcess`
        // only raises "responded with html content" when the request did NOT
        // ask for HTML — so `text/html` is deliberately absent here.
        let mut headers = BTreeMap::from([
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

/// Sonarr's `TorrentleechRequestGenerator.GetRssRequests`
/// (`TorrentleechRequestGenerator.cs:56-59`):
/// `string.Format("{0}/{1}", Settings.BaseUrl.Trim().TrimEnd('/'), Settings.ApiKey)`.
///
/// One addition: if `base_url` already ends with the key — the operator pasted
/// the whole RSS link into the "Website URL" field, which is the commonest
/// misconfiguration for this indexer — the key is not appended twice.
fn build_feed_url(base_url: &str, api_key: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base
        .rsplit('/')
        .next()
        .is_some_and(|segment| segment == api_key)
    {
        return base.to_string();
    }
    format!("{base}/{api_key}")
}

/// `TorrentleechSettingsValidator`: `RuleFor(c => c.BaseUrl).ValidRootUrl()`
/// (`TorrentleechSettings.cs:15`), as a typed configuration error rather than
/// an untyped failure — Scryer has no settings validator, so this surfaces at
/// search time.
///
/// A root URL is a parseable http(s) URL with a host and no query or fragment
/// (a query would end up before the appended key and produce a 404).
fn validate_base_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "base_url",
            "TorrentLeech requires the RSS host, e.g. https://rss.torrentleech.org".to_string(),
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
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid_config_error(
            "base_url",
            format!(
                "'{trimmed}' must be a root URL: the RSS key is appended to it as a path segment, \
                 so a query string or fragment cannot work"
            ),
        ));
    }

    Ok(upgrade_torrentleech_scheme(trimmed))
}

/// Sonarr's default is `http://rss.torrentleech.org` and that URL no longer
/// works: measured on 2026-09-02, `http://rss.torrentleech.org/…` answers
/// **HTTP 403** from Cloudflare with an HTML body, and the other TorrentLeech
/// hosts answer **301**. Every TorrentLeech domain sends
/// `Strict-Transport-Security: max-age=15768000; includeSubdomains; preload`,
/// so upgrading the scheme is what the site itself mandates. The host's plugin
/// HTTP does not follow redirects, so without this an operator on the shipped
/// default gets a hard failure on every poll.
///
/// The upgrade is confined to TorrentLeech's own domains: any other host an
/// operator points at is left exactly as typed.
fn upgrade_torrentleech_scheme(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.scheme() != "http" {
        return url.to_string();
    }
    let Some(host) = parsed.host_str() else {
        return url.to_string();
    };
    let host = host.to_ascii_lowercase();
    let is_torrentleech = TORRENTLEECH_DOMAINS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    if !is_torrentleech {
        return url.to_string();
    }
    format!("https{}", &url["http".len()..])
}

/// `RuleFor(c => c.ApiKey).NotEmpty()` (`TorrentleechSettings.cs:16`), plus the
/// two shapes that cannot possibly work as a path segment.
fn validate_api_key(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "api_key",
            "TorrentLeech requires the RSS key from the RSS link on your profile page".to_string(),
        ));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Err(invalid_config_error(
            "api_key",
            "'api_key' is the RSS key alone (the last path segment of your RSS link), not the \
             whole URL"
                .to_string(),
        ));
    }
    if trimmed
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '/' | '?' | '#' | '&'))
    {
        return Err(invalid_config_error(
            "api_key",
            "'api_key' must be the RSS key alone — no slashes, spaces or query characters"
                .to_string(),
        ));
    }
    Ok(trimmed.to_string())
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

scryer_indexer_component_main!(descriptor = build_descriptor, search = search,);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_pdk::component::StructuredPluginError;
    use scryer_plugin_sdk::PluginSearchContext;

    /// Sonarr's `Files/Indexers/Torrentleech/Torrentleech.xml`, verbatim except
    /// for the leading UTF-8 BOM (the fixture is embedded as a `str`, not read
    /// as bytes). The `1234` in every download URL is Sonarr's redacted RSS
    /// key; it is the same placeholder used as `TEST_API_KEY` below.
    const RECENT_FEED: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
  <channel>
    <title>TorrentLeech</title>
    <link>http://www.torrentleech.org</link>
    <description>The latest torrents from TorrentLeech.org</description>
    <language>en</language>
    <ttl>5</ttl>
    <atom:link href="http://rss.torrentleech.org/1234" rel="self" type="application/rss+xml" />
    <item>
      <title><![CDATA[Classic Car Rescue S02E04 720p HDTV x264-C4TV]]></title>
      <pubDate>Mon, 12 May 2014 19:15:28 +0000</pubDate>
      <category>Episodes HD</category>
      <guid>http://www.torrentleech.org/torrent/513575</guid>
      <comments><![CDATA[http://www.torrentleech.org/torrent/513575#comments]]></comments>
      <link><![CDATA[http://www.torrentleech.org/rss/download/513575/1234/Classic.Car.Rescue.S02E04.720p.HDTV.x264-C4TV.torrent]]></link>
      <description><![CDATA[Category: Episodes HD - Seeders: 1 - Leechers: 7]]></description>
    </item>
    <item>
      <title><![CDATA[24 S03E14 720p WEBRip h264-DRAWER]]></title>
      <pubDate>Mon, 12 May 2014 19:14:09 +0000</pubDate>
      <category>Episodes HD</category>
      <guid>http://www.torrentleech.org/torrent/513574</guid>
      <comments><![CDATA[http://www.torrentleech.org/torrent/513574#comments]]></comments>
      <link><![CDATA[http://www.torrentleech.org/rss/download/513574/1234/24.S03E14.720p.WEBRip.h264-DRAWER.torrent]]></link>
      <description><![CDATA[Category: Episodes HD - Seeders: 13 - Leechers: 11]]></description>
    </item>
    <item>
      <title><![CDATA[24 S03E13 720p WEBRip h264-DRAWER]]></title>
      <pubDate>Mon, 12 May 2014 19:09:18 +0000</pubDate>
      <category>Episodes HD</category>
      <guid>http://www.torrentleech.org/torrent/513573</guid>
      <comments><![CDATA[http://www.torrentleech.org/torrent/513573#comments]]></comments>
      <link><![CDATA[http://www.torrentleech.org/rss/download/513573/1234/24.S03E13.720p.WEBRip.h264-DRAWER.torrent]]></link>
      <description><![CDATA[Category: Episodes HD - Seeders: 19 - Leechers: 7]]></description>
    </item>
    <item>
      <title><![CDATA[24 S03E11 720p WEBRip h264-DRAWER]]></title>
      <pubDate>Mon, 12 May 2014 19:09:10 +0000</pubDate>
      <category>Episodes HD</category>
      <guid>http://www.torrentleech.org/torrent/513572</guid>
      <comments><![CDATA[http://www.torrentleech.org/torrent/513572#comments]]></comments>
      <link><![CDATA[http://www.torrentleech.org/rss/download/513572/1234/24.S03E11.720p.WEBRip.h264-DRAWER.torrent]]></link>
      <description><![CDATA[Category: Episodes HD - Seeders: 19 - Leechers: 7]]></description>
    </item>
    <item>
      <title><![CDATA[Meet Joe Black 1998 1080p HDDVD x264-FSiHD]]></title>
      <pubDate>Mon, 12 May 2014 19:06:59 +0000</pubDate>
      <category>HD</category>
      <guid>http://www.torrentleech.org/torrent/513571</guid>
      <comments><![CDATA[http://www.torrentleech.org/torrent/513571#comments]]></comments>
      <link><![CDATA[http://www.torrentleech.org/rss/download/513571/1234/Meet.Joe.Black.1998.1080p.HDDVD.x264-FSiHD.torrent]]></link>
      <description><![CDATA[Category: HD - Seeders: 1 - Leechers: 10]]></description>
    </item>
  </channel>
</rss>"#;

    /// The document TorrentLeech serves for an unrecognised RSS key, captured
    /// from `https://rss.torrentleech.org/<20 zeroes>` on 2026-09-02 (HTTP 200,
    /// `Content-Type: application/rss+xml`). The site's own spelling of
    /// "occured" is preserved.
    const INVALID_KEY_FEED: &str = r#"<?xml version="1.0" encoding="utf-8" ?><rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom"><channel><title>TorrentLeech</title>
	            <link>https://www.torrentleech.org</link>
	            <description>The latest torrents from TorrentLeech.org</description>
	            <language>en</language>
				<ttl>5</ttl>
				<atom:link href="http://rss.torrentleech.org/" rel="self" type="application/rss+xml" /><item><title>An error has occured!</title><link></link><description><![CDATA[Your RSS key is invalid.]]></description></item></channel></rss>"#;

    const TEST_API_KEY: &str = "1234";

    fn test_config() -> TorrentLeechConfig {
        TorrentLeechConfig {
            feed_url: format!("https://rss.torrentleech.org/{TEST_API_KEY}"),
            api_key: TEST_API_KEY.to_string(),
            user_agent: USER_AGENT.to_string(),
            cookie: None,
            username: None,
            password: None,
            additional_headers: String::new(),
        }
    }

    fn parse_fixture() -> Vec<SearchResult> {
        let config = test_config();
        let items = parse_feed(RECENT_FEED).expect("fixture parses");
        dedupe_results(build_results(&items, &config))
    }

    fn recent_request() -> SearchRequest {
        SearchRequest {
            limit: 1000,
            context: Some(PluginSearchContext {
                request_kind: PluginSearchRequestKind::Recent,
                ..Default::default()
            }),
            ..SearchRequest::default()
        }
    }

    fn plugin_error(error: &Error) -> PluginError {
        error
            .downcast_ref::<StructuredPluginError>()
            .expect("error should be a structured plugin error")
            .plugin_error()
            .clone()
    }

    // -- Parsing parity with Sonarr's fixture (H1) --------------------------

    /// `TorrentleechFixture.should_parse_recent_feed_from_Torrentleech`, value
    /// for value.
    #[test]
    fn should_parse_recent_feed_from_torrentleech() {
        let results = parse_fixture();
        assert_eq!(results.len(), 5);

        let first = &results[0];
        assert_eq!(first.title, "Classic Car Rescue S02E04 720p HDTV x264-C4TV");
        assert_eq!(first.protocol, Some(IndexerProtocol::Torrent));
        assert_eq!(first.source_kind, Some(IndexerSourceKind::Torrent));
        assert_eq!(
            first.download_url.as_deref(),
            Some(
                "http://www.torrentleech.org/rss/download/513575/1234/Classic.Car.Rescue.S02E04.720p.HDTV.x264-C4TV.torrent"
            )
        );
        assert_eq!(
            first.info_url.as_deref(),
            Some("http://www.torrentleech.org/torrent/513575")
        );
        assert_eq!(
            first.comment_url.as_deref(),
            Some("http://www.torrentleech.org/torrent/513575#comments")
        );
        assert_eq!(first.published_at.as_deref(), Some("2014-05-12T19:15:28Z"));
        // Sonarr reports `Size = 0` because the feed carries no size; Scryer
        // reports "unknown" instead. See the report, §3.
        assert_eq!(first.size_bytes, None);
        assert_eq!(first.info_hash_v1, None);
        assert_eq!(first.magnet_url, None);
        assert_eq!(first.seeders, Some(1));
        assert_eq!(first.peers, Some(1 + 7));
        assert_eq!(first.leechers, Some(7));
        assert_eq!(first.categories, vec!["Episodes HD".to_string()]);
        assert_eq!(first.provider_categories, vec!["Episodes HD".to_string()]);
    }

    #[test]
    fn the_remaining_fixture_entries_parse_too() {
        let results = parse_fixture();
        let titles: Vec<&str> = results.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Classic Car Rescue S02E04 720p HDTV x264-C4TV",
                "24 S03E14 720p WEBRip h264-DRAWER",
                "24 S03E13 720p WEBRip h264-DRAWER",
                "24 S03E11 720p WEBRip h264-DRAWER",
                "Meet Joe Black 1998 1080p HDDVD x264-FSiHD",
            ]
        );
        assert_eq!(results[1].seeders, Some(13));
        assert_eq!(results[1].peers, Some(13 + 11));
        assert_eq!(results[2].seeders, Some(19));
        assert_eq!(results[2].leechers, Some(7));
        assert_eq!(results[4].categories, vec!["HD".to_string()]);
        assert_eq!(
            results[4].published_at.as_deref(),
            Some("2014-05-12T19:06:59Z")
        );
    }

    /// The `<guid>` is used as the release identity: it is stable across polls
    /// and, unlike the download URL, contains no RSS key.
    #[test]
    fn the_guid_is_the_details_page_and_carries_no_rss_key() {
        let results = parse_fixture();
        assert_eq!(
            results[0].guid.as_deref(),
            Some("http://www.torrentleech.org/torrent/513575")
        );
        for result in &results {
            let guid = result.guid.as_deref().unwrap_or_default();
            assert!(
                !guid.contains(TEST_API_KEY),
                "guid leaked the RSS key: {guid}"
            );
        }
    }

    /// Without a `<guid>` Sonarr uses `Guid.NewGuid()`, so the same release
    /// never looks like itself twice. The download URL is used instead, with
    /// the RSS key stripped out of the path.
    #[test]
    fn a_release_without_a_guid_falls_back_to_a_key_free_download_url() {
        let config = test_config();
        let feed = RECENT_FEED.replace(
            "<guid>http://www.torrentleech.org/torrent/513575</guid>",
            "",
        );
        let items = parse_feed(&feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(
            results[0].guid.as_deref(),
            Some(
                "http://www.torrentleech.org/rss/download/513575/REDACTED/Classic.Car.Rescue.S02E04.720p.HDTV.x264-C4TV.torrent"
            )
        );
        assert_eq!(results[0].info_url, None);
    }

    #[test]
    fn every_release_reports_torrentleechs_hit_and_run_rule() {
        for result in parse_fixture() {
            assert_eq!(result.minimum_seed_ratio, Some(1.0));
            assert_eq!(result.minimum_seed_time_minutes, Some(14_400));
        }
    }

    /// `result.indexer_flags` is stored but never read back on candidate reuse,
    /// and this feed says nothing about freeleech, so neither the flag list nor
    /// `extra["freeleech"]`/`extra["tags"]` is invented.
    #[test]
    fn no_freeleech_or_tag_metadata_is_invented() {
        for result in parse_fixture() {
            assert!(result.indexer_flags.is_empty());
            assert!(!result.provider_extra.contains_key("freeleech"));
            assert!(!result.provider_extra.contains_key("tags"));
            assert_eq!(result.download_volume_factor, None);
            assert_eq!(result.upload_volume_factor, None);
        }
    }

    #[test]
    fn the_category_is_reported_with_torrentleechs_own_id_when_it_is_known() {
        let results = parse_fixture();
        assert_eq!(
            results[0].provider_extra.get("category"),
            Some(&serde_json::Value::from("Episodes HD"))
        );
        assert_eq!(
            results[0].provider_extra.get("category_id"),
            Some(&serde_json::Value::from(32_i64))
        );
        // "HD" is a category name the site has retired; no id is invented.
        assert_eq!(
            results[4].provider_extra.get("category"),
            Some(&serde_json::Value::from("HD"))
        );
        assert!(!results[4].provider_extra.contains_key("category_id"));
    }

    #[test]
    fn the_description_is_kept_as_provider_metadata() {
        let results = parse_fixture();
        assert_eq!(
            results[0].provider_extra.get("description"),
            Some(&serde_json::Value::from(
                "Category: Episodes HD - Seeders: 1 - Leechers: 7"
            ))
        );
    }

    #[test]
    fn a_release_without_a_title_or_a_link_is_dropped() {
        let config = test_config();
        let feed = r#"<rss><channel>
            <item><title></title><link>http://example.org/a.torrent</link></item>
            <item><title>No link</title></item>
            <item><title>Good</title><link>http://example.org/b.torrent</link></item>
        </channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good");
    }

    #[test]
    fn html_entities_in_a_title_are_decoded() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Rosemary&#39;s Baby S01E01 AT&amp;T</title>
            <link>http://example.org/a.torrent</link>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results[0].title, "Rosemary's Baby S01E01 AT&T");
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_feed_url() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Relative</title>
            <link>/rss/download/1/1234/a.torrent</link>
            <guid>/torrent/1</guid>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("https://rss.torrentleech.org/rss/download/1/1234/a.torrent")
        );
        assert_eq!(
            results[0].info_url.as_deref(),
            Some("https://rss.torrentleech.org/torrent/1")
        );
    }

    #[test]
    fn an_enclosure_wins_over_the_link_and_supplies_a_size() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Enclosed</title>
            <link>http://example.org/details</link>
            <enclosure url="http://example.org/a.torrent" type="application/x-bittorrent" length="1048576" />
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(
            results[0].download_url.as_deref(),
            Some("http://example.org/a.torrent")
        );
        assert_eq!(results[0].size_bytes, Some(1_048_576));
    }

    #[test]
    fn a_magnet_link_feed_is_reported_as_a_magnet() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Magnet</title>
            <link>magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567</link>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert!(results[0].magnet_url.is_some());
    }

    #[test]
    fn duplicate_entries_are_deduped_by_guid() {
        let config = test_config();
        let feed = r#"<rss><channel>
            <item><title>A</title><link>http://example.org/a.torrent</link><guid>http://example.org/1</guid></item>
            <item><title>A again</title><link>http://example.org/b.torrent</link><guid>http://example.org/1</guid></item>
        </channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = dedupe_results(build_results(&items, &config));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A");
    }

    // -- No post-filtering (M3) ---------------------------------------------

    /// `rss-common::execute_rss_urls` runs `filter_results`, which drops a
    /// release whose provider categories do not contain the request's facet
    /// words — and every TorrentLeech `<category>` is a site name like
    /// "Episodes HD". Sonarr never post-filters; nor does this plugin.
    #[test]
    fn a_faceted_poll_does_not_drop_releases_whose_category_is_a_site_name() {
        let config = test_config();
        let items = parse_feed(RECENT_FEED).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results.len(), 5);
        // Nothing in the pipeline consults the request at all beyond `limit`.
        assert!(results.iter().all(|result| !result.categories.is_empty()));
    }

    #[test]
    fn the_host_limit_is_honoured_and_zero_means_everything() {
        assert_eq!(result_limit(&recent_request()), Some(1000));
        assert_eq!(
            result_limit(&SearchRequest {
                limit: 2,
                ..recent_request()
            }),
            Some(2)
        );
        assert_eq!(
            result_limit(&SearchRequest {
                limit: 0,
                ..recent_request()
            }),
            None
        );
    }

    #[test]
    fn only_a_recent_poll_reaches_the_feed() {
        assert!(is_recent_request(&recent_request()));
        assert!(is_recent_request(&SearchRequest::default()));
        assert!(!is_recent_request(&SearchRequest {
            context: Some(PluginSearchContext {
                request_kind: PluginSearchRequestKind::Search,
                ..Default::default()
            }),
            ..SearchRequest::default()
        }));
        // No context: any criterion makes it a search.
        assert!(!is_recent_request(&SearchRequest {
            query: "Classic Car Rescue".to_string(),
            ..SearchRequest::default()
        }));
        assert!(!is_recent_request(&SearchRequest {
            season: Some(2),
            ..SearchRequest::default()
        }));
    }

    // -- Invalid-key sentinel (found beyond the brief) -----------------------

    #[test]
    fn an_invalid_rss_key_is_reported_as_an_auth_failure_not_an_empty_feed() {
        let config = test_config();
        let items = parse_feed(INVALID_KEY_FEED).expect("parses");
        assert_eq!(items.len(), 1);
        let results = build_results(&items, &config);
        assert!(results.is_empty(), "the sentinel item is not a release");

        let error = detect_error_item(&items).expect("sentinel is detected");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"));
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("Your RSS key is invalid.")
        );
    }

    #[test]
    fn another_sentinel_error_defers_rather_than_blaming_the_key() {
        let items = vec![FeedItem {
            title: Some("An error has occured!".to_string()),
            link: Some(String::new()),
            description: Some("The feed is temporarily disabled.".to_string()),
            ..FeedItem::default()
        }];
        let error = plugin_error(&detect_error_item(&items).expect("sentinel is detected"));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::UpstreamFailure,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn a_normal_feed_has_no_sentinel() {
        let items = parse_feed(RECENT_FEED).expect("parses");
        assert!(detect_error_item(&items).is_none());
    }

    // -- Delivery classification (H2) ---------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn an_ok_xml_delivery_is_returned_verbatim() {
        let body = RECENT_FEED.as_bytes();
        let text = classify_response(
            200,
            &headers(&[("Content-Type", "application/rss+xml")]),
            body,
        )
        .expect("200 xml is accepted");
        assert_eq!(text, RECENT_FEED);
    }

    #[test]
    fn a_redirect_names_the_base_url() {
        let error = classify_response(
            301,
            &headers(&[("Location", "https://rss.torrentleech.org/1234")]),
            b"",
        )
        .expect_err("3xx is an error");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("base_url"));
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("https://rss.torrentleech.org/1234")
        );
    }

    /// nginx answers a path that is not `/{RSSKEY}` with a 404 HTML page.
    #[test]
    fn a_404_names_the_base_url_rather_than_deferring() {
        let error = classify_response(
            404,
            &headers(&[("Content-Type", "text/html")]),
            b"<html><head><title>404 Not Found</title></head></html>",
        )
        .expect_err("404 is an error");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("base_url"));
    }

    /// `http://rss.torrentleech.org` answers 403 with a Cloudflare HTML page.
    #[test]
    fn a_403_with_html_is_an_unexpected_content_type() {
        let error = classify_response(
            403,
            &headers(&[("Content-Type", "text/html; charset=iso-8859-1")]),
            b"<html><head><title>403 Forbidden</title></head></html>",
        )
        .expect_err("403 html is an error");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
                }
            ))
        ));
    }

    #[test]
    fn a_bare_401_is_an_auth_failure() {
        let error = classify_response(401, &headers(&[]), b"nope").expect_err("401 is an error");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert!(error.public_message.contains("api_key"));
    }

    #[test]
    fn a_429_defers_with_the_retry_after_header() {
        let error = classify_response(429, &headers(&[("Retry-After", "42")]), b"")
            .expect_err("429 is an error");
        let error = plugin_error(&error);
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(42));
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::RateLimited,
                    retry_after_seconds: Some(42)
                }
            ))
        ));
    }

    #[test]
    fn a_429_without_a_header_uses_sonarrs_one_hour_floor() {
        let error =
            plugin_error(&classify_response(429, &headers(&[]), b"").expect_err("429 is an error"));
        assert_eq!(
            error.retry_after_seconds,
            Some(RATE_LIMITED_FALLBACK_SECONDS)
        );
    }

    #[test]
    fn a_500_defers_as_an_upstream_failure() {
        let error =
            plugin_error(&classify_response(500, &headers(&[]), b"boom").expect_err("5xx errors"));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::UpstreamFailure,
                    ..
                }
            ))
        ));
    }

    /// `rss.torrentleech.cc/{key}` answers 200 with the site's HTML rather than
    /// a feed — only `rss.torrentleech.org` serves RSS.
    #[test]
    fn a_200_html_delivery_is_an_unexpected_content_type() {
        let error = plugin_error(
            &classify_response(
                200,
                &headers(&[("Content-Type", "text/html; charset=UTF-8")]),
                b"<html><body>TorrentLeech</body></html>",
            )
            .expect_err("html is an error"),
        );
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
                }
            ))
        ));
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("rss24h.torrentleech.org")
        );
    }

    #[test]
    fn an_html_body_without_a_content_type_is_still_detected() {
        assert!(is_html_delivery(&headers(&[]), b"  <!DOCTYPE html><html>"));
        assert!(is_html_delivery(
            &headers(&[("Content-Type", "text/plain")]),
            b"<html>"
        ));
        assert!(!is_html_delivery(&headers(&[]), RECENT_FEED.as_bytes()));
    }

    #[test]
    fn an_oversized_body_is_a_truncated_body() {
        let body = vec![b'a'; MAX_RESPONSE_BYTES + 1];
        let error = plugin_error(
            &classify_response(200, &headers(&[]), &body).expect_err("oversized body errors"),
        );
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::TruncatedBody
                }
            ))
        ));
    }

    #[test]
    fn a_non_utf8_body_is_a_malformed_body() {
        let error = plugin_error(
            &classify_response(200, &headers(&[]), &[0xff, 0xfe, 0x00])
                .expect_err("non-utf8 errors"),
        );
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::MalformedBody
                }
            ))
        ));
    }

    #[test]
    fn malformed_xml_is_a_malformed_body() {
        let error = plugin_error(
            &parse_feed("<rss><channel><item><title>x</channel>").expect_err("bad xml errors"),
        );
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::MalformedBody
                }
            ))
        ));
    }

    #[test]
    fn a_document_without_a_channel_is_an_invalid_root() {
        let error = plugin_error(
            &parse_feed("<html><body>hi</body></html>").expect_err("no channel errors"),
        );
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::InvalidRoot
                }
            ))
        ));
    }

    #[test]
    fn an_empty_channel_is_a_quiet_feed_not_an_error() {
        let items = parse_feed("<rss><channel></channel></rss>").expect("parses");
        assert!(items.is_empty());
    }

    // -- Configuration -------------------------------------------------------

    /// Sonarr: `string.Format("{0}/{1}", BaseUrl.Trim().TrimEnd('/'), ApiKey)`.
    #[test]
    fn the_feed_url_is_the_base_url_with_the_key_appended() {
        assert_eq!(
            build_feed_url("https://rss.torrentleech.org", "abcd"),
            "https://rss.torrentleech.org/abcd"
        );
        assert_eq!(
            build_feed_url("https://rss.torrentleech.org/", "abcd"),
            "https://rss.torrentleech.org/abcd"
        );
        assert_eq!(
            build_feed_url("  https://rss24h.torrentleech.org  ", "abcd"),
            "https://rss24h.torrentleech.org/abcd"
        );
    }

    #[test]
    fn a_base_url_that_already_ends_with_the_key_is_not_doubled() {
        assert_eq!(
            build_feed_url("https://rss.torrentleech.org/abcd", "abcd"),
            "https://rss.torrentleech.org/abcd"
        );
    }

    #[test]
    fn sonarrs_http_default_is_upgraded_to_https() {
        assert_eq!(
            validate_base_url(DEFAULT_BASE_URL).expect("default validates"),
            "https://rss.torrentleech.org"
        );
        assert_eq!(
            validate_base_url("http://rss24h.torrentleech.cc").expect("mirror validates"),
            "https://rss24h.torrentleech.cc"
        );
        assert_eq!(
            validate_base_url("http://www.tleechreload.org").expect("mirror validates"),
            "https://www.tleechreload.org"
        );
    }

    #[test]
    fn a_non_torrentleech_host_keeps_the_scheme_the_operator_typed() {
        assert_eq!(
            validate_base_url("http://rss.example.org").expect("validates"),
            "http://rss.example.org"
        );
    }

    #[test]
    fn an_unusable_base_url_is_a_typed_config_error() {
        for value in ["", "   ", "not a url", "ftp://rss.torrentleech.org"] {
            let error = plugin_error(&validate_base_url(value).expect_err("rejected"));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig, "value: {value}");
            assert!(error.public_message.contains("base_url"));
        }
    }

    /// The key is appended as a path segment, so a query or fragment in
    /// `base_url` can never produce a working feed URL.
    #[test]
    fn a_base_url_with_a_query_or_fragment_is_rejected() {
        for value in [
            "https://rss.torrentleech.org/?categories=26",
            "https://rss.torrentleech.org/#rss",
        ] {
            let error = plugin_error(&validate_base_url(value).expect_err("rejected"));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        }
    }

    /// Sonarr: `RuleFor(c => c.ApiKey).NotEmpty()`.
    #[test]
    fn an_empty_api_key_is_a_typed_config_error() {
        let error = plugin_error(&validate_api_key("  ").expect_err("rejected"));
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("api_key"));
    }

    #[test]
    fn a_whole_rss_url_pasted_into_the_api_key_is_named_as_such() {
        let error = plugin_error(
            &validate_api_key("https://rss.torrentleech.org/abcd").expect_err("rejected"),
        );
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error
                .debug_message
                .unwrap_or_default()
                .contains("not the whole URL")
        );
    }

    #[test]
    fn an_api_key_with_path_or_query_characters_is_rejected() {
        for value in ["ab/cd", "ab cd", "ab?cd", "ab#cd", "ab&cd"] {
            let error = plugin_error(&validate_api_key(value).expect_err("rejected"));
            assert_eq!(error.code, PluginErrorCode::InvalidConfig, "value: {value}");
        }
        assert_eq!(
            validate_api_key(" 0123456789abcdef0123 ").expect("valid"),
            "0123456789abcdef0123"
        );
    }

    #[test]
    fn the_rss_key_is_redacted_wherever_it_appears_in_a_path() {
        assert_eq!(
            redact_key(
                "https://rss.torrentleech.org/SECRET/rss/download/1/SECRET/a.torrent",
                "SECRET"
            ),
            "https://rss.torrentleech.org/REDACTED/rss/download/1/REDACTED/a.torrent"
        );
        assert_eq!(redact_key("https://x/y", ""), "https://x/y");
    }

    #[test]
    fn the_request_asks_for_xml_with_a_versioned_user_agent() {
        let headers = test_config().request_headers();
        let accept = headers.get("Accept").expect("Accept is sent");
        assert!(accept.contains("application/rss+xml"));
        // Sonarr's `PreProcess` HTML check only fires when the request did not
        // ask for HTML.
        assert!(!accept.contains("text/html"));
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some(USER_AGENT)
        );
        assert!(USER_AGENT.starts_with("scryer-torrentleech-indexer/"));
    }

    #[test]
    fn optional_transport_settings_become_headers() {
        let config = TorrentLeechConfig {
            cookie: Some("cf_clearance=abc".to_string()),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            additional_headers: "X-Extra: 1\nbroken line\n".to_string(),
            ..test_config()
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
        assert_eq!(headers.get("X-Extra").map(String::as_str), Some("1"));
    }

    // -- Descriptor honesty (M1) --------------------------------------------

    fn indexer_descriptor() -> IndexerDescriptor {
        match build_descriptor().provider {
            ProviderDescriptor::Indexer(descriptor) => descriptor,
            _ => panic!("torrentleech is an indexer"),
        }
    }

    #[test]
    fn the_descriptor_claims_only_what_the_feed_carries() {
        let descriptor = indexer_descriptor();
        let torrent = descriptor.capabilities.torrent.expect("torrent caps");
        assert!(torrent.reports_seeders);
        assert!(torrent.reports_peers);
        assert!(torrent.reports_leechers);
        // The shipped descriptor claimed both; Sonarr's fixture asserts null.
        assert!(!torrent.reports_info_hash);
        assert!(!torrent.reports_magnet_uri);
        assert!(!torrent.reports_volume_factors);
        assert!(torrent.supports_private_tracker_flags);
        assert!(torrent.supports_seed_requirements);

        // Sonarr: `SupportsSearch => false`, and the endpoint takes no query.
        assert!(!descriptor.capabilities.search);
        assert!(descriptor.capabilities.rss);
        assert!(descriptor.capabilities.query_param.is_none());
        assert!(descriptor.capabilities.season_param.is_none());
        assert!(descriptor.capabilities.episode_param.is_none());
        assert!(descriptor.capabilities.supported_ids.is_empty());
        assert_eq!(
            descriptor.capabilities.search_inputs,
            vec![IndexerSearchInput::Limit]
        );

        let features = descriptor
            .capabilities
            .response_features
            .expect("response features");
        assert!(features.comments);
        assert!(features.info_url);
        assert!(features.guid);
        assert!(!features.languages);
        assert!(!features.grabs);
    }

    /// Sonarr's `PageSize => 0`: the feed is one unpaged window whose length the
    /// site does not publish. The shipped descriptor claimed 200.
    #[test]
    fn the_descriptor_declares_one_page_of_unknown_length() {
        let limits = indexer_descriptor()
            .capabilities
            .limits
            .expect("limits are declared");
        assert_eq!(limits.page_size, None);
        assert_eq!(limits.max_page_size, None);
        assert_eq!(limits.max_pages, Some(1));
        assert_eq!(limits.rate_limit_hint_seconds, Some(5));
    }

    /// Prowlarr sets `requestDelay: 4.1` for TorrentLeech; Sonarr uses its
    /// fleet-wide 2 s.
    #[test]
    fn the_rate_limit_follows_prowlarrs_measured_request_delay() {
        assert_eq!(indexer_descriptor().rate_limit_seconds, Some(5));
        const { assert!(REQUEST_INTERVAL_MS >= 4_100) };
    }

    #[test]
    fn the_config_field_keys_are_the_published_contract() {
        let keys: Vec<String> = indexer_descriptor()
            .config_fields
            .into_iter()
            .map(|field| field.key)
            .collect();
        assert_eq!(
            keys,
            vec![
                "base_url",
                "api_key",
                "minimum_seeders",
                "user_agent",
                "cookie",
                "username",
                "password",
                "additional_headers",
            ]
        );
    }

    #[test]
    fn the_published_base_url_default_is_still_sonarrs() {
        let default = indexer_descriptor()
            .config_fields
            .into_iter()
            .find(|field| field.key == "base_url")
            .and_then(|field| field.default_value);
        assert_eq!(default.as_deref(), Some(DEFAULT_BASE_URL));
    }

    #[test]
    fn the_declared_category_table_is_torrentleechs_published_ids() {
        let model = indexer_descriptor()
            .capabilities
            .category_model
            .expect("category model");
        assert!(model.separate_anime_categories);
        assert!(model.provider_category_metadata);
        let by_name = |name: &str| {
            model
                .categories
                .iter()
                .find(|category| category.value == name)
                .cloned()
        };
        assert_eq!(
            by_name("Episodes HD").expect("Episodes HD").facets,
            vec!["series".to_string()]
        );
        assert_eq!(
            by_name("Anime").expect("Anime").facets,
            vec!["anime".to_string()]
        );
        assert_eq!(
            by_name("Bluray").expect("Bluray").facets,
            vec!["movie".to_string()]
        );
        assert!(by_name("Games PC").expect("Games PC").facets.is_empty());
    }

    #[test]
    fn category_ids_resolve_by_name_and_alias_but_never_when_ambiguous() {
        assert_eq!(category_id_for_name("Episodes"), Some(26));
        assert_eq!(category_id_for_name("episodes hd"), Some(32));
        assert_eq!(category_id_for_name("TV Boxsets"), Some(27));
        assert_eq!(category_id_for_name("Boxsets"), Some(15));
        assert_eq!(category_id_for_name("TV Anime"), Some(34));
        assert_eq!(category_id_for_name("TS"), Some(9));
        // Both a Movies (36) and a TV (44) "Foreign" category exist.
        assert_eq!(category_id_for_name("Foreign"), None);
        // Retired / unknown site names get no id rather than a wrong one.
        assert_eq!(category_id_for_name("HD"), None);
        assert_eq!(category_id_for_name(""), None);
    }

    // -- Description parsing -------------------------------------------------

    #[test]
    fn the_seeder_grammar_matches_sonarrs_regexes() {
        let description = "Category: Episodes HD - Seeders: 1 - Leechers: 7";
        assert_eq!(parse_labelled_count(description, "seeder"), Some(1));
        assert_eq!(parse_labelled_count(description, "leecher"), Some(7));
        assert_eq!(parse_labelled_count(description, "peer"), None);

        // The unlabelled alternative, `(?<value>\d+)\s+(seeder)s?`.
        assert_eq!(
            parse_labelled_count("12 seeders, 3 leechers", "seeder"),
            Some(12)
        );
        assert_eq!(
            parse_labelled_count("12 seeders, 3 leechers", "leecher"),
            Some(3)
        );
        // Singular label, and case-insensitivity.
        assert_eq!(parse_labelled_count("SEEDER: 4", "seeder"), Some(4));
        assert_eq!(parse_labelled_count("1 Seeder", "seeder"), Some(1));
        // `\s+` after the colon is mandatory in Sonarr's regex.
        assert_eq!(parse_labelled_count("Seeders:9", "seeder"), None);
        assert_eq!(parse_labelled_count("nothing here", "seeder"), None);
    }

    /// Sonarr's fallbacks: seeders = peers − leechers, peers = seeders +
    /// leechers.
    #[test]
    fn peers_and_seeders_fall_back_to_each_other() {
        let config = test_config();
        let feed = r#"<rss><channel>
            <item><title>A</title><link>http://x/a.torrent</link>
              <description>Peers: 10 - Leechers: 4</description></item>
            <item><title>B</title><link>http://x/b.torrent</link>
              <description>Seeders: 5 - Leechers: 2</description></item>
            <item><title>C</title><link>http://x/c.torrent</link>
              <description>no counts at all</description></item>
        </channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results[0].peers, Some(10));
        assert_eq!(results[0].seeders, Some(6));
        assert_eq!(results[1].seeders, Some(5));
        assert_eq!(results[1].peers, Some(7));
        assert_eq!(results[2].seeders, None);
        assert_eq!(results[2].peers, None);
        assert_eq!(results[2].leechers, None);
    }

    /// Sonarr does not set `ParseSizeInDescription` for TorrentLeech and the
    /// feed carries no size, so nothing must be invented from the category
    /// names or the seeder counts.
    #[test]
    fn no_size_is_invented_from_a_torrentleech_description() {
        for description in [
            "Category: Episodes HD - Seeders: 1 - Leechers: 7",
            "Category: 4KUpscaled - Seeders: 5 - Leechers: 0",
            "Category: Real4K - Seeders: 12 - Leechers: 3",
        ] {
            assert_eq!(
                parse_size_in_description(description),
                None,
                "{description}"
            );
        }
    }

    #[test]
    fn a_size_is_read_if_the_feed_ever_publishes_one() {
        assert_eq!(
            parse_size_in_description("Category: Episodes HD Size: 1.37 GB - Seeders: 1"),
            Some(1_471_026_299)
        );
        assert_eq!(
            parse_size_in_description("556 MB; Episodes"),
            Some(583_008_256)
        );
        assert_eq!(parse_size_in_description("1,024 KB"), Some(1_048_576));
        assert_eq!(parse_size_in_description("2 GiB"), Some(2_147_483_648));
        assert_eq!(parse_size_in_description("12345"), Some(12_345));
        // `(?![\w/])`: a rate is not a size.
        assert_eq!(parse_size_in_description("1.5 GB/s"), None);
        // `(?<!\.\d*)`: the `5` of `0.5` never starts a size.
        assert_eq!(parse_size_in_description("0.5 x 4 GB"), Some(4_294_967_296));
    }

    #[test]
    fn the_category_falls_back_to_the_description_when_the_element_is_absent() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>No category element</title>
            <link>http://x/a.torrent</link>
            <description>Category: Episodes HD - Seeders: 1 - Leechers: 7</description>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results[0].categories, vec!["Episodes HD".to_string()]);
        assert_eq!(
            results[0].provider_extra.get("category_id"),
            Some(&serde_json::Value::from(32_i64))
        );
    }

    #[test]
    fn the_category_element_wins_over_the_description() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Both</title>
            <link>http://x/a.torrent</link>
            <category>Anime</category>
            <description>Category: Episodes - Seeders: 1</description>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results[0].categories, vec!["Anime".to_string()]);
    }

    // -- Dates (L1) ----------------------------------------------------------

    #[test]
    fn rfc_2822_pub_dates_become_rfc_3339_utc() {
        let cases = [
            ("Mon, 12 May 2014 19:15:28 +0000", "2014-05-12T19:15:28Z"),
            ("Mon, 12 May 2014 19:06:59 +0000", "2014-05-12T19:06:59Z"),
            ("12 May 2014 19:15:28 +0000", "2014-05-12T19:15:28Z"),
            ("Tue, 24 Aug 2021 22:18:46 -0000", "2021-08-24T22:18:46Z"),
            ("Wed, 01 Jan 2025 00:30:00 +0530", "2024-12-31T19:00:00Z"),
            ("Sat, 29 Feb 2020 12:00:00 GMT", "2020-02-29T12:00:00Z"),
            ("Fri, 31 Dec 1999 23:59:59 -0500", "2000-01-01T04:59:59Z"),
            ("Mon, 12 May 14 19:15:28 +0000", "2014-05-12T19:15:28Z"),
            ("Mon, 12 May 2014 19:15 +0000", "2014-05-12T19:15:00Z"),
            ("Mon, 12 May 2014 15:15:28 EDT", "2014-05-12T19:15:28Z"),
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
            rfc2822_to_rfc3339_utc("2014-05-12T19:15:28Z").as_deref(),
            Some("2014-05-12T19:15:28Z")
        );
    }

    #[test]
    fn an_unparseable_pub_date_is_none_rather_than_a_value_the_core_drops() {
        for raw in ["", "   ", "not a date", "Mon, 32 Foo 2014 19:15:28 +0000"] {
            assert_eq!(rfc2822_to_rfc3339_utc(raw), None, "{raw}");
        }
    }

    /// Sonarr throws `UnsupportedFeedException` and fails the whole feed when a
    /// `pubDate` is missing or unparseable; losing a grabbable release over a
    /// bad date is the worse outcome.
    #[test]
    fn an_unparseable_pub_date_keeps_the_release() {
        let config = test_config();
        let feed = r#"<rss><channel><item>
            <title>Undated</title>
            <link>http://x/a.torrent</link>
            <pubDate>whenever</pubDate>
        </item></channel></rss>"#;
        let items = parse_feed(feed).expect("parses");
        let results = build_results(&items, &config);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].published_at, None);
    }

    #[test]
    fn the_civil_calendar_round_trips() {
        for days in [-25_567_i64, -1, 0, 1, 16_301, 19_000, 25_567] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }
}
