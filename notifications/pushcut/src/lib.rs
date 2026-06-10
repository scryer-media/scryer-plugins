use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
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
    Ok(serde_json::to_string(&descriptor)?)
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut payload = serde_json::json!({
        "title": req.summary_title,
        "text": req.summary_message,
        "isTimeSensitive": config_bool("time_sensitive"),
        "actions": metadata_actions(&req),
    });
    if config_bool("include_poster") {
        if let Some(poster) = poster_url(&req) {
            payload["image"] = serde_json::Value::String(poster);
        }
    }
    let url = format!(
        "https://api.pushcut.io/v1/notifications/{}",
        required_config("notification_name")?
    );
    let headers = [("API-Key", required_config("api_key")?)];
    let response = send_json(&url, "POST", &headers, payload);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
