use extism_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, NotificationCapabilities,
    NotificationDescriptor, PluginDescriptor, PluginNotificationRequest,
    PluginNotificationResponse, PluginResult, ProviderDescriptor, SDK_VERSION,
};

// ---------------------------------------------------------------------------
// Plugin exports
// ---------------------------------------------------------------------------

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "webhook".to_string(),
        name: "Webhook".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        provider: ProviderDescriptor::Notification(NotificationDescriptor {
            provider_type: "webhook".to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text: false,
                supports_images: false,
                supported_events: vec![],
            },
            config_fields: vec![
            ConfigFieldDef {
                key: "webhook_url".to_string(),
                label: "Webhook URL".to_string(),
                field_type: ConfigFieldType::String,
                required: true,
                default_value: None,
                value_source: Default::default(),
                host_binding: None,
                options: vec![],
                help_text: Some("The URL to POST notification payloads to.".to_string()),
            },
            ConfigFieldDef {
                key: "method".to_string(),
                label: "HTTP Method".to_string(),
                field_type: ConfigFieldType::Select,
                required: false,
                default_value: Some("POST".to_string()),
                value_source: Default::default(),
                host_binding: None,
                options: vec![
                    ConfigFieldOption {
                        value: "POST".to_string(),
                        label: "POST".to_string(),
                    },
                    ConfigFieldOption {
                        value: "PUT".to_string(),
                        label: "PUT".to_string(),
                    },
                ],
                help_text: None,
            },
            ConfigFieldDef {
                key: "content_type".to_string(),
                label: "Content Type".to_string(),
                field_type: ConfigFieldType::Select,
                required: false,
                default_value: Some("application/json".to_string()),
                value_source: Default::default(),
                host_binding: None,
                options: vec![
                    ConfigFieldOption {
                        value: "application/json".to_string(),
                        label: "application/json".to_string(),
                    },
                    ConfigFieldOption {
                        value: "text/plain".to_string(),
                        label: "text/plain".to_string(),
                    },
                ],
                help_text: None,
            },
        ],
        }),
    }
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;

    // Read config values injected by the host
    let webhook_url = config::get("webhook_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    if webhook_url.is_empty() {
        let resp = PluginNotificationResponse {
            success: false,
            error: Some("webhook_url is not configured".to_string()),
        };
        return Ok(serde_json::to_string(&PluginResult::Ok(resp))?);
    }

    let method = config::get("method")
        .ok()
        .flatten()
        .unwrap_or_else(|| "POST".to_string());
    let content_type = config::get("content_type")
        .ok()
        .flatten()
        .unwrap_or_else(|| "application/json".to_string());

    let body = if content_type == "text/plain" {
        format!(
            "[{}] {}: {}",
            req.event_type.as_str(),
            req.summary_title,
            req.summary_message
        )
    } else {
        serde_json::to_string(&req)?
    };

    // Make HTTP request via Extism host function
    let http_req = HttpRequest::new(&webhook_url)
        .with_method(&method)
        .with_header("Content-Type", &content_type)
        .with_header("User-Agent", "scryer-webhook-plugin/0.1");

    match http::request::<Vec<u8>>(&http_req, Some(body.into())) {
        Ok(res) => {
            let status = res.status_code();
            if (200..300).contains(&status) {
                let resp = PluginNotificationResponse {
                    success: true,
                    error: None,
                };
                Ok(serde_json::to_string(&PluginResult::Ok(resp))?)
            } else {
                let body_text = String::from_utf8_lossy(&res.body()).to_string();
                let resp = PluginNotificationResponse {
                    success: false,
                    error: Some(format!("HTTP {}: {}", status, body_text)),
                };
                Ok(serde_json::to_string(&PluginResult::Ok(resp))?)
            }
        }
        Err(e) => {
            let resp = PluginNotificationResponse {
                success: false,
                error: Some(format!("request failed: {}", e)),
            };
            Ok(serde_json::to_string(&PluginResult::Ok(resp))?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_produces_valid_json() {
        let descriptor = build_descriptor();
        let result = serde_json::to_string(&descriptor).unwrap();
        let desc: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(desc["provider"]["kind"], "notification");
        assert_eq!(desc["provider"]["provider_type"], "webhook");
        assert_eq!(desc["provider"]["config_fields"].as_array().unwrap().len(), 3);
        assert!(desc["provider"]["capabilities"].is_object());
    }

    #[test]
    fn webhook_payload_serialization() {
        let payload = PluginNotificationRequest {
            event_type: scryer_plugin_sdk::NotificationEventType::Test,
            summary_title: "Test Notification".to_string(),
            summary_message: "This is a test.".to_string(),
            app: scryer_plugin_sdk::PluginNotificationApp {
                name: "Scryer".to_string(),
                version: "test".to_string(),
            },
            title: Some(scryer_plugin_sdk::PluginNotificationTitle {
                name: "Breaking Bad".to_string(),
                facet: "tv".to_string(),
                year: Some(2008),
                poster_url: None,
                external_ids: Default::default(),
            }),
            episode: None,
            release: None,
            download: None,
            import: None,
            health: None,
            file: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event_type"], "test");
        assert_eq!(parsed["title"]["name"], "Breaking Bad");
        assert!(parsed.get("provider_extra").is_none());
    }
}
