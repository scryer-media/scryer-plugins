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
fn discord_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "discord")
        .wasm("discord_notification.wasm")
        .config(
            "webhook_url",
            "https://discord.test.invalid/api/webhooks/1/token",
        )
        .expects_url_prefix("https://discord.test.invalid/api/webhooks/1/token")
        .required_setting("webhook_url")
        .run();
}
