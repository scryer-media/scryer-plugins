use extism_pdk::*;
use notify_common::*;

const SIMPLEPUSH_URL: &str = "https://api.simplepush.io/send";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "simplepush",
        "Simplepush",
        env!("CARGO_PKG_VERSION"),
        "simplepush",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.simplepush.io"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field("key", "Key", ConfigFieldType::Password, true, None, None),
        field(
            "event",
            "Event",
            ConfigFieldType::String,
            false,
            None,
            Some("Optional Simplepush event key."),
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let (title, message) = title_and_body(&req);
    let mut params = vec![
        ("key".to_string(), required_config("key")?),
        ("title".to_string(), title),
        ("msg".to_string(), message),
    ];
    if let Some(event) = config_value("event") {
        params.push(("event".to_string(), event));
    }
    let response = send_form(SIMPLEPUSH_URL, "POST", &[], &params);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
