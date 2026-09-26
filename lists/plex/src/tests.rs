use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListMediaKind, ListPluginFetchRequest, ListPluginHealthRequest, PluginDescriptor,
    PluginErrorCode, PluginResult,
};

use super::*;

const FEED_URL: &str = "https://rss.plex.tv/0f1e2d3c-fixture-feed";

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
    assert_eq!(list.auth, ListProviderAuth::None);
    assert!(!list.capabilities.requires_member_credential);
    assert_eq!(list.allowed_hosts, vec!["rss.plex.tv".to_string()]);
    assert!(
        list.config_fields.is_empty(),
        "no credential may live in plugin config"
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
