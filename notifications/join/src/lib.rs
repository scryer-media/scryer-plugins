use notify_common::*;

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

const PROVIDER_TYPE: &str = "join";

const JOIN_URL: &str = "https://joinjoaomgcd.appspot.com/_ah/api/messaging/v1/sendPush";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "join",
        "Join",
        env!("CARGO_PKG_VERSION"),
        "join",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        true,
    );
    add_notification_allowed_hosts(&mut descriptor, &["joinjoaomgcd.appspot.com"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "device_names",
            "Device Names",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma separated Join device names."),
        ),
        field(
            "device_ids",
            "Device IDs",
            ConfigFieldType::String,
            false,
            None,
            Some("Deprecated in favor of device names; retained for imported configurations."),
        ),
        field(
            "priority",
            "Priority",
            ConfigFieldType::Number,
            false,
            Some("0"),
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let (title, message) = title_and_body(req);
    if config_value("device_ids").is_some() {
        return Ok(error_response(
            "join device_ids is deprecated; use device_names instead",
            Some("deprecated_device_ids".to_string()),
        ));
    }

    let device_names = config_value("device_names");
    let target_key = if device_names.is_some() {
        "deviceNames"
    } else {
        "deviceId"
    };
    let target_value = device_names.unwrap_or_else(|| "group.all".to_string());
    let url = append_query(
        JOIN_URL,
        &[
            (target_key, target_value),
            ("apikey", required_config("api_key")?),
            ("title", title),
            ("text", message),
            (
                "icon",
                "https://raw.githubusercontent.com/scryer-media/scryer/main/apps/scryer-web/public/icons/icon-512.png".to_string(),
            ),
            (
                "smallicon",
                "https://raw.githubusercontent.com/scryer-media/scryer/main/apps/scryer-web/public/icons/icon-512.png".to_string(),
            ),
            ("priority", config_i64("priority", 0).to_string()),
        ],
    );
    let response = send_bytes(&url, "GET", &[], Vec::new());
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
