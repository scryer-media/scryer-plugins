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
    let configuration_key = config_value("configuration_key");
    let stateless_urls = config_value("stateless_urls");
    let tags = config_csv("tags").join(",");

    if configuration_key.is_none() && stateless_urls.is_none() {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "Use either Configuration Key or Stateless URLs",
            None,
        )))?);
    }
    if configuration_key.is_some() && stateless_urls.is_some() {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "Use either Configuration Key or Stateless URLs",
            None,
        )))?);
    }
    if let Some(key) = configuration_key.as_deref() {
        let valid_key = key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        if !valid_key {
            return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                "Configuration Key may only contain a-z, 0-9, and -",
                None,
            )))?);
        }
    }
    if stateless_urls.is_some() && !tags.is_empty() {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "Stateless URLs do not support tags",
            None,
        )))?);
    }

    let url = if let Some(key) = configuration_key.as_deref() {
        format!("{}/notify/{key}", server.trim_end_matches('/'))
    } else {
        format!("{}/notify", server.trim_end_matches('/'))
    };
    let mut payload = serde_json::json!({
        "title": req.summary_title,
        "body": req.summary_message,
        "type": config_value("notification_type").unwrap_or_else(|| "info".to_string()),
    });
    if configuration_key.is_none()
        && let Some(urls) = stateless_urls
    {
        payload["urls"] = serde_json::Value::String(urls);
    }
    if !tags.is_empty() {
        payload["tag"] = serde_json::Value::String(tags);
    }
    if config_bool("include_poster")
        && let Some(poster) = poster_url(&req)
    {
        payload["attachment"] = serde_json::Value::String(poster);
    }
    let mut headers = Vec::new();
    let username = config_value("auth_username");
    let password = config_value("auth_password");
    if username.is_some() || password.is_some() {
        let username = username.unwrap_or_default();
        let password = password.unwrap_or_default();
        headers.push(("Authorization", basic_auth_header(&username, &password)));
    }
    let response = send_json(&url, "POST", &headers, payload);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
