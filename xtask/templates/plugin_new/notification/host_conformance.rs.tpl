//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The contract is not "these functions behave" but "this exact `.wasm` runs
//! under Scryer's notification host", so the suite builds the shipping
//! `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/notification_component_host.rs`
//! does. The suite itself lives in `scryer-plugin-conformance`; what belongs
//! here is only what is genuinely this channel's.
//!
//! CI runs this on every pull request touching this directory
//! (`.github/workflows/notification-component-conformance.yml`), so add the
//! plugin to that workflow's `paths` and `include` matrix when it is ready.
//!
//! # Growing this file
//!
//! The scaffold starts with the checks a stub can pass: the artifact is a
//! component, its world matches, and `describe` is pure. Three are switched
//! off because they need a channel that actually delivers. Turn each back on —
//! delete the `.without(...)` line — as `handle_notification_command` grows:
//!
//! * `SendReachesTheConfiguredEndpoint` once `send` reads its settings through
//!   `scryer_plugin_pdk::config` and posts over `scryer_plugin_pdk::http`. Add
//!   `.config("base_url", "https://{{plugin_id}}.invalid")` for whatever
//!   settings Scryer would have resolved, and `.expects_url_prefix(...)` to pin
//!   the endpoint it builds from them.
//! * `UpstreamFailureIsReported` once a non-2xx answer becomes an unsuccessful
//!   delivery rather than a plugin error.
//! * `RefusedHttpStaysInBand` once a host that refuses egress does too.
//!
//! Add `.required_setting("api_key")` when a setting is mandatory: a missing
//! one must be a typed `InvalidConfig` error naming the field, never a trap.

use scryer_plugin_conformance::notification::{Check, NotificationConformance};

#[test]
fn {{plugin_fn}}_release_wasm_conforms_to_the_notification_host_contract() {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "{{plugin_id}}")
        // TODO: delete these three as the channel learns to deliver.
        .without(Check::SendReachesTheConfiguredEndpoint)
        .without(Check::UpstreamFailureIsReported)
        .without(Check::RefusedHttpStaysInBand)
        .run();
}
