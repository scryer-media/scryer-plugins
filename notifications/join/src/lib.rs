use extism_pdk::*;
use notify_common::*;

const JOIN_URL: &str = "https://joinjoaomgcd.appspot.com/_ah/api/messaging/v1/sendPush";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
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
    Ok(serde_json::to_string(&descriptor)?)
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
            Some("Deprecated by Sonarr in favor of device names; retained for import parity."),
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let (title, message) = title_and_body(&req);
    let target_key = if config_value("device_names").is_some() {
        "deviceNames"
    } else if config_value("device_ids").is_some() {
        "deviceIds"
    } else {
        "deviceId"
    };
    let target_value = config_value("device_names")
        .or_else(|| config_value("device_ids"))
        .unwrap_or_else(|| "group.all".to_string());
    let url = append_query(
        JOIN_URL,
        &[
            (target_key, target_value),
            ("apikey", required_config("api_key")?),
            ("title", title),
            ("text", message),
            (
                "icon",
                "https://cdn.rawgit.com/Sonarr/Sonarr/main/Logo/256.png".to_string(),
            ),
            (
                "smallicon",
                "https://cdn.rawgit.com/Sonarr/Sonarr/main/Logo/96-Outline-White.png".to_string(),
            ),
            ("priority", config_i64("priority", 0).to_string()),
        ],
    );
    let response = send_bytes(&url, "GET", &[], Vec::new());
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
