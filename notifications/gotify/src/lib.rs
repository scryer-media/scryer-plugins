use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
        "gotify",
        "Gotify",
        env!("CARGO_PKG_VERSION"),
        "gotify",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        true,
    );
    Ok(serde_json::to_string(&descriptor)?)
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let server = required_config("server")?.trim_end_matches('/').to_string();
    let token = required_config("app_token")?;
    let (title, message) = title_and_body(&req);
    let priority = config_i64("priority", 5);
    let (message, extras) = gotify_message_parts(&req, message);
    let body = serde_json::json!({
        "title": title,
        "message": message,
        "priority": priority,
        "extras": extras,
    });
    let url = append_query(&format!("{server}/message"), &[("token", token)]);
    let response = send_json(&url, "POST", &[], body);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
