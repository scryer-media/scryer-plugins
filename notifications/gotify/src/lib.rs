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

const PROVIDER_TYPE: &str = "gotify";

fn build_descriptor() -> PluginDescriptor {
    build_notification_descriptor(
        "gotify",
        "Gotify",
        env!("CARGO_PKG_VERSION"),
        "gotify",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        true,
    )
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "server",
            "Server",
            true,
            None,
            Some("Gotify server URL, for example https://gotify.example"),
        ),
        field(
            "app_token",
            "App Token",
            ConfigFieldType::Password,
            true,
            None,
            Some("Gotify app token."),
        ),
        field(
            "priority",
            "Priority",
            ConfigFieldType::Number,
            false,
            Some("5"),
            None,
        ),
        field(
            "include_series_poster",
            "Include Series Poster",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "metadata_links",
            "Metadata Links",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma, semicolon, or newline separated links: imdb, tvdb, trakt, tvmaze."),
        ),
        field(
            "preferred_metadata_link",
            "Preferred Metadata Link",
            ConfigFieldType::String,
            false,
            Some("tvdb"),
            Some("One of imdb, tvdb, trakt, or tvmaze."),
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let server = required_config("server")?.trim_end_matches('/').to_string();
    let token = required_config("app_token")?;
    let (title, message) = title_and_body(req);
    let priority = config_i64("priority", 5);
    if let Err(message) = validate_metadata_link_config() {
        return Ok(error_response(
            message,
            Some("invalid_metadata_links".to_string()),
        ));
    }
    let (message, extras) = gotify_message_parts(req, message);
    let body = serde_json::json!({
        "title": title,
        "message": message,
        "priority": priority,
        "extras": extras,
    });
    let url = append_query(&format!("{server}/message"), &[("token", token)]);
    let response = send_json(&url, "POST", &[], body);
    Ok(response)
}

fn gotify_message_parts(
    req: &PluginNotificationRequest,
    mut message: String,
) -> (String, serde_json::Value) {
    let mut is_markdown = false;
    let mut notification = serde_json::Map::new();

    if config_bool("include_series_poster")
        && let Some(poster) = poster_url(req)
    {
        is_markdown = true;
        message.push_str(&format!("\n\r![]({poster})"));
        notification.insert("bigImageUrl".to_string(), serde_json::Value::String(poster));
    }

    let links = metadata_links(req);
    if !links.is_empty() {
        is_markdown = true;
        message.push('\n');
        for (_, label, url) in &links {
            message.push_str(&format!("\n[{label}]({url})"));
        }

        let preferred = config_value("preferred_metadata_link")
            .unwrap_or_else(|| "tvdb".to_string())
            .to_ascii_lowercase();
        if let Some((_, _, url)) = links
            .iter()
            .find(|(kind, _, _)| kind.eq_ignore_ascii_case(&preferred))
        {
            notification.insert(
                "click".to_string(),
                serde_json::json!({
                    "url": url,
                }),
            );
        }
    }

    let mut extras = serde_json::Map::new();
    extras.insert(
        "client::display".to_string(),
        serde_json::json!({
            "contentType": if is_markdown {
                "text/markdown"
            } else {
                "text/plain"
            },
        }),
    );
    if !notification.is_empty() {
        extras.insert(
            "client::notification".to_string(),
            serde_json::Value::Object(notification),
        );
    }

    (message, serde_json::Value::Object(extras))
}

fn metadata_links(req: &PluginNotificationRequest) -> Vec<(&'static str, &'static str, String)> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    config_csv("metadata_links")
        .into_iter()
        .filter_map(|link| {
            let kind = link.to_ascii_lowercase();
            match kind.as_str() {
                "imdb" => title
                    .external_ids
                    .imdb_id
                    .as_ref()
                    .map(|id| ("imdb", "IMDb", format!("https://www.imdb.com/title/{id}"))),
                "tvdb" => title.external_ids.tvdb_id.as_ref().map(|id| {
                    (
                        "tvdb",
                        "TVDb",
                        format!("http://www.thetvdb.com/?tab=series&id={id}"),
                    )
                }),
                "trakt" => title.external_ids.tvdb_id.as_ref().map(|id| {
                    (
                        "trakt",
                        "Trakt",
                        format!("http://trakt.tv/search/tvdb/{id}?id_type=show"),
                    )
                }),
                "tvmaze" => title.external_ids.tvmaze_id.as_ref().map(|id| {
                    (
                        "tvmaze",
                        "TVMaze",
                        format!("http://www.tvmaze.com/shows/{id}/_"),
                    )
                }),
                _ => None,
            }
        })
        .collect()
}

fn validate_metadata_link_config() -> Result<(), String> {
    let links = config_csv("metadata_links");
    for link in &links {
        if !valid_metadata_link_kind(link) {
            return Err(format!("invalid gotify metadata link: {link}"));
        }
    }

    if links.is_empty() {
        return Ok(());
    }

    let preferred = config_value("preferred_metadata_link")
        .unwrap_or_else(|| "tvdb".to_string())
        .to_ascii_lowercase();
    if !valid_metadata_link_kind(&preferred) {
        return Err(format!(
            "invalid gotify preferred metadata link: {preferred}"
        ));
    }
    if !links
        .iter()
        .any(|link| link.eq_ignore_ascii_case(&preferred))
    {
        return Err(
            "gotify preferred_metadata_link must be one of the selected metadata_links".to_string(),
        );
    }

    Ok(())
}

fn valid_metadata_link_kind(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "imdb" | "tvdb" | "trakt" | "tvmaze"
    )
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
