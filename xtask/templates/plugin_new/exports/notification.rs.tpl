#[plugin_fn]
pub fn scryer_notification_send(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(PluginNotificationResponse {
        success: true,
        error: None,
    }))?)
}
