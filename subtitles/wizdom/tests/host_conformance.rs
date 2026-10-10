//! Regression coverage through the release WASI component and scripted host.
use base64::Engine as _;
use scryer_plugin_conformance::subtitle::{
    Check, SubtitleConformance, call_subtitle, instantiate, instantiate_with,
};
use scryer_plugin_conformance::{
    HostErrorKind, HostResponder, HttpReply, HttpRoute, Script, default_respond, unsupported,
};
use scryer_plugin_sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_sdk::host::{
    PluginArchiveExtractResponse, PluginArchiveExtractedFile, PluginHostRequest, PluginHostResponse,
};
use scryer_plugin_sdk::{
    PluginErrorCode, PluginResult, SubtitlePluginDownloadRequest,
    SubtitlePluginValidateConfigRequest, SubtitleValidateConfigStatus,
};

const BASE_URL: &str = "https://wizdom.xyz";
const ZIP_BYTES: &[u8] = include_bytes!("fixtures/encoding-variants.zip");
const SUBTITLE: &str = "1\n00:00:01,000 --> 00:00:02,000\nשלום\n";

fn suite() -> SubtitleConformance {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "wizdom")
        .config("base_url", BASE_URL)
        .validate_route("", 200, br#"{"subs":[]}"#.to_vec())
        .validate_reads_config("base_url")
        .validate_url(&format!("{BASE_URL}/api/releases/tt1375666"))
        .download_reference(&download_reference())
        .refused_expects_code(PluginErrorCode::UpstreamUnavailable)
        .without(Check::Download)
}

#[test]
fn wizdom_release_wasm_conforms_to_the_subtitle_host_contract() {
    let suite = suite();
    suite.run();
    assert_validation_rejects_upstream_failures(&suite);
    assert_search_keeps_provider_miss_semantics(&suite);
    assert_aliases_are_tried_until_subtitles_are_found(&suite);
    assert_rate_limits_return_without_an_early_retry(&suite);
    assert_archive_variants_are_selected(&suite);
    assert_missing_extractor_is_reported(&suite);
}

fn assert_validation_rejects_upstream_failures(suite: &SubtitleConformance) {
    for (status, body) in [
        (500, "server error"),
        (200, ""),
        (200, "<html>error</html>"),
    ] {
        let script = suite.script_with_routes(vec![HttpRoute::any(HttpReply::new(
            status,
            body.as_bytes().to_vec(),
        ))]);
        let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
        );
        let PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(response)) = result else {
            panic!("{result:?}")
        };
        assert_ne!(response.status, SubtitleValidateConfigStatus::Valid);
        assert_eq!(store.data().script.requests.len(), 1);
    }
}

fn assert_search_keeps_provider_miss_semantics(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![HttpRoute::contains(
        "/api/releases/tt1375666",
        HttpReply::new(500, vec![]),
    )]);
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
    let result = call_subtitle(&mut store, &plugin, PluginSubtitleCommand::Search(serde_json::from_value(serde_json::json!({
        "media_kind": "movie", "title": "Example", "imdb_id": "tt1375666", "languages": ["heb"]
    })).unwrap()));
    let PluginSubtitleCommandResult::Search(PluginResult::Ok(response)) = result else {
        panic!("{result:?}")
    };
    assert!(response.results.is_empty());
    assert_eq!(store.data().script.requests.len(), 1);
}

fn assert_aliases_are_tried_until_subtitles_are_found(suite: &SubtitleConformance) {
    let configured = self::suite().config("tmdb_api_key", "fixture-key");
    let script = configured.script_with_routes(vec![
        HttpRoute::contains("query=Missing&", HttpReply::ok(r#"{"results":[]}"#)),
        HttpRoute::contains(
            "query=Unlisted&",
            HttpReply::ok(r#"{"results":[{"id":1}]}"#),
        ),
        HttpRoute::contains("/movie/1?", HttpReply::ok(r#"{"imdb_id":"tt1"}"#)),
        HttpRoute::contains("/api/releases/tt1", HttpReply::new(500, vec![])),
        HttpRoute::contains("query=Alias&", HttpReply::ok(r#"{"results":[{"id":2}]}"#)),
        HttpRoute::contains("/movie/2?", HttpReply::ok(r#"{"imdb_id":"tt2"}"#)),
        HttpRoute::contains(
            "/api/releases/tt2",
            HttpReply::ok(r#"{"subs":[{"id":42,"version":"Example.1080p"}]}"#),
        ),
    ]);
    let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
    let result = call_subtitle(&mut store, &plugin, PluginSubtitleCommand::Search(serde_json::from_value(serde_json::json!({
        "media_kind": "movie", "title": "Missing", "title_candidates": ["Missing", "Unlisted"],
        "title_aliases": ["Missing", "Alias"], "languages": ["heb"]
    })).unwrap()));
    let PluginSubtitleCommandResult::Search(PluginResult::Ok(response)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(response.results.len(), 1);
    assert!(response.results[0].provider_file_id.contains("42"));
    assert_eq!(store.data().script.requests.len(), 7);
}

fn assert_rate_limits_return_without_an_early_retry(suite: &SubtitleConformance) {
    for delay in ["10", "3600"] {
        let script = suite.script_with_routes(vec![HttpRoute::any(
            HttpReply::new(429, vec![]).with_header("Retry-After", delay),
        )]);
        let (mut store, plugin) = instantiate(&suite.wasm_path(), script);
        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
        );
        let PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(response)) = result else {
            panic!("{result:?}")
        };
        assert_eq!(response.status, SubtitleValidateConfigStatus::RateLimited);
        assert_eq!(response.retry_after_seconds, Some(delay.parse().unwrap()));
        // Assert requests, not elapsed time: no wall-clock scheduling race.
        assert_eq!(store.data().script.requests.len(), 1);
    }
}

fn assert_archive_variants_are_selected(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![HttpRoute::any(HttpReply::new(
        200,
        ZIP_BYTES.to_vec(),
    ))]);
    let (mut store, plugin) = instantiate_with(
        &suite.wasm_path(),
        script,
        ArchiveResponder { available: true },
    );
    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Ok(response)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(response.content_base64)
            .unwrap(),
        SUBTITLE.as_bytes()
    );
    assert_eq!(response.format, "srt");
    assert_eq!(response.filename.as_deref(), Some("utf8.srt"));
    assert!(store.data().script.made_call("archive_extract:zip"));
    let request = store
        .data()
        .script
        .request_to(&format!("{BASE_URL}/api/files/sub/9229"))
        .unwrap();
    assert_eq!(
        request.header("Referer"),
        Some("https://wizdom.xyz/movies/tt1375666")
    );
}

fn assert_missing_extractor_is_reported(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![HttpRoute::any(HttpReply::new(
        200,
        ZIP_BYTES.to_vec(),
    ))]);
    let (mut store, plugin) = instantiate_with(
        &suite.wasm_path(),
        script,
        ArchiveResponder { available: false },
    );
    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Err(error)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

#[derive(Clone)]
struct ArchiveResponder {
    available: bool,
}

impl HostResponder for ArchiveResponder {
    fn respond(
        &mut self,
        request: PluginHostRequest,
        script: &mut Script,
    ) -> Result<PluginHostResponse, HostErrorKind> {
        match request {
            PluginHostRequest::ArchiveExtract(request) => {
                assert_eq!(request.format, "zip");
                assert_eq!(request.content, ZIP_BYTES);
                script.calls.push("archive_extract:zip".into());
                let files = [
                    ("invalid.srt", b"invalid subtitle".to_vec()),
                    (
                        "legacy.srt",
                        b"1\n00:00:01,000 --> 00:00:02,000\n\xf9\xec\xe5\xed\n".to_vec(),
                    ),
                    ("utf8.srt", SUBTITLE.as_bytes().to_vec()),
                ]
                .into_iter()
                .map(|(name, content)| PluginArchiveExtractedFile {
                    relative_path: name.into(),
                    content,
                })
                .collect();
                Ok(PluginHostResponse::ArchiveExtract(if self.available {
                    PluginResult::Ok(PluginArchiveExtractResponse { files })
                } else {
                    PluginResult::Err(unsupported("no archive extractor installed"))
                }))
            }
            other => default_respond(other, script),
        }
    }
}

fn download_reference() -> String {
    serde_json::json!({"subtitle_id":"9229", "filename":"9229.zip", "page_url":format!("{BASE_URL}/movies/tt1375666")}).to_string()
}
