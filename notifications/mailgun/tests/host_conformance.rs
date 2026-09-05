//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this channel's: its configuration, its endpoint, and the
//! form body no URL assertion can reach.

// conformance: bespoke

use scryer_plugin_conformance::notification::NotificationConformance;

/// A prefix rather than a whole URL: what is pinned is that the sending domain
/// and the region both come from the resolved configuration and are used
/// verbatim in the path. The message itself is a URL-encoded form body,
/// asserted separately below.
const EXPECTED_URL_PREFIX: &str = "https://api.mailgun.net/v3/mg.test.invalid/messages";

#[test]
fn mailgun_release_wasm_conforms_to_the_notification_host_contract() {
    let outcome = NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "mailgun")
        .wasm("mailgun_notification.wasm")
        .config("sender_domain", "mg.test.invalid")
        .config("from", "scryer@test.invalid")
        .config("recipients", "ops@test.invalid")
        .config("api_key", "mailgunkey")
        .expects_url_prefix(EXPECTED_URL_PREFIX)
        .required_setting("sender_domain")
        .run();

    // Mailgun is posted to as a URL-encoded form, and every field that decides
    // where the mail goes travels in that body — never in the URL.
    let request = outcome
        .send
        .request_with_url_prefix(EXPECTED_URL_PREFIX)
        .expect("the message request must have been made");
    let form = String::from_utf8_lossy(&request.body).to_string();
    for field in [
        "from=scryer%40test.invalid",
        "to=ops%40test.invalid",
        "subject=",
        "text=",
        "html=",
    ] {
        assert!(
            form.contains(field),
            "the form body must carry {field}: {form}"
        );
    }
    assert!(
        !outcome.send.urls.iter().any(|url| url.contains('?')),
        "no message content may travel in the URL: {:?}",
        outcome.send.urls
    );
}
