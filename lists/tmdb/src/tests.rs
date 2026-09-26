use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::command::{PluginListCommand, PluginListCommandResult};
use scryer_plugin_sdk::{
    ListMediaKind, ListPluginFetchRequest, ListPluginHealthRequest, PluginDescriptor,
    PluginErrorCode, PluginResult,
};

use super::*;

const KEY: &str = "fixturekey0123";
const BEARER: &str = "eyJhbGciOiJIUzI1NiJ9.eyJmaXh0dXJlIjp0cnVlfQ.c2lnbmF0dXJl";

fn url(path_and_query: &str) -> String {
    let separator = if path_and_query.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{API_BASE}{path_and_query}{separator}api_key={KEY}")
}

fn request(
    source_type: &str,
    params: &[(&str, &str)],
    cursor: Option<&str>,
) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: source_type.to_string(),
        params: params
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>(),
        credential: None,
        page_cursor: cursor.map(str::to_string),
        since_fingerprint: None,
    }
}

fn fetch(
    http: &RecordedHttp,
    key: Option<&str>,
    request: ListPluginFetchRequest,
) -> PluginResult<scryer_plugin_sdk::ListPluginFetchResponse> {
    match block_on(run(http, key, PluginListCommand::Fetch(request))) {
        PluginListCommandResult::Fetch(result) => result,
        other => panic!("unexpected {other:?}"),
    }
}

fn ok<T: std::fmt::Debug>(result: PluginResult<T>) -> T {
    match result {
        PluginResult::Ok(value) => value,
        PluginResult::Err(error) => panic!("unexpected error {error:?}"),
    }
}

fn err<T: std::fmt::Debug>(result: PluginResult<T>) -> scryer_plugin_sdk::PluginError {
    match result {
        PluginResult::Err(error) => error,
        PluginResult::Ok(value) => panic!("unexpected success {value:?}"),
    }
}

const LIST_PAGE_1: &str = r#"{
  "id": 8100001, "name": "Fixture Picks", "item_count": 23, "page": 1, "total_pages": 2, "total_results": 23,
  "items": [
    {"id": 990201, "media_type": "movie", "title": "Fixture Feature Alpha", "release_date": "2031-03-04",
     "vote_average": 7.4, "vote_count": 120, "genre_ids": [18, 53], "original_language": "en"},
    {"id": 990202, "media_type": "tv", "name": "Fixture Serial Beta", "first_air_date": "2029-10-01",
     "vote_average": 0, "vote_count": 0, "genre_ids": [10765], "original_language": "ja"},
    {"id": 990203, "media_type": "person", "name": "Fixture Person"},
    {"id": 990201, "media_type": "movie", "title": "Fixture Feature Alpha", "release_date": "2031-03-04"}
  ]
}"#;

const LIST_PAGE_2: &str = r#"{
  "id": 8100001, "name": "Fixture Picks", "page": 2, "total_pages": 2, "total_results": 23,
  "items": [{"id": 990204, "media_type": "movie", "title": "Fixture Feature Gamma", "release_date": ""}]
}"#;

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
    assert_eq!(list.provider_type, "tmdb");
    assert_eq!(
        list.auth,
        ListProviderAuth::ServerApiKey {
            config_field: CONFIG_API_KEY.to_string()
        }
    );
    assert!(list.capabilities.health);
    assert!(
        list.config_fields
            .iter()
            .any(|field| field.key == CONFIG_API_KEY)
    );
    assert_eq!(list.allowed_hosts, vec!["api.themoviedb.org".to_string()]);
    let sources: Vec<_> = list
        .groups
        .iter()
        .flat_map(|group| &group.items)
        .map(|item| item.source_type.as_str())
        .collect();
    assert_eq!(sources, vec!["list", "person", "company", "keyword"]);
    // Every source a pattern produces is one the descriptor declares, and
    // every capture targets a declared parameter.
    for pattern in &list.url_patterns {
        let item = list.groups[0]
            .items
            .iter()
            .find(|item| item.source_type == pattern.source_type)
            .unwrap();
        for capture in &pattern.captures {
            assert!(item.params.iter().any(|param| param.key == capture.param));
        }
    }
}

#[test]
fn url_patterns_extract_numeric_ids_from_site_addresses() {
    let list = descriptor().list_provider().cloned().unwrap();
    let cases = [
        (
            "https://www.themoviedb.org/list/8100001",
            "list",
            "list_id",
            "8100001",
        ),
        (
            "https://www.themoviedb.org/person/4400-fixture-person?language=en",
            "person",
            "person_id",
            "4400",
        ),
        (
            "https://themoviedb.org/company/77/movie",
            "company",
            "company_id",
            "77",
        ),
        (
            "https://www.themoviedb.org/keyword/9951-fixture-keyword/tv",
            "keyword",
            "keyword_id",
            "9951",
        ),
    ];
    for (address, source, param, id) in cases {
        let matched = list
            .url_patterns
            .iter()
            .find_map(|pattern| {
                let regex = regex::Regex::new(&pattern.pattern).unwrap();
                regex.captures(address).map(|captures| {
                    (
                        pattern,
                        captures[pattern.captures[0].group.as_str()].to_string(),
                    )
                })
            })
            .unwrap_or_else(|| panic!("no pattern for {address}"));
        assert_eq!(matched.0.source_type, source);
        assert_eq!(matched.0.captures[0].param, param);
        assert_eq!(matched.1, id);
    }
    let any = |address: &str| {
        list.url_patterns.iter().any(|pattern| {
            regex::Regex::new(&pattern.pattern)
                .unwrap()
                .is_match(address)
        })
    };
    assert!(!any("https://www.themoviedb.org/movie/990201"));
    assert!(!any("https://themoviedb.org.example.test/list/1"));
}

#[test]
fn list_pages_through_the_cursor_with_global_ranks() {
    let http = RecordedHttp::new()
        .with(&url("/list/8100001?page=1"), 200, LIST_PAGE_1)
        .with(&url("/list/8100001?page=2"), 200, LIST_PAGE_2);
    let first = ok(fetch(
        &http,
        Some(KEY),
        request("list", &[("list_id", "8100001")], None),
    ));
    assert_eq!(first.list_name.as_deref(), Some("Fixture Picks"));
    assert_eq!(
        first.list_url.as_deref(),
        Some("https://www.themoviedb.org/list/8100001")
    );
    assert_eq!(first.next_cursor.as_deref(), Some("2"));
    assert_eq!(first.total_hint, Some(23));
    assert!(first.fingerprint.is_none());
    assert_eq!(
        first.items.len(),
        2,
        "the person and the duplicate are dropped"
    );

    let movie = &first.items[0];
    assert_eq!(movie.item_key, "tmdb:movie:990201");
    assert_eq!(movie.rank, Some(1));
    assert_eq!(movie.year, Some(2031));
    assert_eq!(
        movie.genres,
        vec!["Drama".to_string(), "Thriller".to_string()]
    );
    assert_eq!(movie.language.as_deref(), Some("en"));
    assert_eq!(
        movie
            .provider_rating
            .as_ref()
            .map(|rating| (rating.scale.as_str(), rating.value)),
        Some(("tmdb", 7.4))
    );
    assert_eq!(movie.external_ids[0].kind.as_deref(), Some("movie"));

    let show = &first.items[1];
    assert_eq!(show.item_key, "tmdb:series:990202");
    assert_eq!(show.kind_hint, Some(ListMediaKind::Series));
    assert!(show.provider_rating.is_none(), "no votes, no rating");

    let second = ok(fetch(
        &http,
        Some(KEY),
        request(
            "list",
            &[("list_id", "8100001")],
            first.next_cursor.as_deref(),
        ),
    ));
    assert!(second.next_cursor.is_none());
    assert_eq!(second.items[0].rank, Some(21));
    assert_eq!(second.items[0].year, None);
}

#[test]
fn bearer_tokens_go_in_the_header_not_the_url() {
    let http =
        RecordedHttp::new().with(&format!("{API_BASE}/list/8100001?page=1"), 200, LIST_PAGE_1);
    ok(fetch(
        &http,
        Some(BEARER),
        request("list", &[("list_id", "8100001")], None),
    ));
    let sent = &http.requests()[0];
    assert!(!sent.url.contains("api_key"));
    assert_eq!(
        sent.headers.get("Authorization").map(String::as_str),
        Some(format!("Bearer {BEARER}").as_str())
    );
}

#[test]
fn person_credits_are_filtered_sorted_and_fingerprinted() {
    let body = r#"{
      "id": 4400, "name": "Fixture Person",
      "combined_credits": {
        "cast": [
          {"id": 990301, "media_type": "movie", "title": "Fixture Early", "release_date": "2021-01-01"},
          {"id": 990302, "media_type": "tv", "name": "Fixture Late Show", "first_air_date": "2030-01-01", "genre_ids": [10767]},
          {"id": 990303, "media_type": "tv", "name": "Fixture Serial", "first_air_date": "2028-05-05", "genre_ids": [18]},
          {"id": 990304, "media_type": "movie", "title": "Fixture Recent", "release_date": "2032-02-02"}
        ],
        "crew": [
          {"id": 990301, "media_type": "movie", "title": "Fixture Early", "release_date": "2021-01-01", "job": "Writer"},
          {"id": 990305, "media_type": "movie", "title": "Fixture Directed", "release_date": "2025-07-07", "job": "Director"}
        ]
      }
    }"#;
    let path = "/person/4400?append_to_response=combined_credits";
    let http = RecordedHttp::new().with(&url(path), 200, body);

    let cast = ok(fetch(
        &http,
        Some(KEY),
        request("person", &[("person_id", "4400-fixture-person")], None),
    ));
    let keys: Vec<_> = cast
        .items
        .iter()
        .map(|item| item.item_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            "tmdb:movie:990304",
            "tmdb:series:990303",
            "tmdb:movie:990301"
        ]
    );
    assert_eq!(cast.list_name.as_deref(), Some("Fixture Person"));
    assert!(cast.fingerprint.is_some());

    let all_movies = ok(fetch(
        &http,
        Some(KEY),
        request(
            "person",
            &[("person_id", "4400"), ("credit", "all"), ("kind", "movie")],
            None,
        ),
    ));
    let keys: Vec<_> = all_movies
        .items
        .iter()
        .map(|item| item.item_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            "tmdb:movie:990304",
            "tmdb:movie:990305",
            "tmdb:movie:990301"
        ]
    );

    let mut again = request("person", &[("person_id", "4400")], None);
    again.since_fingerprint = cast.fingerprint.clone();
    let unchanged = ok(fetch(&http, Some(KEY), again));
    assert!(unchanged.unchanged);
    assert!(unchanged.items.is_empty());
}

#[test]
fn company_and_keyword_use_discover_with_a_page_cap() {
    let body = r#"{"page": 50, "total_pages": 500, "total_results": 10000,
      "results": [{"id": 990401, "name": "Fixture Company Serial", "first_air_date": "2033-01-01"}]}"#;
    let http = RecordedHttp::new()
        .with(&url("/discover/tv?with_companies=77&sort_by=first_air_date.desc&include_adult=false&page=50"), 200, body)
        .with(&url("/discover/movie?with_keywords=9951&sort_by=primary_release_date.desc&include_adult=false&page=1"), 200, body);

    let company = ok(fetch(
        &http,
        Some(KEY),
        request(
            "company",
            &[("company_id", "77"), ("kind", "series")],
            Some("50"),
        ),
    ));
    assert!(
        company.next_cursor.is_none(),
        "page {MAX_PAGES} is the last one followed"
    );
    assert_eq!(company.items[0].item_key, "tmdb:series:990401");
    assert_eq!(company.items[0].rank, Some(981));
    assert_eq!(company.total_hint, Some(MAX_PAGES * 20));

    let keyword = ok(fetch(
        &http,
        Some(KEY),
        request("keyword", &[("keyword_id", "9951")], None),
    ));
    assert_eq!(keyword.next_cursor.as_deref(), Some("2"));
    assert_eq!(
        keyword.items[0].item_key, "tmdb:movie:990401",
        "discover's kind wins over the payload"
    );
    assert_eq!(
        keyword.list_url.as_deref(),
        Some("https://www.themoviedb.org/keyword/9951/movie")
    );
}

#[test]
fn errors_map_to_host_failure_classes() {
    let list = |status: u16, headers: &[(&str, &str)], body: &str| {
        let http =
            RecordedHttp::new().with_headers(&url("/list/8100001?page=1"), status, headers, body);
        err(fetch(
            &http,
            Some(KEY),
            request("list", &[("list_id", "8100001")], None),
        ))
    };
    let bad_key = list(
        401,
        &[],
        r#"{"status_code": 7, "status_message": "Invalid API key"}"#,
    );
    assert_eq!(bad_key.code, PluginErrorCode::AuthFailed);
    let private = list(
        401,
        &[],
        r#"{"status_code": 3, "status_message": "Authentication failed"}"#,
    );
    assert_eq!(private.code, PluginErrorCode::Permanent);
    assert!(private.public_message.contains("not found"));
    let missing = list(404, &[], r#"{"status_code": 34}"#);
    assert!(missing.public_message.contains("not found"));
    let limited = list(429, &[("Retry-After", "30")], "");
    assert_eq!(
        (limited.code, limited.retry_after_seconds),
        (PluginErrorCode::RateLimited, Some(30))
    );
    assert_eq!(
        list(503, &[], "").code,
        PluginErrorCode::UpstreamUnavailable
    );
    assert_eq!(list(200, &[], "not json").code, PluginErrorCode::Permanent);

    let http = RecordedHttp::new();
    assert_eq!(
        err(fetch(
            &http,
            None,
            request("list", &[("list_id", "1")], None)
        ))
        .code,
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        err(fetch(
            &http,
            Some(KEY),
            request("list", &[("list_id", "fixture")], None)
        ))
        .code,
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        err(fetch(&http, Some(KEY), request("list", &[], None))).code,
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        err(fetch(&http, Some(KEY), request("collection", &[], None))).code,
        PluginErrorCode::Unsupported
    );
    assert!(http.urls().is_empty());
}

#[test]
fn health_reports_key_state() {
    let health = |http: &RecordedHttp, key: Option<&str>| match block_on(run(
        http,
        key,
        PluginListCommand::Health(ListPluginHealthRequest {}),
    )) {
        PluginListCommandResult::Health(result) => result,
        other => panic!("unexpected {other:?}"),
    };
    let good = RecordedHttp::new().with(&url("/configuration"), 200, r#"{"images": {}}"#);
    assert!(ok(health(&good, Some(KEY))).healthy);
    let bad = RecordedHttp::new().with(&url("/configuration"), 401, r#"{"status_code": 7}"#);
    let unhealthy = ok(health(&bad, Some(KEY)));
    assert!(!unhealthy.healthy);
    assert!(unhealthy.message.is_some());
    assert!(!ok(health(&RecordedHttp::new(), None)).healthy);
    let down = RecordedHttp::new().with(&url("/configuration"), 502, "");
    assert_eq!(
        err(health(&down, Some(KEY))).code,
        PluginErrorCode::UpstreamUnavailable
    );
}
