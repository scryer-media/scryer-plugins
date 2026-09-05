//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this channel's: its configuration, its endpoint, and the
//! one thing no URL assertion can reach — where the credential travels.

// conformance: bespoke

use scryer_plugin_conformance::notification::NotificationConformance;

const EXPECTED_URL_PREFIX: &str = "https://gotify.test.invalid/message";

/// The application token travels in Gotify's documented `X-Gotify-Key` header
/// rather than Sonarr's `?token=` query parameter. Both are still accepted by
/// the server (`appTokenHeader`/`appTokenQuery` in gotify/server's REST-API
/// spec), and only the header keeps the token out of reverse-proxy access logs.
/// The token therefore no longer appears in the URL and is asserted on the
/// request headers instead — a stronger check than the query-string prefix it
/// replaces, because it also pins the name of the credential header.
const EXPECTED_TOKEN_HEADER: (&str, &str) = ("x-gotify-key", "apptoken");

#[test]
fn gotify_release_wasm_conforms_to_the_notification_host_contract() {
    let outcome = NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "gotify")
        .wasm("gotify_notification.wasm")
        .config("server", "https://gotify.test.invalid")
        .config("app_token", "apptoken")
        .expects_url_prefix(EXPECTED_URL_PREFIX)
        .required_setting("server")
        .run();

    // The application token is a header, not a query parameter: it must never
    // appear in the URL, and it must be sent under the name Gotify documents.
    let request = outcome
        .send
        .request_with_url_prefix(EXPECTED_URL_PREFIX)
        .expect("the message request must have been made");
    let (name, value) = EXPECTED_TOKEN_HEADER;
    assert_eq!(
        request.header(name),
        Some(value),
        "the app token must travel in the {name} header; got {:?}",
        request.headers
    );
    assert!(
        !outcome.send.urls.iter().any(|url| url.contains("apptoken")),
        "the app token must not appear in a URL: {:?}",
        outcome.send.urls
    );
}
