//! Native tests over fixture JSON. No network: every Data API reply comes
//! from `tests/fixtures/`, and every call the engine makes is recorded.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use scryer_plugin_sdk::{
    HttpAvailability, HttpTransport, IndexerFeedMode, IndexerProtocol,
    IndexerSearchIncompleteReason, IndexerSearchInput, IndexerSearchPluginError, IndexerSourceKind,
    PluginErrorCode, PluginErrorDetails, PluginSearchRequest as SearchRequest, PluginSearchResult,
    ProviderDescriptor,
};

use crate::api::{
    self, ApiCall, SafeSearch, YoutubeConfig, YoutubeError, classify_error, parse_channel_ids,
};
use crate::cursor::{Mode, PageCursor};
use crate::engine::{
    self, Fetch, HttpReply, PageOutcome, SEARCH_PAGE_CEILING, UPLOADS_PAGE_CEILING,
};
use crate::release::{self, Candidate, Enrichment};
use crate::text::{clean_title, html_unescape, matches_query, parse_iso8601_duration};

const KEY: &str = "SYNTHETIC-KEY-0123456789abcdef";
const CHANNEL_ONE: &str = "UCsyntheticChannel000001";
const CHANNEL_TWO: &str = "UCsyntheticChannel000002";

const SEARCH_LIST: &str = include_str!("../tests/fixtures/search_list.json");
const SEARCH_LIST_LAST: &str = include_str!("../tests/fixtures/search_list_last.json");
const VIDEOS_LIST: &str = include_str!("../tests/fixtures/videos_list.json");
const VIDEOS_LIST_STATES: &str = include_str!("../tests/fixtures/videos_list_states.json");
const CHANNELS_LIST: &str = include_str!("../tests/fixtures/channels_list.json");
const PLAYLIST_ITEMS: &str = include_str!("../tests/fixtures/playlist_items.json");
const PLAYLIST_ITEMS_LAST: &str = include_str!("../tests/fixtures/playlist_items_last.json");
const ERROR_KEY_INVALID: &str = include_str!("../tests/fixtures/error_key_invalid.json");
const ERROR_KEY_INVALID_LEGACY: &str =
    include_str!("../tests/fixtures/error_key_invalid_legacy.json");
const ERROR_FORBIDDEN: &str = include_str!("../tests/fixtures/error_forbidden.json");
const ERROR_ACCESS_NOT_CONFIGURED: &str =
    include_str!("../tests/fixtures/error_access_not_configured.json");
const ERROR_QUOTA_EXCEEDED: &str = include_str!("../tests/fixtures/error_quota_exceeded.json");
const ERROR_NOT_FOUND: &str = include_str!("../tests/fixtures/error_not_found.json");

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Route {
    endpoint: &'static str,
    /// A substring the call's URL must contain; empty matches any.
    needle: &'static str,
    status: u16,
    body: &'static str,
}

fn route(endpoint: &'static str, needle: &'static str, status: u16, body: &'static str) -> Route {
    Route {
        endpoint,
        needle,
        status,
        body,
    }
}

struct FixtureFetch {
    routes: Vec<Route>,
    calls: RefCell<Vec<ApiCall>>,
}

impl FixtureFetch {
    fn new(routes: Vec<Route>) -> Self {
        Self {
            routes,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<ApiCall> {
        self.calls.borrow().clone()
    }

    fn calls_to(&self, endpoint: &str) -> Vec<ApiCall> {
        self.calls()
            .into_iter()
            .filter(|call| call.endpoint == endpoint)
            .collect()
    }
}

impl Fetch for FixtureFetch {
    async fn get(&self, call: &ApiCall) -> Result<HttpReply, YoutubeError> {
        self.calls.borrow_mut().push(call.clone());
        let url = call.url();
        let route = self
            .routes
            .iter()
            .find(|route| route.endpoint == call.endpoint && url.contains(route.needle))
            .unwrap_or_else(|| panic!("no fixture route for {url}"));
        Ok(HttpReply {
            status: route.status,
            body: route.body.as_bytes().to_vec(),
        })
    }
}

/// Every fixture future is ready on its first poll.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture future was not ready"),
    }
}

fn config_with(values: &[(&str, &str)]) -> YoutubeConfig {
    YoutubeConfig::from_values(|key| {
        values
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_string())
    })
    .expect("valid config")
}

fn config() -> YoutubeConfig {
    config_with(&[("api_key", KEY)])
}

fn channel_config() -> YoutubeConfig {
    config_with(&[
        ("api_key", KEY),
        ("channel_ids", &format!("{CHANNEL_ONE}, {CHANNEL_TWO}")),
    ])
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        ..SearchRequest::default()
    }
}

fn run(
    config: &YoutubeConfig,
    request: &SearchRequest,
    fetch: &FixtureFetch,
) -> Result<PageOutcome, engine::Failure> {
    block_on(engine::run_page(config, request, fetch))
}

fn by_id<'a>(results: &'a [PluginSearchResult], id: &str) -> &'a PluginSearchResult {
    results
        .iter()
        .find(|result| result.http.as_ref().is_some_and(|http| http.item_id == id))
        .unwrap_or_else(|| panic!("no result for {id}"))
}

fn availability_of(results: &[PluginSearchResult], id: &str) -> HttpAvailability {
    by_id(results, id).http.as_ref().unwrap().availability
}

fn assert_no_key(fetch: &FixtureFetch) {
    for call in fetch.calls() {
        assert!(!call.url().contains(KEY), "key leaked into {}", call.url());
    }
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[test]
fn descriptor_declares_an_interactive_http_indexer() {
    let descriptor = crate::build_descriptor();
    assert_eq!(descriptor.id, "youtube");
    let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
        panic!("not an indexer");
    };
    assert_eq!(indexer.provider_type, "youtube");
    assert_eq!(indexer.source_kind, IndexerSourceKind::Http);
    assert_eq!(
        indexer.allowed_hosts,
        vec!["www.googleapis.com".to_string()]
    );
    let capabilities = indexer.capabilities;
    assert!(capabilities.search);
    assert!(!capabilities.rss);
    assert_eq!(capabilities.query_param.as_deref(), Some("q"));
    assert_eq!(capabilities.season_param, None);
    assert_eq!(capabilities.episode_param, None);
    assert_eq!(capabilities.protocols, vec![IndexerProtocol::Http]);
    assert_eq!(
        capabilities.feed_modes,
        vec![IndexerFeedMode::InteractiveSearch]
    );
    assert_eq!(
        capabilities.search_inputs,
        vec![
            IndexerSearchInput::TextQuery,
            IndexerSearchInput::TitleQuery,
            IndexerSearchInput::Season,
            IndexerSearchInput::Episode,
            IndexerSearchInput::Limit,
        ]
    );
    assert!(capabilities.supported_external_ids.is_empty());
    let limits = capabilities.limits.expect("limits");
    assert!(limits.paged_search);
    assert_eq!(limits.max_page_size, Some(50));
    let http = capabilities.http.expect("http capabilities");
    assert_eq!(http.transports, vec![HttpTransport::ProviderPage]);
    assert_eq!(http.providers, vec!["youtube".to_string()]);

    let keys = indexer
        .config_fields
        .iter()
        .map(|field| field.key.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            "api_key",
            "safe_search",
            "channel_ids",
            "region_code",
            "relevance_language"
        ]
    );
    let api_key = &indexer.config_fields[0];
    assert!(api_key.required);
    assert_eq!(
        api_key.field_type,
        scryer_plugin_sdk::ConfigFieldType::Password
    );
    assert_eq!(
        indexer.config_fields[1].default_value.as_deref(),
        Some("moderate")
    );
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn config_defaults_and_normalises() {
    let config = config_with(&[
        ("api_key", KEY),
        ("region_code", " gb "),
        ("relevance_language", "zh-Hans"),
    ]);
    assert_eq!(config.region_code.as_deref(), Some("GB"));
    assert_eq!(config.relevance_language.as_deref(), Some("zh-Hans"));
    assert_eq!(config.safe_search, SafeSearch::Moderate);
    assert!(config.channel_ids.is_empty());
}

#[test]
fn config_rejects_bad_values_without_echoing_the_key() {
    let attempt = |values: &[(&str, &str)]| {
        YoutubeConfig::from_values(|key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    };
    assert!(matches!(attempt(&[]), Err(YoutubeError::InvalidConfig(_))));
    for bad in [
        vec![("api_key", "has space")],
        vec![("api_key", KEY), ("region_code", "USA")],
        vec![("api_key", KEY), ("relevance_language", "english!")],
        vec![("api_key", KEY), ("safe_search", "lenient")],
        vec![("api_key", KEY), ("channel_ids", "not-a-channel")],
    ] {
        let error = attempt(&bad).expect_err("rejected");
        assert!(matches!(error, YoutubeError::InvalidConfig(_)));
        assert!(!error.to_string().contains(KEY));
    }
}

#[test]
fn config_debug_redacts_the_key() {
    let rendered = format!("{:?}", config());
    assert!(!rendered.contains(KEY));
    assert!(rendered.contains("[redacted]"));
}

#[test]
fn channel_ids_accept_lists_dedupe_and_cap() {
    assert_eq!(
        parse_channel_ids(&format!("{CHANNEL_ONE};{CHANNEL_TWO}\n{CHANNEL_ONE}")).unwrap(),
        vec![CHANNEL_ONE.to_string(), CHANNEL_TWO.to_string()]
    );
    assert_eq!(
        parse_channel_ids(&format!("[\"{CHANNEL_TWO}\"]")).unwrap(),
        vec![CHANNEL_TWO.to_string()]
    );
    let eleven = (0..11)
        .map(|index| format!("UCsyntheticChannel0000{index:02}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_channel_ids(&eleven).is_err());
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

fn params(call: &ApiCall) -> Vec<(&'static str, String)> {
    call.params.clone()
}

#[test]
fn search_params_without_channel_or_token() {
    let call = api::search_call(&config(), "Synthetic Show", 50, None, None);
    assert_eq!(call.endpoint, "search");
    assert_eq!(
        params(&call),
        vec![
            ("part", "snippet".to_string()),
            ("type", "video".to_string()),
            ("q", "Synthetic Show".to_string()),
            ("maxResults", "50".to_string()),
            ("safeSearch", "moderate".to_string()),
        ]
    );
    assert_eq!(
        call.url(),
        "https://www.googleapis.com/youtube/v3/search?part=snippet&type=video&q=Synthetic%20Show&maxResults=50&safeSearch=moderate"
    );
}

#[test]
fn search_params_with_channel_token_region_and_language() {
    let config = config_with(&[
        ("api_key", KEY),
        ("region_code", "US"),
        ("relevance_language", "en"),
        ("safe_search", "strict"),
    ]);
    let call = api::search_call(
        &config,
        "Synthetic Show",
        500,
        Some(CHANNEL_ONE),
        Some("CDIQAA"),
    );
    assert_eq!(
        params(&call),
        vec![
            ("part", "snippet".to_string()),
            ("type", "video".to_string()),
            ("q", "Synthetic Show".to_string()),
            ("maxResults", "50".to_string()),
            ("channelId", CHANNEL_ONE.to_string()),
            ("pageToken", "CDIQAA".to_string()),
            ("regionCode", "US".to_string()),
            ("relevanceLanguage", "en".to_string()),
            ("safeSearch", "strict".to_string()),
        ]
    );
    assert!(!call.url().contains(KEY));
}

#[test]
fn enrichment_and_uploads_params() {
    let ids = vec!["synthAAA001".to_string(), "synthBBB002".to_string()];
    let videos = api::videos_call(&ids);
    assert_eq!(
        videos.url(),
        "https://www.googleapis.com/youtube/v3/videos?part=snippet%2CcontentDetails%2Cstatus&id=synthAAA001%2CsynthBBB002"
    );
    assert_eq!(videos.param("maxResults"), None);
    let channels = api::channels_call(&[CHANNEL_ONE.to_string()]);
    assert_eq!(channels.param("part"), Some("contentDetails"));
    assert_eq!(channels.param("id"), Some(CHANNEL_ONE));
    let playlist = api::playlist_items_call("UUsyntheticChannel000001", Some("EAAaBlBUOkNESQ"));
    assert_eq!(
        playlist.param("playlistId"),
        Some("UUsyntheticChannel000001")
    );
    assert_eq!(playlist.param("maxResults"), Some("50"));
    assert_eq!(playlist.param("pageToken"), Some("EAAaBlBUOkNESQ"));
}

#[test]
fn search_query_carries_season_or_episode() {
    let mut request = request("  Synthetic   Show ");
    assert_eq!(engine::build_search_query(&request), "Synthetic Show");
    request.season = Some(1);
    assert_eq!(
        engine::build_search_query(&request),
        "Synthetic Show season 1"
    );
    request.episode = Some(2);
    assert_eq!(
        engine::build_search_query(&request),
        "Synthetic Show episode 2"
    );
}

#[test]
fn page_size_follows_the_limit() {
    let mut request = request("Synthetic Show");
    assert_eq!(engine::page_size(&request), 50);
    request.limit = 10;
    assert_eq!(engine::page_size(&request), 10);
    request.limit = 500;
    assert_eq!(engine::page_size(&request), 50);
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

#[test]
fn cursor_round_trips() {
    let cursor = PageCursor {
        mode: Mode::Search,
        pages_read: 2,
        lanes: vec![
            (CHANNEL_ONE.to_string(), "CDIQAA".to_string()),
            ("*".to_string(), "CGQQAA==".to_string()),
        ],
    };
    let encoded = cursor.encode();
    assert_eq!(encoded, format!("yt1.s.2.{CHANNEL_ONE}~CDIQAA,*~CGQQAA=="));
    assert_eq!(PageCursor::decode(&encoded), Some(cursor));
}

#[test]
fn cursor_rejects_foreign_values() {
    for bad in [
        "",
        "yt2.s.1.*~CDIQAA",
        "yt1.x.1.*~CDIQAA",
        "yt1.s.one.*~CDIQAA",
        "yt1.s.1.",
        "yt1.s.1.*~",
        "yt1.s.1.*~bad token",
        "yt1.s.1.*~A,*~B",
        "yt1.s.1.bad/lane~CDIQAA",
    ] {
        assert_eq!(PageCursor::decode(bad), None, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Search flow
// ---------------------------------------------------------------------------

fn search_routes() -> Vec<Route> {
    vec![
        route("search", "pageToken=CDIQAA", 200, SEARCH_LIST_LAST),
        route("search", "", 200, SEARCH_LIST),
        route("videos", "", 200, VIDEOS_LIST),
    ]
}

#[test]
fn first_page_is_one_search_and_one_batched_enrichment() {
    let fetch = FixtureFetch::new(search_routes());
    let outcome = run(&config(), &request("Synthetic Show"), &fetch).expect("page");

    let searches = fetch.calls_to("search");
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].param("channelId"), None);
    assert_eq!(searches[0].param("pageToken"), None);
    let videos = fetch.calls_to("videos");
    assert_eq!(videos.len(), 1);
    assert_eq!(
        videos[0].param("id"),
        Some("synthAAA001,synthBBB002,synthCCC003,synthDDD004")
    );
    assert_eq!(fetch.calls().len(), 2);
    assert_no_key(&fetch);

    let results = &outcome.response.results;
    // The channel result in the listing is skipped.
    assert_eq!(results.len(), 4);
    assert_eq!(
        outcome.response.next_cursor.as_deref(),
        Some("yt1.s.1.*~CDIQAA")
    );
    assert!(!outcome.ceiling_reached);
    assert_eq!(
        availability_of(results, "synthAAA001"),
        HttpAvailability::Available
    );
    assert_eq!(
        availability_of(results, "synthBBB002"),
        HttpAvailability::Upcoming
    );
    assert_eq!(
        availability_of(results, "synthCCC003"),
        HttpAvailability::Available
    );
    // Listed but not returned by videos.list: deleted.
    assert_eq!(
        availability_of(results, "synthDDD004"),
        HttpAvailability::Removed
    );
}

#[test]
fn search_stops_at_its_page_ceiling() {
    assert_eq!(SEARCH_PAGE_CEILING, 2);
    // Every search page offers another token, so only the ceiling stops it.
    let routes = || {
        vec![
            route("search", "", 200, SEARCH_LIST),
            route("videos", "", 200, VIDEOS_LIST),
        ]
    };
    let mut cursor = None;
    let mut search_calls = 0;
    for page in 1..=SEARCH_PAGE_CEILING {
        let fetch = FixtureFetch::new(routes());
        let mut paged = request("Synthetic Show");
        paged.page_cursor = cursor.clone();
        let outcome = run(&config(), &paged, &fetch).expect("page");
        search_calls += fetch.calls_to("search").len();
        assert_eq!(fetch.calls_to("videos").len(), 1);
        if page < SEARCH_PAGE_CEILING {
            assert!(!outcome.ceiling_reached);
            cursor = outcome.response.next_cursor.clone();
            assert!(cursor.is_some());
        } else {
            assert!(outcome.ceiling_reached);
            assert_eq!(outcome.response.next_cursor, None);
            assert_eq!(outcome.response.results.len(), 4);
        }
    }
    assert_eq!(search_calls, SEARCH_PAGE_CEILING as usize);

    // A cursor already at the ceiling never reaches Google.
    let fetch = FixtureFetch::new(routes());
    let mut paged = request("Synthetic Show");
    paged.page_cursor = Some(format!("yt1.s.{SEARCH_PAGE_CEILING}.*~CDIQAA"));
    let outcome = run(&config(), &paged, &fetch).expect("ceiling");
    assert!(fetch.calls().is_empty());
    assert!(outcome.ceiling_reached);
    assert!(outcome.response.results.is_empty());
    assert_eq!(outcome.response.next_cursor, None);

    // The same holds for each allow-listed channel lane.
    let fetch = FixtureFetch::new(routes());
    let mut paged = request("Synthetic Show");
    paged.page_cursor = Some(format!("yt1.s.{SEARCH_PAGE_CEILING}.{CHANNEL_ONE}~CDIQAA"));
    let outcome = run(&channel_config(), &paged, &fetch).expect("ceiling");
    assert!(fetch.calls().is_empty());
    assert!(outcome.ceiling_reached);
}

#[test]
fn next_page_sends_the_token_and_ends_without_a_cursor() {
    let fetch = FixtureFetch::new(search_routes());
    let mut request = request("Synthetic Show");
    request.page_cursor = Some("yt1.s.1.*~CDIQAA".to_string());
    let outcome = run(&config(), &request, &fetch).expect("page");
    let searches = fetch.calls_to("search");
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].param("pageToken"), Some("CDIQAA"));
    assert_eq!(outcome.response.results.len(), 1);
    assert_eq!(outcome.response.next_cursor, None);
}

#[test]
fn allow_list_searches_each_channel_once_per_page() {
    let fetch = FixtureFetch::new(vec![
        route("search", CHANNEL_ONE, 200, SEARCH_LIST),
        route("search", CHANNEL_TWO, 200, SEARCH_LIST_LAST),
        route("videos", "", 200, VIDEOS_LIST),
    ]);
    let outcome = run(&channel_config(), &request("Synthetic Show"), &fetch).expect("page");
    let searches = fetch.calls_to("search");
    assert_eq!(searches.len(), 2);
    assert_eq!(searches[0].param("channelId"), Some(CHANNEL_ONE));
    assert_eq!(searches[1].param("channelId"), Some(CHANNEL_TWO));
    assert_eq!(fetch.calls_to("videos").len(), 1);
    assert_eq!(outcome.response.results.len(), 5);
    // Only the first channel has another page.
    assert_eq!(
        outcome.response.next_cursor,
        Some(format!("yt1.s.1.{CHANNEL_ONE}~CDIQAA"))
    );

    // The continuation reads only that channel.
    let fetch = FixtureFetch::new(vec![
        route("search", "", 200, SEARCH_LIST_LAST),
        route("videos", "", 200, VIDEOS_LIST),
    ]);
    let mut next = request("Synthetic Show");
    next.page_cursor = outcome.response.next_cursor.clone();
    run(&channel_config(), &next, &fetch).expect("page");
    let searches = fetch.calls_to("search");
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].param("channelId"), Some(CHANNEL_ONE));
    assert_eq!(searches[0].param("pageToken"), Some("CDIQAA"));
}

#[test]
fn a_cursor_for_another_configuration_is_refused_without_calls() {
    let fetch = FixtureFetch::new(search_routes());
    let mut request = request("Synthetic Show");
    request.page_cursor = Some("yt1.s.1.*~CDIQAA".to_string());
    let failure = run(&channel_config(), &request, &fetch).expect_err("refused");
    assert_eq!(failure.error, YoutubeError::InvalidCursor);
    request.page_cursor = Some("garbage".to_string());
    let failure = run(&config(), &request, &fetch).expect_err("refused");
    assert_eq!(failure.error, YoutubeError::InvalidCursor);
    assert!(fetch.calls().is_empty());
}

#[test]
fn empty_query_makes_no_calls() {
    let fetch = FixtureFetch::new(Vec::new());
    let outcome = run(&config(), &request("   "), &fetch).expect("empty");
    assert!(outcome.response.results.is_empty());
    assert!(fetch.calls().is_empty());
}

#[test]
fn region_marks_blocked_videos_geo_restricted() {
    let fetch = FixtureFetch::new(search_routes());
    let config = config_with(&[("api_key", KEY), ("region_code", "de")]);
    let outcome = run(&config, &request("Synthetic Show"), &fetch).expect("page");
    let results = &outcome.response.results;
    // Blocked in DE.
    assert_eq!(
        availability_of(results, "synthAAA001"),
        HttpAvailability::GeoRestricted
    );
    // Allowed only in GB and IE.
    assert_eq!(
        availability_of(results, "synthCCC003"),
        HttpAvailability::GeoRestricted
    );

    let fetch = FixtureFetch::new(search_routes());
    let config = config_with(&[("api_key", KEY), ("region_code", "GB")]);
    let outcome = run(&config, &request("Synthetic Show"), &fetch).expect("page");
    let results = &outcome.response.results;
    assert_eq!(
        availability_of(results, "synthAAA001"),
        HttpAvailability::Available
    );
    assert_eq!(
        availability_of(results, "synthCCC003"),
        HttpAvailability::Available
    );
}

#[test]
fn failed_enrichment_reports_listing_data_only() {
    let fetch = FixtureFetch::new(vec![
        route("search", "", 200, SEARCH_LIST),
        route("videos", "", 500, ""),
    ]);
    let outcome = run(&config(), &request("Synthetic Show"), &fetch).expect("page");
    let results = &outcome.response.results;
    assert_eq!(results.len(), 4);
    assert_eq!(outcome.warnings.len(), 1);
    assert_eq!(
        availability_of(results, "synthAAA001"),
        HttpAvailability::Unknown
    );
    assert_eq!(
        availability_of(results, "synthBBB002"),
        HttpAvailability::Upcoming
    );
    let first = by_id(results, "synthAAA001");
    // The listing title had its entities decoded.
    assert_eq!(first.title, "Synthetic Show S01E02 & Friends");
    assert_eq!(first.provider_extra["enriched"], serde_json::json!(false));
    assert!(first.http.as_ref().unwrap().duration_seconds.is_none());
}

#[test]
fn search_errors_fail_the_page() {
    for (status, body, expected) in [
        (403, ERROR_QUOTA_EXCEEDED, YoutubeError::QuotaExceeded),
        (400, ERROR_KEY_INVALID, YoutubeError::InvalidKey),
        (403, ERROR_ACCESS_NOT_CONFIGURED, YoutubeError::ApiDisabled),
    ] {
        let fetch = FixtureFetch::new(vec![route("search", "", status, body)]);
        let failure = run(&config(), &request("Synthetic Show"), &fetch).expect_err("fails");
        assert_eq!(failure.error, expected);
        assert!(failure.partial.is_none());
        // No enrichment after a failed listing.
        assert!(fetch.calls_to("videos").is_empty());
    }
}

#[test]
fn malformed_listing_is_reported() {
    let fetch = FixtureFetch::new(vec![route("search", "", 200, "<html>not json</html>")]);
    let failure = run(&config(), &request("Synthetic Show"), &fetch).expect_err("fails");
    assert!(matches!(failure.error, YoutubeError::Malformed(_)));
}

#[test]
fn a_later_lane_failure_keeps_earlier_results_as_partial() {
    let fetch = FixtureFetch::new(vec![
        route("search", CHANNEL_ONE, 200, SEARCH_LIST),
        route("search", CHANNEL_TWO, 403, ERROR_QUOTA_EXCEEDED),
        route("videos", "", 200, VIDEOS_LIST),
    ]);
    let failure = run(&channel_config(), &request("Synthetic Show"), &fetch).expect_err("partial");
    assert_eq!(failure.error, YoutubeError::QuotaExceeded);
    let partial = failure.partial.clone().expect("partial results");
    assert_eq!(partial.results.len(), 4);
    assert_eq!(partial.next_cursor, None);

    let error = crate::plugin_error(&failure.error, failure.partial);
    assert_eq!(error.code, PluginErrorCode::RateLimited);
    match error.details {
        Some(PluginErrorDetails::IndexerSearch(IndexerSearchPluginError::PartialResults {
            response,
            reason,
            ..
        })) => {
            assert_eq!(reason, IndexerSearchIncompleteReason::RateLimited);
            assert_eq!(response.results.len(), 4);
        }
        other => panic!("unexpected details {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Uploads flow
// ---------------------------------------------------------------------------

fn uploads_routes() -> Vec<Route> {
    vec![
        route("channels", "", 200, CHANNELS_LIST),
        route(
            "playlistItems",
            "UUsyntheticChannel000001",
            200,
            PLAYLIST_ITEMS,
        ),
        route(
            "playlistItems",
            "UUsyntheticChannel000002",
            200,
            PLAYLIST_ITEMS_LAST,
        ),
        route("videos", "", 200, VIDEOS_LIST),
    ]
}

#[test]
fn allow_list_with_an_episode_reads_uploads_instead_of_searching() {
    let fetch = FixtureFetch::new(uploads_routes());
    let mut request = request("Synthetic Show");
    request.season = Some(1);
    request.episode = Some(2);
    assert_eq!(
        engine::choose_mode(&channel_config(), &request),
        Mode::Uploads
    );
    let outcome = run(&channel_config(), &request, &fetch).expect("page");

    assert!(fetch.calls_to("search").is_empty());
    assert_eq!(fetch.calls_to("channels").len(), 1);
    assert_eq!(fetch.calls_to("playlistItems").len(), 2);
    assert_eq!(fetch.calls_to("videos").len(), 1);

    let results = &outcome.response.results;
    // The vlog does not match the title and is filtered out locally.
    let ids = results
        .iter()
        .map(|result| result.http.as_ref().unwrap().item_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["synthAAA001", "synthPRV010", "synthCCC003"]);
    assert_eq!(
        availability_of(results, "synthPRV010"),
        HttpAvailability::AuthRequired
    );
    let playlist = by_id(results, "synthAAA001")
        .http
        .as_ref()
        .unwrap()
        .playlist
        .clone()
        .expect("playlist position");
    assert_eq!(playlist.id, "UUsyntheticChannel000001");
    assert_eq!(playlist.index, Some(0));
    assert_eq!(playlist.count, Some(120));
    assert_eq!(
        outcome.response.next_cursor.as_deref(),
        Some("yt1.u.1.UUsyntheticChannel000001~EAAaBlBUOkNESQ")
    );
}

#[test]
fn uploads_stop_at_the_page_ceiling() {
    let fetch = FixtureFetch::new(uploads_routes());
    let mut request = request("Synthetic Show");
    request.episode = Some(2);
    request.page_cursor = Some(format!(
        "yt1.u.{}.UUsyntheticChannel000001~EAAaBlBUOkNESQ",
        UPLOADS_PAGE_CEILING - 1
    ));
    let outcome = run(&channel_config(), &request, &fetch).expect("page");
    assert!(fetch.calls_to("channels").is_empty());
    let pages = fetch.calls_to("playlistItems");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].param("pageToken"), Some("EAAaBlBUOkNESQ"));
    assert!(outcome.ceiling_reached);
    assert_eq!(outcome.response.next_cursor, None);
}

#[test]
fn unknown_allow_listed_channels_are_not_found() {
    let fetch = FixtureFetch::new(vec![route("channels", "", 200, r#"{"items":[]}"#)]);
    let mut request = request("Synthetic Show");
    request.episode = Some(2);
    let failure = run(&channel_config(), &request, &fetch).expect_err("not found");
    assert!(matches!(failure.error, YoutubeError::NotFound { .. }));
    assert_eq!(failure.warnings.len(), 2);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn error_bodies_classify_distinctly() {
    let classify = |status: u16, body: &str| classify_error(status, body.as_bytes());
    assert_eq!(classify(400, ERROR_KEY_INVALID), YoutubeError::InvalidKey);
    assert_eq!(
        classify(400, ERROR_KEY_INVALID_LEGACY),
        YoutubeError::InvalidKey
    );
    assert_eq!(classify(403, ERROR_FORBIDDEN), YoutubeError::InvalidKey);
    assert_eq!(
        classify(403, ERROR_ACCESS_NOT_CONFIGURED),
        YoutubeError::ApiDisabled
    );
    assert_eq!(
        classify(403, ERROR_QUOTA_EXCEEDED),
        YoutubeError::QuotaExceeded
    );
    assert_eq!(
        classify(404, ERROR_NOT_FOUND),
        YoutubeError::NotFound {
            reason: "playlistNotFound".to_string()
        }
    );
    assert_eq!(
        classify(
            400,
            r#"{"error":{"code":400,"message":"Invalid channel.","errors":[{"reason":"invalidChannelId"}]}}"#
        ),
        YoutubeError::BadRequest {
            reason: "invalidChannelId".to_string()
        }
    );
    assert_eq!(classify(429, ""), YoutubeError::RateLimited);
    assert_eq!(
        classify(503, "<html></html>"),
        YoutubeError::Upstream { status: 503 }
    );
}

#[test]
fn plugin_error_codes() {
    let code = |error: YoutubeError| crate::plugin_error(&error, None).code;
    assert_eq!(code(YoutubeError::InvalidKey), PluginErrorCode::AuthFailed);
    assert_eq!(
        code(YoutubeError::ApiDisabled),
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        code(YoutubeError::QuotaExceeded),
        PluginErrorCode::RateLimited
    );
    assert_eq!(
        code(YoutubeError::NotFound {
            reason: "channelNotFound".into()
        }),
        PluginErrorCode::Permanent
    );
    assert_eq!(
        code(YoutubeError::Upstream { status: 503 }),
        PluginErrorCode::UpstreamUnavailable
    );
    let quota = crate::plugin_error(&YoutubeError::QuotaExceeded, None);
    assert!(matches!(
        quota.details,
        Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::RateLimited,
                ..
            }
        ))
    ));
    // Distinct, credential-safe messages.
    let messages = [
        YoutubeError::InvalidKey,
        YoutubeError::ApiDisabled,
        YoutubeError::QuotaExceeded,
        YoutubeError::NotFound {
            reason: "videoNotFound".into(),
        },
    ]
    .map(|error| crate::plugin_error(&error, None).public_message);
    for (index, message) in messages.iter().enumerate() {
        assert!(!message.contains(KEY));
        assert!(messages[..index].iter().all(|other| other != message));
    }
}

#[test]
fn redact_strips_the_key() {
    assert_eq!(
        crate::redact(&format!("failed with key={KEY}"), KEY),
        "failed with key=[redacted]"
    );
    assert_eq!(crate::redact("nothing here", ""), "nothing here");
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

#[test]
fn iso8601_durations() {
    assert_eq!(parse_iso8601_duration("PT22M5S"), Some(1325));
    assert_eq!(parse_iso8601_duration("PT1H2M3S"), Some(3723));
    assert_eq!(parse_iso8601_duration("P1DT2H"), Some(93_600));
    assert_eq!(parse_iso8601_duration("P1W"), Some(604_800));
    assert_eq!(parse_iso8601_duration("P0D"), Some(0));
    assert_eq!(parse_iso8601_duration("PT45.9S"), Some(45));
    for bad in ["", "P", "PT", "1H", "P1M", "P1Y", "PT5", "PTXS", "P1H"] {
        assert_eq!(parse_iso8601_duration(bad), None, "{bad}");
    }
}

#[test]
fn html_entities_are_decoded() {
    assert_eq!(
        html_unescape("A &amp; B &#39;C&#39; &quot;D&quot; &lt;E&gt;"),
        "A & B 'C' \"D\" <E>"
    );
    assert_eq!(html_unescape("&#x41;&unknown; &"), "A&unknown; &");
}

#[test]
fn title_cleanup_preserves_episode_markers() {
    let channel = Some("Synthetic Channel");
    assert_eq!(
        clean_title("Synthetic Show S01E02 | Synthetic Channel", channel),
        "Synthetic Show S01E02"
    );
    assert_eq!(
        clean_title("Episode 2 | Synthetic Channel", channel),
        "Synthetic Channel - Episode 2"
    );
    assert_eq!(
        clean_title("#7 - Synthetic Channel", channel),
        "Synthetic Channel - #7"
    );
    assert_eq!(
        clean_title(
            "Synthetic Show Episode 3 [Official Video] (Full Episode)",
            channel
        ),
        "Synthetic Show Episode 3"
    );
    // Bracketed content that is not noise is kept.
    assert_eq!(
        clean_title("Synthetic Show (2026) [S01E04]", channel),
        "Synthetic Show (2026) [S01E04]"
    );
    assert_eq!(
        clean_title("  Synthetic   Show   Ep. 4  ", None),
        "Synthetic Show Ep. 4"
    );
    // An edit that would leave nothing keeps the raw title.
    assert_eq!(clean_title("[Official Video]", channel), "[Official Video]");
    assert_eq!(
        clean_title("Synthetic Channel", channel),
        "Synthetic Channel"
    );
}

#[test]
fn uploads_title_filter() {
    assert!(matches_query(
        "Synthetic Show",
        "Synthetic Show S01E02",
        None
    ));
    assert!(matches_query(
        "Synthetic Channel",
        "Episode 2",
        Some("Synthetic Channel")
    ));
    assert!(!matches_query(
        "Synthetic Show",
        "Unrelated Synthetic Vlog",
        Some("Synthetic Channel")
    ));
}

// ---------------------------------------------------------------------------
// Release mapping
// ---------------------------------------------------------------------------

fn videos(body: &str) -> Vec<api::Video> {
    serde_json::from_str::<api::VideoListResponse>(body)
        .expect("fixture")
        .items
}

fn candidate(id: &str) -> Candidate {
    Candidate {
        video_id: id.to_string(),
        ..Candidate::default()
    }
}

#[test]
fn availability_mapping() {
    let states = videos(VIDEOS_LIST_STATES);
    let find = |id: &str| states.iter().find(|video| video.id == id).unwrap();
    let check = |id: &str| release::availability(&candidate(id), Enrichment::Found(find(id)), None);
    assert_eq!(check("synthPRV006"), HttpAvailability::AuthRequired);
    assert_eq!(check("synthREJ007"), HttpAvailability::Removed);
    assert_eq!(check("synthLIV008"), HttpAvailability::Live);

    assert_eq!(
        release::availability(&candidate("synthGONE01"), Enrichment::Missing, None),
        HttpAvailability::Removed
    );
    let private = Candidate {
        listed_private: true,
        ..candidate("synthPRV010")
    };
    assert_eq!(
        release::availability(&private, Enrichment::Missing, None),
        HttpAvailability::AuthRequired
    );
    let live = Candidate {
        live_broadcast_content: Some("live".into()),
        ..candidate("synthLIV008")
    };
    assert_eq!(
        release::availability(&live, Enrichment::NotFetched, None),
        HttpAvailability::Live
    );
}

#[test]
fn release_shape() {
    let fetch = FixtureFetch::new(search_routes());
    let outcome = run(&config(), &request("Synthetic Show"), &fetch).expect("page");
    let result = by_id(&outcome.response.results, "synthAAA001");

    let page_url = "https://www.youtube.com/watch?v=synthAAA001";
    assert_eq!(result.title, "Synthetic Show S01E02 & Friends");
    assert_eq!(result.link.as_deref(), Some(page_url));
    assert_eq!(result.info_url.as_deref(), Some(page_url));
    assert_eq!(result.guid.as_deref(), Some("youtube:synthAAA001"));
    assert_eq!(result.protocol, Some(IndexerProtocol::Http));
    assert_eq!(result.source_kind, Some(IndexerSourceKind::Http));
    assert_eq!(result.languages, vec!["en".to_string()]);
    assert_eq!(result.resolution, None);
    assert_eq!(result.published_at.as_deref(), Some("2026-01-02T03:04:05Z"));

    let extra = &result.provider_extra;
    assert_eq!(
        extra["raw_title"],
        serde_json::json!("Synthetic Show S01E02 & Friends | Synthetic Channel")
    );
    assert_eq!(extra["definition"], serde_json::json!("hd"));
    assert_eq!(extra["caption"], serde_json::json!(true));
    assert_eq!(extra["duration_seconds"], serde_json::json!(1325));
    assert_eq!(
        extra["thumbnail_url"],
        serde_json::json!("https://i.ytimg.com/vi/synthAAA001/mqdefault.jpg")
    );

    let http = result.http.as_ref().expect("http release");
    assert_eq!(http.provider, "youtube");
    assert_eq!(http.item_id, "synthAAA001");
    assert_eq!(http.page_url, page_url);
    assert_eq!(http.duration_seconds, Some(1325));
    let channel = http.channel.as_ref().expect("channel");
    assert_eq!(channel.id.as_deref(), Some(CHANNEL_ONE));
    assert_eq!(channel.name, "Synthetic Channel");
    assert_eq!(http.resources.len(), 1);
    assert_eq!(http.resources[0].transport, HttpTransport::ProviderPage);
    assert_eq!(http.resources[0].url, page_url);
    assert!(http.resolve.is_none());

    let json = serde_json::to_string_pretty(result).expect("serialise");
    assert!(!json.contains(KEY));
}

// ---------------------------------------------------------------------------
// Cache key
// ---------------------------------------------------------------------------

#[test]
fn cache_key_ignores_the_api_key_but_not_the_request() {
    let request = request("Synthetic Show");
    let one = config();
    let other_key = config_with(&[("api_key", "SYNTHETIC-OTHER-KEY")]);
    assert_eq!(
        crate::cache::request_key(&one, &request),
        crate::cache::request_key(&other_key, &request)
    );
    assert_ne!(
        crate::cache::request_key(&one, &request),
        crate::cache::request_key(&channel_config(), &request)
    );
    let mut paged = request.clone();
    paged.page_cursor = Some("yt1.s.1.*~CDIQAA".into());
    assert_ne!(
        crate::cache::request_key(&one, &request),
        crate::cache::request_key(&one, &paged)
    );
    // Without a host clock nothing is cached.
    assert!(crate::cache::lookup(&crate::cache::request_key(&one, &request)).is_none());
}
