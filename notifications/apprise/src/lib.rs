use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
        "apprise",
        "Apprise",
        env!("CARGO_PKG_VERSION"),
        "apprise",
        vec![
            NotificationDeliveryMode::Push,
            NotificationDeliveryMode::Aggregator,
        ],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        true,
    );
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field("server_url", "Server URL", true, None, None),
        field(
            "configuration_key",
            "Configuration Key",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "stateless_urls",
            "Stateless URLs",
            ConfigFieldType::Multiline,
            false,
            None,
            None,
        ),
        select_field(
            "notification_type",
            "Notification Type",
            Some("info"),
            &[
                ("info", "Info"),
                ("success", "Success"),
                ("warning", "Warning"),
                ("failure", "Failure"),
            ],
        ),
        field("tags", "Tags", ConfigFieldType::String, false, None, None),
        field(
            "include_poster",
            "Include Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let server = required_config("server_url")?;
    let url = if let Some(key) = config_value("configuration_key") {
        format!("{}/notify/{key}", server.trim_end_matches('/'))
    } else {
        format!("{}/notify", server.trim_end_matches('/'))
    };
    let mut payload = serde_json::json!({
        "title": req.summary_title,
        "body": req.summary_message,
        "type": config_value("notification_type").unwrap_or_else(|| "info".to_string()),
    });
    if config_value("configuration_key").is_none() {
        if let Some(urls) = config_value("stateless_urls") {
            payload["urls"] = serde_json::Value::String(urls);
        }
    }
    let tags = config_csv("tags").join(",");
    if !tags.is_empty() {
        payload["tag"] = serde_json::Value::String(tags);
    }
    if config_bool("include_poster") {
        if let Some(poster) = poster_url(&req) {
            payload["attachment"] = serde_json::Value::String(poster);
        }
    }
    let mut headers = Vec::new();
    if let (Some(username), Some(password)) =
        (config_value("auth_username"), config_value("auth_password"))
    {
        headers.push(("Authorization", basic_auth_header(&username, &password)));
    }
    let response = send_json(&url, "POST", &headers, payload);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
