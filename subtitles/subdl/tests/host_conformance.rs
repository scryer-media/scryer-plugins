//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this provider's: its descriptor
//! identity, the configuration Scryer would have resolved for it, and the
//! upstream shape of its probe and its download.

use scryer_plugin_conformance::subtitle::SubtitleConformance;
use scryer_plugin_sdk::PluginErrorCode;

/// Subdl's API base is a compiled-in constant rather than a config field, so
/// the URL assertions pin the advertised base rather than a scripted one.
const API_BASE: &str = "https://api.subdl.com/api/v1";
const SUBTITLE_DOWNLOAD_URL: &str = "https://dl.subdl.com/subtitle/Show.S01E01.eng.zip";
const TEST_API_KEY: &str = "test-api-key";
const ZIP_BYTES: &[u8] = b"PK\x03\x04 pretend this is a subtitle archive";

#[test]
fn subdl_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "subdl")
        .wasm("subdl_subtitle_provider.wasm")
        .config("api_key", TEST_API_KEY)
        .validate_route(
            "",
            200,
            br#"{"status":true,"results":[],"subtitles":[]}"#.to_vec(),
        )
        .validate_reads_config("api_key")
        .validate_url_prefix(&format!("{API_BASE}/subtitles?"))
        // The key read from config must reach the request, and the probe
        // carries the provider's own fixed title.
        .validate_url_contains(&format!("api_key={TEST_API_KEY}"))
        .validate_url_contains("film_name=Inception")
        // Unlike the tsukihime pilot, this provider never opens a container:
        // Subdl serves zipped subtitles and they are handed to Scryer as-is.
        .download_route("", 200, ZIP_BYTES.to_vec())
        .download_reference(&download_reference())
        .download_expects_bytes(ZIP_BYTES.to_vec())
        .download_expects_format("zip")
        .download_expects_filename("Show.S01E01.eng.zip")
        .download_expects_content_type("application/zip")
        .download_expects_call(&format!("http:{SUBTITLE_DOWNLOAD_URL}"))
        // The assertion that proves the migration carries `FailureKind` to the
        // host: an unreachable upstream arrives as `UpstreamUnavailable`, not
        // as a bare message.
        .refused_expects_code(PluginErrorCode::UpstreamUnavailable)
        .run();
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one Subdl subtitle row.
fn download_reference() -> String {
    serde_json::json!({
        "download_url": SUBTITLE_DOWNLOAD_URL,
        "filename": "Show.S01E01.eng.zip",
        "content_type": "application/zip",
        "page_url": "https://subdl.com/subtitle/1",
    })
    .to_string()
}
