use notify_common::*;
use scryer_plugin_sdk::to_webhook_json;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

const PROVIDER_TYPE: &str = "notifiarr";

const NOTIFIARR_URL: &str = "https://notifiarr.com/api/v1/notification/sonarr";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "notifiarr",
        "Notifiarr",
        env!("CARGO_PKG_VERSION"),
        "notifiarr",
        vec![
            NotificationDeliveryMode::Webhook,
            NotificationDeliveryMode::Aggregator,
        ],
        vec![NotificationPayloadFormat::StructuredJson],
        config_fields(),
        true,
        true,
    );
    add_notification_allowed_hosts(&mut descriptor, &["notifiarr.com"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![field(
        "api_key",
        "API Key",
        ConfigFieldType::Password,
        true,
        None,
        Some("Notifiarr API key."),
    )]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let headers = [("X-API-Key", required_config("api_key")?)];
    let mut response = send_json(NOTIFIARR_URL, "POST", &headers, to_webhook_json(req));
    if response.provider_status.as_deref() == Some("http_400") {
        if let Some(error) = response.error.take() {
            response.warnings.push(error);
        }
        response.success = true;
    }
    Ok(response)
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per Extism entry point this plugin used to export. `action` is not
/// one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
///
/// A configuration failure was a `FnResult` hard fault under Extism — the host
/// saw a string and a generic ABI error, and could not tell a misconfigured
/// channel from a broken one. It is now a typed `PluginResult::Err`.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(match send_notification(&request) {
                Ok(response) => PluginResult::Ok(response),
                Err(error) => PluginResult::Err(config_error(error)),
            })
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}
