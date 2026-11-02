use super::*;
use scryer_plugin_sdk::{PluginSearchContext, PluginSearchRequest, PluginSearchRequestKind};
use serde_json::{Value, json};

fn series_request() -> PluginSearchRequest {
    PluginSearchRequest {
        facet: Some("series".into()),
        ids: std::collections::HashMap::from([("imdb_id".into(), "tt0012345".into())]),
        ..Default::default()
    }
}

fn recent_request() -> PluginSearchRequest {
    PluginSearchRequest {
        context: Some(PluginSearchContext {
            request_kind: PluginSearchRequestKind::Recent,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn row(id: u64) -> Value {
    json!({"id":id,"hash":"0123456789abcdef0123456789abcdef01234567",
        "filename":"Example.Series.S01E02.1080p.WEB.mkv", "title":"Example Series S01E02",
        "magnet_url":"magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
        "imdb_id":"12345","season":"1","episode":"2","seeds":0,"peers":7,
        "date_released_unix":1793656694_u64,"size_bytes":"439434266"})
}

#[test]
fn imdb_ids_are_normalized_without_accepting_queries_or_zero() {
    assert_eq!(normalize_imdb("tt0012345"), Some("12345".into()));
    assert_eq!(normalize_imdb("12345"), Some("12345".into()));
    for invalid in [
        "",
        "tt",
        "0",
        "tt0000",
        "12x34",
        "https://imdb.com/title/tt12345",
        "-1",
    ] {
        assert_eq!(normalize_imdb(invalid), None, "{invalid}");
    }
}

#[test]
fn routing_supports_recent_and_series_imdb_only() {
    assert_eq!(route(&recent_request()).unwrap(), None);
    assert_eq!(route(&series_request()).unwrap(), Some("12345".into()));
    let mut request = series_request();
    request.season = Some(1);
    request.episode = Some(2);
    assert_eq!(route(&request).unwrap(), Some("12345".into()));
    request.facet = Some("movie".into());
    assert!(route(&request).is_err());
    request.facet = Some("series".into());
    request.absolute_episode = Some(12);
    assert!(route(&request).is_err());
    request.absolute_episode = None;
    request.ids.clear();
    request.query = "Example Series".into();
    assert!(route(&request).is_err());
}

#[test]
fn provider_rows_preserve_identity_and_do_not_invent_peer_semantics() {
    let result = map_row(&row(3183760)).unwrap();
    assert_eq!(result.guid.as_deref(), Some("eztv:3183760"));
    assert_eq!(result.size_bytes, Some(439434266));
    assert_eq!(result.seeders, Some(0));
    assert_eq!(result.peers, None);
    assert_eq!(result.leechers, None);
    assert_eq!(result.provider_extra.get("peers"), Some(&json!(7)));
    assert_eq!(
        result.info_hash_v1.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert!(result.published_at.is_some());
    assert!(result.magnet_url.is_some());
}

#[test]
fn malformed_release_identity_or_conflicting_magnet_is_rejected() {
    for (key, value) in [
        ("id", json!(0)),
        ("hash", json!("invalid")),
        (
            "magnet_url",
            json!("magnet:?xt=urn:btih:ffffffffffffffffffffffffffffffffffffffff"),
        ),
    ] {
        let mut item = row(1);
        item[key] = value;
        assert!(map_row(&item).is_none(), "{key}");
    }
}

#[test]
fn api_envelope_accepts_numeric_strings_and_rejects_html_and_missing_rows() {
    let page = parse_page(
        &serde_json::to_vec(
            &json!({"torrents_count":"2","limit":100,"page":1,"torrents":[row(1)]}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.rows.len(), 1);
    for body in [
        b"<html>challenge</html>".as_slice(),
        b"{}",
        b"{\"torrents_count\":1,\"torrents\":null}",
    ] {
        assert!(parse_page(body).is_err());
    }
}

#[test]
fn empty_lookup_may_omit_torrents_but_nonempty_or_null_envelopes_may_not() {
    let page =
        parse_page(br#"{"imdb_id":"999999999999","torrents_count":0,"limit":1,"page":1}"#).unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());
    let string_count = parse_page(br#"{"torrents_count":"0","limit":"1","page":"1"}"#).unwrap();
    assert_eq!(string_count.total, 0);
    assert!(string_count.rows.is_empty());
    for body in [
        br#"{"torrents_count":1,"limit":1,"page":1}"#.as_slice(),
        br#"{"torrents_count":0,"limit":1,"page":1,"torrents":null}"#,
    ] {
        assert!(parse_page(body).is_err());
    }
}

mod host {
    use super::*;
    use scryer_plugin_pdk::runtime::{
        HostError, HostRuntime, PluginHttpFieldsResponse, PluginHttpRequest, install_host_runtime,
    };
    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    struct State {
        responses: VecDeque<Result<PluginHttpFieldsResponse, HostError>>,
        requests: Vec<PluginHttpRequest>,
        state: HashMap<String, Vec<u8>>,
        now: u64,
        deadline: u64,
        sleeps: Vec<u64>,
        base_url: String,
    }
    impl Default for State {
        fn default() -> Self {
            Self {
                responses: VecDeque::new(),
                requests: vec![],
                state: HashMap::new(),
                now: 10_000,
                deadline: 1_000_000,
                sleeps: vec![],
                base_url: "https://eztvx.to".into(),
            }
        }
    }
    thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }
    struct Host;
    static HOST: Host = Host;
    impl HostRuntime for Host {
        fn http_fields(
            &self,
            request: PluginHttpRequest,
        ) -> Pin<Box<dyn Future<Output = Result<PluginHttpFieldsResponse, HostError>> + '_>>
        {
            Box::pin(async move {
                STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.requests.push(request);
                    state
                        .responses
                        .pop_front()
                        .expect("unexpected HTTP request")
                })
            })
        }
        fn sleep(&self, ms: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async move {
                STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    state.sleeps.push(ms);
                    state.now += ms;
                })
            })
        }
        fn monotonic_now_ms(&self) -> u64 {
            STATE.with(|s| s.borrow().now)
        }
        fn operation_deadline_monotonic_ms(&self) -> u64 {
            STATE.with(|s| s.borrow().deadline)
        }
        fn wall_now_ms(&self) -> u64 {
            1_800_000_000_000
        }
        fn config_get(&self, key: &str) -> Option<String> {
            (key == "base_url").then(|| STATE.with(|s| s.borrow().base_url.clone()))
        }
        fn provider_profile_bytes(&self) -> Option<Vec<u8>> {
            None
        }
        fn state_get(&self, key: &str) -> Option<Vec<u8>> {
            STATE.with(|s| s.borrow().state.get(key).cloned())
        }
        fn state_cas(
            &self,
            key: &str,
            expected: Option<&[u8]>,
            replacement: Option<&[u8]>,
        ) -> bool {
            STATE.with(|s| {
                let mut state = s.borrow_mut();
                if state.state.get(key).map(Vec::as_slice) != expected {
                    return false;
                }
                if let Some(value) = replacement {
                    state.state.insert(key.into(), value.to_vec());
                } else {
                    state.state.remove(key);
                }
                true
            })
        }
        fn log(&self, _: scryer_plugin_pdk::log::LogLevel, _: &str) {}
    }
    pub fn reset(responses: Vec<Result<PluginHttpFieldsResponse, HostError>>) {
        install_host_runtime(&HOST);
        STATE.with(|s| {
            *s.borrow_mut() = State {
                responses: responses.into(),
                ..Default::default()
            }
        });
    }
    pub fn response(total: usize, rows: Vec<Value>) -> Result<PluginHttpFieldsResponse, HostError> {
        page_response(1, 100, total, rows)
    }
    pub fn page_response(
        page: usize,
        limit: usize,
        total: usize,
        rows: Vec<Value>,
    ) -> Result<PluginHttpFieldsResponse, HostError> {
        Ok(PluginHttpFieldsResponse {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(
                &json!({"torrents_count":total,"limit":limit,"page":page,"torrents":rows}),
            )
            .unwrap(),
        })
    }
    pub fn run<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => {
                panic!("scripted host futures must complete without external scheduling")
            }
        }
    }
    pub fn urls() -> Vec<url::Url> {
        STATE.with(|s| {
            s.borrow()
                .requests
                .iter()
                .map(|r| url::Url::parse(&r.url).unwrap())
                .collect()
        })
    }
    pub fn sleeps() -> Vec<u64> {
        STATE.with(|s| s.borrow().sleeps.clone())
    }
    pub fn expire() {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.deadline = s.now;
        });
    }
    pub fn set_base(value: &str) {
        STATE.with(|s| s.borrow_mut().base_url = value.into());
    }
}

#[test]
fn unsupported_search_never_contacts_upstream() {
    host::reset(vec![]);
    let mut request = series_request();
    request.facet = Some("movie".into());
    assert!(host::run(execute(&request)).is_err());
    assert!(host::urls().is_empty());
}

#[test]
fn imdb_search_uses_only_documented_parameters_and_one_page() {
    host::reset(vec![host::response(250, vec![row(1)])]);
    let mut request = series_request();
    request.season = Some(1);
    request.episode = Some(2);
    let response = host::run(execute(&request)).unwrap();
    assert_eq!(response.results.len(), 1);
    assert!(response.next_cursor.is_some());
    let urls = host::urls();
    assert_eq!(urls.len(), 1);
    assert_eq!(urls[0].path(), "/api/get-torrents");
    let parameters: std::collections::HashMap<_, _> = urls[0].query_pairs().into_owned().collect();
    assert_eq!(parameters.len(), 3);
    assert_eq!(parameters.get("imdb_id").map(String::as_str), Some("12345"));
    assert_eq!(parameters.get("page").map(String::as_str), Some("1"));
}

#[test]
fn cursors_cannot_be_reused_for_a_different_series() {
    host::reset(vec![host::response(250, vec![row(1)])]);
    let mut request = series_request();
    request.page_cursor = host::run(execute(&request)).unwrap().next_cursor;
    request.ids.insert("imdb_id".into(), "tt99999".into());
    assert!(host::run(execute(&request)).is_err());
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn initial_recent_poll_reads_only_newest_page() {
    host::reset(vec![host::response(1000, vec![row(1)])]);
    let response = host::run(execute(&recent_request())).unwrap();
    assert!(response.next_cursor.is_none());
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn consecutive_search_pages_are_paced_by_monotonic_start_time() {
    host::reset(vec![
        host::response(250, vec![row(1)]),
        host::page_response(2, 100, 250, vec![row(2)]),
    ]);
    let mut request = series_request();
    request.page_cursor = host::run(execute(&request)).unwrap().next_cursor;
    host::run(execute(&request)).unwrap();
    assert_eq!(host::urls().len(), 2);
    assert_eq!(host::sleeps(), vec![2000]);
}

fn distinct_row(id: u64) -> Value {
    let mut value = row(id);
    let hash = format!("{id:040x}");
    value["hash"] = json!(hash);
    value["magnet_url"] = json!(format!("magnet:?xt=urn:btih:{hash}"));
    value
}

fn details(error: &Error) -> IndexerSearchPluginError {
    match scryer_plugin_pdk::error::plugin_error_from(error)
        .details
        .unwrap()
    {
        PluginErrorDetails::IndexerSearch(detail) => detail,
    }
}

fn partial_results(error: &Error, expected: IndexerSearchIncompleteReason) -> PluginSearchResponse {
    match details(error) {
        IndexerSearchPluginError::PartialResults {
            response, reason, ..
        } => {
            assert_eq!(reason, expected);
            assert!(response.next_cursor.is_none());
            *response
        }
        other => panic!("expected partial results, got {other:?}"),
    }
}

#[test]
fn malformed_rows_preserve_valid_candidates_and_differ_from_an_empty_feed() {
    host::reset(vec![host::response(2, vec![row(1), json!({"id":2})])]);
    let error = host::run(execute(&series_request())).unwrap_err();
    assert_eq!(
        partial_results(&error, IndexerSearchIncompleteReason::MalformedContent)
            .results
            .len(),
        1
    );
    host::reset(vec![host::response(1, vec![json!({"id":2})])]);
    let error = host::run(execute(&series_request())).unwrap_err();
    assert!(
        partial_results(&error, IndexerSearchIncompleteReason::MalformedContent)
            .results
            .is_empty()
    );
    host::reset(vec![host::response(0, vec![])]);
    let response = host::run(execute(&series_request())).unwrap();
    assert!(response.results.is_empty());
    assert!(response.next_cursor.is_none());
}

#[test]
fn duplicate_hashes_are_emitted_once_and_distinct_hashes_survive() {
    host::reset(vec![host::response(
        4,
        vec![row(1), row(1), row(2), distinct_row(3)],
    )]);
    let response = host::run(execute(&series_request())).unwrap();
    assert_eq!(response.results.len(), 2);
}

#[test]
fn numeric_string_row_fields_are_normalized_without_losing_zero_seeders() {
    let mut value = row(1);
    value["id"] = json!("3183760");
    value["seeds"] = json!("0");
    value["date_released_unix"] = json!("1793656694");
    let result = map_row(&value).unwrap();
    assert_eq!(result.guid.as_deref(), Some("eztv:3183760"));
    assert_eq!(result.seeders, Some(0));
    assert_eq!(result.published_at, map_row(&row(1)).unwrap().published_at);
}

#[test]
fn rss_catchup_stops_at_identity_and_keeps_distinct_newer_pages() {
    host::reset(vec![
        host::response(1000, vec![distinct_row(1)]),
        host::page_response(2, 100, 1000, vec![distinct_row(2)]),
    ]);
    let mut request = recent_request();
    request.rss_catch_up = Some(PluginRssCatchUp {
        last_seen_published_at: "2020-01-01T00:00:00Z".into(),
        last_seen_identity: Some("eztv:2".into()),
    });
    let response = host::run(execute(&request)).unwrap();
    assert_eq!(response.results.len(), 2);
    assert_eq!(host::urls().len(), 2);
    assert!(response.next_cursor.is_none());
}

#[test]
fn rss_catchup_ceiling_keeps_every_page_as_typed_incomplete() {
    host::reset(
        (1..=10)
            .map(|id| host::page_response(id as usize, 100, 100_000, vec![distinct_row(id)]))
            .collect(),
    );
    let mut request = recent_request();
    request.rss_catch_up = Some(PluginRssCatchUp {
        last_seen_published_at: "2020-01-01T00:00:00Z".into(),
        last_seen_identity: Some("eztv:99999".into()),
    });
    let error = host::run(execute(&request)).unwrap_err();
    assert_eq!(
        partial_results(&error, IndexerSearchIncompleteReason::PageCeilingReached)
            .results
            .len(),
        10
    );
    assert_eq!(host::urls().len(), 10);
}

#[test]
fn search_provider_page_ceiling_is_typed_incomplete() {
    host::reset(vec![host::page_response(100, 100, 100_000, vec![row(1)])]);
    let mut request = series_request();
    request.page_cursor = Some(
        serde_json::to_string(&Cursor {
            version: 1,
            page: 100,
            binding: format!("{}|{}", base_url().unwrap(), binding(&request)),
        })
        .unwrap(),
    );
    let error = host::run(execute(&request)).unwrap_err();
    assert_eq!(
        partial_results(&error, IndexerSearchIncompleteReason::PageCeilingReached)
            .results
            .len(),
        1
    );
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn cursor_rejects_malformed_payload_and_changed_page_size_before_http() {
    host::reset(vec![host::response(250, vec![row(1)])]);
    let mut request = series_request();
    request.page_cursor = host::run(execute(&request)).unwrap().next_cursor;
    request.limit = 5;
    assert!(host::run(execute(&request)).is_err());
    request.page_cursor = Some("invalid".into());
    assert!(host::run(execute(&request)).is_err());
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn expired_deadline_does_not_start_http() {
    host::reset(vec![]);
    host::expire();
    let error = host::run(execute(&series_request())).unwrap_err();
    assert!(matches!(
        details(&error),
        IndexerSearchPluginError::Deferred { .. }
    ));
    assert!(host::urls().is_empty());
}

#[test]
fn transport_cancellation_and_upstream_http_failures_remain_errors() {
    use scryer_plugin_pdk::runtime::{HostError, PluginHttpFieldsResponse};
    host::reset(vec![Err(HostError::Cancelled)]);
    let error = host::run(execute(&series_request())).unwrap_err();
    assert_eq!(
        scryer_plugin_pdk::error::plugin_error_from(&error).code,
        PluginErrorCode::Temporary
    );
    host::reset(vec![Ok(PluginHttpFieldsResponse {
        status: 503,
        headers: vec![],
        body: vec![],
    })]);
    let error = host::run(execute(&series_request())).unwrap_err();
    assert_eq!(
        scryer_plugin_pdk::error::plugin_error_from(&error).code,
        PluginErrorCode::UpstreamUnavailable
    );
    host::reset(vec![Ok(PluginHttpFieldsResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "text/html".into())],
        body: b"<html>challenge</html>".to_vec(),
    })]);
    let error = host::run(execute(&series_request())).unwrap_err();
    assert!(matches!(
        details(&error),
        IndexerSearchPluginError::InvalidResponse { .. }
    ));
}

#[test]
fn rate_limit_retry_after_reaches_the_typed_error() {
    use scryer_plugin_pdk::runtime::PluginHttpFieldsResponse;
    host::reset(vec![Ok(PluginHttpFieldsResponse {
        status: 429,
        headers: vec![("Retry-After".into(), "17".into())],
        body: vec![],
    })]);
    let error = host::run(execute(&series_request())).unwrap_err();
    let error = scryer_plugin_pdk::error::plugin_error_from(&error);
    assert_eq!(error.code, PluginErrorCode::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(17));
}

#[test]
fn rss_catchup_compares_instants_across_timezone_offsets() {
    let mut release = row(1);
    release["date_released_unix"] = json!(3600);
    host::reset(vec![host::response(1000, vec![release])]);
    let mut request = recent_request();
    request.rss_catch_up = Some(PluginRssCatchUp {
        last_seen_published_at: "1970-01-01T00:30:00-01:00".into(),
        last_seen_identity: None,
    });
    let response = host::run(execute(&request)).unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn cursor_binding_includes_connection_origin() {
    host::reset(vec![host::response(250, vec![row(1)])]);
    let mut request = series_request();
    request.page_cursor = host::run(execute(&request)).unwrap().next_cursor;
    host::set_base("https://mirror.example");
    assert!(host::run(execute(&request)).is_err());
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn invalid_connection_origins_are_rejected_without_http() {
    for value in [
        "ftp://mirror.example",
        "https://user:secret@mirror.example",
        "https://mirror.example/path",
        "https://mirror.example/?token=secret",
        "https://mirror.example/#fragment",
    ] {
        host::reset(vec![]);
        host::set_base(value);
        assert!(host::run(execute(&series_request())).is_err(), "{value}");
        assert!(host::urls().is_empty());
    }
}

#[test]
fn descriptor_constructs_and_advertises_only_supported_search_features() {
    let descriptor = build_descriptor();
    assert_eq!(descriptor.id, "eztv");
    let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
        panic!("indexer descriptor required")
    };
    let capabilities = indexer.capabilities;
    assert!(capabilities.rss && capabilities.imdb_search);
    assert!(!capabilities.tvdb_search && !capabilities.anidb_search);
    assert_eq!(
        capabilities.supported_ids.get("series"),
        Some(&vec!["imdb_id".to_string()])
    );
    assert!(!capabilities.supported_ids.contains_key("movie"));
    assert!(
        !capabilities
            .search_inputs
            .contains(&IndexerSearchInput::TitleQuery)
    );
    assert!(
        !capabilities
            .search_inputs
            .contains(&IndexerSearchInput::AbsoluteEpisode)
    );
    let limits = capabilities.limits.unwrap();
    assert!(limits.paged_search);
    assert_eq!(limits.max_page_size, Some(100));
    assert_eq!(limits.max_pages, Some(100));
    assert!(
        indexer
            .config_fields
            .iter()
            .any(|field| field.key == "base_url")
    );
    assert!(indexer.config_fields.iter().all(|field| !matches!(
        field.key.as_str(),
        "api_key" | "username" | "password" | "categories"
    )));
}

#[test]
fn duplicate_provider_id_with_different_hash_is_not_emitted_twice() {
    let mut changed = distinct_row(2);
    changed["id"] = json!(1);
    host::reset(vec![host::response(2, vec![row(1), changed])]);
    assert_eq!(
        host::run(execute(&series_request())).unwrap().results.len(),
        1
    );
}

#[test]
fn invalid_rss_marker_is_rejected_before_http() {
    host::reset(vec![]);
    let mut request = recent_request();
    request.rss_catch_up = Some(PluginRssCatchUp {
        last_seen_published_at: "yesterday".into(),
        last_seen_identity: None,
    });
    assert!(host::run(execute(&request)).is_err());
    assert!(host::urls().is_empty());
}

#[test]
fn mismatched_echoed_page_or_limit_is_a_typed_invalid_response() {
    for response in [
        host::page_response(2, 100, 250, vec![row(1)]),
        host::page_response(1, 50, 250, vec![row(1)]),
    ] {
        host::reset(vec![response]);
        let error = host::run(execute(&series_request())).unwrap_err();
        assert!(matches!(
            details(&error),
            IndexerSearchPluginError::InvalidResponse { .. }
        ));
        assert_eq!(host::urls().len(), 1);
    }
}

#[test]
fn explicit_recent_paging_continues_one_page_per_invocation() {
    host::reset(vec![
        host::page_response(1, 100, 200, vec![distinct_row(1)]),
        host::page_response(2, 100, 200, vec![distinct_row(2)]),
    ]);
    let mut request = recent_request();
    request.recent_paging = true;
    let first = host::run(execute(&request)).unwrap();
    assert_eq!(first.results.len(), 1);
    request.page_cursor = Some(first.next_cursor.unwrap());
    let second = host::run(execute(&request)).unwrap();
    assert_eq!(second.results[0].guid.as_deref(), Some("eztv:2"));
    assert!(second.next_cursor.is_none());
    assert_eq!(host::urls().len(), 2);
}

#[test]
fn background_recent_cursor_is_rejected_before_http() {
    host::reset(vec![]);
    let mut request = recent_request();
    request.page_cursor = Some(
        serde_json::to_string(&Cursor {
            version: 1,
            page: 2,
            binding: format!("{}|{}", base_url().unwrap(), binding(&request)),
        })
        .unwrap(),
    );
    let error = host::run(execute(&request)).unwrap_err();
    assert_eq!(
        scryer_plugin_pdk::error::plugin_error_from(&error).code,
        PluginErrorCode::Unsupported
    );
    assert!(host::urls().is_empty());
}

#[test]
fn empty_provider_page_with_remaining_total_preserves_continuation() {
    host::reset(vec![host::response(250, vec![])]);
    let response = host::run(execute(&series_request())).unwrap();
    assert!(response.results.is_empty());
    assert!(response.next_cursor.is_some());
    assert_eq!(host::urls().len(), 1);
}

#[test]
fn timestamp_parser_rejects_signed_segments_and_preserves_fractional_instants() {
    for invalid in [
        "2026-+1-01T00:00:00Z",
        "2026-01-+1T00:00:00Z",
        "2026-01-01T+1:00:00Z",
        "2026-01-01T00:+1:00Z",
        "2026-01-01T00:00:+1Z",
        "2026-01-01T00:00:00++1:00",
        "2026-01-01T00:00:00+00:+1",
    ] {
        assert!(marker_unix(invalid).is_none(), "{invalid}");
    }
    assert_eq!(marker_unix("1970-01-01T00:00:01.25Z"), Some(1_250_000_000));
    assert_eq!(
        marker_unix("1970-01-01T00:00:01.250Z"),
        marker_unix("1970-01-01T01:00:01.25+01:00")
    );
    assert_ne!(
        marker_unix("1970-01-01T00:00:01.25Z"),
        marker_unix("1970-01-01T00:00:01.24Z")
    );
}

#[test]
fn http_date_retry_after_uses_host_wall_clock() {
    use scryer_plugin_pdk::runtime::PluginHttpFieldsResponse;
    host::reset(vec![Ok(PluginHttpFieldsResponse {
        status: 429,
        headers: vec![("Retry-After".into(), "Fri, 15 Jan 2027 08:00:17 GMT".into())],
        body: vec![],
    })]);
    let error = host::run(execute(&series_request())).unwrap_err();
    let error = scryer_plugin_pdk::error::plugin_error_from(&error);
    assert_eq!(error.code, PluginErrorCode::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(17));
}
