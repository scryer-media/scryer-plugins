use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListMediaKind, ListPluginFetchRequest, ListPluginHealthRequest, PluginDescriptor,
    PluginErrorCode, PluginResult,
};
use serde_json::json;

use super::*;

const FEED_URL: &str = "https://rss.plex.tv/0f1e2d3c-fixture-feed";
const TOKEN: &str = "fixture-plex-token-0001";
/// 2030-06-15 12:00 UTC.
const NOW_MS: u64 = 22_080 * MILLIS_PER_DAY + 12 * 60 * 60 * 1000;

const WATCHLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/">
  <channel>
    <title>Fixture Member's Watchlist</title>
    <link>https://watch.plex.tv/watchlist</link>
    <item>
      <title>Fixture Feature One</title>
      <pubDate>Mon, 01 Jan 2035 00:00:00 +0000</pubDate>
      <link>https://watch.plex.tv/movie/fixture-feature-one</link>
      <category>movie</category>
      <guid isPermaLink="false">imdb://tt9900101</guid>
      <guid isPermaLink="false">tmdb://990101</guid>
      <guid isPermaLink="false">tvdb://770101</guid>
      <media:thumbnail url="https://images.example.test/1.jpg"/>
    </item>
    <item>
      <title>Fixture Serial Two</title>
      <link>https://watch.plex.tv/show/fixture-serial-two</link>
      <category>show</category>
      <guid isPermaLink="false">tvdb://880102</guid>
      <guid isPermaLink="false">imdb://tt9900102</guid>
    </item>
    <item>
      <title>Fixture Without Ids</title>
      <category>movie</category>
    </item>
    <item>
      <title>Fixture Feature One</title>
      <category>movie</category>
      <guid isPermaLink="false">tmdb://990101</guid>
    </item>
  </channel>
</rss>"#;

fn request(url: &str, since: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: SOURCE_WATCHLIST_RSS.to_string(),
        params: BTreeMap::from([(PARAM_URL.to_string(), url.to_string())]),
        credential: None,
        page_cursor: None,
        since_fingerprint: since.map(str::to_string),
    }
}

fn list_descriptor() -> scryer_plugin_sdk::ListProviderDescriptor {
    descriptor().list_provider().cloned().unwrap()
}

#[test]
fn descriptor_round_trips_and_passes_host_checks() {
    let original = descriptor();
    let bytes = serde_json::to_vec(&original).unwrap();
    let decoded: PluginDescriptor = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    scryer_plugin_sdk::validate_plugin_descriptor_sdk_contract(
        &decoded,
        scryer_plugin_sdk::SDK_VERSION,
    )
    .unwrap();
    scryer_plugin_sdk::validate_plugin_descriptor_host_permissions(&decoded).unwrap();

    let list = list_descriptor();
    assert_eq!(list.provider_type, "plex");
    assert_eq!(
        list.auth,
        ListProviderAuth::MemberAccount {
            flow: ListAccountFlow::PlexPin,
            exchange: ListAccountExchange::Direct,
            byo_app: false,
            scopes: Vec::new(),
        }
    );
    assert!(list.capabilities.account);
    assert!(!list.capabilities.health);
    assert!(
        !list.capabilities.requires_member_credential,
        "the RSS source must keep working with no account"
    );
    assert_eq!(
        list.allowed_hosts,
        vec![
            "rss.plex.tv".to_string(),
            "discover.provider.plex.tv".to_string(),
            "plex.tv".to_string()
        ]
    );
    assert!(
        list.config_fields.is_empty(),
        "no credential may live in plugin config"
    );
}

#[test]
fn only_the_account_group_is_personal() {
    let list = list_descriptor();
    let items: Vec<_> = list
        .groups
        .iter()
        .flat_map(|group| {
            group
                .items
                .iter()
                .map(move |item| (group.auth_badge, item.clone()))
        })
        .collect();
    assert_eq!(items.len(), 2);
    for (badge, item) in &items {
        assert_eq!(
            item.personal,
            *badge == ListAuthBadge::MemberAccount,
            "{}",
            item.id
        );
        assert_eq!(item.default_interval_seconds, 6 * 60 * 60, "{}", item.id);
    }

    let (rss_badge, rss) = &items[0];
    assert_eq!(*rss_badge, ListAuthBadge::NoAccountNeedsValue);
    assert_eq!(rss.source_type, SOURCE_WATCHLIST_RSS);
    assert!(!rss.personal);
    assert_eq!(rss.params.len(), 1);

    let (own_badge, own) = &items[1];
    assert_eq!(*own_badge, ListAuthBadge::MemberAccount);
    assert_eq!(own.source_type, SOURCE_WATCHLIST);
    assert!(own.personal);
    assert!(own.params.is_empty());
    assert_eq!(own.kinds, vec![ListMediaKind::Movie, ListMediaKind::Series]);

    assert!(
        list.url_patterns
            .iter()
            .all(|pattern| pattern.source_type == SOURCE_WATCHLIST_RSS),
        "a pasted URL may only ever create the public source"
    );
}

#[test]
fn url_pattern_recognises_only_plex_feeds() {
    let list = list_descriptor();
    let pattern = regex::Regex::new(&list.url_patterns[0].pattern).unwrap();
    let captures = pattern
        .captures("https://rss.plex.tv/0f1e2d3c-fixture-feed/")
        .unwrap();
    assert_eq!(&captures["url"], FEED_URL);
    assert!(!pattern.is_match("https://rss.plex.tv.example.test/abc"));
    assert!(!pattern.is_match("http://rss.plex.tv/abc"));
    assert!(!pattern.is_match("https://rss.plex.tv/abc?x=1"));
}

#[test]
fn fetch_maps_guids_and_categories_and_skips_idless_entries() {
    let http = RecordedHttp::new().with(FEED_URL, 200, WATCHLIST);
    let response = block_on(fetch(&http, &request(FEED_URL, None))).unwrap();

    assert_eq!(http.urls(), vec![FEED_URL.to_string()]);
    assert_eq!(
        response.list_name.as_deref(),
        Some("Fixture Member's Watchlist")
    );
    assert_eq!(response.list_url.as_deref(), Some(FEED_URL));
    assert!(response.next_cursor.is_none());
    assert_eq!(response.items.len(), 2);

    let movie = &response.items[0];
    assert_eq!(movie.item_key, "tmdb:movie:990101");
    assert_eq!(movie.rank, Some(1));
    assert_eq!(movie.kind_hint, Some(ListMediaKind::Movie));
    let sources: Vec<_> = movie
        .external_ids
        .iter()
        .map(|id| (id.source.as_str(), id.id.as_str(), id.kind.as_deref()))
        .collect();
    assert_eq!(
        sources,
        vec![
            ("tmdb", "990101", Some("movie")),
            ("imdb", "tt9900101", Some("movie")),
            ("tvdb", "770101", Some("movie"))
        ]
    );

    let show = &response.items[1];
    assert_eq!(show.item_key, "imdb:tt9900102");
    assert_eq!(show.kind_hint, Some(ListMediaKind::Series));
    assert_eq!(show.rank, Some(2));
}

#[test]
fn unchanged_feed_short_circuits_on_the_stored_fingerprint() {
    let http = RecordedHttp::new().with(FEED_URL, 200, WATCHLIST);
    let first = block_on(fetch(&http, &request(FEED_URL, None))).unwrap();
    let second = block_on(fetch(
        &http,
        &request(FEED_URL, first.fingerprint.as_deref()),
    ))
    .unwrap();
    assert!(second.unchanged);
    assert!(second.items.is_empty());
    assert_eq!(second.fingerprint, first.fingerprint);
}

#[test]
fn non_plex_urls_are_rejected_before_any_request() {
    let http = RecordedHttp::new();
    for url in [
        "https://feeds.example.test/watchlist",
        "http://rss.plex.tv/abc",
        "https://rss.plex.tv/",
        "https://rss.plex.tv/a?b=c",
    ] {
        let error = block_on(fetch(&http, &request(url, None))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{url}");
    }
    let missing = ListPluginFetchRequest {
        params: BTreeMap::new(),
        ..request(FEED_URL, None)
    };
    assert_eq!(
        block_on(fetch(&http, &missing)).unwrap_err().code,
        PluginErrorCode::InvalidConfig
    );
    assert!(http.urls().is_empty());
}

#[test]
fn upstream_failures_map_to_host_classes() {
    let cases = [
        (404, PluginErrorCode::Permanent, true),
        (403, PluginErrorCode::Permanent, true),
        (429, PluginErrorCode::RateLimited, false),
        (502, PluginErrorCode::UpstreamUnavailable, false),
    ];
    for (status, code, not_found) in cases {
        let http =
            RecordedHttp::new().with_headers(FEED_URL, status, &[("Retry-After", "120")], "");
        let error = block_on(fetch(&http, &request(FEED_URL, None))).unwrap_err();
        assert_eq!(error.code, code, "{status}");
        assert_eq!(
            error.public_message.contains("not found"),
            not_found,
            "{status}"
        );
        if status == 429 {
            assert_eq!(error.retry_after_seconds, Some(120));
        }
    }
    let html = RecordedHttp::new().with(FEED_URL, 200, "<html><body>sign in</body></html>");
    assert_eq!(
        block_on(fetch(&html, &request(FEED_URL, None)))
            .unwrap_err()
            .code,
        PluginErrorCode::Permanent
    );
}

#[test]
fn unknown_sources_and_other_commands_are_unsupported() {
    let http = RecordedHttp::new();
    let other = ListPluginFetchRequest {
        source_type: "friends".to_string(),
        ..request(FEED_URL, None)
    };
    assert_eq!(
        block_on(fetch(&http, &other)).unwrap_err().code,
        PluginErrorCode::Unsupported
    );
    match block_on(run(
        &http,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(PluginResult::Err(error)) => {
            assert_eq!(error.code, PluginErrorCode::Unsupported)
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn credential(token: &str) -> ListCredential {
    ListCredential {
        access_token: token.to_string(),
        token_type: None,
        external_user_id: Some("990001".to_string()),
        username: Some("fixture_member".to_string()),
    }
}

fn watchlist_request(token: Option<&str>, cursor: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: SOURCE_WATCHLIST.to_string(),
        params: BTreeMap::new(),
        credential: token.map(credential),
        page_cursor: cursor.map(str::to_string),
        since_fingerprint: None,
    }
}

fn watchlist_url(offset: u32) -> String {
    format!(
        "https://discover.provider.plex.tv/library/sections/watchlist/all?includeGuids=1\
         &excludeElements=Image&sort=watchlistedAt%3Adesc\
         &X-Plex-Container-Start={offset}&X-Plex-Container-Size=100"
    )
}

fn container(offset: u32, total: Option<u64>, entries: Vec<Value>) -> String {
    let mut container = json!({
        "offset": offset,
        "size": entries.len(),
        "identifier": "tv.plex.provider.discover",
        "Metadata": entries,
    });
    if let Some(total) = total {
        container["totalSize"] = json!(total);
    }
    json!({ "MediaContainer": container }).to_string()
}

fn movie(n: u32) -> Value {
    json!({
        "ratingKey": format!("fixture{n:04}"),
        "type": "movie",
        "title": format!("Fixture Feature {n}"),
        "year": 2020,
        "Guid": [{ "id": format!("tmdb://99{n:04}") }],
    })
}

/// The same two titles as the RSS fixture, plus one Plex knows only by its
/// own guid.
fn own_watchlist() -> String {
    container(
        0,
        Some(3),
        vec![
            json!({
                "ratingKey": "fixture0101",
                "guid": "plex://movie/fixture0101",
                "type": "movie",
                "title": "Fixture Feature One",
                "year": 2035,
                "originallyAvailableAt": "2035-01-01",
                "Guid": [
                    { "id": "imdb://tt9900101" },
                    { "id": "tmdb://990101" },
                    { "id": "tvdb://770101" }
                ]
            }),
            json!({
                "ratingKey": "fixture0102",
                "guid": "plex://show/fixture0102",
                "type": "show",
                "title": "Fixture Serial Two",
                "year": 2021,
                "originallyAvailableAt": "2021-03-04",
                "Guid": [
                    { "id": "tvdb://880102" },
                    { "id": "imdb://tt9900102" }
                ]
            }),
            json!({
                "ratingKey": "fixture0103",
                "guid": "plex://movie/fixture0103",
                "type": "movie",
                "title": "Fixture Without Ids",
                "Guid": [{ "id": "plex://movie/fixture0103" }]
            }),
        ],
    )
}

/// The client identity every member request carries.
fn assert_plex_identity(request: &PluginHttpRequest) {
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    assert_eq!(header("X-Plex-Client-Identifier"), Some("scryer"));
    assert_eq!(header("X-Plex-Product"), Some("Scryer"));
    assert_eq!(header("X-Plex-Version"), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(header("X-Plex-Platform"), Some("Scryer"));
    assert_eq!(header("X-Plex-Device-Name"), Some("Scryer"));
    assert!(
        !request.url.contains("X-Plex-Client-Identifier"),
        "identity goes in headers"
    );
}

fn token_header(request: &PluginHttpRequest) -> Option<&str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(TOKEN_HEADER))
        .map(|(_, value)| value.as_str())
}

#[test]
fn own_watchlist_reads_with_the_token_header_and_matches_rss_keys() {
    let http = RecordedHttp::new().with(&watchlist_url(0), 200, &own_watchlist());
    let response = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap();

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, watchlist_url(0));
    assert_eq!(requests[0].method.as_deref(), Some("GET"));
    assert_eq!(token_header(&requests[0]), Some(TOKEN));
    assert_eq!(
        requests[0].headers.get("Accept").map(String::as_str),
        Some("application/json")
    );
    assert!(!requests[0].url.contains(TOKEN));
    assert!(!requests[0].url.contains("X-Plex-Token"));
    assert_plex_identity(&requests[0]);

    assert!(response.next_cursor.is_none());
    assert!(response.fingerprint.is_none());
    assert!(!response.unchanged);
    assert_eq!(response.total_hint, Some(3));
    assert_eq!(response.items.len(), 2);

    let movie = &response.items[0];
    assert_eq!(movie.rank, Some(1));
    assert_eq!(movie.kind_hint, Some(ListMediaKind::Movie));
    assert_eq!(movie.title.as_deref(), Some("Fixture Feature One"));
    assert_eq!(movie.year, Some(2035));
    let sources: Vec<_> = movie
        .external_ids
        .iter()
        .map(|id| (id.source.as_str(), id.id.as_str(), id.kind.as_deref()))
        .collect();
    assert_eq!(
        sources,
        vec![
            ("tmdb", "990101", Some("movie")),
            ("imdb", "tt9900101", Some("movie")),
            ("tvdb", "770101", Some("movie"))
        ]
    );
    let show = &response.items[1];
    assert_eq!(show.rank, Some(2));
    assert_eq!(show.kind_hint, Some(ListMediaKind::Series));

    let feed_http = RecordedHttp::new().with(FEED_URL, 200, WATCHLIST);
    let feed = block_on(fetch(&feed_http, &request(FEED_URL, None))).unwrap();
    let keys = |items: &[ListPluginItem]| -> Vec<String> {
        items.iter().map(|item| item.item_key.clone()).collect()
    };
    assert_eq!(keys(&response.items), keys(&feed.items));
    assert_eq!(
        keys(&response.items),
        vec![
            "tmdb:movie:990101".to_string(),
            "imdb:tt9900102".to_string()
        ]
    );
}

#[test]
fn the_public_feed_never_receives_the_member_token() {
    let http = RecordedHttp::new().with(FEED_URL, 200, WATCHLIST);
    let with_credential = ListPluginFetchRequest {
        credential: Some(credential(TOKEN)),
        ..request(FEED_URL, None)
    };
    block_on(fetch_at(&http, &with_credential, NOW_MS)).unwrap();
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, FEED_URL);
    assert_eq!(token_header(&requests[0]), None);
    assert!(
        !requests[0].headers.contains_key("X-Plex-Client-Identifier"),
        "the public feed is read anonymously"
    );
    assert!(
        requests[0]
            .headers
            .values()
            .all(|value| !value.contains(TOKEN))
    );
}

#[test]
fn watchlist_pages_by_container_offset() {
    // Plex answered with fewer titles than asked: the next page starts where
    // this one ended, and ranks continue from the offset.
    let http = RecordedHttp::new()
        .with(
            &watchlist_url(0),
            200,
            &container(0, Some(5), (1..=3).map(movie).collect()),
        )
        .with(
            &watchlist_url(3),
            200,
            &container(3, Some(5), (4..=5).map(movie).collect()),
        );
    let first = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap();
    assert_eq!(first.next_cursor.as_deref(), Some("1:3"));
    assert_eq!(first.total_hint, Some(5));
    assert!(first.fingerprint.is_none());
    let ranks: Vec<_> = first.items.iter().map(|item| item.rank).collect();
    assert_eq!(ranks, vec![Some(1), Some(2), Some(3)]);

    let second = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), first.next_cursor.as_deref()),
        NOW_MS,
    ))
    .unwrap();
    assert!(second.next_cursor.is_none());
    let keyed: Vec<_> = second
        .items
        .iter()
        .map(|item| (item.item_key.as_str(), item.rank))
        .collect();
    assert_eq!(
        keyed,
        vec![
            ("tmdb:movie:990004", Some(4)),
            ("tmdb:movie:990005", Some(5))
        ]
    );
    assert_eq!(http.urls(), vec![watchlist_url(0), watchlist_url(3)]);
    assert!(
        http.requests()
            .iter()
            .all(|request| token_header(request) == Some(TOKEN))
    );
}

#[test]
fn watchlist_paging_without_a_total_and_at_the_cap() {
    // The total can come from the container header instead.
    let header_total = RecordedHttp::new().with_headers(
        &watchlist_url(0),
        200,
        &[("X-Plex-Container-Total-Size", "2")],
        &container(0, None, (1..=2).map(movie).collect()),
    );
    let response = block_on(fetch_at(
        &header_total,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap();
    assert!(response.next_cursor.is_none());
    assert_eq!(response.total_hint, Some(2));

    // A watchlist past the cap fails rather than being cut short, which
    // would read as its tail leaving the list. A reported total fails it on
    // the first page.
    let cap = u64::from(MAX_PAGES * PAGE_SIZE);
    let oversized = RecordedHttp::new().with(
        &watchlist_url(0),
        200,
        &container(0, Some(cap + 1), (1..=100).map(movie).collect()),
    );
    let error = block_on(fetch_at(
        &oversized,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Permanent);

    // Short pages can use up the requests before the total is reached: the
    // last allowed request fails when Plex still has more to give.
    let last = RecordedHttp::new().with(
        &watchlist_url(1900),
        200,
        &container(1900, Some(cap), (1..=50).map(movie).collect()),
    );
    let error = block_on(fetch_at(
        &last,
        &watchlist_request(Some(TOKEN), Some("19:1900")),
        NOW_MS,
    ))
    .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Permanent);

    // A watchlist exactly at the cap ends on its last allowed request.
    let full = RecordedHttp::new().with(
        &watchlist_url(1900),
        200,
        &container(1900, Some(cap), (1..=100).map(movie).collect()),
    );
    let response = block_on(fetch_at(
        &full,
        &watchlist_request(Some(TOKEN), Some("19:1900")),
        NOW_MS,
    ))
    .unwrap();
    assert!(response.next_cursor.is_none());
    assert_eq!(response.total_hint, Some(MAX_PAGES * PAGE_SIZE));
    assert_eq!(response.items[0].rank, Some(1901));

    for cursor in ["20:2000", "7", "a:b", "1:-3"] {
        let http = RecordedHttp::new();
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(TOKEN), Some(cursor)),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{cursor}");
        assert!(http.urls().is_empty(), "{cursor}");
    }
}

#[test]
fn a_short_page_with_a_total_continues_where_it_ended() {
    let http = RecordedHttp::new().with(
        &watchlist_url(100),
        200,
        &container(100, Some(250), (1..=40).map(movie).collect()),
    );
    let response = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), Some("1:100")),
        NOW_MS,
    ))
    .unwrap();
    assert_eq!(response.next_cursor.as_deref(), Some("2:140"));
    assert_eq!(response.total_hint, Some(250));
    assert_eq!(response.items[0].rank, Some(101));
}

#[test]
fn an_empty_page_before_the_total_fails_instead_of_ending_the_list() {
    for body in [
        container(200, Some(250), Vec::new()),
        r#"{"MediaContainer": {"offset": 200, "size": 0, "totalSize": 250}}"#.to_string(),
    ] {
        let http = RecordedHttp::new().with(&watchlist_url(200), 200, &body);
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(TOKEN), Some("2:200")),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable, "{body}");
        assert!(!error.public_message.contains("not found"));
    }
}

#[test]
fn a_page_without_a_total_fails_instead_of_guessing_the_end() {
    // Plex may send short pages, so neither a short, a full nor an empty page
    // says where the watchlist ends when no total comes with it.
    for count in [0, 3, 100] {
        let http = RecordedHttp::new().with(
            &watchlist_url(0),
            200,
            &container(0, None, (1..=count).map(movie).collect()),
        );
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(TOKEN), None),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{count}");
        assert!(!error.public_message.contains("not found"), "{count}");
    }
    // An unreadable header total is no total either.
    let http = RecordedHttp::new().with_headers(
        &watchlist_url(0),
        200,
        &[("X-Plex-Container-Total-Size", "fixture")],
        &container(0, None, (1..=3).map(movie).collect()),
    );
    let error = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Permanent);
}

#[test]
fn released_follows_the_first_release_date_against_the_clock() {
    let entry = |n: u32, date: Option<&str>, year: Option<i32>| {
        let mut entry = movie(n);
        entry["year"] = json!(year);
        if let Some(date) = date {
            entry["originallyAvailableAt"] = json!(date);
        }
        entry
    };
    let body = container(
        0,
        Some(8),
        vec![
            entry(1, Some("2030-06-15"), Some(2030)),
            entry(2, Some("2030-06-16"), Some(2030)),
            entry(3, Some("2019-11-02"), Some(2019)),
            entry(4, None, Some(2029)),
            entry(5, None, Some(2031)),
            entry(6, None, Some(2030)),
            entry(7, None, None),
            entry(8, Some("not a date"), Some(2001)),
        ],
    );
    let http = RecordedHttp::new().with(&watchlist_url(0), 200, &body);
    let released: Vec<_> = block_on(fetch_at(
        &http,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap()
    .items
    .iter()
    .map(|item| item.released)
    .collect();
    assert_eq!(
        released,
        vec![
            Some(true),
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            None,
            None,
            Some(true)
        ]
    );

    // An unknown clock decides nothing.
    let unknown = block_on(fetch_at(&http, &watchlist_request(Some(TOKEN), None), 0)).unwrap();
    assert!(unknown.items.iter().all(|item| item.released.is_none()));
    // Plex lists no streaming services here, so nothing is claimed.
    assert!(
        unknown
            .items
            .iter()
            .all(|item| item.available_on.is_empty())
    );
}

#[test]
fn calendar_days_count_from_the_unix_epoch() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(1969, 12, 31), -1);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(days_from_civil(2030, 6, 15), 22_080);
    assert_eq!(epoch_day("2030-06-15"), Some(22_080));
    assert_eq!(epoch_day("2030-06-15T00:00:00Z"), Some(22_080));
    assert_eq!(epoch_day("2030-13-01"), None);
    assert_eq!(epoch_day("2030"), None);
    assert_eq!(today(NOW_MS), Some(22_080));
    assert_eq!(today(0), None);
}

const ACCOUNT_BODY: &str = r#"{
  "user": {
    "id": 990001,
    "uuid": "fixture-uuid-0001",
    "username": "fixture_member",
    "title": "Fixture Member",
    "email": "fixture.member@example.test",
    "thumb": "https://images.example.test/fixture-member.png",
    "authToken": "fixture-echoed-token"
  }
}"#;

fn account_request(token: &str) -> ListPluginAccountRequest {
    ListPluginAccountRequest {
        credential: credential(token),
    }
}

#[test]
fn account_reads_the_plex_identity_through_the_header() {
    let http = RecordedHttp::new().with(ACCOUNT_URL, 200, ACCOUNT_BODY);
    let result = block_on(run(
        &http,
        PluginListCommand::Account(account_request(TOKEN)),
    ));
    let account = match result {
        PluginListCommandResult::Account(PluginResult::Ok(account)) => account,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(account.external_user_id, "990001");
    assert_eq!(account.username, "fixture_member");
    assert_eq!(account.display_name.as_deref(), Some("Fixture Member"));
    assert_eq!(
        account.avatar_url.as_deref(),
        Some("https://images.example.test/fixture-member.png")
    );
    assert!(account.owned_lists.is_empty());
    assert!(account.statuses.is_empty());
    let serialized = serde_json::to_string(&account).unwrap();
    assert!(!serialized.contains("fixture.member@example.test"));
    assert!(!serialized.contains("fixture-echoed-token"));

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://plex.tv/users/account.json");
    assert_eq!(token_header(&requests[0]), Some(TOKEN));
    assert!(!requests[0].url.contains(TOKEN));
    assert_plex_identity(&requests[0]);
}

#[test]
fn account_falls_back_to_the_uuid_and_title() {
    let body = r#"{"user": {"uuid": "fixture-uuid-0002", "title": "Fixture Home User"}}"#;
    let http = RecordedHttp::new().with(ACCOUNT_URL, 200, body);
    let identity = block_on(account(&http, &account_request(TOKEN))).unwrap();
    assert_eq!(identity.external_user_id, "fixture-uuid-0002");
    assert_eq!(identity.username, "Fixture Home User");
    assert_eq!(identity.display_name.as_deref(), Some("Fixture Home User"));
    assert_eq!(identity.avatar_url, None);

    // Only an https avatar is passed on.
    let body = r#"{"user": {"id": 990003, "thumb": "http://images.example.test/plain.png"}}"#;
    let http = RecordedHttp::new().with(ACCOUNT_URL, 200, body);
    let identity = block_on(account(&http, &account_request(TOKEN))).unwrap();
    assert_eq!(identity.avatar_url, None);

    for body in [
        r#"{"user": {"title": "No Id"}}"#,
        r#"{"error": "x"}"#,
        "<html/>",
    ] {
        let http = RecordedHttp::new().with(ACCOUNT_URL, 200, body);
        let error = block_on(account(&http, &account_request(TOKEN))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{body}");
    }
}

#[test]
fn a_rejected_token_marks_the_account_expired() {
    for status in [401, 403] {
        let watchlist = RecordedHttp::new().with(&watchlist_url(0), status, "");
        let error = block_on(fetch_at(
            &watchlist,
            &watchlist_request(Some(TOKEN), None),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{status}");
        assert!(!error.public_message.contains(TOKEN));
        assert!(!error.public_message.contains("discover.provider"));
        assert!(error.debug_message.is_none());

        let account_http = RecordedHttp::new().with(ACCOUNT_URL, status, "");
        let error = block_on(account(&account_http, &account_request(TOKEN))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{status}");
        assert!(!error.public_message.contains(TOKEN));
    }
}

#[test]
fn other_watchlist_failures_map_to_host_classes() {
    let cases = [
        (404, PluginErrorCode::Permanent),
        (429, PluginErrorCode::RateLimited),
        (502, PluginErrorCode::UpstreamUnavailable),
    ];
    for (status, code) in cases {
        let http = RecordedHttp::new().with_headers(
            &watchlist_url(0),
            status,
            &[("Retry-After", "90")],
            "",
        );
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(TOKEN), None),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, code, "{status}");
        // A moved service must not read as the member's list being gone.
        assert!(!error.public_message.contains("not found"), "{status}");
        assert!(!error.public_message.contains(TOKEN), "{status}");
        if status == 429 {
            assert_eq!(error.retry_after_seconds, Some(90));
        }
    }
    for body in ["<html>sign in</html>", r#"{"Metadata": []}"#] {
        let http = RecordedHttp::new().with(&watchlist_url(0), 200, body);
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(TOKEN), None),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::Permanent, "{body}");
    }
    // An empty watchlist is an empty list, not a failure.
    let empty = RecordedHttp::new().with(
        &watchlist_url(0),
        200,
        r#"{"MediaContainer": {"offset": 0, "size": 0, "totalSize": 0}}"#,
    );
    let response = block_on(fetch_at(
        &empty,
        &watchlist_request(Some(TOKEN), None),
        NOW_MS,
    ))
    .unwrap();
    assert!(response.items.is_empty());
    assert!(response.next_cursor.is_none());
}

#[test]
fn a_personal_source_without_a_credential_fails_before_any_request() {
    let http = RecordedHttp::new();
    for token in [None, Some(""), Some("   ")] {
        let error = block_on(fetch_at(&http, &watchlist_request(token, None), NOW_MS)).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{token:?}");
    }
    let error = block_on(account(&http, &account_request(" "))).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::AuthFailed);

    // A token that could break the request header is refused unsent, and
    // never echoed.
    for broken in [
        "fixture\r\nX-Injected: 1",
        "fixture token",
        "fixture\ttoken",
    ] {
        let error = block_on(fetch_at(
            &http,
            &watchlist_request(Some(broken), None),
            NOW_MS,
        ))
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{broken:?}");
        assert!(!error.public_message.contains("fixture"));
        let error = block_on(account(&http, &account_request(broken))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::AuthFailed, "{broken:?}");
    }
    assert!(http.urls().is_empty());
}
