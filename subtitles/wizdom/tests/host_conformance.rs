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

/// Wizdom needs no credentials; `base_url` is its only required setting, and
/// the probe is a releases lookup against a fixed IMDb ID.
const BASE_URL: &str = "https://wizdom.xyz";
const ZIP_BYTES: &[u8] = b"PK\x03\x04 pretend this is a subtitle archive";

#[test]
fn wizdom_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "wizdom")
        .config("base_url", BASE_URL)
        .validate_route("", 200, br#"{"subs":[]}"#.to_vec())
        .validate_reads_config("base_url")
        .validate_url(&format!("{BASE_URL}/api/releases/tt1375666"))
        // Wizdom serves zipped subtitles and they are handed to Scryer as-is.
        .download_route("", 200, ZIP_BYTES.to_vec())
        .download_reference(&download_reference())
        .download_expects_bytes(ZIP_BYTES.to_vec())
        .download_expects_format("zip")
        .download_expects_filename("9229.zip")
        .download_expects_content_type("application/zip")
        .download_expects_call(&format!("http:{BASE_URL}/api/files/sub/9229"))
        // An unreachable upstream arrives as `UpstreamUnavailable`, not as a
        // bare message.
        .refused_expects_code(PluginErrorCode::UpstreamUnavailable)
        .run();
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one Wizdom releases row.
fn download_reference() -> String {
    serde_json::json!({
        "subtitle_id": "9229",
        "filename": "9229.zip",
        "page_url": format!("{BASE_URL}/movies/tt1375666"),
    })
    .to_string()
}
