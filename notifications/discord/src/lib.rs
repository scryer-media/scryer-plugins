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

const PROVIDER_TYPE: &str = "discord";

fn build_descriptor() -> PluginDescriptor {
    build_notification_descriptor(
        "discord",
        "Discord",
        env!("CARGO_PKG_VERSION"),
        "discord",
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
        true,
    )
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field("webhook_url", "Webhook URL", true, None, None),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        connection_field("avatar", "Avatar URL", false, None, None),
        field(
            "author",
            "Author",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let mut embed = serde_json::json!({
        "description": discord_description(req),
        "title": discord_title(req),
        "color": discord_color(req),
        "fields": discord_fields(req),
    });
    embed["author"] = serde_json::json!({
        "name": config_value("author").unwrap_or_else(|| req.app.name.clone()),
        "icon_url": "https://raw.githubusercontent.com/scryer-media/scryer/main/apps/scryer-web/public/icons/icon-512.png",
    });
    if let Some(poster_url) = poster_url(req) {
        embed["thumbnail"] = serde_json::json!({ "url": poster_url });
    }
    if let Some(occurred_at) = req.occurred_at.clone() {
        embed["timestamp"] = serde_json::Value::String(occurred_at);
    }

    let mut payload = serde_json::json!({
        "content": serde_json::Value::Null,
        "embeds": [embed],
    });
    if let Some(username) = config_value("username") {
        payload["username"] = serde_json::Value::String(username);
    }
    if let Some(avatar) = config_value("avatar") {
        payload["avatar_url"] = serde_json::Value::String(avatar);
    }

    let response = send_json(&required_config("webhook_url")?, "POST", &[], payload);
    Ok(response)
}

fn discord_description(req: &PluginNotificationRequest) -> &'static str {
    match req.event_type {
        NotificationEventType::Grab => "Episode Grabbed",
        NotificationEventType::Download => "Episode Imported",
        NotificationEventType::Upgrade => "Episode Upgraded",
        NotificationEventType::ImportComplete => "Import Complete",
        NotificationEventType::Rename => "Renamed",
        NotificationEventType::FileDeleted | NotificationEventType::FileDeletedForUpgrade => {
            "Episode Deleted"
        }
        NotificationEventType::TitleAdded => "Series Added",
        NotificationEventType::TitleDeleted => "Series Deleted",
        NotificationEventType::HealthIssue => "Health Issue",
        NotificationEventType::HealthRestored => "Health Issue Resolved",
        NotificationEventType::ApplicationUpdate => "Application Updated",
        NotificationEventType::ManualInteractionRequired => "Manual Interaction Required",
        _ => "Notification",
    }
}

fn discord_title(req: &PluginNotificationRequest) -> String {
    if let Some(title) = req.title.as_ref() {
        return title.name.clone();
    }
    req.summary_title.clone()
}

fn discord_fields(req: &PluginNotificationRequest) -> Vec<serde_json::Value> {
    let mut fields = Vec::new();
    if !req.summary_message.trim().is_empty() {
        fields.push(serde_json::json!({
            "name": "Message",
            "value": req.summary_message,
            "inline": false,
        }));
    }
    if let Some(release) = req.release.as_ref() {
        if let Some(quality) = release.quality.clone() {
            fields.push(serde_json::json!({
                "name": "Quality",
                "value": quality,
                "inline": true,
            }));
        }
        if let Some(indexer) = release.indexer.clone() {
            fields.push(serde_json::json!({
                "name": "Indexer",
                "value": indexer,
                "inline": true,
            }));
        }
    }
    if let Some(download) = req.download.as_ref()
        && let Some(client_name) = download.client_name.clone()
    {
        fields.push(serde_json::json!({
            "name": "Download Client",
            "value": client_name,
            "inline": true,
        }));
    }
    fields
}

fn discord_color(req: &PluginNotificationRequest) -> i64 {
    match req.event_type {
        NotificationEventType::FileDeleted | NotificationEventType::TitleDeleted => 15_749_200,
        NotificationEventType::HealthIssue
        | NotificationEventType::Grab
        | NotificationEventType::ManualInteractionRequired => 16_753_920,
        NotificationEventType::Upgrade => 4_089_856,
        NotificationEventType::Download
        | NotificationEventType::ImportComplete
        | NotificationEventType::TitleAdded
        | NotificationEventType::HealthRestored => 2_605_644,
        _ => 16_761_392,
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
