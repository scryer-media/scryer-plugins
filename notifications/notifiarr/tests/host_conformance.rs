//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way the host does.
//! What stays here is what is genuinely this channel's: its descriptor
//! identity, the configuration Scryer would have resolved for it, and the
//! endpoint a delivery has to reach.

use scryer_plugin_conformance::notification::NotificationConformance;

/// Notifiarr's own 36-character key shape (`APIKeyLength`,
/// `Notifiarr/notifiarr:pkg/website/website_routes.go:24`).
const SCRIPTED_API_KEY: &str = "00000000-1111-2222-3333-444444444444";

#[test]
fn notifiarr_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "notifiarr")
        .wasm("notifiarr_notification.wasm")
        .config("api_key", SCRIPTED_API_KEY)
        // `channel_id` is the Discord channel Notifiarr's passthrough
        // integration requires (`discord.ids.channel`, Required).
        .config("channel_id", "910000000000000001")
        // The endpoint comes from the resolved configuration and is used
        // verbatim — here including the API key, which the passthrough
        // integration takes as a path segment.
        .expects_url_prefix("https://notifiarr.com/api/v1/notification/passthrough/")
        .required_setting("api_key")
        .run();
}
