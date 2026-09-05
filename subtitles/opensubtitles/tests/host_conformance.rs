//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this provider's.
//!
//! ## Why the script routes by URL
//!
//! Unlike the single-hop providers, one OpenSubtitles operation is several
//! upstream requests: log in, ask for a download link, then fetch the link.
//! A one-response script cannot express that, so the stand-in matches a
//! request URL against an ordered route table. An empty table is the
//! "host refuses everything" case, which is what the in-band assertion needs.

use scryer_plugin_conformance::subtitle::SubtitleConformance;
use scryer_plugin_sdk::PluginErrorCode;

/// OpenSubtitles' API base is a compiled-in constant (the login response may
/// renegotiate it), so the URL assertions pin the advertised base.
const API_BASE: &str = "https://api.opensubtitles.com/api/v1";
const CONTENT_URL: &str = "https://dl.opensubtitles.invalid/sub.srt";
const TEST_API_KEY: &str = "test-api-key";
const TEST_USERNAME: &str = "test-user";
const TEST_PASSWORD: &str = "test-password";
const SUBTITLE_TEXT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n";

#[test]
fn opensubtitles_release_wasm_conforms_to_the_subtitle_host_contract() {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "opensubtitles")
        .wasm("opensubtitles_subtitle_provider.wasm")
        .config("api_key", TEST_API_KEY)
        .config("username", TEST_USERNAME)
        .config("password", TEST_PASSWORD)
        // All three credentials and the login round trip travel over the one
        // `host-call` import, and the login goes to the advertised API base.
        .validate_route("/login", 200, br#"{"token":"test-token"}"#.to_vec())
        .validate_route("/infos/user", 200, br#"{"data":{}}"#.to_vec())
        .validate_reads_config("api_key")
        .validate_reads_config("username")
        .validate_reads_config("password")
        .validate_url(&format!("{API_BASE}/login"))
        // validate_config must confirm the session it just opened.
        .validate_expects_call(&format!("http:{API_BASE}/infos/user"))
        // Download is three upstream hops — login, download link, content — and
        // every one of them crosses the single host-services import. Nothing
        // here opens a container: OpenSubtitles hands back a plain `srt`.
        .download_route("/login", 200, br#"{"token":"test-token"}"#.to_vec())
        .download_route(
            "/download",
            200,
            format!(r#"{{"link":"{CONTENT_URL}"}}"#).into_bytes(),
        )
        .download_route("dl.opensubtitles.invalid", 200, SUBTITLE_TEXT.to_vec())
        .download_reference("123456")
        .download_expects_bytes(SUBTITLE_TEXT.to_vec())
        .download_expects_format("srt")
        .download_expects_content_type("text/plain; charset=utf-8")
        .download_expects_call(&format!("http:{API_BASE}/login"))
        .download_expects_call(&format!("http:{API_BASE}/download"))
        .download_expects_call(&format!("http:{CONTENT_URL}"))
        // The login hop is the one that trips when the host refuses egress.
        .refused_expects_code(PluginErrorCode::UpstreamUnavailable)
        .run();
}
