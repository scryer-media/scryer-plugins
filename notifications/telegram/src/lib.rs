use extism_pdk::*;
use notify_common::*;

const TELEGRAM_API_URL: &str = "https://api.telegram.org";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "telegram",
        "Telegram",
        env!("CARGO_PKG_VERSION"),
        "telegram",
        vec![
            NotificationDeliveryMode::Chat,
            NotificationDeliveryMode::Push,
        ],
        vec![
            NotificationPayloadFormat::PlainText,
            NotificationPayloadFormat::Html,
        ],
        config_fields(),
        true,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.telegram.org"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "bot_token",
            "Bot Token",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "chat_id",
            "Chat ID",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "topic_id",
            "Topic ID",
            ConfigFieldType::Number,
            false,
            None,
            None,
        ),
        field(
            "send_silently",
            "Send Silently",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "include_app_name_in_title",
            "Include App Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "include_instance_name_in_title",
            "Include Instance Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut title = req.summary_title.clone();
    if config_bool("include_app_name_in_title") {
        title = format!("{} - {title}", req.app.name);
    }
    if config_bool("include_instance_name_in_title") {
        title = format!("{title} - {}", req.app.name);
    }
    let mut payload = serde_json::json!({
        "chat_id": required_config("chat_id")?,
        "parse_mode": "HTML",
        "text": format!("<b>{}</b>\n{}", html_escape(&title), html_escape(&req.summary_message)),
        "disable_notification": config_bool("send_silently"),
        "link_preview_options": { "is_disabled": true },
    });
    if let Some(raw_topic_id) = config_value("topic_id") {
        match raw_topic_id.parse::<i64>() {
            Ok(topic_id) if topic_id > 1 => {
                payload["message_thread_id"] = serde_json::Value::from(topic_id);
            }
            Ok(_) => {
                return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                    "Topic ID must be greater than 1 or empty",
                    None,
                )))?);
            }
            Err(_) => {
                return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                    "Topic ID must be a number",
                    None,
                )))?);
            }
        }
    }
    let url = format!(
        "{TELEGRAM_API_URL}/bot{}/sendmessage",
        required_config("bot_token")?
    );
    let response = send_json(&url, "POST", &[], payload);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
