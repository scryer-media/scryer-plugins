//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`. What stays here is
//! what is genuinely this channel's: a library refresh rather than a message,
//! and the only channel in the family that *implements* actions.

// conformance: bespoke

use scryer_plugin_conformance::notification::{
    Check, NotificationConformance, call_notification, instantiate,
};
use scryer_plugin_sdk::NotificationEventType;
use scryer_plugin_sdk::PluginResult;
use scryer_plugin_sdk::command::{
    PluginActionRequest, PluginNotificationCommand, PluginNotificationCommandResult,
};

#[test]
fn plex_release_wasm_conforms_to_the_notification_host_contract() {
    // The delivery is a library refresh, so the notification carries a
    // `Download` rather than the family's `Test`. The channel tolerates an
    // unset setting rather than reporting a typed configuration error, so the
    // family's missing-required-setting check does not apply; and its action
    // arm is wired rather than unsupported, so that check is replaced by the
    // stronger one below rather than dropped.
    let conformance = NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "plex")
        .wasm("plex_notification.wasm")
        .config("base_url", "http://plex.test.invalid:32400")
        .config("update_library", "true")
        .config("section_ids", "3")
        .config("auth_token", "plextoken")
        .expects_url_prefix(
            "http://plex.test.invalid:32400/library/sections/3/refresh?X-Plex-Client-Identifier=scryer",
        )
        .event_type(NotificationEventType::Download)
        .without(Check::MissingRequiredSetting)
        .without(Check::ActionIsUnsupported);
    conformance.run();

    assert_an_unknown_action_answers_empty_rather_than_trapping(&conformance);
}

/// This channel *implements* actions, so the assertion is the opposite of the
/// family's usual one: the operation is wired, it reaches the handler with the
/// action name intact, and an action it does not recognise answers with an
/// empty payload rather than trapping.
///
/// The host splits what used to arrive as one JSON blob into
/// `PluginActionRequest::action` and `::payload`; `action_request_value`
/// re-joins them, and this is what proves that join is right — an unrecognised
/// name has to arrive as an unrecognised name, not as a missing one.
fn assert_an_unknown_action_answers_empty_rather_than_trapping(
    conformance: &NotificationConformance,
) {
    let (mut store, plugin) = instantiate(&conformance.wasm_path(), conformance.script());
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Action(PluginActionRequest {
            action: "notAnAction".to_string(),
            payload: serde_json::json!({ "query": {} }),
        }),
    );
    let PluginNotificationCommandResult::Action(PluginResult::Ok(response)) = result else {
        panic!("an unknown action must answer in-band: {result:?}");
    };
    assert_eq!(response.payload, serde_json::json!({}));
}
