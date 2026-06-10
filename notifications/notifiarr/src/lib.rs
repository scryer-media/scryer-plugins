use extism_pdk::*;
use notify_common::*;
use scryer_plugin_sdk::to_webhook_json;

const NOTIFIARR_URL: &str = "https://notifiarr.com/api/v1/notification/sonarr";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "notifiarr",
        "Notifiarr",
        env!("CARGO_PKG_VERSION"),
        "notifiarr",
        vec![
            NotificationDeliveryMode::Webhook,
            NotificationDeliveryMode::Aggregator,
        ],
        vec![NotificationPayloadFormat::StructuredJson],
        config_fields(),
        true,
        true,
    );
    add_notification_allowed_hosts(&mut descriptor, &["notifiarr.com"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![field(
        "api_key",
        "API Key",
        ConfigFieldType::Password,
        true,
        None,
        Some("Notifiarr API key."),
    )]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let headers = [("X-API-Key", required_config("api_key")?)];
    let response = send_json(NOTIFIARR_URL, "POST", &headers, to_webhook_json(&req));
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
