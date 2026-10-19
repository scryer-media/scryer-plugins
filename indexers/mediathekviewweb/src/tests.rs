//! Native tests over fixture JSON. No network: every API reply comes from
//! `tests/fixtures/`, and every call the engine makes is recorded. The one
//! live check is ignored and runs only with `MVW_LIVE=1`.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use scryer_plugin_sdk::{
    ConfigFieldType, HttpAudioRole, HttpAvailability, HttpRangeSupport, HttpResourceRole,
    HttpTransport, IndexerFeedMode, IndexerProtocol, IndexerSearchIncompleteReason,
    IndexerSearchInput, IndexerSearchPluginError, IndexerSourceKind, PluginErrorCode,
    PluginErrorDetails, PluginHttpResource, PluginSearchRequest as SearchRequest,
    PluginSearchResult, ProviderDescriptor,
};
use serde_json::{Value, json};

use crate::api::{MvwConfig, MvwError, QueryCall, parse_channels, parse_reply};
use crate::cursor::PageCursor;
use crate::engine::{self, Fetch, HttpReply, PAGE_CEILING, PageOutcome};
use crate::release::{infer_height, release_title, rfc3339_from_unix, subtitle_format};

/// A real three-result answer for "Die Sendung mit der Maus".
const QUERY_SAMPLE: &str = include_str!("../tests/fixtures/query_sample.json");
/// A real two-result answer with a `channel` query for KiKA.
const QUERY_CHANNEL: &str = include_str!("../tests/fixtures/query_channel.json");
/// Synthetic entries covering HLS, accessibility variants, clips, string
/// numbers, missing pages and unusable entries.
const QUERY_VARIANTS: &str = include_str!("../tests/fixtures/query_variants.json");
/// The real HTTP 500 body the API returns for a malformed query.
const ERROR_SERVER: &str = include_str!("../tests/fixtures/error_server.json");
const ERROR_REJECTED: &str = include_str!("../tests/fixtures/error_rejected.json");
const ERROR_NOT_JSON: &str = include_str!("../tests/fixtures/error_not_json.html");

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Answers calls in order with canned replies and records each call.
struct FixtureFetch {
    replies: RefCell<Vec<Result<HttpReply, MvwError>>>,
    calls: RefCell<Vec<QueryCall>>,
}

impl FixtureFetch {
    fn new(replies: Vec<Result<HttpReply, MvwError>>) -> Self {
        Self {
            replies: RefCell::new(replies),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn ok(body: &str) -> Self {
        Self::new(vec![Ok(reply(200, body))])
    }

    fn calls(&self) -> Vec<QueryCall> {
        self.calls.borrow().clone()
    }

    fn only_body(&self) -> Value {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "expected exactly one call");
        serde_json::from_str(&calls[0].body()).unwrap()
    }
}

fn reply(status: u16, body: &str) -> HttpReply {
    HttpReply {
        status,
        body: body.as_bytes().to_vec(),
    }
}

impl Fetch for FixtureFetch {
    async fn post(&self, call: &QueryCall) -> Result<HttpReply, MvwError> {
        self.calls.borrow_mut().push(call.clone());
        let mut replies = self.replies.borrow_mut();
        assert!(!replies.is_empty(), "unexpected call: {}", call.body());
        replies.remove(0)
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

fn config_with(values: &[(&str, &str)]) -> MvwConfig {
    MvwConfig::from_values(|key| {
        values
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_string())
    })
    .expect("valid config")
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        ..SearchRequest::default()
    }
}

fn request_with(query: &str, limit: usize, cursor: Option<&str>) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        limit,
        page_cursor: cursor.map(str::to_string),
        ..SearchRequest::default()
    }
}

fn run(
    config: &MvwConfig,
    request: &SearchRequest,
    fetch: &FixtureFetch,
) -> Result<PageOutcome, MvwError> {
    block_on(engine::run_page(config, request, fetch))
}

fn by_id<'a>(results: &'a [PluginSearchResult], id: &str) -> &'a PluginSearchResult {
    results
        .iter()
        .find(|result| result.http.as_ref().is_some_and(|http| http.item_id == id))
        .unwrap_or_else(|| panic!("no result for {id}"))
}

fn ids(results: &[PluginSearchResult]) -> Vec<String> {
    results
        .iter()
        .map(|result| result.http.as_ref().unwrap().item_id.clone())
        .collect()
}

fn resources_of<'a>(
    result: &'a PluginSearchResult,
    role: HttpResourceRole,
) -> Vec<&'a PluginHttpResource> {
    result
        .http
        .as_ref()
        .unwrap()
        .resources
        .iter()
        .filter(|resource| resource.role == role)
        .collect()
}

const MAUS_KIKA: &str = "FTXJUjzJBBIF+3iuitHuePQ9ufKsHjf8v8KCGeT8nF8=";
const MAUS_WDR_AD: &str = "qCVEmOr960WdoLZCbxhYZud3WsB/4bTlEmGfmI21JgU=";
const MAUS_WDR: &str = "KT5Qbixjp17ivaB7tsHOEcZ0rjOVNM/DNYMJ7RuxTRw=";

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[test]
fn descriptor_declares_an_interactive_http_indexer() {
    let descriptor = crate::build_descriptor();
    assert_eq!(descriptor.id, "mediathekviewweb");
    assert_eq!(descriptor.name, "MediathekViewWeb");
    assert_eq!(descriptor.version, "0.1.0");
    let ProviderDescriptor::Indexer(indexer) = descriptor.provider else {
        panic!("not an indexer");
    };
    assert_eq!(indexer.provider_type, "mediathekviewweb");
    assert_eq!(indexer.source_kind, IndexerSourceKind::Http);
    assert_eq!(
        indexer.allowed_hosts,
        vec!["mediathekviewweb.de".to_string()]
    );
    let capabilities = indexer.capabilities;
    assert!(capabilities.search);
    assert!(!capabilities.rss);
    assert_eq!(capabilities.query_param.as_deref(), Some("q"));
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
    assert_eq!(
        capabilities.supported_query_facets,
        vec!["series".to_string(), "movie".to_string()]
    );
    assert!(capabilities.supported_external_ids.is_empty());
    let limits = capabilities.limits.expect("limits");
    assert!(limits.paged_search);
    assert_eq!(limits.max_page_size, Some(50));
    let http = capabilities.http.expect("http capabilities");
    assert_eq!(
        http.transports,
        vec![
            HttpTransport::DirectFile,
            HttpTransport::Hls,
            HttpTransport::ProviderPage
        ]
    );
    assert_eq!(http.providers, vec!["mediathekviewweb".to_string()]);

    let fields = indexer
        .config_fields
        .iter()
        .map(|field| (field.key.as_str(), field.field_type, field.required))
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        vec![
            ("channels", ConfigFieldType::Tag, false),
            ("include_audio_description", ConfigFieldType::Bool, false),
            ("include_sign_language", ConfigFieldType::Bool, false),
            ("min_duration_minutes", ConfigFieldType::Number, false),
        ]
    );
    assert_eq!(
        indexer.config_fields[1].default_value.as_deref(),
        Some("false")
    );
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn config_defaults() {
    let config = config_with(&[]);
    assert_eq!(config, MvwConfig::default());
}

#[test]
fn config_parses_every_field() {
    let config = config_with(&[
        ("channels", "ARD, zdf;\nKiKA, ard"),
        ("include_audio_description", "true"),
        ("include_sign_language", "1"),
        ("min_duration_minutes", "12.5"),
    ]);
    assert_eq!(config.channels, vec!["ARD", "zdf", "KiKA"]);
    assert!(config.include_audio_description);
    assert!(config.include_sign_language);
    assert_eq!(config.min_duration_seconds, Some(750));
    assert!(config.allows_channel("ZDF"));
    assert!(!config.allows_channel("ARTE.DE"));

    let json_list = parse_channels(r#"["ARTE.DE", "3Sat"]"#).unwrap();
    assert_eq!(json_list, vec!["ARTE.DE", "3Sat"]);
    assert_eq!(
        config_with(&[("min_duration_minutes", "0")]).min_duration_seconds,
        None
    );
}

#[test]
fn config_rejects_bad_values() {
    for (key, value) in [
        ("include_audio_description", "maybe"),
        ("min_duration_minutes", "-3"),
        ("min_duration_minutes", "soon"),
        ("channels", "[not json"),
    ] {
        let error = MvwConfig::from_values(|name| (name == key).then(|| value.to_string()))
            .expect_err(value);
        assert!(matches!(error, MvwError::InvalidConfig(_)), "{error:?}");
    }
    let many = (0..21)
        .map(|index| format!("Channel{index}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_channels(&many).is_err());
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

#[test]
fn query_body_matches_the_api_contract() {
    let call = QueryCall {
        query: "Die Sendung mit der Maus".to_string(),
        channel: None,
        offset: 0,
        size: 5,
    };
    let body: Value = serde_json::from_str(&call.body()).unwrap();
    assert_eq!(
        body,
        json!({
            "queries": [{"fields": ["title", "topic"], "query": "Die Sendung mit der Maus"}],
            "sortBy": "timestamp",
            "sortOrder": "desc",
            "future": false,
            "offset": 0,
            "size": 5,
        })
    );

    let narrowed = QueryCall {
        channel: Some("KiKA".to_string()),
        offset: 50,
        size: 500,
        ..call
    };
    let body: Value = serde_json::from_str(&narrowed.body()).unwrap();
    assert_eq!(
        body["queries"][1],
        json!({"fields": ["channel"], "query": "KiKA"})
    );
    assert_eq!(body["offset"], 50);
    assert_eq!(body["size"], 50, "size is clamped");
}

#[test]
fn season_and_episode_are_not_added_to_the_query() {
    let fetch = FixtureFetch::ok(QUERY_SAMPLE);
    let request = SearchRequest {
        query: "  Die   Sendung mit der Maus ".to_string(),
        season: Some(2026),
        episode: Some(42),
        ..SearchRequest::default()
    };
    run(&config_with(&[]), &request, &fetch).unwrap();
    let body = fetch.only_body();
    assert_eq!(body["queries"][0]["query"], "Die Sendung mit der Maus");
    assert_eq!(body["size"], 50, "no limit asks for a full page");
}

#[test]
fn cursor_round_trips_and_rejects_foreign_values() {
    let cursor = PageCursor {
        pages_read: 1,
        offset: 50,
    };
    assert_eq!(cursor.encode(), "mvw1.1.50");
    assert_eq!(PageCursor::decode("mvw1.1.50"), Some(cursor));
    for foreign in [
        "",
        "mvw1",
        "mvw2.1.50",
        "mvw1.x.50",
        "mvw1.1.50.9",
        "mvw1.0.50",
        "mvw1.1.0",
        "mvw1.1.99999",
        "mvw1.1.-5",
        "yt1.s.1.*~CAUQAA",
    ] {
        assert_eq!(PageCursor::decode(foreign), None, "{foreign}");
    }
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[test]
fn empty_query_makes_no_call() {
    let fetch = FixtureFetch::new(vec![]);
    let outcome = run(&config_with(&[]), &request("   "), &fetch).unwrap();
    assert!(outcome.response.results.is_empty());
    assert!(outcome.response.next_cursor.is_none());
    assert!(!outcome.ceiling_reached);
    assert!(fetch.calls().is_empty());
}

#[test]
fn a_full_first_page_hands_back_an_offset_cursor() {
    let fetch = FixtureFetch::ok(QUERY_SAMPLE);
    let outcome = run(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 3, None),
        &fetch,
    )
    .unwrap();
    assert_eq!(fetch.only_body()["offset"], 0);
    assert_eq!(fetch.only_body()["size"], 3);
    // The audio-described entry is dropped by default, but the cursor
    // counts every entry read.
    assert_eq!(ids(&outcome.response.results), vec![MAUS_KIKA, MAUS_WDR]);
    assert_eq!(outcome.response.next_cursor.as_deref(), Some("mvw1.1.3"));
    assert!(!outcome.ceiling_reached);
}

#[test]
fn a_short_page_ends_the_search() {
    let fetch = FixtureFetch::ok(QUERY_CHANNEL);
    let outcome = run(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 3, Some("mvw1.1.3")),
        &fetch,
    )
    .unwrap();
    assert_eq!(fetch.only_body()["offset"], 3);
    assert_eq!(outcome.response.results.len(), 2);
    assert!(outcome.response.next_cursor.is_none());
    assert!(!outcome.ceiling_reached);
}

#[test]
fn the_search_stops_at_its_page_ceiling() {
    assert_eq!(PAGE_CEILING, 2);
    let fetch = FixtureFetch::ok(QUERY_CHANNEL);
    let outcome = run(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 2, Some("mvw1.1.2")),
        &fetch,
    )
    .unwrap();
    assert_eq!(outcome.response.results.len(), 2);
    assert!(outcome.response.next_cursor.is_none());
    assert!(outcome.ceiling_reached);

    // A cursor already at the ceiling reads nothing.
    let fetch = FixtureFetch::new(vec![]);
    let outcome = run(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 2, Some("mvw1.2.100")),
        &fetch,
    )
    .unwrap();
    assert!(outcome.ceiling_reached);
    assert!(outcome.response.results.is_empty());
    assert!(fetch.calls().is_empty());
}

#[test]
fn the_page_ceiling_is_reported_as_partial_results() {
    let error = crate::ceiling_error(scryer_plugin_sdk::PluginSearchResponse::default());
    assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
    assert!(matches!(
        error.details,
        Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::PartialResults {
                reason: IndexerSearchIncompleteReason::PageCeilingReached,
                ..
            }
        ))
    ));
}

#[test]
fn a_foreign_cursor_is_refused_without_a_call() {
    let fetch = FixtureFetch::new(vec![]);
    let error = run(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 0, Some("yt1.s.1.*~CAUQAA")),
        &fetch,
    )
    .unwrap_err();
    assert_eq!(error, MvwError::InvalidCursor);
    assert!(fetch.calls().is_empty());
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

#[test]
fn one_allowed_channel_narrows_the_query_itself() {
    let fetch = FixtureFetch::ok(QUERY_CHANNEL);
    let outcome = run(
        &config_with(&[("channels", "kika")]),
        &request("Die Sendung mit der Maus"),
        &fetch,
    )
    .unwrap();
    let body = fetch.only_body();
    assert_eq!(
        body["queries"][1],
        json!({"fields": ["channel"], "query": "kika"})
    );
    assert_eq!(outcome.response.results.len(), 2);
}

#[test]
fn several_allowed_channels_filter_after_the_call() {
    let fetch = FixtureFetch::ok(QUERY_VARIANTS);
    let outcome = run(
        &config_with(&[
            ("channels", "ard, 3sat"),
            ("include_audio_description", "true"),
        ]),
        &request("Synthetic"),
        &fetch,
    )
    .unwrap();
    assert_eq!(
        fetch.only_body()["queries"].as_array().unwrap().len(),
        1,
        "no channel query for a longer list"
    );
    assert_eq!(
        ids(&outcome.response.results),
        vec!["syntheticAdEntry00002=", "syntheticFilmEntry005="]
    );
}

#[test]
fn accessibility_variants_and_clips_follow_the_settings() {
    let defaults = run(
        &config_with(&[]),
        &request("Synthetic"),
        &FixtureFetch::ok(QUERY_VARIANTS),
    )
    .unwrap();
    // Unusable entries (no video, no id) are always dropped.
    assert_eq!(
        ids(&defaults.response.results),
        vec![
            "syntheticHlsEntry0001=",
            "syntheticClipEntry004=",
            "syntheticFilmEntry005="
        ]
    );

    let everything = run(
        &config_with(&[
            ("include_audio_description", "true"),
            ("include_sign_language", "true"),
            ("min_duration_minutes", "5"),
        ]),
        &request("Synthetic"),
        &FixtureFetch::ok(QUERY_VARIANTS),
    )
    .unwrap();
    assert_eq!(
        ids(&everything.response.results),
        vec![
            "syntheticHlsEntry0001=",
            "syntheticAdEntry00002=",
            "syntheticDgsEntry0003=",
            "syntheticFilmEntry005="
        ]
    );
}

// ---------------------------------------------------------------------------
// Release mapping
// ---------------------------------------------------------------------------

#[test]
fn a_sample_entry_becomes_a_direct_file_release() {
    let outcome = run(
        &config_with(&[]),
        &request("Die Sendung mit der Maus"),
        &FixtureFetch::ok(QUERY_SAMPLE),
    )
    .unwrap();
    let result = by_id(&outcome.response.results, MAUS_KIKA);
    assert_eq!(
        result.title,
        "Die Sendung mit der Maus - Die Sendung vom 18.10.2026 [KiKA]"
    );
    assert_eq!(
        result.guid.as_deref(),
        Some("mediathekviewweb:FTXJUjzJBBIF+3iuitHuePQ9ufKsHjf8v8KCGeT8nF8=")
    );
    assert_eq!(result.published_at.as_deref(), Some("2026-10-18T09:30:00Z"));
    assert_eq!(result.protocol, Some(IndexerProtocol::Http));
    assert_eq!(result.source_kind, Some(IndexerSourceKind::Http));
    assert_eq!(result.size_bytes, None, "the API size is only an estimate");
    let page = "https://www.kika.de/die-sendung-mit-der-maus/videos/die-sendung-mit-der-maus-vom-achtzehnzehnzweitausendsechsundzwanzig-102";
    assert_eq!(result.info_url.as_deref(), Some(page));
    assert_eq!(result.link.as_deref(), Some(page));
    assert_eq!(result.provider_extra["topic"], "Die Sendung mit der Maus");
    assert_eq!(result.provider_extra["audio_description"], false);

    let http = result.http.as_ref().unwrap();
    assert_eq!(http.provider, "mediathekviewweb");
    assert_eq!(http.item_id, MAUS_KIKA);
    assert_eq!(http.page_url, page);
    assert_eq!(http.duration_seconds, Some(1818));
    assert_eq!(http.channel.as_ref().unwrap().name, "KiKA");
    assert_eq!(http.availability, HttpAvailability::Unknown);

    let media = resources_of(result, HttpResourceRole::Muxed)
        .into_iter()
        .filter(|resource| resource.transport == HttpTransport::DirectFile)
        .collect::<Vec<_>>();
    let shape = media
        .iter()
        .map(|resource| {
            (
                resource.video.as_ref().and_then(|video| video.height),
                resource.size_estimate,
                resource.container.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shape,
        vec![
            (Some(360), None, Some("mp4")),
            (Some(540), Some(305_135_616), Some("mp4")),
            (Some(1080), None, Some("mp4")),
        ]
    );
    assert!(media.iter().all(|resource| {
        resource.size_bytes.is_none()
            && resource.range_support == HttpRangeSupport::Unknown
            && resource.audio.is_none()
    }));

    // KiKA's subtitle path names no format, so none is claimed.
    let subtitles = resources_of(result, HttpResourceRole::Subtitle);
    assert_eq!(subtitles.len(), 1);
    assert_eq!(subtitles[0].transport, HttpTransport::DirectFile);
    assert_eq!(subtitles[0].container, None);
    assert_eq!(subtitles[0].subtitle.as_ref().unwrap().format, None);

    let pages = http
        .resources
        .iter()
        .filter(|resource| resource.transport == HttpTransport::ProviderPage)
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url, page);
    assert_eq!(http.resources.len(), 5);
}

#[test]
fn ard_ebu_tt_subtitles_are_ttml() {
    let outcome = run(
        &config_with(&[]),
        &request("Die Sendung mit der Maus"),
        &FixtureFetch::ok(QUERY_SAMPLE),
    )
    .unwrap();
    let result = by_id(&outcome.response.results, MAUS_WDR);
    assert_eq!(
        result.title, "Die Sendung mit der Maus vom 18.10.2026 [WDR]",
        "a title that already starts with its topic is not doubled"
    );
    let subtitle = resources_of(result, HttpResourceRole::Subtitle)[0];
    assert_eq!(subtitle.container.as_deref(), Some("ttml"));
    assert_eq!(
        subtitle.subtitle.as_ref().unwrap().format.as_deref(),
        Some("ttml")
    );
}

#[test]
fn audio_description_is_a_separate_release_with_the_audio_role() {
    let outcome = run(
        &config_with(&[("include_audio_description", "true")]),
        &request("Die Sendung mit der Maus"),
        &FixtureFetch::ok(QUERY_SAMPLE),
    )
    .unwrap();
    assert_eq!(
        ids(&outcome.response.results),
        vec![MAUS_KIKA, MAUS_WDR_AD, MAUS_WDR]
    );
    let described = by_id(&outcome.response.results, MAUS_WDR_AD);
    assert_eq!(
        described.title,
        "Die Sendung mit der Maus vom 18.10.2026 (Audiodeskription) [WDR]"
    );
    assert_eq!(described.provider_extra["audio_description"], true);
    let media = resources_of(described, HttpResourceRole::Muxed)
        .into_iter()
        .filter(|resource| resource.transport == HttpTransport::DirectFile)
        .collect::<Vec<_>>();
    assert_eq!(media.len(), 3);
    assert!(media.iter().all(|resource| {
        resource.audio.as_ref().and_then(|audio| audio.role)
            == Some(HttpAudioRole::AudioDescription)
    }));
    let plain = by_id(&outcome.response.results, MAUS_WDR);
    assert!(
        resources_of(plain, HttpResourceRole::Muxed)
            .iter()
            .all(|resource| resource.audio.is_none())
    );
}

#[test]
fn synthetic_variants_map_hls_missing_pages_and_string_numbers() {
    let outcome = run(
        &config_with(&[("include_sign_language", "true")]),
        &request("Synthetic"),
        &FixtureFetch::ok(QUERY_VARIANTS),
    )
    .unwrap();
    let results = &outcome.response.results;

    // HLS default, no other variant, a WebVTT sidecar, and a zero size.
    let hls = by_id(results, "syntheticHlsEntry0001=");
    assert_eq!(hls.title, "Synthetic Show - Folge 3: Der Testfall [ORF]");
    let media = resources_of(hls, HttpResourceRole::Muxed);
    assert_eq!(media[0].transport, HttpTransport::Hls);
    assert_eq!(media[0].container, None);
    assert_eq!(media[0].size_estimate, None);
    let subtitle = resources_of(hls, HttpResourceRole::Subtitle)[0];
    assert_eq!(subtitle.container.as_deref(), Some("vtt"));
    assert_eq!(
        subtitle.subtitle.as_ref().unwrap().format.as_deref(),
        Some("webvtt")
    );

    // Sign language: a separate release, heights from `<w>x<h>` names.
    let signed = by_id(results, "syntheticDgsEntry0003=");
    assert_eq!(signed.provider_extra["sign_language"], true);
    let heights = resources_of(signed, HttpResourceRole::Muxed)
        .iter()
        .filter_map(|resource| resource.video.as_ref().and_then(|video| video.height))
        .collect::<Vec<_>>();
    assert_eq!(heights, vec![540, 1080]);

    // No programme page: the media URL is the identity and no page is
    // offered; string numbers still parse; a duplicate URL is listed once.
    let film = by_id(results, "syntheticFilmEntry005=");
    let http = film.http.as_ref().unwrap();
    assert_eq!(
        http.page_url,
        "https://cdn.example.test/3sat/film_3360k_p36v15.mp4"
    );
    assert_eq!(film.info_url, None);
    assert_eq!(http.duration_seconds, Some(5400));
    assert_eq!(film.published_at.as_deref(), Some("2026-09-21T14:16:40Z"));
    assert!(
        http.resources
            .iter()
            .all(|resource| resource.transport != HttpTransport::ProviderPage)
    );
    let media = resources_of(film, HttpResourceRole::Muxed);
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].video, None, "a bitrate-coded name has no height");
    let subtitle = resources_of(film, HttpResourceRole::Subtitle)[0];
    assert_eq!(
        subtitle.subtitle.as_ref().unwrap().format.as_deref(),
        Some("srt")
    );
}

#[test]
fn release_titles_keep_topic_title_and_channel() {
    let entry = |topic: &str, title: &str, channel: &str| crate::api::Entry {
        topic: topic.to_string(),
        title: title.to_string(),
        channel: channel.to_string(),
        ..crate::api::Entry::default()
    };
    assert_eq!(
        release_title(&entry("Synthetic Show", "Folge 1", "ARD")),
        "Synthetic Show - Folge 1 [ARD]"
    );
    assert_eq!(
        release_title(&entry("Synthetic Show", "synthetic show: Folge 1", "")),
        "synthetic show: Folge 1"
    );
    assert_eq!(
        release_title(&entry("Synthetic Show", "", "ZDF")),
        "Synthetic Show [ZDF]"
    );
    assert_eq!(release_title(&entry("", "Folge 1", "ZDF")), "Folge 1 [ZDF]");
}

#[test]
fn heights_come_only_from_recognised_name_patterns() {
    for (url, height) in [
        ("https://cdn.example.test/a/b_AVC-360.mp4", Some(360)),
        ("https://cdn.example.test/a/b_AVC-1080.mp4?x=1", Some(1080)),
        ("https://cdn.example.test/a/b-720.mp4", Some(720)),
        ("https://cdn.example.test/a/b_720p.mp4", Some(720)),
        (
            "https://cdn.example.test/a/b_1280x720-50p-3200kbit.mp4",
            Some(720),
        ),
        ("https://cdn.example.test/a/b_3360k_p36v15.mp4", None),
        ("https://cdn.example.test/2026/10/13/b-1234.mp4", None),
        ("https://cdn.example.test/a/master.m3u8", None),
    ] {
        assert_eq!(infer_height(url), height, "{url}");
    }
}

#[test]
fn subtitle_formats_come_from_the_url() {
    for (url, format) in [
        ("https://cdn.example.test/s/a.ttml", Some("ttml")),
        ("https://cdn.example.test/s/a.XML", Some("ttml")),
        ("https://cdn.example.test/s/a.vtt?token=1", Some("webvtt")),
        ("https://cdn.example.test/s/a.srt", Some("srt")),
        (
            "https://cdn.example.test/subtitle/ebutt/urn:x:1",
            Some("ttml"),
        ),
        ("https://cdn.example.test/videos/x/subtitle", None),
    ] {
        assert_eq!(
            subtitle_format(url).map(|(format, _)| format),
            format,
            "{url}"
        );
    }
}

#[test]
fn unix_timestamps_render_as_rfc3339() {
    assert_eq!(
        rfc3339_from_unix(0).as_deref(),
        Some("1970-01-01T00:00:00Z")
    );
    assert_eq!(
        rfc3339_from_unix(951_782_400).as_deref(),
        Some("2000-02-29T00:00:00Z")
    );
    assert_eq!(
        rfc3339_from_unix(1_792_308_600).as_deref(),
        Some("2026-10-18T07:30:00Z")
    );
    assert_eq!(rfc3339_from_unix(u64::MAX), None);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn replies_classify_by_status_then_body() {
    assert_eq!(
        parse_reply(500, ERROR_SERVER.as_bytes()).unwrap_err(),
        MvwError::Upstream {
            status: 500,
            message: Some("Cannot read properties of undefined (reading 'slice')".into()),
        }
    );
    assert_eq!(
        parse_reply(503, ERROR_NOT_JSON.as_bytes()).unwrap_err(),
        MvwError::Upstream {
            status: 503,
            message: None
        }
    );
    assert_eq!(parse_reply(429, b"").unwrap_err(), MvwError::RateLimited);
    assert_eq!(
        parse_reply(404, ERROR_NOT_JSON.as_bytes()).unwrap_err(),
        MvwError::BadStatus {
            status: 404,
            message: None
        }
    );
    assert_eq!(
        parse_reply(200, ERROR_REJECTED.as_bytes()).unwrap_err(),
        MvwError::Rejected("Synthetic query failure".into())
    );
    assert!(matches!(
        parse_reply(200, ERROR_NOT_JSON.as_bytes()).unwrap_err(),
        MvwError::Malformed(_)
    ));
    assert!(matches!(
        parse_reply(200, br#"{"result":null,"err":null}"#).unwrap_err(),
        MvwError::Malformed(_)
    ));
    assert_eq!(
        parse_reply(200, QUERY_SAMPLE.as_bytes())
            .unwrap()
            .results
            .len(),
        3
    );
}

#[test]
fn engine_failures_surface_without_results() {
    for reply_result in [
        Ok(reply(500, ERROR_SERVER)),
        Ok(reply(429, "")),
        Err(MvwError::Transport("connection reset".into())),
    ] {
        let fetch = FixtureFetch::new(vec![reply_result]);
        assert!(run(&config_with(&[]), &request("Synthetic"), &fetch).is_err());
        assert_eq!(fetch.calls().len(), 1);
    }
}

#[test]
fn plugin_error_codes() {
    let code = |error: MvwError| crate::plugin_error(&error).code;
    assert_eq!(
        code(MvwError::Upstream {
            status: 502,
            message: None
        }),
        PluginErrorCode::UpstreamUnavailable
    );
    assert_eq!(
        code(MvwError::Transport("dns".into())),
        PluginErrorCode::UpstreamUnavailable
    );
    assert_eq!(code(MvwError::Cancelled), PluginErrorCode::Temporary);
    assert_eq!(
        code(MvwError::Rejected("x".into())),
        PluginErrorCode::Permanent
    );
    assert_eq!(
        code(MvwError::Malformed("x".into())),
        PluginErrorCode::Permanent
    );
    assert_eq!(
        code(MvwError::BadStatus {
            status: 400,
            message: None
        }),
        PluginErrorCode::Permanent
    );
    assert_eq!(code(MvwError::InvalidCursor), PluginErrorCode::Permanent);
    assert_eq!(
        code(MvwError::InvalidConfig("x".into())),
        PluginErrorCode::InvalidConfig
    );

    let limited = crate::plugin_error(&MvwError::RateLimited);
    assert_eq!(limited.code, PluginErrorCode::RateLimited);
    assert!(matches!(
        limited.details,
        Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::RateLimited,
                ..
            }
        ))
    ));
    let rejected = crate::plugin_error(&MvwError::Rejected("Synthetic query failure".into()));
    assert!(rejected.public_message.contains("Synthetic query failure"));
}

// ---------------------------------------------------------------------------
// Live
// ---------------------------------------------------------------------------

/// Posts through the system `curl`, so the live check needs no HTTP
/// dependency.
struct CurlFetch;

impl Fetch for CurlFetch {
    async fn post(&self, call: &QueryCall) -> Result<HttpReply, MvwError> {
        let output = std::process::Command::new("curl")
            .args([
                "-sS",
                "-m",
                "30",
                "-X",
                "POST",
                "-H",
                "Content-Type: text/plain",
                "-H",
                "Accept: application/json",
                "--data-binary",
                &call.body(),
                "-w",
                "\n%{http_code}",
                crate::api::API_URL,
            ])
            .output()
            .map_err(|error| MvwError::Transport(error.to_string()))?;
        if !output.status.success() {
            return Err(MvwError::Transport(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let mut body = output.stdout;
        let split = body
            .iter()
            .rposition(|byte| *byte == b'\n')
            .ok_or_else(|| MvwError::Transport("no status line".into()))?;
        let status = std::str::from_utf8(&body[split + 1..])
            .ok()
            .and_then(|status| status.trim().parse::<u16>().ok())
            .ok_or_else(|| MvwError::Transport("unreadable status".into()))?;
        body.truncate(split);
        Ok(HttpReply { status, body })
    }
}

/// One real query. Run with
/// `MVW_LIVE=1 cargo nextest run --locked --lib --run-ignored only live_search`.
#[test]
#[ignore = "calls the live MediathekViewWeb API; set MVW_LIVE=1"]
fn live_search_maps_releases() {
    if std::env::var("MVW_LIVE").as_deref() != Ok("1") {
        eprintln!("MVW_LIVE is not 1; skipping the live call");
        return;
    }
    let outcome = block_on(engine::run_page(
        &config_with(&[]),
        &request_with("Die Sendung mit der Maus", 5, None),
        &CurlFetch,
    ))
    .expect("live search");
    let results = &outcome.response.results;
    assert!(!results.is_empty(), "no releases");
    for result in results {
        let http = result.http.as_ref().unwrap();
        let direct = http
            .resources
            .iter()
            .filter(|resource| resource.transport == HttpTransport::DirectFile)
            .count();
        eprintln!(
            "{} | {} | {}s | resources: {} ({} direct)",
            result.title,
            result.published_at.as_deref().unwrap_or("-"),
            http.duration_seconds.unwrap_or(0),
            http.resources.len(),
            direct
        );
        assert_eq!(http.provider, "mediathekviewweb");
        assert!(!http.item_id.is_empty());
    }
    assert!(
        results.iter().any(|result| {
            result
                .http
                .as_ref()
                .unwrap()
                .resources
                .iter()
                .any(|resource| resource.transport == HttpTransport::DirectFile)
        }),
        "no direct_file resource"
    );
    eprintln!("next_cursor: {:?}", outcome.response.next_cursor);
}
