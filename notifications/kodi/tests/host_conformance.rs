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
fn kodi_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "kodi")
        .wasm("kodi_notification.wasm")
        // `server_url`, not `host`: Scryer builds this channel's HTTP allowlist
        // from configuration values that parse as URLs, so a bare host reaches
        // nothing in production. The endpoint expectation below is therefore
        // also the regression guard for that.
        .config("server_url", "http://kodi.test.invalid:8080")
        .expects_url_prefix("http://kodi.test.invalid:8080/jsonrpc")
        .without(Check::MissingRequiredSetting)
        .run();
}
