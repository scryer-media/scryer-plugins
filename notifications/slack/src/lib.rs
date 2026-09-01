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

const PROVIDER_TYPE: &str = "slack";

fn build_descriptor() -> PluginDescriptor {
    build_notification_descriptor(
        "slack",
        "Slack",
        env!("CARGO_PKG_VERSION"),
        "slack",
        vec![
            NotificationDeliveryMode::Chat,
            NotificationDeliveryMode::Webhook,
        ],
        vec![
            NotificationPayloadFormat::PlainText,
            NotificationPayloadFormat::RichEmbed,
        ],
        config_fields(),
        true,
        false,
    )
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field("webhook_url", "Webhook URL", true, None, None),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            Some("Scryer"),
            None,
        ),
        field(
            "icon",
            "Icon",
            ConfigFieldType::String,
            false,
            None,
            Some("Emoji name wrapped in colons or an icon URL."),
        ),
        field(
            "channel",
            "Channel",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let (title, message) = title_and_body(req);
    let mut payload = serde_json::json!({
        "text": slack_text(req, &message),
        "username": config_value("username").unwrap_or_else(|| "Scryer".to_string()),
        "attachments": [{
            "fallback": message,
            "title": attachment_title(req, &title),
            "text": message,
            "color": slack_color(req),
        }],
    });

    if let Some(icon) = config_value("icon") {
        if icon.starts_with(':') && icon.ends_with(':') {
            payload["icon_emoji"] = serde_json::Value::String(icon);
        } else {
            payload["icon_url"] = serde_json::Value::String(icon);
        }
    }
    if let Some(channel) = config_value("channel") {
        payload["channel"] = serde_json::Value::String(channel);
    }

    let response = send_json(&required_config("webhook_url")?, "POST", &[], payload);
    Ok(response)
}

fn slack_text(req: &PluginNotificationRequest, message: &str) -> String {
    match req.event_type {
        NotificationEventType::Grab => format!("Grabbed: {message}"),
        NotificationEventType::Download | NotificationEventType::Upgrade => {
            format!("Imported: {message}")
        }
        NotificationEventType::ImportComplete => {
            format!("Imported all expected episodes: {message}")
        }
        NotificationEventType::Rename => "Renamed".to_string(),
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            "Episode Deleted".to_string()
        }
        NotificationEventType::TitleAdded => "Series Added".to_string(),
        NotificationEventType::TitleDeleted => "Series Deleted".to_string(),
        NotificationEventType::HealthIssue => "Health Issue".to_string(),
        NotificationEventType::HealthRestored => "Health Issue Resolved".to_string(),
        NotificationEventType::ApplicationUpdate => "Application Updated".to_string(),
        NotificationEventType::ManualInteractionRequired => {
            "Manual Interaction Required".to_string()
        }
        _ => req.summary_title.clone(),
    }
}

fn attachment_title(req: &PluginNotificationRequest, fallback: &str) -> String {
    req.title
        .as_ref()
        .map(|title| title.name.clone())
        .unwrap_or_else(|| fallback.to_string())
}

fn slack_color(req: &PluginNotificationRequest) -> &'static str {
    match req.event_type {
        NotificationEventType::Grab
        | NotificationEventType::ManualInteractionRequired
        | NotificationEventType::HealthIssue => "warning",
        NotificationEventType::FileDeleted | NotificationEventType::TitleDeleted => "danger",
        _ => "good",
    }
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
