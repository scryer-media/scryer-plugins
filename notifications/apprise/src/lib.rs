use notify_common::*;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

const PROVIDER_TYPE: &str = "apprise";

fn build_descriptor() -> PluginDescriptor {
    build_notification_descriptor(
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
    )
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

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let server = required_config("server_url")?;
    let configuration_key = config_value("configuration_key");
    let stateless_urls = config_value("stateless_urls");
    let tags = config_csv("tags").join(",");

    if configuration_key.is_none() && stateless_urls.is_none() {
        return Ok(error_response(
            "Use either Configuration Key or Stateless URLs",
            None,
        ));
    }
    if configuration_key.is_some() && stateless_urls.is_some() {
        return Ok(error_response(
            "Use either Configuration Key or Stateless URLs",
            None,
        ));
    }
    if let Some(key) = configuration_key.as_deref() {
        let valid_key = key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        if !valid_key {
            return Ok(error_response(
                "Configuration Key may only contain a-z, 0-9, and -",
                None,
            ));
        }
    }
    if stateless_urls.is_some() && !tags.is_empty() {
        return Ok(error_response("Stateless URLs do not support tags", None));
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
        && let Some(poster) = poster_url(req)
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
    Ok(response)
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per Extism entry point this plugin used to export. `action` is not
/// one of them: the descriptor advertises no action, so the host does not route
/// one here and the arm answers **in-band** with `Unsupported` rather than
/// trapping. A trap under a component costs the whole instance and replaces the
/// plugin's own diagnosis with a generic ABI failure.
///
/// A configuration failure was a `FnResult` hard fault under Extism — the host
/// saw a string and a generic ABI error, and could not tell a misconfigured
/// channel from a broken one. It is now a typed `PluginResult::Err`.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(match send_notification(&request) {
                Ok(response) => PluginResult::Ok(response),
                Err(error) => PluginResult::Err(config_error(error)),
            })
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}
