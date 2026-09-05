//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this provider's: its descriptor identity, the
//! configuration Scryer would have resolved for it, and the upstream shape of
//! its probe and its download.

use scryer_plugin_conformance::subtitle::SubtitleConformance;

const TEST_BASE_URL: &str = "https://feed.animetosho.invalid";
const TEST_SITE_URL: &str = "https://animetosho.invalid";
const TEST_API_KEY: &str = "test-api-key";
const SUBTITLE_DOWNLOAD_URL: &str = "https://animetosho.invalid/download/619135/subs/file/94001";

#[test]
fn animetosho_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "animetosho-xyz")
        .wasm("animetosho_xyz_subtitle_provider.wasm")
        .descriptor_id("animetosho-xyz-subtitles")
        .config("base_url", TEST_BASE_URL)
        .config("site_url", TEST_SITE_URL)
        .config("api_key", TEST_API_KEY)
        // The API key it read is appended to the *configured* base URL rather
        // than to a compiled-in default.
        .validate_route("", 200, br#"{"data":[]}"#.to_vec())
        .validate_reads_config("base_url")
        .validate_reads_config("api_key")
        .validate_url(&format!(
            "{TEST_BASE_URL}/json/v1/search?q=naruto&limit=1&apikey={TEST_API_KEY}"
        ))
        // Unlike the tsukihime pilot, this provider never opens the container:
        // it checks the XZ magic and hands the compressed attachment to Scryer
        // as `application/x-xz`.
        .download_route("", 200, xz_fixture())
        .download_reference(&download_reference())
        .download_expects_bytes(xz_fixture())
        .download_expects_format("ass")
        .download_expects_filename("animetosho-xyz-619135-94001-eng.ass.xz")
        .download_expects_content_type("application/x-xz")
        .download_expects_call(&format!("http:{SUBTITLE_DOWNLOAD_URL}"))
        // This provider reports the transport failure it saw rather than a
        // code of its own.
        .refused_expects_message_contains("request failed")
        .run();
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from a parsed release page.
fn download_reference() -> String {
    serde_json::json!({
        "url": SUBTITLE_DOWNLOAD_URL,
        "filename": "animetosho-xyz-619135-94001-eng.ass.xz",
        "language": "eng",
        "format": "ass",
    })
    .to_string()
}

/// A real XZ stream. The provider does not decode it, but the magic-number
/// guard in `download_subtitle_impl` rejects anything that is not one.
fn xz_fixture() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(
            "/Td6WFoAAATm1rRGBMAeGiEBFgAAAAAAAAAAAPycLfcBABlbU2NyaXB0IEluZm9dClRpdGxlOiBUZXN0CgAAABKoqqDNCqTNAAE6GiiSTfgftvN9AQAAAAAEWVo=",
        )
        .expect("fixture base64")
}
