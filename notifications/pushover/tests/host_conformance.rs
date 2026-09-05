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
fn pushover_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "pushover")
        .wasm("pushover_notification.wasm")
        .config("api_key", "pushoverkey")
        .config("user_key", "pushoveruser")
        .expects_url_prefix("https://api.pushover.net/1/messages.json")
        .required_setting("api_key")
        .run();
}
