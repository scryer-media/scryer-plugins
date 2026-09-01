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

const PROVIDER_TYPE: &str = "telegram";

const TELEGRAM_API_URL: &str = "https://api.telegram.org";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "telegram",
        "Telegram",
        env!("CARGO_PKG_VERSION"),
        "telegram",
        vec![
            NotificationDeliveryMode::Chat,
            NotificationDeliveryMode::Push,
        ],
        vec![
            NotificationPayloadFormat::PlainText,
            NotificationPayloadFormat::Html,
        ],
        config_fields(),
        true,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.telegram.org"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "bot_token",
            "Bot Token",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "chat_id",
            "Chat ID",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "topic_id",
            "Topic ID",
            ConfigFieldType::Number,
            false,
            None,
            None,
        ),
        field(
            "send_silently",
            "Send Silently",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "include_app_name_in_title",
            "Include App Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "include_instance_name_in_title",
            "Include Instance Name In Title",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let mut title = req.summary_title.clone();
    if config_bool("include_app_name_in_title") {
        title = format!("{} - {title}", req.app.name);
    }
    if config_bool("include_instance_name_in_title") {
        title = format!("{title} - {}", req.app.name);
    }
    let mut payload = serde_json::json!({
        "chat_id": required_config("chat_id")?,
        "parse_mode": "HTML",
        "text": format!("<b>{}</b>\n{}", html_escape(&title), html_escape(&req.summary_message)),
        "disable_notification": config_bool("send_silently"),
        "link_preview_options": { "is_disabled": true },
    });
    if let Some(raw_topic_id) = config_value("topic_id") {
        match raw_topic_id.parse::<i64>() {
            Ok(topic_id) if topic_id > 1 => {
                payload["message_thread_id"] = serde_json::Value::from(topic_id);
            }
            Ok(_) => {
                return Ok(error_response(
                    "Topic ID must be greater than 1 or empty",
                    None,
                ));
            }
            Err(_) => {
                return Ok(error_response("Topic ID must be a number", None));
            }
        }
    }
    let url = format!(
        "{TELEGRAM_API_URL}/bot{}/sendmessage",
        required_config("bot_token")?
    );
    let response = send_json(&url, "POST", &[], payload);
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
