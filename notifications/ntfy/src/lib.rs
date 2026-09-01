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

const PROVIDER_TYPE: &str = "ntfy";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "ntfy",
        "Ntfy",
        env!("CARGO_PKG_VERSION"),
        "ntfy",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["ntfy.sh"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "server_url",
            "Server URL",
            false,
            Some("https://ntfy.sh"),
            Some("Ntfy server URL."),
        ),
        field(
            "access_token",
            "Access Token",
            ConfigFieldType::Password,
            false,
            None,
            None,
        ),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            None,
        ),
        field(
            "priority",
            "Priority",
            ConfigFieldType::Number,
            false,
            Some("3"),
            None,
        ),
        field(
            "topics",
            "Topics",
            ConfigFieldType::String,
            true,
            None,
            Some("Comma, semicolon, or newline separated ntfy topics."),
        ),
        field(
            "tags",
            "Tags",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma separated ntfy tags/emojis."),
        ),
        connection_field("click_url", "Click URL", false, None, None),
        field(
            "headers",
            "Headers",
            ConfigFieldType::Multiline,
            false,
            None,
            Some("Additional headers, one per line as Header-Name: value."),
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let server = config_value("server_url").unwrap_or_else(|| "https://ntfy.sh".to_string());
    let topics = config_csv("topics");
    if topics.is_empty() {
        return Ok(error_response("topics is not configured", None));
    }
    for topic in &topics {
        if !valid_ntfy_topic(topic) {
            return Ok(error_response(
                format!("invalid ntfy topic: {topic}"),
                Some("invalid_topic".to_string()),
            ));
        }
    }

    let priority_value = config_i64("priority", 3);
    if !(1..=5).contains(&priority_value) {
        return Ok(error_response(
            "ntfy priority must be between 1 and 5",
            Some("invalid_priority".to_string()),
        ));
    }

    let access_token = config_value("access_token");
    let username = config_value("username");
    let password = config_value("password");
    if access_token.is_none() && (username.is_some() ^ password.is_some()) {
        return Ok(error_response(
            "ntfy username and password must be configured together",
            Some("invalid_auth".to_string()),
        ));
    }

    let (title, message) = title_and_body(req);
    let priority = priority_value.to_string();
    let tags = config_csv("tags").join(",");
    let click = config_value("click_url");
    let mut headers = configured_headers();
    if let Some(token) = access_token {
        headers.push(("Authorization", format!("Bearer {token}")));
    } else if let (Some(username), Some(password)) = (username, password) {
        headers.push(("Authorization", basic_auth_header(&username, &password)));
    }

    let mut responses = Vec::new();
    for topic in topics {
        let mut params = vec![
            ("title", title.clone()),
            ("message", message.clone()),
            ("priority", priority.clone()),
        ];
        if !tags.is_empty() {
            params.push(("tags", tags.clone()));
        }
        if let Some(click) = click.clone() {
            params.push(("click", click));
        }
        let url = append_query(
            &format!("{}/{}", server.trim_end_matches('/'), topic),
            &params,
        );
        responses.push(send_bytes(&url, "POST", &headers, Vec::new()));
    }

    Ok(merge_responses(responses))
}

fn configured_headers() -> Vec<(&'static str, String)> {
    config_value("headers")
        .map(|value| {
            value
                .lines()
                .filter_map(|line| line.split_once(':'))
                .map(|(key, value)| (leak_header_key(key), value.trim().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn valid_ntfy_topic(topic: &str) -> bool {
    const INVALID_TOPICS: &[&str] = &[
        "announcements",
        "app",
        "docs",
        "settings",
        "stats",
        "mytopic-rw",
        "mytopic-ro",
        "mytopic-wo",
    ];

    !INVALID_TOPICS.contains(&topic)
        && topic
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn leak_header_key(key: &str) -> &'static str {
    Box::leak(key.trim().to_string().into_boxed_str())
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
