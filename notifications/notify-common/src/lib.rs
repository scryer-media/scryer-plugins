//! Shared building blocks for Scryer's notification channels.
//!
//! # What this crate is
//!
//! Every channel is a `scryer:notification/notification@1.0.0` component, and
//! each one is a thin body: parse config, build a request, render a payload.
//! Everything those bodies have in common lives here — the config accessors,
//! the payload and delivery types, the typed error constructors, the retry and
//! redaction helpers, and [`process_exec`] for the one channel family that
//! needs to run a host executable.
//!
//! # How it reaches Scryer
//!
//! A component reaches the host through exactly one import,
//! `scryer:host/services@1.0.0`, which the family entry macro binds to
//! [`scryer_plugin_pdk::host`]. This crate re-exports the PDK's `config`,
//! `http` and `var` shapes over that transport, and [`process_exec`] rides the
//! same door as `PluginHostRequest::ProcessExec` — the family that needs
//! authority beyond HTTP imports the same one function as the families that do
//! not.
//!
//! # Nothing here names a world
//!
//! `wit_bindgen::generate!` has to live in the plugin crate: bindings are
//! generated per world, and a shared crate that named one would drag that
//! world's import into every component linking it (the failure mode
//! `subtitles/amenzb` hit through `newznab-common`). This crate only ever
//! touches the PDK's injected `fn`-pointer transport, so it is world-agnostic
//! by construction and links into any family world unchanged.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_sdk::current_sdk_constraint;

/// The PDK's host-service shapes, re-exported for channel bodies.
///
/// Re-exported so `use notify_common::*;` keeps supplying `config::get`,
/// `http::request`, `HttpRequest` and friends to every channel body.
pub use scryer_plugin_pdk::{Error, FnResult, HttpRequest, HttpResponse, config, http, var};
pub use scryer_plugin_sdk::command::{
    PluginActionRequest, PluginActionResponse, PluginNotificationCommand,
    PluginNotificationCommandResult,
};
pub use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType, NotificationCapabilities,
    NotificationDeliveryMode, NotificationEventType, NotificationPayloadFormat, PluginDescriptor,
    PluginError, PluginErrorCode, PluginNotificationMediaFile, PluginNotificationRequest,
    PluginNotificationResponse, PluginResult, ProviderDescriptor, SDK_VERSION,
};

// Pre-existing shape, not introduced by the component migration: this is a
// flat descriptor constructor whose arguments are the descriptor's own fields,
// so a parameter struct would only move the same list one level out.
#[allow(clippy::too_many_arguments)]
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
                // Added by SDK 3.10; no notification channel drives dependent
                // fields from a select option, so the default empty map keeps
                // the descriptors byte-identical to what 3.2 produced.
                config_overrides: Default::default(),
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

pub fn path_segment(value: &str) -> String {
    urlencoding::encode(value).into_owned()
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
    /// Always `false` since the move to host services.
    ///
    /// The predecessor host function reported a timeout as a *successful*
    /// response with this flag set. `PluginHostRequest::ProcessExec` has no
    /// such field:
    /// a host-enforced timeout is a typed `PluginError` and therefore arrives
    /// on the `Err` arm of [`process_exec`], carrying the host's message. The
    /// field is kept because it is part of this crate's published shape and
    /// because a future host may populate it again.
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

/// Run one allowlisted host executable.
///
/// This used to be a dedicated host-function extern exchanging JSON strings.
/// It is now `PluginHostRequest::ProcessExec` over the same
/// `scryer:host/services@1.0.0` import every other service uses — which is the
/// whole point of the notification world's design note: the family that needs
/// authority beyond HTTP imports the same one function as the families that do
/// not.
///
/// Authority is unchanged and still entirely host-side. The descriptor's
/// `requires_host_process` capability plus the loader's first-party gate decide
/// whether the allowlist is populated; a community channel that asks for this
/// gets `permission_denied` per call, in-band.
pub fn process_exec(request: ProcessExecRequest) -> Result<ProcessExecResponse, ProcessError> {
    let stdin = match &request.stdin_base64 {
        Some(encoded) => STANDARD.decode(encoded).map_err(|error| ProcessError {
            code: "protocol_error".to_string(),
            message: format!("failed to decode process stdin: {error}"),
        })?,
        None => Vec::new(),
    };

    let response =
        scryer_plugin_pdk::host::process_exec(scryer_plugin_sdk::host::PluginProcessExecRequest {
            command: request.command,
            args: request.args,
            env: request.env,
            cwd: request.working_directory,
            stdin,
            timeout_ms: request.timeout_ms,
        })
        .map_err(process_error)?;

    Ok(ProcessExecResponse {
        status_code: Some(response.exit_code),
        stdout_base64: STANDARD.encode(&response.stdout),
        stderr_base64: STANDARD.encode(&response.stderr),
        timed_out: false,
    })
}

/// Translate a host-services failure into this crate's process error shape.
///
/// A host with no process service configured answers **in-band** with a typed
/// `PluginError`, so that arrives as [`scryer_plugin_pdk::host::HostCallError::Service`]
/// and keeps the host's own diagnosis — `permission_denied`, an allowlist miss,
/// or a timeout — instead of collapsing to a generic ABI fault the way the
/// predecessor host function did.
fn process_error(error: scryer_plugin_pdk::host::HostCallError) -> ProcessError {
    match error {
        scryer_plugin_pdk::host::HostCallError::Service(error) => ProcessError {
            code: process_error_code(error.code).to_string(),
            message: error.public_message,
        },
        scryer_plugin_pdk::host::HostCallError::Unavailable => ProcessError {
            code: "unsupported".to_string(),
            message: "this host provides no process execution service".to_string(),
        },
        error => ProcessError {
            code: "protocol_error".to_string(),
            message: error.to_string(),
        },
    }
}

fn process_error_code(code: PluginErrorCode) -> &'static str {
    match code {
        PluginErrorCode::InvalidConfig => "invalid_config",
        PluginErrorCode::AuthFailed => "permission_denied",
        PluginErrorCode::RateLimited => "rate_limited",
        PluginErrorCode::UpstreamUnavailable => "upstream_unavailable",
        PluginErrorCode::Unsupported => "unsupported",
        PluginErrorCode::Temporary => "temporary",
        PluginErrorCode::Permanent => "permanent",
    }
}

/// The in-band answer a channel with no action operation gives.
///
/// Every notification world carries `send` and `action`; most channels
/// implement only `send`. The host reads that from the descriptor and does not
/// route an action here, so this arm exists to *answer* rather than to trap —
/// a guest trap would cost the host the plugin's own diagnosis and, under a
/// component, the whole instance.
pub fn unsupported_action(provider: &str) -> PluginResult<PluginActionResponse> {
    PluginResult::Err(PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: format!("{provider} does not implement notification actions"),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    })
}

/// Turn a configuration-resolution failure into a typed plugin error.
///
/// These used to be hard ABI faults: the host saw a string and a generic
/// failure, and could not tell a misconfigured channel from a broken one. They are now an ordinary `PluginResult::Err` on the operation's
/// own result, which is what lets Scryer surface "this channel is not
/// configured" to the operator instead of "the plugin crashed".
pub fn config_error(error: impl std::fmt::Display) -> PluginError {
    PluginError {
        code: PluginErrorCode::InvalidConfig,
        public_message: error.to_string(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}
