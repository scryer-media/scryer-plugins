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
fn telegram_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "telegram")
        .wasm("telegram_notification.wasm")
        .config("chat_id", "123456")
        .config("bot_token", "bottoken")
        .expects_url_prefix("https://api.telegram.org/botbottoken/sendmessage")
        .required_setting("chat_id")
        .run();
}
