//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this channel's: its descriptor
//! identity, the configuration Scryer would have resolved for it, and the
//! endpoint a delivery has to reach.

use scryer_plugin_conformance::notification::NotificationConformance;

#[test]
fn apprise_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "apprise")
        .wasm("apprise_notification.wasm")
        .config("server_url", "https://apprise.test.invalid")
        .config("configuration_key", "scryer")
        .expects_url_prefix("https://apprise.test.invalid/notify/scryer")
        .required_setting("server_url")
        .run();
}
