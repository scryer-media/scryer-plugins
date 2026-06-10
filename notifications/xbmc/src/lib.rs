use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = build_notification_descriptor(
        "xbmc",
        "Kodi",
        env!("CARGO_PKG_VERSION"),
        "xbmc",
        vec![
            NotificationDeliveryMode::Chat,
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
            Some("8080"),
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
            Some("/jsonrpc"),
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
            "display_time",
            "Display Time",
            ConfigFieldType::Number,
            false,
            Some("5"),
            None,
        ),
        field(
            "notify",
            "GUI Notification",
            ConfigFieldType::Bool,
            false,
            Some("true"),
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
            "always_update",
            "Always Update",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            Some("Update or clean the Kodi library even while video is playing."),
        ),
        field(
            "clean_library",
            "Clean Library",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let mut responses = Vec::new();
    if config_bool("notify") {
        responses.push(json_rpc(
            "GUI.ShowNotification",
            serde_json::json!([
                notification_header(&req),
                req.summary_message,
                "https://raw.github.com/Sonarr/Sonarr/develop/Logo/64.png",
                config_i64("display_time", 5) * 1000,
            ]),
        ));
    }
    if config_bool("update_library") && should_update_library(&req) {
        responses.push(scan_library(&req));
    }
    if config_bool("clean_library") && should_clean_library(&req) {
        responses.push(library_action("VideoLibrary.Clean", serde_json::json!([])));
    }
    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
}

fn scan_library(req: &PluginNotificationRequest) -> PluginNotificationResponse {
    let params = series_path(req)
        .map(|path| serde_json::json!([path]))
        .unwrap_or_else(|| serde_json::json!([]));
    library_action("VideoLibrary.Scan", params)
}

fn library_action(method: &str, params: serde_json::Value) -> PluginNotificationResponse {
    if !config_bool("always_update") && has_active_video_player() {
        return ok_response();
    }

    json_rpc(method, params)
}

fn has_active_video_player() -> bool {
    let response = json_rpc_value("Player.GetActivePlayers", serde_json::json!([]));
    response
        .get("result")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .any(|player| {
            player
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("video"))
        })
}

fn json_rpc(method: &str, params: serde_json::Value) -> PluginNotificationResponse {
    let response = json_rpc_value(method, params);
    let status = response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .map(ToString::to_string);
    if let Some(status) = status {
        return error_response(status, None);
    }

    ok_response()
}

fn json_rpc_value(method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    });
    let mut headers = Vec::new();
    if let (Some(username), Some(password)) = (config_value("username"), config_value("password")) {
        headers.push(("Authorization", basic_auth_header(&username, &password)));
    }
    let mut request = HttpRequest::new(base_url())
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "scryer-notification-plugin/0.1");
    for (name, value) in headers {
        request = request.with_header(name, value);
    }
    match http::request::<Vec<u8>>(
        &request,
        Some(serde_json::to_vec(&body).unwrap_or_default()),
    ) {
        Ok(response) if !(200..300).contains(&response.status_code()) => serde_json::json!({
            "error": {
                "message": format!("Kodi returned HTTP {}", response.status_code())
            }
        }),
        Ok(response) => serde_json::from_slice(&response.body()).unwrap_or_else(|_| {
            serde_json::json!({
                "error": {
                    "message": format!("Kodi returned HTTP {}", response.status_code())
                }
            })
        }),
        Err(error) => serde_json::json!({
            "error": {
                "message": format!("Kodi request failed: {error}")
            }
        }),
    }
}

fn base_url() -> String {
    let scheme = if config_bool("use_ssl") {
        "https"
    } else {
        "http"
    };
    let host = required_config("host").unwrap_or_default();
    let port = config_i64("port", 8080);
    let url_base = config_value("url_base").unwrap_or_else(|| "/jsonrpc".to_string());
    format!(
        "{scheme}://{host}:{port}/{}",
        url_base.trim_start_matches('/')
    )
}

fn notification_header(req: &PluginNotificationRequest) -> &'static str {
    match req.event_type {
        NotificationEventType::Grab => "Sonarr - Grabbed",
        NotificationEventType::Download => "Sonarr - Downloaded",
        NotificationEventType::Upgrade | NotificationEventType::ImportComplete => {
            "Sonarr - Imported"
        }
        NotificationEventType::FileDeleted
        | NotificationEventType::FileDeletedForUpgrade
        | NotificationEventType::TitleDeleted => "Sonarr - Deleted",
        NotificationEventType::TitleAdded => "Sonarr - Added",
        NotificationEventType::HealthIssue => "Sonarr - Health Issue",
        NotificationEventType::HealthRestored => "Sonarr - Health Restored",
        NotificationEventType::ApplicationUpdate => "Sonarr - Application Updated",
        NotificationEventType::ManualInteractionRequired => "Manual Interaction Required",
        _ => "Sonarr",
    }
}

fn series_path(req: &PluginNotificationRequest) -> Option<String> {
    let response = json_rpc_value(
        "VideoLibrary.GetTvShows",
        serde_json::json!([["file", "imdbnumber"]]),
    );
    let tvshows = response
        .get("result")
        .and_then(|result| result.get("tvshows").or_else(|| result.get("TvShows")))
        .and_then(|tvshows| tvshows.as_array())?;

    tvshows
        .iter()
        .find(|show| show_matches_title(show, req))
        .and_then(|show| string_member(show, &["file", "File"]))
}

fn show_matches_title(show: &serde_json::Value, req: &PluginNotificationRequest) -> bool {
    let Some(title) = &req.title else {
        return false;
    };

    if let Some(tvdb_id) = title.external_ids.tvdb_id.as_deref()
        && string_member(show, &["imdbnumber", "ImdbNumber"])
            .is_some_and(|value| numeric_ids_equal(&value, tvdb_id))
    {
        return true;
    }

    string_member(show, &["label", "Label"]).is_some_and(|label| label == title.name)
}

fn numeric_ids_equal(actual: &str, expected: &str) -> bool {
    match (actual.parse::<i64>(), expected.parse::<i64>()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
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

fn should_clean_library(req: &PluginNotificationRequest) -> bool {
    matches!(
        req.event_type,
        NotificationEventType::Download
            | NotificationEventType::Upgrade
            | NotificationEventType::FileDeleted
            | NotificationEventType::FileDeletedForUpgrade
            | NotificationEventType::TitleAdded
            | NotificationEventType::TitleDeleted
    )
}
