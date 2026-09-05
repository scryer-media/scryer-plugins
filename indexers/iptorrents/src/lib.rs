//! IPTorrents (`iptorrents.com`) RSS indexer.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Indexers/IPTorrents`
//! (`IPTorrents.cs`, `IPTorrentsRequestGenerator.cs`, `IPTorrentsSettings.cs`),
//! Sonarr's `RssParser`/`TorrentRssParser`, its `IPTorrentsFixture.cs` and the
//! `Files/Indexers/IPTorrents/IPTorrents.xml` fixture, plus Prowlarr's
//! `Indexers/Definitions/IPTorrents.cs` for the site's current category table
//! and hit-and-run rules.
//!
//! Shape of the integration:
//!
//! * IPTorrents publishes **one** personalised RSS feed per account
//!   (`https://iptorrents.com/t.rss?u=UID;tp=PASSKEY;<cat>;…;download`, or the
//!   older `…/torrents/rss?…`). There is no query parameter, so the feed is a
//!   recent list and nothing else — exactly why Sonarr declares
//!   `SupportsSearch => false` and answers every search-criteria overload with
//!   an empty request chain (`IPTorrentsRequestGenerator.cs:20-53`).
//! * The plugin therefore serves the recent/RSS poll only. A request that
//!   carries search criteria is answered with an empty response **without**
//!   spending an upstream call, which is Sonarr's behaviour and also protects a
//!   Cloudflare-fronted private tracker from pointless traffic.
//! * The fetch, the delivery classification, the XML parse and the result
//!   assembly are all done in-plugin rather than through
//!   `rss-indexer-common::execute_rss_urls`, because that helper cannot report
//!   a typed error (every failure becomes `Temporary`) and it post-filters
//!   parsed releases against the request, which Sonarr never does. See the
//!   README and the reconciliation report.

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

const PROVIDER_ID: &str = "iptorrents";
const USER_AGENT: &str = concat!("scryer-iptorrents-indexer/", env!("CARGO_PKG_VERSION"));
/// Sonarr's `HttpIndexerBase.RateLimit` for every indexer.
const REQUEST_INTERVAL_MS: u64 = 2_000;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Sonarr's `minimumBackoff` when a rate-limited response carries no
/// `Retry-After` (`HttpIndexerBase.FetchReleases`).
const RATE_LIMITED_FALLBACK_SECONDS: i64 = 3_600;

/// IPTorrents' published hit-and-run rule: seed to a 1.0 ratio **or** for 336
/// hours, whichever comes first. Prowlarr hard-codes exactly these two values
/// for every IPTorrents release (`Indexers/Definitions/IPTorrents.cs`:
/// `MinimumRatio = 1`, `MinimumSeedTime = 1209600` seconds).
const SITE_MINIMUM_SEED_RATIO: f64 = 1.0;
const SITE_MINIMUM_SEED_TIME_MINUTES: i64 = 20_160;

// ---------------------------------------------------------------------------
// Category table
// ---------------------------------------------------------------------------

/// IPTorrents' category ids and the names the site shows for them, taken from
/// Prowlarr's `SetCapabilities()` mapping (the current first-party integration).
///
/// The RSS feed does not carry a `<category>` element; it carries the site's
/// category **name** inside `<description>` (`Category: TV/x264 Size: 1.37 GB`),
/// which is why the name is what the plugin matches on. The id is what an
/// operator puts in the feed URL, so both are reported.
///
/// The facet column is derived from Prowlarr's newznab mapping: `Movie*` →
/// `movie`, `TV*`/`Sports` → `series`, `Anime` → `anime`, everything else has
/// no Scryer facet.
const CATEGORIES: &[(i64, &str, &str)] = &[
    (72, "Movies", "movie"),
    (87, "Movie/3D", "movie"),
    (77, "Movie/480p", "movie"),
    (101, "Movie/4K", "movie"),
    (89, "Movie/BD-R", "movie"),
    (90, "Movie/BD-Rip", "movie"),
    (96, "Movie/Cam", "movie"),
    (6, "Movie/DVD-R", "movie"),
    (48, "Movie/HD/Bluray", "movie"),
    (54, "Movie/Kids", "movie"),
    (62, "Movie/MP4", "movie"),
    (38, "Movie/Non-English", "movie"),
    (68, "Movie/Packs", "movie"),
    (20, "Movie/Web-DL", "movie"),
    (7, "Movie/Xvid", "movie"),
    (100, "Movie/x265", "movie"),
    (73, "TV", "series"),
    (26, "TV/Documentaries", "series"),
    (55, "Sports", "series"),
    (78, "TV/480p", "series"),
    (23, "TV/BD", "series"),
    (24, "TV/DVD-R", "series"),
    (25, "TV/DVD-Rip", "series"),
    (66, "TV/Mobile", "series"),
    (82, "TV/Non-English", "series"),
    (65, "TV/Packs", "series"),
    (83, "TV/Packs/Non-English", "series"),
    (79, "TV/SD/x264", "series"),
    (22, "TV/Web-DL", "series"),
    (5, "TV/x264", "series"),
    (99, "TV/x265", "series"),
    (4, "TV/Xvid", "series"),
    (60, "Anime", "anime"),
    (74, "Games", ""),
    (2, "Games/Mixed", ""),
    (47, "Games/Nintendo DS", ""),
    (43, "Games/PC-ISO", ""),
    (45, "Games/PC-Rip", ""),
    (71, "Games/PS3", ""),
    (50, "Games/Wii", ""),
    (44, "Games/Xbox-360", ""),
    (75, "Music", ""),
    (3, "Music/Audio", ""),
    (80, "Music/Flac", ""),
    (93, "Music/Packs", ""),
    (37, "Music/Video", ""),
    (21, "Podcast", ""),
    (76, "Other/Miscellaneous", ""),
    (1, "Appz", ""),
    (86, "Appz/Non-English", ""),
    (69, "Appz/Mac", ""),
    (58, "Appz/Mobile", ""),
    (64, "AudioBook", ""),
    (35, "Books", ""),
    (102, "Books/Non-English", ""),
    (94, "Books/Comics", ""),
    (95, "Books/Educational", ""),
    (92, "Books/Magazines & Newspapers", ""),
    (98, "Other/Fonts", ""),
    (36, "Other/Pics/Wallpapers", ""),
    (88, "XXX", ""),
    (85, "XXX/Magazines", ""),
    (8, "XXX/Movie", ""),
    (81, "XXX/Movie/0Day", ""),
    (91, "XXX/Packs", ""),
    (84, "XXX/Pics/Wallpapers", ""),
];

fn category_id_for_name(name: &str) -> Option<i64> {
    let name = name.trim();
    CATEGORIES
        .iter()
        .find(|(_, label, _)| label.eq_ignore_ascii_case(name))
        .map(|(id, _, _)| *id)
}

fn category_descriptors() -> Vec<IndexerCategoryDescriptor> {
    CATEGORIES
        .iter()
        .map(|(id, name, facet)| IndexerCategoryDescriptor {
            value: (*name).to_string(),
            label: Some(format!("{name} (IPTorrents category {id})")),
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
        name: "IPTorrents Indexer".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Indexer(IndexerDescriptor {
            provider_type: PROVIDER_ID.to_string(),
            provider_aliases: vec!["ip-torrents".to_string()],
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
                    // IPTorrents files anime under its own `Anime` category
                    // rather than inside the TV tree.
                    separate_anime_categories: true,
                    provider_category_metadata: true,
                    categories: category_descriptors(),
                }),
                limits: Some(IndexerLimitCapabilities {
                    // IPTorrents does not publish how many items `t.rss`
                    // returns and the endpoint takes no paging parameter, so
                    // the honest answer is "one page of unknown length".
                    page_size: None,
                    max_page_size: None,
                    max_pages: Some(1),
                    rate_limit_hint_seconds: Some(2),
                    api_quota_supported: false,
                    grab_quota_supported: false,
                }),
                torrent: Some(IndexerTorrentCapabilities {
                    // Sonarr's fixture asserts null for every one of these: the
                    // feed carries a title, a link, a pubDate and a
                    // description, and nothing else.
                    reports_seeders: false,
                    reports_peers: false,
                    reports_leechers: false,
                    reports_info_hash: false,
                    reports_magnet_uri: false,
                    reports_volume_factors: false,
                    supports_private_tracker_flags: true,
                    // The plugin reports IPTorrents' site-wide hit-and-run
                    // minimums on every release (see `SITE_MINIMUM_*`).
                    supports_seed_requirements: true,
                }),
                response_features: Some(IndexerResponseFeatures {
                    languages: false,
                    subtitles: false,
                    grabs: false,
                    votes: false,
                    comments: false,
                    // The feed's `<link>` *is* the download URL; there is no
                    // separate details page. Sonarr's fixture asserts an empty
                    // `InfoUrl` and `CommentUrl`.
                    info_url: false,
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
            "feed_url",
            "Feed URL",
            true,
            None,
            Some(
                "IPTorrents direct-download RSS URL. Take it from the site's RSS page with \
                 'Download' ticked, e.g. \
                 https://iptorrents.com/t.rss?u=USERID;tp=PASSKEY;5;22;65;download",
            ),
        ),
        field(
            "minimum_seeders",
            "Minimum Seeders",
            ConfigFieldType::Number,
            false,
            Some("1"),
            Some(
                "Minimum seeders preference for host-side release decisions. The IPTorrents feed \
                 reports no seeder counts, so Scryer treats every release from it as 'unknown' \
                 and never withholds one on this setting.",
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
                "Optional raw Cookie header. The RSS feed authenticates with the u=/tp= values in \
                 the URL; a cookie is only needed when a Cloudflare clearance cookie has to be \
                 supplied by hand.",
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
    let config = IpTorrentsConfig::from_host()?;

    // Sonarr's `IPTorrentsRequestGenerator` answers every `GetSearchRequests`
    // overload with an empty `IndexerPageableRequestChain`
    // (`IPTorrentsRequestGenerator.cs:20-53`): the feed has no query parameter,
    // so a search cannot be narrowed and issuing it would just re-fetch the
    // recent list. Answer empty without spending the upstream call.
    if !is_recent_request(&request) {
        return Ok(SearchResponse::default());
    }

    let body = fetch_feed(&config).await?;
    let results = parse_feed(&body, &config.feed_url)?;
    let mut results = dedupe_results(results);
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
/// (`crates/scryer-plugins/src/indexer_adapter.rs`), far above any real feed
/// length; it is honoured rather than ignored so a host that ever sends a
/// smaller value is respected.
fn result_limit(request: &SearchRequest) -> Option<usize> {
    (request.limit > 0).then_some(request.limit)
}

// ---------------------------------------------------------------------------
// Transport and delivery classification
// ---------------------------------------------------------------------------

async fn fetch_feed(config: &IpTorrentsConfig) -> Result<String, Error> {
    StartRateGate::new(
        format!("{PROVIDER_ID}.request-start"),
        1,
        REQUEST_INTERVAL_MS,
    )
    .acquire()
    .await
    .map_err(component::deadline_deferred_error)?;

    let logged_url = redact_feed_url(&config.feed_url);
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
            "IPTorrents could not be reached".to_string(),
            format!("IPTorrents request to {logged_url} failed: {error:?}"),
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
/// IPTorrents has no documented status table for `t.rss`. What it does in
/// practice, and what the classification below encodes:
///
/// * a wrong or revoked `u=`/`tp=` pair, and a Cloudflare interstitial, both
///   arrive as an **HTML page** — sometimes with a 200, sometimes with a 403;
/// * a plain 401/403 with no HTML body is a credential rejection;
/// * a 429 is a rate limit; everything else non-200 is an upstream failure.
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
                "feed_url",
                format!(
                    "IPTorrents redirected the feed with HTTP {status} to {location}; the \
                     configured feed URL is not the RSS endpoint (the host does not follow \
                     redirects)"
                ),
            ));
        }
        401 | 403 => {
            if is_html_delivery(headers, body) {
                return Err(invalid_response_error(
                    IndexerSearchInvalidResponseKind::UnexpectedContentType,
                    format!(
                        "IPTorrents answered HTTP {status} with an HTML page: the site is likely \
                         blocking Scryer (Cloudflare) or the feed credentials in 'feed_url' are \
                         no longer valid: {}",
                        body_excerpt(body)
                    ),
                ));
            }
            return Err(auth_failed_error(format!(
                "IPTorrents rejected the feed credentials with HTTP {status}: {}",
                body_excerpt(body)
            )));
        }
        429 => return Err(rate_limited_error(retry_after_seconds(headers))),
        _ => {
            return Err(deferred_error(
                IndexerSearchIncompleteReason::UpstreamFailure,
                None,
                format!("IPTorrents returned HTTP {status}"),
                format!("IPTorrents returned HTTP {status}: {}", body_excerpt(body)),
            ));
        }
    }

    if body.len() > MAX_RESPONSE_BYTES {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::TruncatedBody,
            format!(
                "IPTorrents returned {} bytes, above the {MAX_RESPONSE_BYTES} byte ceiling",
                body.len()
            ),
        ));
    }

    let text = std::str::from_utf8(body).map_err(|error| {
        invalid_response_error(
            IndexerSearchInvalidResponseKind::MalformedBody,
            format!("IPTorrents feed was not valid UTF-8: {error}"),
        )
    })?;

    if is_html_delivery(headers, body) {
        return Err(invalid_response_error(
            IndexerSearchInvalidResponseKind::UnexpectedContentType,
            format!(
                "IPTorrents returned content type {:?} instead of RSS: the site is likely blocking \
                 Scryer (Cloudflare) or the feed credentials in 'feed_url' are no longer valid: {}",
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

/// The feed URL *is* the credential, so it must never reach a log verbatim.
/// IPTorrents separates its query members with `;` rather than `&`, so both
/// separators are honoured.
fn redact_feed_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    out.push('?');
    let mut separators = query.match_indices([';', '&']).map(|(_, sep)| sep);
    let mut first = true;
    for pair in query.split([';', '&']) {
        if !first {
            out.push_str(separators.next().unwrap_or(";"));
        }
        first = false;
        match pair.split_once('=') {
            Some((key, _)) if is_secret_param(key) => {
                out.push_str(key);
                out.push_str("=REDACTED");
            }
            _ => out.push_str(pair),
        }
    }
    out
}

fn is_secret_param(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "u" | "tp"
            | "torrent_pass"
            | "passkey"
            | "apikey"
            | "api_key"
            | "key"
            | "pass"
            | "password"
    )
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
/// surface it rather than cool the indexer down for a typo. That is finding H1
/// — a mis-typed feed URL used to arrive as `Temporary`.
fn invalid_config_error(field: &str, detail: String) -> Error {
    typed_error(
        PluginErrorCode::InvalidConfig,
        format!("IPTorrents setting '{field}' is not usable"),
        detail,
        None,
        None,
    )
}

fn auth_failed_error(detail: String) -> Error {
    typed_error(
        PluginErrorCode::AuthFailed,
        "IPTorrents rejected the credentials in the configured 'feed_url'".to_string(),
        detail,
        None,
        None,
    )
}

fn rate_limited_error(retry_after_seconds: i64) -> Error {
    typed_error(
        PluginErrorCode::RateLimited,
        "IPTorrents is rate limiting Scryer".to_string(),
        format!("IPTorrents returned HTTP 429; retrying after {retry_after_seconds}s"),
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
        "IPTorrents returned a response Scryer could not read".to_string(),
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
    description: Option<String>,
    published_at: Option<String>,
    categories: Vec<String>,
    /// The `<category>` currently being assembled; flushed into `categories`
    /// when the element closes, so several `<category>` elements stay separate.
    category_buffer: Option<String>,
    enclosure_url: Option<String>,
    enclosure_length: Option<i64>,
}

/// Parse one RSS 2.0 document into releases.
///
/// Sonarr's `RssParser.GetItems` walks `rss > channel > item`; a document
/// without a `channel` yields nothing. Here a document with no `channel` at all
/// is reported as `InvalidResponse(InvalidRoot)` rather than silently returning
/// zero releases, because an empty feed and a wrong endpoint are different
/// operator problems. An **empty** `channel` is a legitimate quiet feed.
fn parse_feed(body: &str, feed_url: &str) -> Result<Vec<SearchResult>, Error> {
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
            // `GetTitle` runs `WebUtility.HtmlDecode` over the result — the
            // fixture's `Rosemary&#39;s Baby` only parses correctly with this.
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
                        "IPTorrents feed is not well-formed XML at byte {}: {error}",
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
            "IPTorrents response has no RSS <channel> element; the configured 'feed_url' is not \
             an RSS feed"
                .to_string(),
        ));
    }

    Ok(items
        .into_iter()
        .filter_map(|item| build_result(item, feed_url))
        .collect())
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

/// Sonarr's `IsValidRelease` (`HttpIndexerBase.cs`): a release with no title or
/// no download URL is dropped rather than surfaced.
fn build_result(item: FeedItem, feed_url: &str) -> Option<SearchResult> {
    let title = item.title.as_deref().map(str::trim).unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    // `TorrentRssParser` prefers a torrent enclosure and falls back to `<link>`;
    // the IPTorrents feed only ever carries `<link>`.
    let download_url = item
        .enclosure_url
        .as_deref()
        .or(item.link.as_deref())
        .and_then(|value| resolve_url(feed_url, value))?;
    let magnet_url = download_url
        .starts_with("magnet:?")
        .then(|| download_url.clone());

    let description = item
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let size_bytes = item
        .enclosure_length
        .filter(|value| *value > 0)
        .or_else(|| parse_size_in_description(description));

    let category = parse_category_in_description(description);
    let mut categories = item.categories.clone();
    if let Some(category) = category.as_deref()
        && !categories.iter().any(|value| value == category)
    {
        categories.insert(0, category.to_string());
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
    if let Some(category) = category.as_deref() {
        provider_extra.insert("category".to_string(), serde_json::Value::from(category));
        if let Some(id) = category_id_for_name(category) {
            provider_extra.insert("category_id".to_string(), serde_json::Value::from(id));
        }
    }
    if let Some(size) = size_bytes {
        provider_extra.insert("reported_size".to_string(), serde_json::Value::from(size));
    }

    Some(SearchResult {
        link: Some(download_url.clone()),
        // The `<link>` is the download URL, so there is no details page and no
        // comment page — Sonarr's fixture asserts both are empty.
        info_url: None,
        comment_url: None,
        guid: Some(stable_guid(&download_url)),
        size_bytes,
        published_at: item
            .published_at
            .as_deref()
            .and_then(rfc2822_to_rfc3339_utc),
        // IPTorrents' published hit-and-run rule, the same pair Prowlarr sets on
        // every IPTorrents release. Scryer's seeding gate treats these as a
        // floor under a seeding profile that honours tracker minimums
        // (`crates/scryer-application/src/acquisition/seed_goals.rs`), so a
        // release grabbed here is not removed before the tracker is satisfied.
        minimum_seed_ratio: Some(SITE_MINIMUM_SEED_RATIO),
        minimum_seed_time_minutes: Some(SITE_MINIMUM_SEED_TIME_MINUTES),
        magnet_url,
        categories: categories.clone(),
        provider_categories: categories,
        provider_extra,
        ..torrent_result(title, Some(download_url))
    })
}

/// A guid the host can dedupe and remember a release by.
///
/// The feed carries no `<guid>`, and Sonarr fills that gap with
/// `Guid.NewGuid()` — a value that changes on every poll, so the same release
/// never looks like itself twice. The download URL is stable and unique per
/// torrent, so it is used instead, with the account's secrets stripped: a guid
/// is persisted and shown in the UI, and `torrent_pass`/`tp` are credentials.
fn stable_guid(download_url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(download_url) else {
        return download_url.to_string();
    };
    let filtered = parsed
        .query_pairs()
        .filter(|(key, _)| !is_secret_param(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        parsed.set_query(None);
    } else {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in filtered {
            serializer.append_pair(&key, &value);
        }
        parsed.set_query(Some(&serializer.finish()));
    }
    parsed.to_string()
}

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

/// Sonarr's `RssParser.ParseSize(value, defaultToBinaryPrefix: true)`, which is
/// what `ParseSizeInDescription = true` runs over `<description>`.
///
/// Regex, for reference:
/// `(?<value>(?<!\.\d*)(?:\d+,)*\d+(?:\.\d{1,3})?)\W?(?<unit>[KMG]i?B)(?![\w/])`
/// with the whole string short-circuiting to `long.Parse` when it is all
/// digits. Rust's `regex` crate has no look-around, so this is the same grammar
/// written as a leftmost-match scanner.
///
/// Both fixture shapes must parse: `Category: TV/x264 Size: 1.37 GB ` and
/// `556 MB; TV/x264`.
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

/// IPTorrents puts the site's category name in the description, in one of two
/// shapes seen in Sonarr's fixture: `Category: TV/x264 Size: 1.37 GB` and
/// `556 MB; TV/x264`. Sonarr throws the whole description away; the category is
/// real provider metadata, so Scryer reports it.
fn parse_category_in_description(description: &str) -> Option<String> {
    let text = description.trim();
    if text.is_empty() {
        return None;
    }

    let lowered = text.to_ascii_lowercase();
    if let Some(offset) = lowered.find("category:") {
        let tail = &text[offset + "category:".len()..];
        let end = tail
            .to_ascii_lowercase()
            .find("size:")
            .unwrap_or(tail.len());
        let candidate = tail[..end].trim().trim_end_matches([';', ',']).trim();
        if !candidate.is_empty() {
            return Some(candidate.to_string());
        }
    }

    // No labelled category: the description is a `;`-separated list in which
    // exactly one member is the size.
    text.split([';', '|'])
        .map(str::trim)
        .find(|part| !part.is_empty() && parse_size_in_description(part).is_none())
        .map(ToString::to_string)
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// RSS `pubDate` is RFC 2822 (`Mon, 12 May 2014 19:06:34 +0000`).
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

    // Drop the optional `Wed, ` day-of-week prefix.
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
struct IpTorrentsConfig {
    feed_url: String,
    user_agent: String,
    cookie: Option<String>,
    username: Option<String>,
    password: Option<String>,
    additional_headers: String,
}

impl IpTorrentsConfig {
    fn from_host() -> Result<Self, Error> {
        Ok(Self {
            feed_url: validate_feed_url(config_value("feed_url").as_deref().unwrap_or_default())?,
            user_agent: config_value("user_agent").unwrap_or_else(|| USER_AGENT.to_string()),
            cookie: config_value("cookie"),
            username: config_value("username"),
            password: config_value("password"),
            additional_headers: config_value("additional_headers").unwrap_or_default(),
        })
    }

    fn request_headers(&self) -> BTreeMap<String, String> {
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

/// `IPTorrentsSettingsValidator` (`IPTorrentsSettings.cs:17-23`), as a typed
/// configuration error rather than an untyped failure:
///
/// 1. `ValidRootUrl()` — a parseable http(s) URL;
/// 2. `Matches(@"(?:/|t\.)rss\?.+$")` — it must be an RSS endpoint;
/// 3. `Matches(@"(?:/|t\.)rss\?.+;download(?:;|$)")` — "Use Direct Download Url
///    (;download)", because without that flag the feed's `<link>` points at the
///    details page and nothing is importable.
///
/// Both shapes the site has used are accepted: the old
/// `https://iptorrents.com/torrents/rss?…` and the current
/// `https://iptorrents.com/t.rss?…`.
///
/// One deliberate leniency over Sonarr: the markers are matched
/// case-insensitively, so a hand-typed `T.RSS?…;DOWNLOAD` is accepted instead
/// of being rejected as "not in the correct format".
fn validate_feed_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid_config_error(
            "feed_url",
            "IPTorrents requires the account's RSS feed URL".to_string(),
        ));
    }

    let parsed = url::Url::parse(trimmed).map_err(|error| {
        invalid_config_error(
            "feed_url",
            format!("'{}' is not a valid URL: {error}", redact_feed_url(trimmed)),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid_config_error(
            "feed_url",
            format!(
                "'{}' must be an http(s) URL with a host",
                redact_feed_url(trimmed)
            ),
        ));
    }

    let Some(query) = rss_query(trimmed) else {
        return Err(invalid_config_error(
            "feed_url",
            "the IPTorrents feed URL must be an RSS endpoint — 'https://iptorrents.com/t.rss?…' \
             (or the older 'https://iptorrents.com/torrents/rss?…'). Copy it from the site's RSS \
             page rather than from the browser address bar."
                .to_string(),
        ));
    };

    if !has_download_flag(query) {
        return Err(invalid_config_error(
            "feed_url",
            "the IPTorrents feed URL must be the direct-download form: tick 'Download' on the \
             site's RSS page so the URL ends with ';download'. Without it the feed links to the \
             details page and Scryer cannot fetch the torrent."
                .to_string(),
        ));
    }

    Ok(trimmed.to_string())
}

/// The `(?:/|t\.)rss\?.+` half of Sonarr's regex: returns the non-empty query
/// that follows an `…/rss?` or `…t.rss?` marker.
fn rss_query(url: &str) -> Option<&str> {
    let lowered = url.to_ascii_lowercase();
    let mut from = 0;
    while let Some(offset) = lowered[from..].find("rss?") {
        let marker = from + offset;
        let query_start = marker + "rss?".len();
        let prefix = &lowered[..marker];
        if (prefix.ends_with('/') || prefix.ends_with("t.")) && query_start < url.len() {
            return Some(&url[query_start..]);
        }
        from = marker + 1;
    }
    None
}

/// The `.+;download(?:;|$)` half: a `;download` member with at least one
/// character of query before it, terminated by `;` or the end of the URL.
fn has_download_flag(query: &str) -> bool {
    let lowered = query.to_ascii_lowercase();
    let mut from = 0;
    while let Some(offset) = lowered[from..].find(";download") {
        let start = from + offset;
        let end = start + ";download".len();
        let terminated = end == lowered.len() || lowered.as_bytes()[end] == b';';
        if start > 0 && terminated {
            return true;
        }
        from = start + 1;
    }
    false
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

    /// Sonarr's `Files/Indexers/IPTorrents/IPTorrents.xml`.
    ///
    /// Two deliberate edits: the leading UTF-8 BOM is dropped (the fixture is
    /// embedded as a `str`, not read as bytes) and the four repeated
    /// `download.php/1234/` ids are given distinct values. Sonarr reuses the
    /// same id for every entry, which is fine when the guid is a fresh
    /// `Guid.NewGuid()` but would collapse under this plugin's stable,
    /// URL-derived guid; `duplicate_entries_are_deduped_by_guid` covers the
    /// collision case explicitly. Every asserted value below is Sonarr's.
    const RECENT_FEED: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<rss version="2.0">
  <channel>
    <item>
      <title>24 S03E12 720p WEBRip h264-DRAWER</title>
      <link>http://iptorrents.com/download.php/1234/24.S03E12.720p.WEBRip.h264-DRAWER.torrent?torrent_pass=abcd</link>
      <pubDate>Mon, 12 May 2014 19:06:34 +0000</pubDate>
      <description>Category: TV/x264 Size: 1.37 GB </description>
    </item>
    <item>
      <title>Rosemary&#39;s Baby S01E01 Part 1 1080p WEB-DL DD5 1 H 264-BS</title>
      <link>http://iptorrents.com/download.php/1235/Rosemary&#39;s.Baby.S01E01.Part.1.1080p.WEB-DL.DD5.1.H.264-BS.torrent?torrent_pass=abcd</link>
      <pubDate>Mon, 12 May 2014 19:06:25 +0000</pubDate>
      <description>556 MB; TV/x264</description>
    </item>
    <item>
      <title>Rosemary&#39;s Baby S01E01 Part 1 720p WEB-DL DD5 1 H 264-BS</title>
      <link>http://iptorrents.com/download.php/1236/Rosemary&#39;s.Baby.S01E01.Part.1.720p.WEB-DL.DD5.1.H.264-BS.torrent?torrent_pass=abcd</link>
      <pubDate>Mon, 12 May 2014 19:04:09 +0000</pubDate>
      <description>Category: TV/x264 Size: 2.65 GB </description>
    </item>
    <item>
      <title>24 S03E11 720p WEBRip h264-DRAWER</title>
      <link>http://iptorrents.com/download.php/1237/24.S03E11.720p.WEBRip.h264-DRAWER.torrent?torrent_pass=abcd</link>
      <pubDate>Mon, 12 May 2014 19:02:54 +0000</pubDate>
      <description>Category: TV/x264 Size: 1.33 GB </description>
    </item>
    <item>
      <title>Da Vincis Demons S02E08 1080p WEB-DL DD5 1 H 264-BS</title>
      <link>http://iptorrents.com/download.php/1238/Da.Vincis.Demons.S02E08.1080p.WEB-DL.DD5.1.H.264-BS.torrent?torrent_pass=abcd</link>
      <pubDate>Mon, 12 May 2014 19:02:11 +0000</pubDate>
      <description>Category: TV/x264 Size: 1.92 GB </description>
    </item>
  </channel>
</rss>"#;

    /// Sonarr's `GivenNewFeedFormat()`.
    const NEW_FEED_URL: &str =
        "https://iptorrents.com/t.rss?u=USERID;tp=APIKEY;3;80;93;37;download";
    /// Sonarr's `GivenOldFeedFormat()`.
    const OLD_FEED_URL: &str =
        "https://iptorrents.com/torrents/rss?u=snip;tp=snip;3;80;93;37;download";

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

    fn parse_fixture() -> Vec<SearchResult> {
        parse_feed(RECENT_FEED, NEW_FEED_URL).expect("fixture parses")
    }

    // -- H1: feed URL validation ------------------------------------------

    #[test]
    fn should_validate_old_feed_format() {
        assert_eq!(validate_feed_url(OLD_FEED_URL).unwrap(), OLD_FEED_URL);
    }

    #[test]
    fn should_validate_new_feed_format() {
        assert_eq!(validate_feed_url(NEW_FEED_URL).unwrap(), NEW_FEED_URL);
    }

    #[test]
    fn should_not_validate_bad_format() {
        // Sonarr's default fixture settings: `BaseUrl = "http://fake.com/"`.
        let error = plugin_error(&validate_feed_url("http://fake.com/").unwrap_err());
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.public_message.contains("feed_url"));
        assert!(error.debug_message.unwrap().contains("t.rss"));
    }

    #[test]
    fn should_not_validate_no_download_format() {
        let error = plugin_error(
            &validate_feed_url("https://iptorrents.com/t.rss?u=USERID;tp=APIKEY;3;80;93;37")
                .unwrap_err(),
        );
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.debug_message.unwrap().contains(";download"));
    }

    #[test]
    fn a_missing_feed_url_is_a_typed_config_error_not_a_temporary_failure() {
        let error = plugin_error(&validate_feed_url("   ").unwrap_err());
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.details.is_none(), "a config fault must not defer");
    }

    #[test]
    fn a_non_http_feed_url_is_rejected() {
        let error =
            plugin_error(&validate_feed_url("ftp://iptorrents.com/t.rss?a;download").unwrap_err());
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    }

    #[test]
    fn the_download_flag_must_be_a_whole_member() {
        assert!(!has_download_flag("u=1;tp=2;downloads"));
        assert!(has_download_flag("u=1;tp=2;download"));
        assert!(has_download_flag("u=1;tp=2;download;5"));
        // `.+` before `;download`: the flag alone is not a feed.
        assert!(!has_download_flag(";download"));
    }

    #[test]
    fn the_rss_marker_must_follow_a_slash_or_a_dot() {
        assert!(rss_query("https://iptorrents.com/t.rss?a;download").is_some());
        assert!(rss_query("https://iptorrents.com/torrents/rss?a;download").is_some());
        // Sonarr's `(?:/|t\.)` prefix: a bare `…foorss?` is not an RSS endpoint.
        assert!(rss_query("https://iptorrents.com/foorss?a;download").is_none());
        // `.+` after the `?`.
        assert!(rss_query("https://iptorrents.com/t.rss?").is_none());
    }

    #[test]
    fn the_feed_url_markers_are_matched_case_insensitively() {
        assert!(
            validate_feed_url("https://iptorrents.com/T.RSS?u=USERID;tp=APIKEY;5;DOWNLOAD").is_ok()
        );
    }

    // -- H2: parsing parity, pinned on Sonarr's fixture --------------------

    #[test]
    fn should_parse_recent_feed_from_ip_torrents() {
        let releases = parse_fixture();
        assert_eq!(releases.len(), 5);

        let first = &releases[0];
        assert_eq!(first.title, "24 S03E12 720p WEBRip h264-DRAWER");
        assert_eq!(first.protocol, Some(IndexerProtocol::Torrent));
        assert_eq!(first.source_kind, Some(IndexerSourceKind::Torrent));
        assert_eq!(
            first.download_url.as_deref(),
            Some(
                "http://iptorrents.com/download.php/1234/24.S03E12.720p.WEBRip.h264-DRAWER.torrent?torrent_pass=abcd"
            )
        );
        assert_eq!(first.info_url, None);
        assert_eq!(first.comment_url, None);
        assert_eq!(first.published_at.as_deref(), Some("2014-05-12T19:06:34Z"));
        assert_eq!(first.size_bytes, Some(1_471_026_299));
        assert_eq!(first.info_hash_v1, None);
        assert_eq!(first.magnet_url, None);
        assert_eq!(first.seeders, None);
        assert_eq!(first.peers, None);
        assert_eq!(first.leechers, None);
    }

    #[test]
    fn the_html_entities_in_a_title_are_decoded() {
        let releases = parse_fixture();
        assert_eq!(
            releases[1].title,
            "Rosemary's Baby S01E01 Part 1 1080p WEB-DL DD5 1 H 264-BS"
        );
    }

    #[test]
    fn the_unlabelled_description_shape_still_yields_a_size() {
        // `556 MB; TV/x264` — the second fixture entry.
        assert_eq!(parse_fixture()[1].size_bytes, Some(583_008_256));
    }

    #[test]
    fn every_fixture_entry_carries_a_size_and_a_publish_date() {
        for release in parse_fixture() {
            assert!(release.size_bytes.is_some(), "{}", release.title);
            assert!(release.published_at.is_some(), "{}", release.title);
        }
    }

    #[test]
    fn the_description_category_becomes_provider_metadata() {
        for release in parse_fixture() {
            assert_eq!(release.provider_categories, vec!["TV/x264".to_string()]);
            assert_eq!(release.categories, vec!["TV/x264".to_string()]);
            assert_eq!(
                release.provider_extra.get("category"),
                Some(&serde_json::Value::from("TV/x264"))
            );
            assert_eq!(
                release.provider_extra.get("category_id"),
                Some(&serde_json::Value::from(5_i64)),
                "TV/x264 is IPTorrents category 5"
            );
        }
    }

    #[test]
    fn the_raw_description_is_reported() {
        assert_eq!(
            parse_fixture()[0].provider_extra.get("description"),
            Some(&serde_json::Value::from("Category: TV/x264 Size: 1.37 GB"))
        );
    }

    #[test]
    fn the_guid_is_stable_unique_and_carries_no_passkey() {
        let guids = parse_fixture()
            .iter()
            .map(|release| release.guid.clone().expect("guid"))
            .collect::<Vec<_>>();
        assert_eq!(
            guids[0],
            "http://iptorrents.com/download.php/1234/24.S03E12.720p.WEBRip.h264-DRAWER.torrent"
        );
        for guid in &guids {
            assert!(!guid.contains("torrent_pass"), "{guid}");
        }
        let mut unique = guids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            guids.len(),
            "guids must be unique per release"
        );
        // Stable across polls: the same feed yields the same guids.
        assert_eq!(
            guids,
            parse_fixture()
                .iter()
                .map(|release| release.guid.clone().expect("guid"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_guid_keeps_the_non_secret_query_members() {
        assert_eq!(
            stable_guid("http://x/download.php?id=1234&torrent_pass=abcd"),
            "http://x/download.php?id=1234"
        );
        assert_eq!(
            stable_guid("http://x/download.php?torrent_pass=abcd"),
            "http://x/download.php"
        );
    }

    #[test]
    fn every_release_carries_the_sites_hit_and_run_minimums() {
        for release in parse_fixture() {
            assert_eq!(release.minimum_seed_ratio, Some(1.0));
            assert_eq!(release.minimum_seed_time_minutes, Some(20_160));
        }
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_feed_url() {
        let feed = r#"<rss><channel><item>
            <title>Some Release</title>
            <link>/download.php/9/Some.Release.torrent?torrent_pass=abcd</link>
            <pubDate>Mon, 12 May 2014 19:06:34 +0000</pubDate>
            <description>Category: TV/x264 Size: 1 GB</description>
        </item></channel></rss>"#;
        let releases = parse_feed(feed, NEW_FEED_URL).expect("parses");
        assert_eq!(
            releases[0].download_url.as_deref(),
            Some("https://iptorrents.com/download.php/9/Some.Release.torrent?torrent_pass=abcd")
        );
    }

    #[test]
    fn a_release_without_a_title_or_a_link_is_dropped() {
        let feed = r#"<rss><channel>
            <item><link>http://x/a.torrent</link></item>
            <item><title>No Link</title></item>
            <item><title>Good</title><link>http://x/b.torrent</link></item>
        </channel></rss>"#;
        let releases = parse_feed(feed, NEW_FEED_URL).expect("parses");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].title, "Good");
    }

    #[test]
    fn a_torrent_enclosure_is_preferred_over_the_link() {
        let feed = r#"<rss><channel><item>
            <title>Some Release</title>
            <link>http://iptorrents.com/details.php?id=9</link>
            <enclosure url="http://iptorrents.com/download.php/9/x.torrent" length="1048576" type="application/x-bittorrent" />
            <pubDate>Mon, 12 May 2014 19:06:34 +0000</pubDate>
        </item></channel></rss>"#;
        let releases = parse_feed(feed, NEW_FEED_URL).expect("parses");
        assert_eq!(
            releases[0].download_url.as_deref(),
            Some("http://iptorrents.com/download.php/9/x.torrent")
        );
        assert_eq!(releases[0].size_bytes, Some(1_048_576));
    }

    #[test]
    fn an_empty_channel_is_a_quiet_feed_not_an_error() {
        let releases = parse_feed(r#"<rss><channel></channel></rss>"#, NEW_FEED_URL)
            .expect("an empty feed is valid");
        assert!(releases.is_empty());
    }

    #[test]
    fn duplicate_entries_are_deduped_by_guid() {
        // The same torrent, twice, with a re-rolled passkey in the URL.
        let feed = r#"<rss><channel>
            <item><title>A</title><link>http://x/a.torrent?torrent_pass=1</link></item>
            <item><title>A again</title><link>http://x/a.torrent?torrent_pass=2</link></item>
            <item><title>B</title><link>http://x/b.torrent</link></item>
        </channel></rss>"#;
        let releases = dedupe_results(parse_feed(feed, NEW_FEED_URL).expect("parses"));
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].title, "A");
        assert_eq!(releases[1].title, "B");
    }

    // -- Size parsing (Sonarr's `RssParser.ParseSize`) ---------------------

    #[test]
    fn the_size_grammar_matches_sonarrs_parse_size() {
        assert_eq!(
            parse_size_in_description("Category: TV/x264 Size: 1.37 GB "),
            Some(1_471_026_299)
        );
        assert_eq!(
            parse_size_in_description("556 MB; TV/x264"),
            Some(583_008_256)
        );
        assert_eq!(parse_size_in_description("2.65 GB"), Some(2_845_415_834));
        // `defaultToBinaryPrefix: true` — KB/MB/GB are 1024-based.
        assert_eq!(parse_size_in_description("1 KB"), Some(1_024));
        assert_eq!(parse_size_in_description("1 KiB"), Some(1_024));
        assert_eq!(parse_size_in_description("1MB"), Some(1_048_576));
        // Thousands separators.
        assert_eq!(parse_size_in_description("1,024 MB"), Some(1_073_741_824));
        // All digits: a raw byte count.
        assert_eq!(parse_size_in_description("1471026299"), Some(1_471_026_299));
        // `(?![\w/])` — a rate is not a size.
        assert_eq!(parse_size_in_description("1.5 GB/s"), None);
        assert_eq!(parse_size_in_description("1.5 GBit"), None);
        // No unit at all.
        assert_eq!(parse_size_in_description("Category: TV/x264"), None);
        assert_eq!(parse_size_in_description(""), None);
    }

    #[test]
    fn the_size_scan_skips_digits_that_are_not_a_size() {
        // The `264` of `x264` must not become a size; the real one wins.
        assert_eq!(
            parse_size_in_description("Category: TV/x264 Size: 1.37 GB"),
            Some(1_471_026_299)
        );
    }

    #[test]
    fn a_fraction_tail_never_starts_a_size() {
        // `(?<!\.\d*)`: the `5` of `0.5` may not be read as "5 GB".
        assert_eq!(parse_size_in_description("0.5 GB"), Some(536_870_912));
    }

    // -- Category parsing --------------------------------------------------

    #[test]
    fn both_description_shapes_yield_the_category() {
        assert_eq!(
            parse_category_in_description("Category: TV/x264 Size: 1.37 GB "),
            Some("TV/x264".to_string())
        );
        assert_eq!(
            parse_category_in_description("556 MB; TV/x264"),
            Some("TV/x264".to_string())
        );
        assert_eq!(parse_category_in_description("1.37 GB"), None);
        assert_eq!(parse_category_in_description(""), None);
    }

    #[test]
    fn the_published_category_table_maps_names_to_ids() {
        assert_eq!(category_id_for_name("TV/x264"), Some(5));
        assert_eq!(category_id_for_name("tv/web-dl"), Some(22));
        assert_eq!(category_id_for_name("Anime"), Some(60));
        assert_eq!(category_id_for_name("Movie/4K"), Some(101));
        assert_eq!(category_id_for_name("Not A Category"), None);
    }

    #[test]
    fn the_declared_category_table_carries_facets_for_the_media_categories() {
        let descriptors = category_descriptors();
        assert_eq!(descriptors.len(), CATEGORIES.len());
        let tv = descriptors
            .iter()
            .find(|descriptor| descriptor.value == "TV/x264")
            .expect("TV/x264 declared");
        assert_eq!(tv.facets, vec!["series".to_string()]);
        assert_eq!(tv.label.as_deref(), Some("TV/x264 (IPTorrents category 5)"));
        let anime = descriptors
            .iter()
            .find(|descriptor| descriptor.value == "Anime")
            .expect("Anime declared");
        assert_eq!(anime.facets, vec!["anime".to_string()]);
        let games = descriptors
            .iter()
            .find(|descriptor| descriptor.value == "Games")
            .expect("Games declared");
        assert!(games.facets.is_empty());
    }

    // -- L1: publish dates -------------------------------------------------

    #[test]
    fn rfc_2822_pub_dates_become_rfc_3339_utc() {
        assert_eq!(
            rfc2822_to_rfc3339_utc("Mon, 12 May 2014 19:06:34 +0000").as_deref(),
            Some("2014-05-12T19:06:34Z")
        );
        // Non-UTC offsets are converted, not truncated.
        assert_eq!(
            rfc2822_to_rfc3339_utc("Mon, 12 May 2014 19:06:34 +0200").as_deref(),
            Some("2014-05-12T17:06:34Z")
        );
        assert_eq!(
            rfc2822_to_rfc3339_utc("Mon, 12 May 2014 19:06:34 -0430").as_deref(),
            Some("2014-05-12T23:36:34Z")
        );
        // Obsolete alphabetic zones (RFC 2822 §4.3).
        assert_eq!(
            rfc2822_to_rfc3339_utc("12 May 2014 19:06:34 GMT").as_deref(),
            Some("2014-05-12T19:06:34Z")
        );
        assert_eq!(
            rfc2822_to_rfc3339_utc("Mon, 12 May 2014 19:06:34 EST").as_deref(),
            Some("2014-05-13T00:06:34Z")
        );
        // No seconds, no day-of-week, and no zone at all.
        assert_eq!(
            rfc2822_to_rfc3339_utc("12 May 2014 19:06").as_deref(),
            Some("2014-05-12T19:06:00Z")
        );
        // Leap day and year boundaries.
        assert_eq!(
            rfc2822_to_rfc3339_utc("Sat, 29 Feb 2020 00:00:00 +0000").as_deref(),
            Some("2020-02-29T00:00:00Z")
        );
        assert_eq!(
            rfc2822_to_rfc3339_utc("Wed, 31 Dec 2025 23:59:59 -0100").as_deref(),
            Some("2026-01-01T00:59:59Z")
        );
        // Junk yields nothing rather than a value the core would silently drop.
        assert_eq!(rfc2822_to_rfc3339_utc("not a date"), None);
        assert_eq!(rfc2822_to_rfc3339_utc(""), None);
    }

    #[test]
    fn an_rfc_3339_pub_date_is_passed_through() {
        assert_eq!(
            rfc2822_to_rfc3339_utc("2014-05-12T19:06:34Z").as_deref(),
            Some("2014-05-12T19:06:34Z")
        );
    }

    #[test]
    fn the_civil_calendar_round_trips() {
        for (year, month, day) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2014, 5, 12),
            (2026, 9, 2),
            (2100, 3, 1),
        ] {
            assert_eq!(
                civil_from_days(days_from_civil(year, month, day)),
                (year, month, day)
            );
        }
    }

    // -- H3: delivery classification ---------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn an_ok_xml_delivery_is_returned_verbatim() {
        let text = classify_response(
            200,
            &headers(&[("Content-Type", "text/xml")]),
            RECENT_FEED.as_bytes(),
        )
        .expect("200 xml is accepted");
        assert_eq!(text, RECENT_FEED);
    }

    #[test]
    fn an_html_page_on_a_two_hundred_is_an_unexpected_content_type() {
        let error = plugin_error(
            &classify_response(
                200,
                &headers(&[("Content-Type", "text/html; charset=utf-8")]),
                b"<html><body>Just a moment...</body></html>",
            )
            .unwrap_err(),
        );
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::UnexpectedContentType
                }
            ))
        ));
        assert!(error.debug_message.unwrap().contains("Cloudflare"));
    }

    #[test]
    fn an_html_body_without_a_content_type_is_still_detected() {
        let error = plugin_error(
            &classify_response(200, &headers(&[]), b"<!DOCTYPE html>\n<html>...").unwrap_err(),
        );
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
    fn a_forbidden_html_page_is_reported_as_a_blocked_site_not_a_bad_password() {
        let error = plugin_error(
            &classify_response(
                403,
                &headers(&[("content-type", "text/html")]),
                b"<html>Attention Required! | Cloudflare</html>",
            )
            .unwrap_err(),
        );
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
    fn a_bare_unauthorised_response_is_a_typed_auth_failure() {
        for status in [401_u16, 403] {
            let error =
                plugin_error(&classify_response(status, &headers(&[]), b"denied").unwrap_err());
            assert_eq!(error.code, PluginErrorCode::AuthFailed);
            assert!(error.public_message.contains("feed_url"));
            assert!(error.debug_message.unwrap().contains("denied"));
            assert!(error.details.is_none(), "an auth fault must not defer");
        }
    }

    #[test]
    fn a_redirect_names_the_feed_url_and_the_location() {
        let error = plugin_error(
            &classify_response(
                302,
                &headers(&[("Location", "https://iptorrents.com/login.php")]),
                b"",
            )
            .unwrap_err(),
        );
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(error.debug_message.unwrap().contains("login.php"));
    }

    #[test]
    fn a_rate_limit_defers_with_the_retry_after_header() {
        let error = plugin_error(
            &classify_response(429, &headers(&[("Retry-After", "120")]), b"slow down").unwrap_err(),
        );
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(120));
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::Deferred {
                    reason: IndexerSearchIncompleteReason::RateLimited,
                    retry_after_seconds: Some(120),
                }
            ))
        ));
    }

    #[test]
    fn a_rate_limit_without_a_header_falls_back_to_sonarrs_hour() {
        let error = plugin_error(&classify_response(429, &headers(&[]), b"").unwrap_err());
        assert_eq!(
            error.retry_after_seconds,
            Some(RATE_LIMITED_FALLBACK_SECONDS)
        );
    }

    #[test]
    fn a_server_error_defers_on_upstream_failure() {
        for status in [500_u16, 502, 503] {
            let error = plugin_error(&classify_response(status, &headers(&[]), b"").unwrap_err());
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
    }

    #[test]
    fn an_oversized_body_is_reported_as_truncated() {
        let body = vec![b'a'; MAX_RESPONSE_BYTES + 1];
        let error = plugin_error(&classify_response(200, &headers(&[]), &body).unwrap_err());
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
    fn a_non_utf8_body_is_malformed() {
        let error =
            plugin_error(&classify_response(200, &headers(&[]), &[0xff, 0xfe, 0x00]).unwrap_err());
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
    fn broken_xml_is_a_malformed_body_not_an_empty_feed() {
        let error = plugin_error(
            &parse_feed(
                "<rss><channel><item><title>a</title></channel>",
                NEW_FEED_URL,
            )
            .unwrap_err(),
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
    fn a_document_with_no_channel_is_an_invalid_root() {
        let error =
            plugin_error(&parse_feed("<html><body>hi</body></html>", NEW_FEED_URL).unwrap_err());
        assert!(matches!(
            error.details,
            Some(PluginErrorDetails::IndexerSearch(
                IndexerSearchPluginError::InvalidResponse {
                    kind: IndexerSearchInvalidResponseKind::InvalidRoot
                }
            ))
        ));
    }

    // -- M2: request handling ----------------------------------------------

    #[test]
    fn an_explicit_recent_context_is_the_feed_poll() {
        assert!(is_recent_request(&recent_request()));
    }

    #[test]
    fn a_request_with_search_criteria_is_not_served_from_the_feed() {
        // Sonarr's request generator returns an empty chain for every search
        // criteria overload.
        let request = SearchRequest {
            query: "Some Show".to_string(),
            season: Some(3),
            episode: Some(12),
            context: Some(PluginSearchContext {
                request_kind: PluginSearchRequestKind::Search,
                ..Default::default()
            }),
            ..SearchRequest::default()
        };
        assert!(!is_recent_request(&request));
    }

    #[test]
    fn a_context_free_request_with_no_criteria_is_still_a_feed_poll() {
        assert!(is_recent_request(&SearchRequest::default()));
        assert!(!is_recent_request(&SearchRequest {
            query: "anything".to_string(),
            ..SearchRequest::default()
        }));
    }

    #[test]
    fn a_facet_on_the_poll_does_not_drop_the_feed() {
        // The shared `rss-common` pipeline post-filters on the request's facet
        // and category words; this plugin does not filter at all, so an RSS
        // poll that carries a facet still returns every parsed release.
        let request = SearchRequest {
            facet: Some("movie".to_string()),
            categories: vec!["5000".to_string()],
            ..recent_request()
        };
        assert!(is_recent_request(&request));
        assert_eq!(parse_fixture().len(), 5);
    }

    #[test]
    fn the_host_limit_is_honoured_and_zero_means_everything() {
        assert_eq!(result_limit(&recent_request()), Some(1000));
        assert_eq!(
            result_limit(&SearchRequest {
                limit: 0,
                ..SearchRequest::default()
            }),
            None
        );
        assert_eq!(
            result_limit(&SearchRequest {
                limit: 3,
                ..SearchRequest::default()
            }),
            Some(3)
        );
    }

    // -- M1: descriptor honesty --------------------------------------------

    fn indexer_descriptor() -> IndexerDescriptor {
        match build_descriptor().provider {
            ProviderDescriptor::Indexer(descriptor) => descriptor,
            _ => panic!("iptorrents is an indexer"),
        }
    }

    #[test]
    fn the_descriptor_claims_only_what_the_feed_carries() {
        let torrent = indexer_descriptor()
            .capabilities
            .torrent
            .expect("torrent capabilities");
        assert!(!torrent.reports_seeders);
        assert!(!torrent.reports_peers);
        assert!(!torrent.reports_leechers);
        assert!(!torrent.reports_info_hash);
        assert!(!torrent.reports_magnet_uri);
        assert!(!torrent.reports_volume_factors);
        assert!(torrent.supports_private_tracker_flags);
        // The plugin fills `minimum_seed_ratio`/`minimum_seed_time_minutes`.
        assert!(torrent.supports_seed_requirements);
    }

    #[test]
    fn the_descriptor_declares_an_rss_only_indexer() {
        let capabilities = indexer_descriptor().capabilities;
        assert!(!capabilities.search);
        assert!(capabilities.rss);
        assert!(capabilities.query_param.is_none());
        assert!(capabilities.season_param.is_none());
        assert!(capabilities.episode_param.is_none());
        assert!(capabilities.supported_ids.is_empty());
        assert!(capabilities.supported_external_ids.is_empty());
        assert_eq!(
            capabilities.feed_modes,
            vec![IndexerFeedMode::Recent, IndexerFeedMode::Rss]
        );
        assert_eq!(capabilities.search_inputs, vec![IndexerSearchInput::Limit]);
    }

    #[test]
    fn the_descriptor_declares_one_unpaged_request() {
        let limits = indexer_descriptor()
            .capabilities
            .limits
            .expect("limits declared");
        assert_eq!(limits.page_size, None);
        assert_eq!(limits.max_page_size, None);
        assert_eq!(limits.max_pages, Some(1));
        assert_eq!(limits.rate_limit_hint_seconds, Some(2));
    }

    #[test]
    fn the_config_field_keys_are_the_published_contract() {
        let keys = indexer_descriptor()
            .config_fields
            .iter()
            .map(|field| field.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "feed_url",
                "minimum_seeders",
                "user_agent",
                "cookie",
                "username",
                "password",
                "additional_headers",
            ]
        );
    }

    // -- Transport headers and redaction -----------------------------------

    fn config() -> IpTorrentsConfig {
        IpTorrentsConfig {
            feed_url: NEW_FEED_URL.to_string(),
            user_agent: USER_AGENT.to_string(),
            cookie: None,
            username: None,
            password: None,
            additional_headers: String::new(),
        }
    }

    #[test]
    fn the_request_asks_for_xml_with_a_versioned_user_agent() {
        let headers = config().request_headers();
        assert!(headers["Accept"].contains("application/rss+xml"));
        // Sonarr's `PreProcess` treats an HTML body as a blocked site only when
        // the request did not ask for HTML, so the plugin never asks for it.
        assert!(!headers["Accept"].contains("text/html"));
        assert!(headers["User-Agent"].starts_with("scryer-iptorrents-indexer/"));
        assert!(!headers.contains_key("Cookie"));
        assert!(!headers.contains_key("Authorization"));
    }

    #[test]
    fn the_optional_transport_settings_are_applied() {
        let config = IpTorrentsConfig {
            cookie: Some("cf_clearance=abc".to_string()),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            additional_headers: "X-Proxy: yes\n\nbroken line\nX-Empty:  ".to_string(),
            ..config()
        };
        let headers = config.request_headers();
        assert_eq!(headers["Cookie"], "cf_clearance=abc");
        assert_eq!(headers["Authorization"], "Basic dXNlcjpwYXNz");
        assert_eq!(headers["X-Proxy"], "yes");
        assert!(!headers.contains_key("X-Empty"));
    }

    #[test]
    fn the_feed_url_is_redacted_before_it_reaches_a_log() {
        let redacted = redact_feed_url(NEW_FEED_URL);
        assert_eq!(
            redacted,
            "https://iptorrents.com/t.rss?u=REDACTED;tp=REDACTED;3;80;93;37;download"
        );
        assert!(!redacted.contains("USERID"));
        assert!(!redacted.contains("APIKEY"));
        assert_eq!(
            redact_feed_url("http://x/download.php?id=1&torrent_pass=abcd"),
            "http://x/download.php?id=1&torrent_pass=REDACTED"
        );
        assert_eq!(
            redact_feed_url("https://iptorrents.com/"),
            "https://iptorrents.com/"
        );
    }

    #[test]
    fn entity_decoding_handles_named_numeric_and_unknown_references() {
        assert_eq!(decode_reference("#39").as_deref(), Some("'"));
        assert_eq!(decode_reference("#x27").as_deref(), Some("'"));
        assert_eq!(decode_reference("amp").as_deref(), Some("&"));
        assert_eq!(decode_reference("lt").as_deref(), Some("<"));
        assert_eq!(decode_reference("gt").as_deref(), Some(">"));
        assert_eq!(decode_reference("quot").as_deref(), Some("\""));
        assert_eq!(decode_reference("apos").as_deref(), Some("'"));
        // Unknown references are put back verbatim, never dropped.
        assert_eq!(
            decode_reference("notanentity").as_deref(),
            Some("&notanentity;")
        );
    }

    #[test]
    fn entities_inside_element_text_are_resolved_in_place() {
        // quick-xml splits a text node at every reference, so the fragments
        // must be re-joined without losing the surrounding whitespace.
        let feed = r#"<rss><channel><item>
            <title>Tom &amp; Jerry &#8212; S01E01 &lt;RAW&gt;</title>
            <link>http://x/a.torrent</link>
            <category>TV &amp; Film</category>
        </item></channel></rss>"#;
        let releases = parse_feed(feed, NEW_FEED_URL).expect("parses");
        assert_eq!(releases[0].title, "Tom & Jerry — S01E01 <RAW>");
        assert_eq!(releases[0].provider_categories, vec!["TV & Film"]);
    }

    #[test]
    fn several_category_elements_stay_separate() {
        let feed = r#"<rss><channel><item>
            <title>A</title>
            <link>http://x/a.torrent</link>
            <category>TV/x264</category>
            <category>TV/Packs</category>
        </item></channel></rss>"#;
        let releases = parse_feed(feed, NEW_FEED_URL).expect("parses");
        assert_eq!(
            releases[0].provider_categories,
            vec!["TV/x264".to_string(), "TV/Packs".to_string()]
        );
    }
}
