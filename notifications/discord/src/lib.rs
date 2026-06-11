use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
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
    );
    Ok(serde_json::to_string(&descriptor)?)
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut embed = serde_json::json!({
        "description": discord_description(&req),
        "title": discord_title(&req),
        "color": discord_color(&req),
        "fields": discord_fields(&req),
    });
    embed["author"] = serde_json::json!({
        "name": config_value("author").unwrap_or_else(|| req.app.name.clone()),
        "icon_url": "https://raw.githubusercontent.com/Sonarr/Sonarr/develop/Logo/256.png",
    });
    if let Some(poster_url) = poster_url(&req) {
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
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
