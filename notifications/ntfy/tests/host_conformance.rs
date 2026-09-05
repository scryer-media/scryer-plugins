//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this channel's: its configuration, its endpoint, and the
//! JSON body no URL assertion can reach.

// conformance: bespoke

use scryer_plugin_conformance::notification::{Check, NotificationConformance};

/// ntfy is published to as JSON at the **server root**
/// (docs.ntfy.sh/publish/#publish-as-json), so the topic is a body field rather
/// than a path segment and the notification text is a body field rather than a
/// query parameter. What is pinned is that the endpoint comes from the resolved
/// configuration and is used verbatim.
const EXPECTED_URL: &str = "https://ntfy.test.invalid/";

/// The configured topic must reach ntfy in the JSON body, and the notification
/// text must never travel in the URL: that is the whole point of the move off
/// Sonarr's query-parameter publish, and a regression to it would be invisible
/// to a URL-prefix assertion alone.
const EXPECTED_TOPIC: &str = "scryer";

#[test]
fn ntfy_release_wasm_conforms_to_the_notification_host_contract() {
    // This channel tolerates an unset setting rather than reporting a typed
    // configuration error, so the family's missing-required-setting check does
    // not apply and is opted out of rather than weakened.
    let outcome = NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "ntfy")
        .wasm("ntfy_notification.wasm")
        .config("server_url", "https://ntfy.test.invalid")
        .config("topics", EXPECTED_TOPIC)
        .expects_url(EXPECTED_URL)
        .without(Check::MissingRequiredSetting)
        .run();

    // The topic travels in the JSON body, and the notification text with it.
    let request = outcome
        .send
        .request_to(EXPECTED_URL)
        .expect("the publish request must have been made");
    let payload: serde_json::Value =
        serde_json::from_slice(&request.body).expect("ntfy is published to as JSON");
    assert_eq!(
        payload["topic"], EXPECTED_TOPIC,
        "the configured topic must be a body field: {payload}"
    );
    assert!(
        payload.get("message").is_some(),
        "the notification text must be a body field: {payload}"
    );
    assert!(
        !outcome.send.urls.iter().any(|url| url.contains('?')),
        "no notification content may travel in the URL: {:?}",
        outcome.send.urls
    );
}
