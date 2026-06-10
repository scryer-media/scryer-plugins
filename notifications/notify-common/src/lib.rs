use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
pub use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType, NotificationCapabilities,
    NotificationDeliveryMode, NotificationEventType, NotificationPayloadFormat, PluginDescriptor,
    PluginNotificationRequest, PluginNotificationResponse, PluginResult, ProviderDescriptor,
    SDK_VERSION,
};

pub fn build_notification_descriptor(
    id: &str,
    name: &str,
    version: &str,
    provider_type: &str,
    delivery_modes: Vec<NotificationDeliveryMode>,
    payload_formats: Vec<NotificationPayloadFormat>,
    config_fields: Vec<ConfigFieldDef>,
    supports_rich_text: bool,
    supports_images: bool,
) -> PluginDescriptor {
    PluginDescriptor {
        id: id.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Notification(scryer_plugin_sdk::NotificationDescriptor {
            provider_type: provider_type.to_string(),
            provider_aliases: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: NotificationCapabilities {
                supports_rich_text,
                supports_images,
                supports_test: true,
                supports_batch: false,
                supports_coalescing: false,
                requires_host_filesystem: false,
                requires_host_process: false,
                delivery_modes,
                payload_formats,
                supported_events: general_notification_events(),
                event_options: Default::default(),
            },
            config_fields,
        }),
    }
}

pub fn add_notification_allowed_hosts(descriptor: &mut PluginDescriptor, hosts: &[&str]) {
    if let ProviderDescriptor::Notification(notification) = &mut descriptor.provider {
        notification
            .allowed_hosts
            .extend(hosts.iter().map(|host| (*host).to_string()));
        notification.allowed_hosts.sort();
        notification.allowed_hosts.dedup();
    }
}

pub fn general_notification_events() -> Vec<NotificationEventType> {
    vec![
        NotificationEventType::Grab,
        NotificationEventType::Download,
        NotificationEventType::Upgrade,
        NotificationEventType::ImportComplete,
        NotificationEventType::ImportRejected,
        NotificationEventType::Rename,
        NotificationEventType::TitleAdded,
        NotificationEventType::TitleDeleted,
        NotificationEventType::FileDeleted,
        NotificationEventType::FileDeletedForUpgrade,
        NotificationEventType::PostProcessingCompleted,
        NotificationEventType::SubtitleDownloaded,
        NotificationEventType::SubtitleSearchFailed,
        NotificationEventType::MediaRequestSubmitted,
        NotificationEventType::MediaRequestApproved,
        NotificationEventType::MediaRequestRejected,
        NotificationEventType::MediaRequestCanceled,
        NotificationEventType::HealthIssue,
        NotificationEventType::HealthRestored,
        NotificationEventType::ApplicationUpdate,
        NotificationEventType::ManualInteractionRequired,
        NotificationEventType::Test,
    ]
}

pub fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}

pub fn connection_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        role: Some(ConfigFieldRole::ConnectionUrl),
        ..field(
            key,
            label,
            ConfigFieldType::String,
            required,
            default_value,
            help_text,
        )
    }
}

pub fn select_field(
    key: &str,
    label: &str,
    default_value: Option<&str>,
    options: &[(&str, &str)],
) -> ConfigFieldDef {
    ConfigFieldDef {
        options: options
            .iter()
            .map(|(value, label)| ConfigFieldOption {
                value: (*value).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
        ..field(
            key,
            label,
            ConfigFieldType::Select,
            false,
            default_value,
            None,
        )
    }
}

pub fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn required_config(key: &str) -> Result<String, Error> {
    config_value(key).ok_or_else(|| Error::msg(format!("{key} is not configured")))
}

pub fn config_bool(key: &str) -> bool {
    config_value(key)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub fn config_i64(key: &str, default_value: i64) -> i64 {
    config_value(key)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default_value)
}

pub fn config_csv(key: &str) -> Vec<String> {
    config_value(key)
        .map(|value| {
            value
                .split([',', '\n', ';'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn notification_title(req: &PluginNotificationRequest) -> String {
    req.summary_title.trim().to_string()
}

pub fn script_environment(
    req: &PluginNotificationRequest,
) -> std::collections::BTreeMap<String, String> {
    scryer_plugin_sdk::notification::to_script_environment(req)
}

pub fn notification_body(req: &PluginNotificationRequest) -> String {
    req.summary_message.trim().to_string()
}

pub fn title_and_body(req: &PluginNotificationRequest) -> (String, String) {
    (notification_title(req), notification_body(req))
}

pub fn poster_url(req: &PluginNotificationRequest) -> Option<String> {
    req.title
        .as_ref()
        .and_then(|title| {
            title
                .poster_url
                .clone()
                .or_else(|| title.background_url.clone())
        })
        .filter(|url| !url.trim().is_empty())
}

pub fn ok_response() -> PluginNotificationResponse {
    PluginNotificationResponse {
        success: true,
        error: None,
        delivery_id: None,
        provider_status: None,
        retry_after_seconds: None,
        warnings: Vec::new(),
        target_results: Vec::new(),
    }
}

pub fn error_response(
    error: impl Into<String>,
    provider_status: Option<String>,
) -> PluginNotificationResponse {
    PluginNotificationResponse {
        success: false,
        error: Some(error.into()),
        delivery_id: None,
        provider_status,
        retry_after_seconds: None,
        warnings: Vec::new(),
        target_results: Vec::new(),
    }
}

pub fn merge_responses(responses: Vec<PluginNotificationResponse>) -> PluginNotificationResponse {
    let mut merged = ok_response();
    for response in responses {
        if !response.success {
            merged.success = false;
        }
        if let Some(error) = response.error {
            merged.warnings.push(error);
        }
        if let Some(status) = response.provider_status {
            merged.warnings.push(status);
        }
    }
    if !merged.success && merged.error.is_none() {
        merged.error = Some("one or more notification targets failed".to_string());
    }
    merged
}

pub fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}").as_bytes())
    )
}

pub fn append_query(url: &str, params: &[(&str, String)]) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let query = params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        url.to_string()
    } else {
        format!("{url}{separator}{query}")
    }
}

pub fn form_body(params: &[(String, String)]) -> Vec<u8> {
    params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

pub fn send_json(
    url: &str,
    method: &str,
    headers: &[(&str, String)],
    body: serde_json::Value,
) -> PluginNotificationResponse {
    send_bytes(
        url,
        method,
        &[headers, &[("Content-Type", "application/json".to_string())]].concat(),
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

pub fn send_form(
    url: &str,
    method: &str,
    headers: &[(&str, String)],
    params: &[(String, String)],
) -> PluginNotificationResponse {
    send_bytes(
        url,
        method,
        &[
            headers,
            &[(
                "Content-Type",
                "application/x-www-form-urlencoded".to_string(),
            )],
        ]
        .concat(),
        form_body(params),
    )
}

pub fn send_bytes(
    url: &str,
    method: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> PluginNotificationResponse {
    let mut request = HttpRequest::new(url)
        .with_method(method)
        .with_header("User-Agent", "scryer-notification-plugin/0.1");
    for (key, value) in headers {
        request = request.with_header(*key, value);
    }

    match http::request::<Vec<u8>>(&request, Some(body)) {
        Ok(response) => {
            let status = response.status_code();
            if (200..300).contains(&status) {
                ok_response()
            } else {
                let body_text = String::from_utf8_lossy(&response.body()).to_string();
                error_response(
                    format!("HTTP {}: {}", status, body_text),
                    Some(format!("http_{status}")),
                )
            }
        }
        Err(error) => error_response(format!("request failed: {error}"), None),
    }
}

pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessExecRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessExecResponse {
    pub status_code: Option<i32>,
    pub stdout_base64: String,
    pub stderr_base64: String,
    pub timed_out: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
pub struct ProcessResponse<T> {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProcessError>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessError {
    pub code: String,
    pub message: String,
}

pub fn process_exec(request: ProcessExecRequest) -> Result<ProcessExecResponse, ProcessError> {
    process_guest::process_exec(request)
}

#[cfg(target_arch = "wasm32")]
mod process_guest {
    use super::*;
    use extism_pdk::host_fn;

    #[host_fn]
    extern "ExtismHost" {
        fn scryer_process_exec(input: String) -> String;
    }

    pub fn process_exec(request: ProcessExecRequest) -> Result<ProcessExecResponse, ProcessError> {
        let input = serde_json::to_string(&request).map_err(|error| ProcessError {
            code: "protocol_error".to_string(),
            message: format!("failed to encode process request: {error}"),
        })?;
        let raw = unsafe { scryer_process_exec(input) }.map_err(|error| ProcessError {
            code: "protocol_error".to_string(),
            message: format!("process host function failed: {error}"),
        })?;
        decode_process_response(&raw)
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod process_guest {
    use super::*;

    pub fn process_exec(_request: ProcessExecRequest) -> Result<ProcessExecResponse, ProcessError> {
        Err(ProcessError {
            code: "unsupported".to_string(),
            message: "process host functions are only available to wasm plugins".to_string(),
        })
    }
}

fn decode_process_response(raw: &str) -> Result<ProcessExecResponse, ProcessError> {
    let response: ProcessResponse<ProcessExecResponse> =
        serde_json::from_str(raw).map_err(|error| ProcessError {
            code: "protocol_error".to_string(),
            message: format!("failed to decode process response: {error}"),
        })?;
    if response.ok {
        response.value.ok_or_else(|| ProcessError {
            code: "protocol_error".to_string(),
            message: "process response was successful but missing a value".to_string(),
        })
    } else {
        Err(response.error.unwrap_or(ProcessError {
            code: "protocol_error".to_string(),
            message: "process response failed without an error".to_string(),
        }))
    }
}
