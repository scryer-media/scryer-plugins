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

const PROVIDER_TYPE: &str = "mailgun";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "mailgun",
        "Mailgun",
        env!("CARGO_PKG_VERSION"),
        "mailgun",
        vec![NotificationDeliveryMode::Email],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.mailgun.net", "api.eu.mailgun.net"]);
    descriptor
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
            "use_eu_endpoint",
            "Use EU Endpoint",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "from",
            "From Address",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "sender_domain",
            "Sender Domain",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "recipients",
            "Recipients",
            ConfigFieldType::String,
            true,
            None,
            Some("Comma, semicolon, or newline separated recipient email addresses."),
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let (title, message) = title_and_body(req);
    let base_url = if config_bool("use_eu_endpoint") {
        "https://api.eu.mailgun.net/v3"
    } else {
        "https://api.mailgun.net/v3"
    };
    let domain = required_config("sender_domain")?;
    let recipients = config_csv("recipients");
    if recipients.is_empty() {
        return Ok(error_response("mailgun recipients is not configured", None));
    }

    let mut params = vec![
        ("from".to_string(), required_config("from")?),
        ("subject".to_string(), title),
        ("text".to_string(), message),
    ];
    for recipient in recipients {
        params.push(("to".to_string(), recipient));
    }
    let headers = [(
        "Authorization",
        basic_auth_header("api", &required_config("api_key")?),
    )];
    let response = send_form(
        &format!("{}/{}/messages", base_url, domain.trim_matches('/')),
        "POST",
        &headers,
        &params,
    );
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
