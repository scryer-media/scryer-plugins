use extism_pdk::*;
use notify_common::*;

const PROWL_URL: &str = "https://api.prowlapp.com/publicapi/add";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "prowl",
        "Prowl",
        env!("CARGO_PKG_VERSION"),
        "prowl",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.prowlapp.com"]);
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
    let params = vec![
        ("apikey".to_string(), required_config("api_key")?),
        ("application".to_string(), req.app.name),
        ("event".to_string(), title),
        ("description".to_string(), message),
        (
            "priority".to_string(),
            config_i64("priority", 0).to_string(),
        ),
    ];
    let response = send_form(PROWL_URL, "POST", &[], &params);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
