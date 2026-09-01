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

const PROVIDER_TYPE: &str = "pushcut";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "pushcut",
        "Pushcut",
        env!("CARGO_PKG_VERSION"),
        "pushcut",
        vec![NotificationDeliveryMode::Push],
        vec![
            NotificationPayloadFormat::PlainText,
            NotificationPayloadFormat::RichEmbed,
        ],
        config_fields(),
        true,
        true,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.pushcut.io"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "notification_name",
            "Notification Name",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "time_sensitive",
            "Time Sensitive",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "include_poster",
            "Include Poster",
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
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    let mut payload = serde_json::json!({
        "title": req.summary_title,
        "text": req.summary_message,
        "isTimeSensitive": config_bool("time_sensitive"),
        "actions": metadata_actions(req),
    });
    if config_bool("include_poster")
        && let Some(poster) = poster_url(req)
    {
        payload["image"] = serde_json::Value::String(poster);
    }
    let notification_name = path_segment(&required_config("notification_name")?);
    let url = format!("https://api.pushcut.io/v1/notifications/{notification_name}");
    let headers = [("API-Key", required_config("api_key")?)];
    let response = send_json(&url, "POST", &headers, payload);
    Ok(response)
}

fn metadata_actions(req: &PluginNotificationRequest) -> Vec<serde_json::Value> {
    let Some(title) = req.title.as_ref() else {
        return Vec::new();
    };
    config_csv("metadata_links")
        .into_iter()
        .filter_map(|link| {
            let kind = link.to_ascii_lowercase();
            match kind.as_str() {
                "imdb" => title.external_ids.imdb_id.as_ref().map(|id| {
                    serde_json::json!({
                        "name": "IMDb",
                        "url": format!("https://www.imdb.com/title/{id}"),
                    })
                }),
                "tvdb" => title.external_ids.tvdb_id.as_ref().map(|id| {
                    serde_json::json!({
                        "name": "TVDb",
                        "url": format!("http://www.thetvdb.com/?tab=series&id={id}"),
                    })
                }),
                "trakt" => title.external_ids.tvdb_id.as_ref().map(|id| {
                    serde_json::json!({
                        "name": "Trakt",
                        "url": format!("http://trakt.tv/search/tvdb/{id}?id_type=show"),
                    })
                }),
                "tvmaze" => title.external_ids.tvmaze_id.as_ref().map(|id| {
                    serde_json::json!({
                        "name": "TVMaze",
                        "url": format!("http://www.tvmaze.com/shows/{id}/_"),
                    })
                }),
                _ => None,
            }
        })
        .collect()
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
