use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListMediaKind, ListPluginFetchRequest, ListPluginFetchResponse, ListPluginHealthRequest,
    PluginDescriptor, PluginErrorCode,
};

use super::*;

const KEY: &str = "fixturemdbkey";
const PUBLIC_URL: &str = "https://mdblist.com/lists/fixture-curator/fixture-picks/json";

const PUBLIC_JSON: &str = r#"[
  {"id": 990501, "rank": 2, "adult": 0, "title": "Fixture Film Two", "imdb_id": "tt9900502", "tvdb_id": null,
   "language": "en", "mediatype": "movie", "release_year": 2032},
  {"id": 990500, "rank": 1, "adult": 0, "title": "Fixture Film One", "imdb_id": "tt9900501", "tvdb_id": null,
   "language": "en", "mediatype": "movie", "release_year": 2031},
  {"id": 880503, "rank": 3, "adult": 0, "title": "Fixture Show Three", "imdb_id": "tt9900503", "tvdb_id": 770503,
   "language": "ko", "mediatype": "show", "release_year": 2030},
  {"rank": 4, "title": "", "mediatype": "movie"}
]"#;

const API_JSON: &str = r#"{
  "movies": [
    {"id": 990600, "rank": 1, "title": "Fixture Api Film", "imdb_id": "tt9900600",
     "ids": {"mdblist": "m990600", "imdb": "tt9900600", "tmdb": 990600, "tvdb": null},
     "language": "en", "mediatype": "movie", "release_year": 2034}
  ],
  "shows": [
    {"id": 880601, "rank": 2, "title": "Fixture Api Show", "imdb_id": "tt9900601", "tvdb_id": 770601,
     "language": "en", "mediatype": "show", "release_year": 2033}
  ]
}"#;

fn request(list: &str, cursor: Option<&str>, since: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: SOURCE_LIST.to_string(),
        params: BTreeMap::from([(PARAM_LIST.to_string(), list.to_string())]),
        credential: None,
        page_cursor: cursor.map(str::to_string),
        since_fingerprint: since.map(str::to_string),
    }
}

fn api_url(path: &str, offset: u32) -> String {
    format!("{API_BASE}{path}?limit={PAGE_LIMIT}&offset={offset}&apikey={KEY}")
}

fn fetch_ok(
    http: &RecordedHttp,
    key: Option<&str>,
    request: ListPluginFetchRequest,
) -> ListPluginFetchResponse {
    block_on(fetch(http, key, &request)).unwrap()
}

#[test]
fn descriptor_round_trips_and_passes_host_checks() {
    let original = descriptor();
    let decoded: PluginDescriptor =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
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
    let list = decoded.list_provider().unwrap();
    assert_eq!(list.provider_type, "mdblist");
    assert_eq!(
        list.auth,
        ListProviderAuth::ServerApiKey {
            config_field: CONFIG_API_KEY.to_string()
        }
    );
    assert!(
        !list.config_fields[0].required,
        "public lists work without a key"
    );
    assert_eq!(
        list.allowed_hosts,
        vec!["api.mdblist.com".to_string(), "mdblist.com".to_string()]
    );
}

#[test]
fn url_pattern_and_list_refs_agree() {
    let list = descriptor().list_provider().cloned().unwrap();
    let pattern = regex::Regex::new(&list.url_patterns[0].pattern).unwrap();
    for address in [
        "https://mdblist.com/lists/fixture-curator/fixture-picks",
        "https://www.mdblist.com/lists/fixture-curator/fixture-picks/json",
        "https://mdblist.com/lists/fixture-curator/fixture-picks/?sort=rank",
    ] {
        let captured = pattern.captures(address).unwrap()["list"].to_string();
        assert_eq!(captured, "fixture-curator/fixture-picks", "{address}");
        assert_eq!(
            parse_list_ref(address).unwrap(),
            parse_list_ref(&captured).unwrap()
        );
    }
    assert!(!pattern.is_match("https://mdblist.com/lists/fixture-curator"));
    assert!(!pattern.is_match("https://mdblist.com.example.test/lists/a/b"));
    assert_eq!(
        parse_list_ref(" 4242 ").unwrap(),
        ListRef::Id("4242".to_string())
    );
    assert!(parse_list_ref("fixture-curator").is_err());
    assert!(parse_list_ref("a/b/c").is_err());
    assert!(parse_list_ref("../etc/x").is_err());
}

#[test]
fn public_json_is_read_without_a_key_in_rank_order() {
    let http = RecordedHttp::new().with(PUBLIC_URL, 200, PUBLIC_JSON);
    let response = fetch_ok(
        &http,
        None,
        request("fixture-curator/fixture-picks", None, None),
    );
    assert_eq!(http.urls(), vec![PUBLIC_URL.to_string()]);
    assert_eq!(
        response.list_url.as_deref(),
        Some("https://mdblist.com/lists/fixture-curator/fixture-picks")
    );
    assert!(response.next_cursor.is_none());
    let keys: Vec<_> = response
        .items
        .iter()
        .map(|item| (item.item_key.as_str(), item.rank))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("tmdb:movie:990500", Some(1)),
            ("tmdb:movie:990501", Some(2)),
            ("tmdb:series:880503", Some(3))
        ]
    );
    let show = &response.items[2];
    assert_eq!(show.kind_hint, Some(ListMediaKind::Series));
    assert_eq!(show.year, Some(2030));
    assert_eq!(show.language.as_deref(), Some("ko"));
    let sources: Vec<_> = show
        .external_ids
        .iter()
        .map(|id| id.source.as_str())
        .collect();
    assert_eq!(sources, vec!["tmdb", "imdb", "tvdb"]);

    let again = fetch_ok(
        &http,
        None,
        request(
            "fixture-curator/fixture-picks",
            None,
            response.fingerprint.as_deref(),
        ),
    );
    assert!(again.unchanged);
}

#[test]
fn api_key_reads_the_items_endpoint_and_pages_on_has_more() {
    let path = "/lists/fixture-curator/fixture-picks/items";
    let http = RecordedHttp::new()
        .with_headers(&api_url(path, 0), 200, &[("X-Has-More", "true"), ("X-Total-Items", "3")], API_JSON)
        .with_headers(&api_url(path, 2), 200, &[("X-Has-More", "false")], r#"{"movies": [{"id": 990602, "rank": 3, "title": "Fixture Api Tail", "mediatype": "movie"}], "shows": []}"#);
    let first = fetch_ok(
        &http,
        Some(KEY),
        request("fixture-curator/fixture-picks", None, None),
    );
    assert_eq!(first.next_cursor.as_deref(), Some("2"));
    assert_eq!(first.total_hint, Some(3));
    assert!(first.fingerprint.is_none());
    assert_eq!(first.items[0].item_key, "tmdb:movie:990600");
    assert_eq!(first.items[1].item_key, "tmdb:series:880601");

    let second = fetch_ok(
        &http,
        Some(KEY),
        request("fixture-curator/fixture-picks", Some("2"), None),
    );
    assert!(second.next_cursor.is_none());
    assert_eq!(second.items[0].rank, Some(3));
}

#[test]
fn single_api_page_is_fingerprinted_and_numeric_ids_need_the_key() {
    let http = RecordedHttp::new().with_headers(
        &api_url("/lists/4242/items", 0),
        200,
        &[("X-Has-More", "false")],
        API_JSON,
    );
    let response = fetch_ok(&http, Some(KEY), request("4242", None, None));
    assert!(response.fingerprint.is_some());
    assert!(response.list_url.is_none());

    let error = block_on(fetch(
        &RecordedHttp::new(),
        None,
        &request("4242", None, None),
    ))
    .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
}

#[test]
fn errors_map_to_host_failure_classes() {
    let path = "/lists/fixture-curator/fixture-picks/items";
    let with_key = |status: u16, headers: &[(&str, &str)], body: &str| {
        let http = RecordedHttp::new().with_headers(&api_url(path, 0), status, headers, body);
        block_on(fetch(
            &http,
            Some(KEY),
            &request("fixture-curator/fixture-picks", None, None),
        ))
        .unwrap_err()
    };
    assert_eq!(
        with_key(401, &[], r#"{"error":"Invalid API key"}"#).code,
        PluginErrorCode::AuthFailed
    );
    let denied = with_key(403, &[], r#"{"error":"Permission denied"}"#);
    assert!(denied.public_message.contains("not found"));
    let missing = with_key(404, &[], r#"{"error":"Not found"}"#);
    assert_eq!(missing.code, PluginErrorCode::Permanent);
    assert!(missing.public_message.contains("not found"));
    let limited = with_key(
        429,
        &[("Retry-After", "3600")],
        r#"{"error":"Daily API limit exceeded!"}"#,
    );
    assert_eq!(
        (limited.code, limited.retry_after_seconds),
        (PluginErrorCode::RateLimited, Some(3600))
    );
    assert_eq!(
        with_key(500, &[], "").code,
        PluginErrorCode::UpstreamUnavailable
    );

    let public = |status: u16, body: &str| {
        let http = RecordedHttp::new().with(PUBLIC_URL, status, body);
        block_on(fetch(
            &http,
            None,
            &request("fixture-curator/fixture-picks", None, None),
        ))
        .unwrap_err()
    };
    assert_eq!(public(401, "").code, PluginErrorCode::Permanent);
    assert!(
        public(200, r#"{"error":"List not found"}"#)
            .public_message
            .contains("not found")
    );
    assert_eq!(
        public(200, "<html></html>").code,
        PluginErrorCode::Permanent
    );

    let unknown = ListPluginFetchRequest {
        source_type: "toplists".to_string(),
        ..request("a/b", None, None)
    };
    assert_eq!(
        block_on(fetch(&RecordedHttp::new(), None, &unknown))
            .unwrap_err()
            .code,
        PluginErrorCode::Unsupported
    );
}

#[test]
fn health_checks_the_key_when_one_is_set() {
    let health = |http: &RecordedHttp, key: Option<&str>| match block_on(run(
        http,
        key,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(scryer_plugin_sdk::PluginResult::Ok(health)) => health,
        other => panic!("unexpected {other:?}"),
    };
    let user_url = format!("{API_BASE}/user?apikey={KEY}");
    let good = RecordedHttp::new().with(
        &user_url,
        200,
        r#"{"api_requests": 1000, "api_requests_count": 12, "username": "fixture"}"#,
    );
    let healthy = health(&good, Some(KEY));
    assert!(healthy.healthy);
    assert_eq!(
        healthy.message.as_deref(),
        Some("12 of 1000 daily API requests used")
    );
    let bad = RecordedHttp::new().with(&user_url, 401, r#"{"error":"Invalid API key"}"#);
    assert!(!health(&bad, Some(KEY)).healthy);
    let none = RecordedHttp::new();
    assert!(health(&none, None).healthy);
    assert!(none.urls().is_empty());
}
