use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let (title, message) = title_and_body(&req);
    let mut payload = serde_json::json!({
        "text": slack_text(&req, &message),
        "username": config_value("username").unwrap_or_else(|| "Scryer".to_string()),
        "attachments": [{
            "fallback": message,
            "title": attachment_title(&req, &title),
            "text": message,
            "color": slack_color(&req),
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
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
