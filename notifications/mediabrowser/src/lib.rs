use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
        "mediabrowser",
        "Emby / Jellyfin",
        env!("CARGO_PKG_VERSION"),
        "mediabrowser",
        vec![
            NotificationDeliveryMode::Push,
            NotificationDeliveryMode::MediaServerUpdate,
        ],
        vec![
            NotificationPayloadFormat::PlainText,
            NotificationPayloadFormat::StructuredJson,
        ],
        config_fields(),
        false,
        true,
    );
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field("host", "Host", ConfigFieldType::String, true, None, None),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("8096"),
            None,
        ),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "url_base",
            "URL Base",
            ConfigFieldType::String,
            false,
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
            "notify",
            "Send Notifications",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "update_library",
            "Update Library",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            None,
        ),
        field(
            "map_from",
            "Map Paths From",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "map_to",
            "Map Paths To",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut responses = Vec::new();
    if config_bool("notify") {
        responses.push(send_admin_notification(&req));
    }
    if config_bool("update_library") && should_update_library(&req) {
        match update_paths(&req) {
            Ok(paths) => {
                for path in paths {
                    responses.push(send_media_updated(&path, media_update_type(&req)));
                }
            }
            Err(error) => responses.push(error_response(error.to_string(), None)),
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
}

fn send_admin_notification(req: &PluginNotificationRequest) -> PluginNotificationResponse {
    let body = serde_json::json!({
        "Name": req.summary_title,
        "Description": req.summary_message,
        "ImageUrl": "https://raw.github.com/Sonarr/Sonarr/develop/Logo/64.png",
    });
    send_json(
        &format!("{}/Notifications/Admin", base_url()),
        "POST",
        &auth_headers(),
        body,
    )
}

fn send_media_updated(path: &str, update_type: &'static str) -> PluginNotificationResponse {
    let body = serde_json::json!({
        "Updates": [{
            "Path": path,
            "UpdateType": update_type,
        }],
    });
    send_json(
        &format!("{}/Library/Media/Updated", base_url()),
        "POST",
        &auth_headers(),
        body,
    )
}

fn auth_headers() -> [(&'static str, String); 1] {
    [(
        "X-MediaBrowser-Token",
        required_config("api_key").unwrap_or_default(),
    )]
}

fn base_url() -> String {
    let scheme = if config_bool("use_ssl") {
        "https"
    } else {
        "http"
    };
    let host = required_config("host").unwrap_or_default();
    let port = config_i64("port", 8096);
    let url_base = config_value("url_base").unwrap_or_default();
    if url_base.is_empty() {
        format!("{scheme}://{host}:{port}")
    } else {
        format!("{scheme}://{host}:{port}/{}", url_base.trim_matches('/'))
    }
}

fn update_paths(req: &PluginNotificationRequest) -> Result<Vec<String>, Error> {
    let mut paths = media_browser_paths(req)?;
    if let Some(path) = req.title.as_ref().and_then(|title| title.path.as_deref()) {
        paths.push(map_path(path));
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn media_browser_paths(req: &PluginNotificationRequest) -> Result<Vec<String>, Error> {
    let Some(title) = &req.title else {
        return Ok(Vec::new());
    };

    let mut params = vec![
        ("recursive", "true".to_string()),
        ("includeItemTypes", "Series".to_string()),
        ("fields", "Path,ProviderIds".to_string()),
    ];
    if let Some(year) = title.year {
        params.push(("years", year.to_string()));
    }
    let url = append_query(&format!("{}/Items", base_url()), &params);
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-notification-plugin/0.1")
        .with_header(
            "X-MediaBrowser-Token",
            required_config("api_key").unwrap_or_default(),
        );
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Emby/Jellyfin item lookup failed: HTTP {status}: {body}"
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&response.body())?;
    Ok(matching_item_paths(&value, req))
}

fn map_path(path: &str) -> String {
    match (config_value("map_from"), config_value("map_to")) {
        (Some(from), Some(to)) if path.starts_with(&from) => path.replacen(&from, &to, 1),
        _ => path.to_string(),
    }
}

fn matching_item_paths(value: &serde_json::Value, req: &PluginNotificationRequest) -> Vec<String> {
    let Some(title) = &req.title else {
        return Vec::new();
    };
    let Some(items) = value.get("Items").and_then(|items| items.as_array()) else {
        return Vec::new();
    };

    let mut id_matches = Vec::new();
    let mut name_matches = Vec::new();
    for item in items {
        let Some(path) = string_member(item, &["Path"]) else {
            continue;
        };

        if item_matches_external_ids(item, req) {
            id_matches.push(path);
        } else if string_member(item, &["Name"]).is_some_and(|name| name == title.name) {
            name_matches.push(path);
        }
    }

    if id_matches.is_empty() {
        name_matches
    } else {
        id_matches
    }
}

fn item_matches_external_ids(item: &serde_json::Value, req: &PluginNotificationRequest) -> bool {
    let Some(title) = &req.title else {
        return false;
    };
    let Some(provider_ids) = item.get("ProviderIds") else {
        return false;
    };
    let external_ids = &title.external_ids;

    provider_id_matches(provider_ids, "Tvdb", external_ids.tvdb_id.as_deref())
        || provider_id_matches(provider_ids, "Imdb", external_ids.imdb_id.as_deref())
        || provider_id_matches(provider_ids, "TvMaze", external_ids.tvmaze_id.as_deref())
        || provider_id_matches(provider_ids, "Tmdb", external_ids.tmdb_id.as_deref())
}

fn provider_id_matches(
    provider_ids: &serde_json::Value,
    key: &str,
    expected: Option<&str>,
) -> bool {
    let Some(expected) = expected.filter(|value| !value.trim().is_empty()) else {
        return false;
    };

    string_member(provider_ids, &[key]).is_some_and(|actual| external_ids_equal(&actual, expected))
}

fn external_ids_equal(actual: &str, expected: &str) -> bool {
    match (actual.parse::<i64>(), expected.parse::<i64>()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual.eq_ignore_ascii_case(expected),
    }
}

fn string_member(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| {
            value.get(*key).and_then(|value| match value {
                serde_json::Value::String(value) => Some(value.trim().to_string()),
                serde_json::Value::Number(value) => Some(value.to_string()),
                serde_json::Value::Bool(value) => Some(value.to_string()),
                _ => None,
            })
        })
        .filter(|value| !value.is_empty())
}

fn media_update_type(req: &PluginNotificationRequest) -> &'static str {
    match req.event_type {
        NotificationEventType::FileDeleted
        | NotificationEventType::FileDeletedForUpgrade
        | NotificationEventType::TitleDeleted => "Deleted",
        NotificationEventType::Rename => "Modified",
        _ => "Created",
    }
}

fn should_update_library(req: &PluginNotificationRequest) -> bool {
    matches!(
        req.event_type,
        NotificationEventType::Download
            | NotificationEventType::Upgrade
            | NotificationEventType::ImportComplete
            | NotificationEventType::Rename
            | NotificationEventType::FileDeleted
            | NotificationEventType::FileDeletedForUpgrade
            | NotificationEventType::TitleAdded
            | NotificationEventType::TitleDeleted
    )
}
