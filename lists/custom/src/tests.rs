use std::collections::BTreeMap;

use list_provider_common::testing::{RecordedHttp, block_on};
use scryer_plugin_sdk::{ListMediaKind, ListPluginFetchRequest, PluginDescriptor, PluginErrorCode};

use super::*;

const RSS_URL: &str = "https://feeds.example.test/fixture.rss";
const JSON_URL: &str = "https://feeds.example.test/fixture.json";

fn request(source_type: &str, url: Option<&str>, since: Option<&str>) -> ListPluginFetchRequest {
    ListPluginFetchRequest {
        source_type: source_type.to_string(),
        params: url
            .map(|url| BTreeMap::from([(PARAM_URL.to_string(), url.to_string())]))
            .unwrap_or_default(),
        credential: None,
        page_cursor: None,
        since_fingerprint: since.map(str::to_string),
    }
}

fn keys(response: &scryer_plugin_sdk::ListPluginFetchResponse) -> Vec<&str> {
    response
        .items
        .iter()
        .map(|item| item.item_key.as_str())
        .collect()
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
    assert_eq!(list.auth, ListProviderAuth::None);
    assert!(list.config_fields.is_empty());
    let sources: Vec<_> = list.groups[0]
        .items
        .iter()
        .map(|item| item.source_type.as_str())
        .collect();
    assert_eq!(sources, vec!["rss", "json", "stevenlu"]);

    let pattern = regex::Regex::new(&list.url_patterns[0].pattern).unwrap();
    assert_eq!(
        &pattern.captures(STEVENLU_URL).unwrap()["url"],
        STEVENLU_URL
    );
    assert!(pattern.is_match("https://popular-movies-data.stevenlu.com/movies-imdb-min8.json"));
    assert!(!pattern.is_match("https://popular-movies-data.stevenlu.com.example.test/movies.json"));
    assert!(!pattern.is_match(RSS_URL));
}

#[test]
fn rss_prefers_ids_and_keeps_title_only_entries_as_hints() {
    let feed = r#"<rss version="2.0"><channel><title>Fixture Feed</title>
      <item><title>Fixture Film (2031)</title><link>https://www.imdb.com/title/tt9900701/</link></item>
      <item><title>Fixture Serial</title><guid>tvdb://770702</guid><category>tv</category></item>
      <item><title>Fixture Only Title 2029</title><guid>urn:fixture:3</guid></item>
      <item><title>Fixture Film (2031)</title><guid>https://www.imdb.com/title/tt9900701</guid></item>
    </channel></rss>"#;
    let http = RecordedHttp::new().with(RSS_URL, 200, feed);
    let response = block_on(fetch(&http, &request(SOURCE_RSS, Some(RSS_URL), None))).unwrap();
    assert_eq!(response.list_name.as_deref(), Some("Fixture Feed"));
    assert_eq!(
        keys(&response),
        vec![
            "imdb:tt9900701",
            "tvdb:series:770702",
            "title:fixture-only-title:2029"
        ]
    );
    let hint = &response.items[2];
    assert!(hint.external_ids.is_empty());
    assert_eq!(hint.title.as_deref(), Some("Fixture Only Title"));
    assert_eq!(hint.year, Some(2029));
    assert_eq!(response.items[1].kind_hint, Some(ListMediaKind::Series));
    assert_eq!(response.items[0].year, Some(2031));

    let again = block_on(fetch(
        &http,
        &request(SOURCE_RSS, Some(RSS_URL), response.fingerprint.as_deref()),
    ))
    .unwrap();
    assert!(again.unchanged);
}

#[test]
fn json_accepts_the_arr_spellings() {
    let body = r#"[
      {"imdb_id": "tt9900801", "title": "Fixture One", "year": 2030},
      {"tmdbId": 990802, "title": "Fixture Two", "media_type": "movie"},
      {"tvdbId": 770803},
      {"id": 990804},
      {"id": "tt9900805"},
      {"ids": {"imdb": "tt9900806", "tmdb": 990806}, "mediatype": "show", "name": "Fixture Six"},
      {"Title": "Fixture Seven (2028)"},
      {"poster": "https://images.example.test/none.jpg"}
    ]"#;
    let http = RecordedHttp::new().with(JSON_URL, 200, body);
    let response = block_on(fetch(&http, &request(SOURCE_JSON, Some(JSON_URL), None))).unwrap();
    assert_eq!(
        keys(&response),
        vec![
            "imdb:tt9900801",
            "tmdb:movie:990802",
            "tvdb:series:770803",
            "tmdb:990804",
            "imdb:tt9900805",
            "tmdb:series:990806",
            "title:fixture-seven:2028",
        ]
    );
    assert_eq!(response.items[0].year, Some(2030));
    assert_eq!(response.items[6].rank, Some(7));
}

#[test]
fn json_objects_carry_kind_from_their_container() {
    let body = r#"{"movies": [{"imdb_id": "tt9900901"}], "shows": [{"tvdb_id": 770902, "imdb_id": "tt9900902"}]}"#;
    let http = RecordedHttp::new().with(JSON_URL, 200, body);
    let response = block_on(fetch(&http, &request(SOURCE_JSON, Some(JSON_URL), None))).unwrap();
    assert_eq!(response.items[0].kind_hint, Some(ListMediaKind::Movie));
    assert_eq!(response.items[1].kind_hint, Some(ListMediaKind::Series));
    assert_eq!(
        response.items[1].external_ids[0].kind.as_deref(),
        Some("series")
    );
}

#[test]
fn stevenlu_defaults_its_url_and_marks_movies() {
    let body = r#"[
      {"title": "Fixture Popular One", "imdb_id": "tt9901001", "poster_url": "https://images.example.test/1.jpg", "genres": ["Drama"]},
      {"title": "Fixture Popular Two", "imdb_id": "tt9901002", "poster_url": "https://images.example.test/2.jpg", "genres": []}
    ]"#;
    let http = RecordedHttp::new().with(STEVENLU_URL, 200, body);
    let response = block_on(fetch(&http, &request(SOURCE_STEVENLU, None, None))).unwrap();
    assert_eq!(http.urls(), vec![STEVENLU_URL.to_string()]);
    assert_eq!(keys(&response), vec!["imdb:tt9901001", "imdb:tt9901002"]);
    assert!(
        response
            .items
            .iter()
            .all(|item| item.kind_hint == Some(ListMediaKind::Movie))
    );
}

#[test]
fn bad_urls_and_failures_map_to_host_classes() {
    let http = RecordedHttp::new();
    for url in [
        "ftp://feeds.example.test/x",
        "feeds.example.test/x",
        "https:///x",
        "https://feeds.example.test/a b",
    ] {
        let error = block_on(fetch(&http, &request(SOURCE_RSS, Some(url), None))).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidConfig, "{url}");
    }
    assert_eq!(
        block_on(fetch(&http, &request(SOURCE_JSON, None, None)))
            .unwrap_err()
            .code,
        PluginErrorCode::InvalidConfig
    );
    assert_eq!(
        block_on(fetch(&http, &request("csv", Some(JSON_URL), None)))
            .unwrap_err()
            .code,
        PluginErrorCode::Unsupported
    );
    assert!(http.urls().is_empty());

    let status = |status: u16, body: &str| {
        let http =
            RecordedHttp::new().with_headers(JSON_URL, status, &[("Retry-After", "90")], body);
        block_on(fetch(&http, &request(SOURCE_JSON, Some(JSON_URL), None))).unwrap_err()
    };
    assert!(status(404, "").public_message.contains("not found"));
    assert_eq!(status(429, "").retry_after_seconds, Some(90));
    assert_eq!(status(503, "").code, PluginErrorCode::UpstreamUnavailable);
    assert_eq!(status(200, "<rss/>").code, PluginErrorCode::Permanent);
}
