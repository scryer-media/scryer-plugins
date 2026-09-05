//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs` does.
//!
//! # What is specific to ameNZB
//!
//! conformance: bespoke — this is the only subtitle provider that does not own
//! its search: it delegates to a shared newznab protocol engine that lives
//! outside the subtitle family. That shared crate is exactly where a stray
//! world import would come from, and it would compile and build perfectly
//! before failing to instantiate here. So the shared suite's world-conformance
//! check is load-bearing for this plugin in a way it is not for the
//! self-contained providers, and [`assert_search_drives_the_shared_newznab_engine`]
//! — which has no counterpart in any other provider — is the assertion that
//! proves the shared engine actually *works* over `scryer:host/services`
//! rather than merely linking.

use std::collections::BTreeMap;

use scryer_plugin_conformance::subtitle::{SubtitleConformance, call_subtitle, instantiate};
use scryer_plugin_conformance::{HttpReply, HttpRoute};
use scryer_plugin_sdk::PluginErrorCode;
use scryer_plugin_sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_sdk::{PluginResult, SubtitlePluginSearchRequest, SubtitleQueryMediaKind};

const BASE_URL: &str = "https://amenzb.test";
const API_ENDPOINT: &str = "https://amenzb.test/api";
const RELEASE_ID: &str = "172993653";
const SUBTITLE_ID: &str = "10857";
const TEST_API_KEY: &str = "test-api-key";
const SUBTITLE_BYTES: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n";

#[test]
fn amenzb_release_wasm_conforms_to_the_subtitle_host_contract() {
    let suite = suite();

    suite.assert_artifact_is_a_component();
    suite.assert_world_conformance();
    suite.assert_describe_returns_a_subtitle_descriptor();
    suite.assert_validate_config_reaches_the_host_services();
    assert_search_drives_the_shared_newznab_engine(&suite);
    suite.assert_download_streams_the_file_through_host_http();
    suite.assert_a_refused_host_capability_stays_in_band();
    suite.assert_generate_is_unsupported_in_band();
    suite.assert_another_family_is_an_invocation_error();
}

fn suite() -> SubtitleConformance {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "amenzb")
        .wasm("amenzb_subtitles.wasm")
        .descriptor_id("amenzb-subtitles")
        .config("api_key", TEST_API_KEY)
        .config("base_url", BASE_URL)
        // ameNZB validates its configuration locally — there is no upstream
        // probe — so the assertion pins both halves: the keys are read through
        // host services, and no HTTP is attempted while doing it. A validation
        // that quietly started calling upstream would be a new egress from a
        // code path Scryer runs on every settings save.
        .validate_reads_config("api_key")
        .validate_reads_config("base_url")
        .validate_makes_no_http()
        // ameNZB serves plain subtitle files, so the bytes are handed to
        // Scryer exactly as they arrive.
        .download_reference(&download_reference())
        .download_expects_bytes(SUBTITLE_BYTES.to_vec())
        .download_expects_call(&format!("http:{}", subtitle_url()))
        .refused_is_not_code(PluginErrorCode::Unsupported)
        // Ordered, and the order matters: the release page's URL is a prefix
        // of the subtitle URL beneath it, so the more specific route has to be
        // matched first.
        .download_route(&subtitle_url(), 200, SUBTITLE_BYTES.to_vec())
        .download_route(
            &format!("{BASE_URL}/release/{RELEASE_ID}"),
            200,
            release_page().into_bytes(),
        )
}

/// The assertion this whole plugin's suite exists for.
///
/// ameNZB's search is `newznab_common::execute_raw_search` — shared code that
/// lives outside the subtitle family — followed by one detail-page fetch per
/// release. Both hops must travel over `scryer:host/services`, at the
/// configured base URL, carrying the key read from config. If the shared engine
/// were still bound to another world this component would not have
/// instantiated; if it were bound to no transport at all it would instantiate
/// and then reach nothing, which is what this checks.
fn assert_search_drives_the_shared_newznab_engine(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![
        HttpRoute::contains(
            API_ENDPOINT,
            HttpReply::new(200, newznab_feed().into_bytes()),
        ),
        HttpRoute::contains(
            &subtitle_url(),
            HttpReply::new(200, SUBTITLE_BYTES.to_vec()),
        ),
        HttpRoute::contains(
            &format!("{BASE_URL}/release/{RELEASE_ID}"),
            HttpReply::new(200, release_page().into_bytes()),
        ),
    ]);
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Search(search_request()),
    );
    let PluginSubtitleCommandResult::Search(PluginResult::Ok(response)) = result else {
        panic!("search did not return a typed ok result: {result:?}");
    };

    let candidate = response
        .results
        .first()
        .unwrap_or_else(|| panic!("search returned no candidates: {response:?}"));
    assert_eq!(candidate.language, "eng");

    let calls = &store.data().script.calls;
    let api_call = calls
        .iter()
        .find(|call| call.starts_with(&format!("http:{API_ENDPOINT}?")))
        .unwrap_or_else(|| {
            panic!("the shared newznab engine made no API call over host services: {calls:?}")
        });
    assert!(
        api_call.contains(&format!("apikey={TEST_API_KEY}")),
        "the key read from config must reach the newznab request: {api_call}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{BASE_URL}/release/{RELEASE_ID}")),
        "the release detail page must be fetched over host services: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("archive_extract")),
        "this provider does not open archives: {calls:?}"
    );
}

fn search_request() -> SubtitlePluginSearchRequest {
    SubtitlePluginSearchRequest {
        media_kind: SubtitleQueryMediaKind::Episode,
        facet: Some("anime".to_string()),
        file_hash: None,
        imdb_id: None,
        series_imdb_id: None,
        title: "Kinomi Master".to_string(),
        title_aliases: vec![],
        title_candidates: vec![],
        year: None,
        season: Some(1),
        episode: Some(12),
        absolute_episode: None,
        external_ids: BTreeMap::new(),
        languages: vec!["eng".to_string()],
        release_group: None,
        source: None,
        video_codec: None,
        audio_codec: None,
        resolution: None,
        hearing_impaired: None,
        include_ai_translated: false,
        include_machine_translated: false,
    }
}

/// One newznab item, as the shared engine parses it. The `guid` attribute is
/// what ameNZB turns into the release id it then fetches a detail page for.
fn newznab_feed() -> String {
    format!(
        r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>[SubsPlease] Kinomi Master - 12 (1080p) [WEB-DL]</title>
    <guid>{RELEASE_ID}</guid>
    <link>{BASE_URL}/release/{RELEASE_ID}</link>
    <pubDate>Tue, 02 Jan 2024 14:00:00 +0000</pubDate>
    <enclosure url="{BASE_URL}/dl/{RELEASE_ID}" length="1048576" type="application/x-nzb"/>
    <newznab:attr name="guid" value="{RELEASE_ID}"/>
    <newznab:attr name="grabs" value="42"/>
    <newznab:attr name="subs" value="English"/>
  </item>
</channel>
</rss>"#
    )
}

/// The subtitle table ameNZB scrapes off a release page.
fn release_page() -> String {
    format!(
        r#"<html><body>
        <div id="subtitlesBody" class="collapse">
          <table><tbody>
            <tr>
              <td><code>eng</code></td>
              <td>English subs <span class="badge">Default</span></td>
              <td><code>srt</code></td>
              <td>36 KB</td>
              <td><a href="/release/{RELEASE_ID}/subtitles/{SUBTITLE_ID}">Download</a></td>
            </tr>
          </tbody></table>
        </div>
        </body></html>"#
    )
}

fn subtitle_url() -> String {
    format!("{BASE_URL}/release/{RELEASE_ID}/subtitles/{SUBTITLE_ID}")
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one subtitle row.
fn download_reference() -> String {
    serde_json::json!({
        "url": subtitle_url(),
        "release_id": RELEASE_ID,
        "subtitle_id": SUBTITLE_ID,
        "filename": "Kinomi.Master.S01E12.eng.srt",
        "language": "eng",
        "format": "srt",
        "label": "English subs",
    })
    .to_string()
}
