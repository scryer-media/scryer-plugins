//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs` does.
//!
//! # What is specific to Tsukihime
//!
//! conformance: bespoke — this is the archive-delegation pilot, and the only
//! catalog provider whose download crosses a *second* host capability. Its
//! attachments are XZ streams that the host's archive service opens; every
//! other provider hands its container straight through. So the shared
//! download and refused-capability checks are replaced by two of this
//! provider's own, driven by a responder that answers `ArchiveExtract`.

use scryer_plugin_conformance::subtitle::{
    Check, SubtitleConformance, call_subtitle, instantiate_with,
};
use scryer_plugin_conformance::{
    HostErrorKind, HostResponder, HttpReply, HttpRoute, Script, default_respond, unsupported,
};
use scryer_plugin_sdk::PluginErrorCode;
use scryer_plugin_sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_sdk::host::{
    PluginArchiveExtractResponse, PluginArchiveExtractedFile, PluginHostRequest, PluginHostResponse,
};
use scryer_plugin_sdk::{PluginResult, SubtitlePluginDownloadRequest};

const TEST_BASE_URL: &str = "https://api.tsukihime.invalid/v1";
const SUBTITLE_TEXT: &[u8] = b"[Script Info]\nTitle: Test\n";

#[test]
fn tsukihime_release_wasm_conforms_to_the_subtitle_host_contract() {
    let suite = suite();

    suite.assert_artifact_is_a_component();
    suite.assert_world_conformance();
    suite.assert_describe_returns_a_subtitle_descriptor();
    suite.assert_validate_config_reaches_the_host_services();
    assert_download_delegates_xz_to_the_host_archive_service(&suite);
    assert_missing_archive_extractor_stays_in_band(&suite);
    suite.assert_generate_is_unsupported_in_band();
    suite.assert_another_family_is_an_invocation_error();
}

fn suite() -> SubtitleConformance {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "tsukihime")
        .wasm("tsukihime_subtitles.wasm")
        .descriptor_id("tsukihime-subtitles")
        .config("base_url", TEST_BASE_URL)
        // The provider's configuration, rate-limit state, and upstream request
        // all travel over the one `host-call` import.
        .validate_route("", 200, br#"{"torrents":1}"#.to_vec())
        .validate_reads_config("base_url")
        // The provider owns its rate-limit window in host state.
        .validate_expects_call_prefix("state_get:")
        .validate_url(&format!("{TEST_BASE_URL}/stats"))
        // Download and the refused-capability case are this provider's own,
        // below: both need the archive responder.
        .without(Check::Download)
        .without(Check::RefusedHostCapability)
}

/// The point of the migration: XZ is opened by the host's archive service, not
/// by a decompressor bundled into this plugin.
fn assert_download_delegates_xz_to_the_host_archive_service(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![HttpRoute::any(HttpReply::new(200, xz_fixture()))]);
    let responder = ArchiveResponder::Files(vec![PluginArchiveExtractedFile {
        relative_path: "Show_track3.eng.ass".to_string(),
        content: SUBTITLE_TEXT.to_vec(),
    }]);
    let (mut store, plugin) = instantiate_with(&suite.wasm_path(), script, responder);

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Ok(response)) = result else {
        panic!("download did not return a typed ok result: {result:?}");
    };

    use base64::Engine as _;
    let content = base64::engine::general_purpose::STANDARD
        .decode(response.content_base64)
        .expect("download content is base64");
    assert_eq!(content, SUBTITLE_TEXT);
    assert_eq!(response.format, "ass");
    assert_eq!(response.filename.as_deref(), Some("Show_track3.eng.ass.xz"));
    assert_eq!(response.content_type.as_deref(), Some("text/x-ssa"));

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call == "archive_extract:xz"),
        "the XZ attachment must be opened by the host archive service: {calls:?}"
    );
}

/// Capability availability is in-band. A Scryer with no archive extractor
/// installed answers `Unsupported` through the response, never through
/// `host-error`, and the provider must surface that as a typed plugin error
/// rather than a world-level invocation failure.
fn assert_missing_archive_extractor_stays_in_band(suite: &SubtitleConformance) {
    let script = suite.script_with_routes(vec![HttpRoute::any(HttpReply::new(200, xz_fixture()))]);
    let (mut store, plugin) =
        instantiate_with(&suite.wasm_path(), script, ArchiveResponder::Unsupported);

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Err(error)) = result else {
        panic!("a missing extractor must be a typed plugin error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

/// The host's archive service, as the shared switchboard does not model it.
/// Everything else is handed straight back to `default_respond`.
#[derive(Clone, Debug)]
enum ArchiveResponder {
    Files(Vec<PluginArchiveExtractedFile>),
    /// What a Scryer with no archive extractor installed answers.
    Unsupported,
}

impl HostResponder for ArchiveResponder {
    fn respond(
        &mut self,
        request: PluginHostRequest,
        script: &mut Script,
    ) -> Result<PluginHostResponse, HostErrorKind> {
        match request {
            PluginHostRequest::ArchiveExtract(request) => {
                script
                    .calls
                    .push(format!("archive_extract:{}", request.format));
                Ok(match self {
                    Self::Files(files) => PluginHostResponse::ArchiveExtract(PluginResult::Ok(
                        PluginArchiveExtractResponse {
                            files: files.clone(),
                        },
                    )),
                    Self::Unsupported => PluginHostResponse::ArchiveExtract(PluginResult::Err(
                        unsupported("no archive extractor is installed"),
                    )),
                })
            }
            other => default_respond(other, script),
        }
    }
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from a Tsukihime attachment.
fn download_reference() -> String {
    serde_json::json!({
        "torrent_id": 1,
        "file_id": 2,
        "attachment_id": 42,
        "url": "https://storage.tsukihime.invalid/attach/0000002A/Show_track3.eng.ass.xz",
        "filename": "Show_track3.eng.ass.xz",
        "format": "ass",
        "language": "eng",
    })
    .to_string()
}

/// A real XZ stream of [`SUBTITLE_TEXT`].
///
/// The plugin no longer decodes this itself, but sending real bytes keeps the
/// scripted extractor honest about what it is standing in for.
fn xz_fixture() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(
            "/Td6WFoAAATm1rRGBMAeGiEBFgAAAAAAAAAAAPycLfcBABlbU2NyaXB0IEluZm9dClRpdGxlOiBUZXN0CgAAABKoqqDNCqTNAAE6GiiSTfgftvN9AQAAAAAEWVo=",
        )
        .expect("fixture base64")
}
