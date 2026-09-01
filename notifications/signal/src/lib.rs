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

const PROVIDER_TYPE: &str = "signal";

fn build_descriptor() -> PluginDescriptor {
    build_notification_descriptor(
        "signal",
        "Signal",
        env!("CARGO_PKG_VERSION"),
        "signal",
        vec![
            NotificationDeliveryMode::Chat,
            NotificationDeliveryMode::Push,
        ],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    )
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "host",
            "Host",
            ConfigFieldType::String,
            true,
            Some("localhost"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("8080"),
            None,
        ),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "sender_number",
            "Sender Number",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "receiver_id",
            "Receiver ID",
            ConfigFieldType::String,
            true,
            None,
            Some("Signal group ID or phone number."),
        ),
        field(
            "auth_username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "auth_password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let scheme = if config_bool("use_ssl") {
        "https"
    } else {
        "http"
    };
    let url = format!(
        "{scheme}://{}:{}/v2/send",
        required_config("host")?,
        config_i64("port", 8080)
    );
    let payload = serde_json::json!({
        "message": format!("{}\n{}\n", req.summary_title, req.summary_message),
        "number": required_config("sender_number")?,
        "recipients": [required_config("receiver_id")?],
    });
    let mut headers = Vec::new();
    if let (Some(username), Some(password)) =
        (config_value("auth_username"), config_value("auth_password"))
    {
        headers.push(("Authorization", basic_auth_header(&username, &password)));
    }
    let response = send_json(&url, "POST", &headers, payload);
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
