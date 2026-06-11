use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
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
    );
    Ok(serde_json::to_string(&descriptor)?)
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
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
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
