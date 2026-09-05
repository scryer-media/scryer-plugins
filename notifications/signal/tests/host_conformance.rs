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
fn signal_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "signal")
        .wasm("signal_notification.wasm")
        // `server_url` rather than the legacy `host`/`port` pair, because that
        // is the only shape Scryer's loader can turn into an HTTP allowlist
        // entry: it unions the descriptor's static hosts with the hostname of
        // every configuration value that parses as a URL
        // (`crates/scryer-plugins/src/loader.rs`,
        // `allowed_hosts_for_descriptor`/`host_from_url`), and a bare host
        // never does.
        .config("server_url", "http://signal.test.invalid:8080")
        .config("sender_number", "+15550001111")
        .config("receiver_id", "+15550002222")
        .expects_url_prefix("http://signal.test.invalid:8080/v2/send")
        .required_setting("server_url")
        // The operator has to be told about the legacy fallback too.
        .required_setting_mentions("host")
        .run();
}
