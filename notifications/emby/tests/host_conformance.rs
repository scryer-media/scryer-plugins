//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this channel's: its descriptor
//! identity, the configuration Scryer would have resolved for it, and the
//! endpoint a delivery has to reach.

use scryer_plugin_conformance::notification::Check;
use scryer_plugin_conformance::notification::NotificationConformance;

// This channel's `send` is a library refresh rather than a message, and it
// tolerates an unset setting rather than reporting a typed configuration
// error — so the family's missing-required-setting check does not apply and is
// opted out of rather than weakened.

#[test]
fn emby_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "emby")
        .wasm("emby_notification.wasm")
        .config("base_url", "https://emby.test.invalid")
        .config("api_key", "embykey")
        .expects_url_prefix("https://emby.test.invalid/System/Info")
        .without(Check::MissingRequiredSetting)
        .run();
}
