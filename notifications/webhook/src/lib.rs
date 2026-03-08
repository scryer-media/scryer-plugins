use std::collections::HashMap;

use extism_pdk::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types (must match the host-side JSON schema in scryer-plugins/src/types.rs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PluginDescriptor {
    name: String,
    version: String,
    sdk_version: String,
    plugin_type: String,
    provider_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_aliases: Vec<String>,
    capabilities: IndexerCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scoring_policies: Vec<()>,
    config_fields: Vec<ConfigFieldDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification_capabilities: Option<NotificationCapabilities>,
}

#[derive(Serialize)]
struct IndexerCapabilities {
    search: bool,
    imdb_search: bool,
    tvdb_search: bool,
}

#[derive(Serialize)]
struct NotificationCapabilities {
    supports_rich_text: bool,
    supports_images: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_events: Vec<String>,
}

#[derive(Serialize)]
struct ConfigFieldDef {
    key: String,
    label: String,
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    options: Vec<ConfigFieldOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    help_text: Option<String>,
}

#[derive(Serialize)]
struct ConfigFieldOption {
    value: String,
    label: String,
}

#[derive(Deserialize)]
struct PluginNotificationRequest {
    event_type: String,
    title: String,
    message: String,
    #[serde(default)]
    title_name: Option<String>,
    #[serde(default)]
    title_year: Option<i32>,
    #[serde(default)]
    title_facet: Option<String>,
    #[serde(default)]
    poster_url: Option<String>,
    #[serde(default)]
    episode_info: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    release_title: Option<String>,
    #[serde(default)]
    download_client: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    health_message: Option<String>,
    #[serde(default)]
    application_version: Option<String>,
    #[serde(default)]
    metadata: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct PluginNotificationResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Webhook payload — the JSON body sent to the configured URL
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WebhookPayload {
    event_type: String,
    title: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_facet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poster_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    episode_info: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    health_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    application_version: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    metadata: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Plugin exports
// ---------------------------------------------------------------------------

#[plugin_fn]
pub fn describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        name: "Webhook".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: "0.1".to_string(),
        plugin_type: "notification".to_string(),
        provider_type: "webhook".to_string(),
        provider_aliases: vec![],
        capabilities: IndexerCapabilities {
            search: false,
            imdb_search: false,
            tvdb_search: false,
        },
        scoring_policies: vec![],
        config_fields: vec![
            ConfigFieldDef {
                key: "webhook_url".to_string(),
                label: "Webhook URL".to_string(),
                field_type: "string".to_string(),
                required: true,
                default_value: None,
                options: vec![],
                help_text: Some("The URL to POST notification payloads to.".to_string()),
            },
            ConfigFieldDef {
                key: "method".to_string(),
                label: "HTTP Method".to_string(),
                field_type: "select".to_string(),
                required: false,
                default_value: Some("POST".to_string()),
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
                field_type: "select".to_string(),
                required: false,
                default_value: Some("application/json".to_string()),
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
        allowed_hosts: vec!["*".to_string()],
        notification_capabilities: Some(NotificationCapabilities {
            supports_rich_text: false,
            supports_images: false,
            supported_events: vec![],
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn send_notification(input: String) -> FnResult<String> {
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
        return Ok(serde_json::to_string(&resp)?);
    }

    let method = config::get("method")
        .ok()
        .flatten()
        .unwrap_or_else(|| "POST".to_string());
    let content_type = config::get("content_type")
        .ok()
        .flatten()
        .unwrap_or_else(|| "application/json".to_string());

    // Build payload
    let payload = WebhookPayload {
        event_type: req.event_type,
        title: req.title,
        message: req.message,
        title_name: req.title_name,
        title_year: req.title_year,
        title_facet: req.title_facet,
        poster_url: req.poster_url,
        episode_info: req.episode_info,
        quality: req.quality,
        release_title: req.release_title,
        download_client: req.download_client,
        file_path: req.file_path,
        health_message: req.health_message,
        application_version: req.application_version,
        metadata: req.metadata,
    };

    let body = if content_type == "text/plain" {
        format!("[{}] {}: {}", payload.event_type, payload.title, payload.message)
    } else {
        serde_json::to_string(&payload)?
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
                Ok(serde_json::to_string(&resp)?)
            } else {
                let body_text = String::from_utf8_lossy(&res.body()).to_string();
                let resp = PluginNotificationResponse {
                    success: false,
                    error: Some(format!("HTTP {}: {}", status, body_text)),
                };
                Ok(serde_json::to_string(&resp)?)
            }
        }
        Err(e) => {
            let resp = PluginNotificationResponse {
                success: false,
                error: Some(format!("request failed: {}", e)),
            };
            Ok(serde_json::to_string(&resp)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_descriptor() -> PluginDescriptor {
        PluginDescriptor {
            name: "Webhook".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            sdk_version: "0.1".to_string(),
            plugin_type: "notification".to_string(),
            provider_type: "webhook".to_string(),
            provider_aliases: vec![],
            capabilities: IndexerCapabilities {
                search: false,
                imdb_search: false,
                tvdb_search: false,
            },
            scoring_policies: vec![],
            config_fields: vec![
                ConfigFieldDef {
                    key: "webhook_url".to_string(),
                    label: "Webhook URL".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: vec![],
                    help_text: Some("The URL to POST notification payloads to.".to_string()),
                },
                ConfigFieldDef {
                    key: "method".to_string(),
                    label: "HTTP Method".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("POST".to_string()),
                    options: vec![
                        ConfigFieldOption { value: "POST".to_string(), label: "POST".to_string() },
                        ConfigFieldOption { value: "PUT".to_string(), label: "PUT".to_string() },
                    ],
                    help_text: None,
                },
                ConfigFieldDef {
                    key: "content_type".to_string(),
                    label: "Content Type".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("application/json".to_string()),
                    options: vec![
                        ConfigFieldOption { value: "application/json".to_string(), label: "application/json".to_string() },
                        ConfigFieldOption { value: "text/plain".to_string(), label: "text/plain".to_string() },
                    ],
                    help_text: None,
                },
            ],
            allowed_hosts: vec!["*".to_string()],
            notification_capabilities: Some(NotificationCapabilities {
                supports_rich_text: false,
                supports_images: false,
                supported_events: vec![],
            }),
        }
    }

    #[test]
    fn describe_produces_valid_json() {
        let descriptor = build_descriptor();
        let result = serde_json::to_string(&descriptor).unwrap();
        let desc: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(desc["plugin_type"], "notification");
        assert_eq!(desc["provider_type"], "webhook");
        assert_eq!(desc["config_fields"].as_array().unwrap().len(), 3);
        assert!(desc["notification_capabilities"].is_object());
    }

    #[test]
    fn webhook_payload_serialization() {
        let payload = WebhookPayload {
            event_type: "test".to_string(),
            title: "Test Notification".to_string(),
            message: "This is a test.".to_string(),
            title_name: Some("Breaking Bad".to_string()),
            title_year: Some(2008),
            title_facet: Some("tv".to_string()),
            poster_url: None,
            episode_info: None,
            quality: None,
            release_title: None,
            download_client: None,
            file_path: None,
            health_message: None,
            application_version: None,
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event_type"], "test");
        assert_eq!(parsed["title_name"], "Breaking Bad");
        assert!(parsed.get("poster_url").is_none());
        assert!(parsed.get("metadata").is_none());
    }
}
