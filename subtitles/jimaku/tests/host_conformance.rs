//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this provider's: its descriptor identity, the
//! configuration Scryer would have resolved for it, and the upstream shape of
//! its probe and its download.

use scryer_plugin_conformance::subtitle::SubtitleConformance;
use scryer_plugin_sdk::PluginErrorCode;

/// Jimaku's API base is a compiled-in constant rather than a config field, so
/// the URL assertions pin the advertised base rather than a scripted one.
const EXPECTED_VALIDATION_URL: &str = "https://jimaku.cc/api/entries/search?query=naruto";
const SUBTITLE_DOWNLOAD_URL: &str = "https://jimaku.cc/api/entries/42/files/Show.S01E01.eng.srt";
const TEST_API_KEY: &str = "test-api-key";

#[test]
fn jimaku_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "jimaku")
        .wasm("jimaku_subtitle_provider.wasm")
        .config("api_key", TEST_API_KEY)
        .validate_route("", 200, b"[]".to_vec())
        .validate_reads_config("api_key")
        .validate_url(EXPECTED_VALIDATION_URL)
        // Unlike the tsukihime pilot, this provider never opens a container:
        // Jimaku serves plain subtitle files and archives alike, and both are
        // handed to Scryer as-is.
        .download_route("", 200, subtitle_fixture())
        .download_reference(&download_reference())
        .download_expects_bytes(subtitle_fixture())
        .download_expects_format("srt")
        .download_expects_filename("Show.S01E01.eng.srt")
        .download_expects_call(&format!("http:{SUBTITLE_DOWNLOAD_URL}"))
        .refused_expects_code(PluginErrorCode::UpstreamUnavailable)
        .run();
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one Jimaku entry file.
fn download_reference() -> String {
    serde_json::json!({
        "url": SUBTITLE_DOWNLOAD_URL,
        "filename": "Show.S01E01.eng.srt",
        "language": "eng",
        "episode": 1,
    })
    .to_string()
}

/// A plain subtitle body comfortably over the provider's 500-byte floor for
/// non-archive files.
fn subtitle_fixture() -> Vec<u8> {
    let mut body = String::new();
    for index in 1..=20 {
        body.push_str(&format!(
            "{index}\n00:00:{index:02},000 --> 00:00:{index:02},900\nLine {index}\n\n"
        ));
    }
    body.into_bytes()
}
