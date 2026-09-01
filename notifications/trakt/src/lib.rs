use notify_common::*;
use std::time::{SystemTime, UNIX_EPOCH};

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

const TRAKT_API_URL: &str = "https://api.trakt.tv";
const TRAKT_OAUTH_URL: &str = "https://trakt.tv/oauth/authorize";
const TRAKT_REDIRECT_URI: &str = "https://auth.servarr.com/v1/trakt_sonarr/auth";
const TRAKT_RENEW_URL: &str = "https://auth.servarr.com/v1/trakt_sonarr/renew";
const TRAKT_CLIENT_ID: &str = "d44ba57cab40c31eb3f797dcfccd203500796539125b333883ec1d94aa62ed4c";

fn build_descriptor() -> PluginDescriptor {
    let mut descriptor = build_notification_descriptor(
        "trakt",
        "Trakt",
        env!("CARGO_PKG_VERSION"),
        "trakt",
        vec![NotificationDeliveryMode::ExternalSync],
        vec![NotificationPayloadFormat::StructuredJson],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.trakt.tv", "auth.servarr.com"]);
    descriptor
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "access_token",
            "Access Token",
            ConfigFieldType::Password,
            true,
            None,
            Some("Trakt OAuth access token."),
        ),
        field(
            "refresh_token",
            "Refresh Token",
            ConfigFieldType::Password,
            true,
            None,
            Some("Trakt OAuth refresh token. Used to renew the access token before sync."),
        ),
        field(
            "expires",
            "Expires",
            ConfigFieldType::String,
            true,
            None,
            Some("Token expiry value retained for imported Trakt credentials."),
        ),
        field(
            "auth_user",
            "Authenticated User",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
    ]
}

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
    // `seasons_payload` reads `episodes[].media_file_id` / `media_file_path`
    // off the request as JSON. Under Extism it re-parsed the raw input string,
    // which no longer exists: the world's dispatch decodes the envelope and
    // hands over the typed request. Re-serializing is lossless here — the SDK
    // models both of those fields — so the payload builder is unchanged.
    let raw_req = serde_json::to_value(req)?;
    let Some(body) = trakt_payload(req, &raw_req) else {
        return Ok(ok_response());
    };
    let endpoint = if remove_event(req) {
        "sync/collection/remove"
    } else {
        "sync/collection"
    };
    let access_token = refreshed_access_token().unwrap_or(required_config("access_token")?);
    let headers = [
        ("trakt-api-version", "2".to_string()),
        ("trakt-api-key", TRAKT_CLIENT_ID.to_string()),
        ("Authorization", format!("Bearer {access_token}")),
    ];
    let response = send_json(
        &format!("{TRAKT_API_URL}/{endpoint}"),
        "POST",
        &headers,
        body,
    );
    Ok(response)
}

fn handle_action(action: &PluginActionRequest) -> FnResult<serde_json::Value> {
    let request = action_request_value(action);
    let response = match action_name(&request).as_deref() {
        Some("startOAuth") => start_oauth(&request)?,
        Some("getOAuthToken") => get_oauth_token(&request)?,
        _ => serde_json::json!({}),
    };

    Ok(response)
}

fn start_oauth(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let callback_url = required_action_param(request, "callbackUrl")?;
    let oauth_url = append_query(
        TRAKT_OAUTH_URL,
        &[
            ("client_id", TRAKT_CLIENT_ID.to_string()),
            ("response_type", "code".to_string()),
            ("redirect_uri", TRAKT_REDIRECT_URI.to_string()),
            ("state", callback_url),
        ],
    );

    Ok(serde_json::json!({
        "OauthUrl": oauth_url,
        "oauthUrl": oauth_url,
    }))
}

fn get_oauth_token(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let access_token = required_action_param(request, "access_token")?;
    let refresh_token = required_action_param(request, "refresh_token")?;
    let expires_in = required_action_param(request, "expires_in")?
        .parse::<i64>()
        .map_err(|_| Error::msg("QueryParam expires_in invalid."))?;
    let auth_user = get_user_name(&access_token)?;

    Ok(serde_json::json!({
        "accessToken": access_token,
        "expires": expires_after_seconds(expires_in),
        "refreshToken": refresh_token,
        "authUser": auth_user,
    }))
}

fn refreshed_access_token() -> Option<String> {
    let refresh_token = config_value("refresh_token")?;
    let url = append_query(TRAKT_RENEW_URL, &[("refresh_token", refresh_token)]);
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-trakt-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None).ok()?;
    if !(200..300).contains(&response.status_code()) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&response.body()).ok()?;
    value
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn get_user_name(access_token: &str) -> Result<String, Error> {
    let request = HttpRequest::new(format!("{TRAKT_API_URL}/users/settings"))
        .with_method("GET")
        .with_header("User-Agent", "scryer-trakt-plugin/0.1")
        .with_header("trakt-api-version", "2")
        .with_header("trakt-api-key", TRAKT_CLIENT_ID)
        .with_header("Authorization", format!("Bearer {access_token}"));
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Trakt user lookup failed: HTTP {status}: {body}"
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&response.body())?;
    value
        .get("user")
        .and_then(|user| user.get("username"))
        .and_then(|username| username.as_str())
        .map(str::to_string)
        .filter(|username| !username.trim().is_empty())
        .ok_or_else(|| Error::msg("Trakt user settings response did not include username"))
}

fn trakt_payload(
    req: &PluginNotificationRequest,
    raw_req: &serde_json::Value,
) -> Option<serde_json::Value> {
    let title = req.title.as_ref()?;
    let mut show = serde_json::json!({
        "title": title.name,
        "year": title.year,
        "ids": {
            "imdb": title.external_ids.imdb_id,
            "tvdb": parse_i64(title.external_ids.tvdb_id.as_deref()),
        },
    });

    let seasons = seasons_payload(req, raw_req);
    if !seasons.is_empty() {
        show["seasons"] = serde_json::Value::Array(seasons);
    }

    Some(serde_json::json!({
        "shows": [show],
    }))
}

fn seasons_payload(
    req: &PluginNotificationRequest,
    raw_req: &serde_json::Value,
) -> Vec<serde_json::Value> {
    let mut by_season: std::collections::BTreeMap<i64, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    let common_media_metadata = common_media_metadata(&req.media_files);

    let raw_episodes = raw_req.get("episodes").and_then(|value| value.as_array());
    let episodes = if req.episodes.is_empty() {
        req.episode
            .as_ref()
            .map(|episode| vec![(episode, raw_req.get("episode"))])
            .unwrap_or_default()
    } else {
        req.episodes
            .iter()
            .enumerate()
            .map(|(index, episode)| {
                (
                    episode,
                    raw_episodes.and_then(|episodes| episodes.get(index)),
                )
            })
            .collect()
    };

    for (episode, raw_episode) in episodes {
        let Some(season) = parse_i64(episode.season_number.as_deref()) else {
            continue;
        };
        let Some(number) = parse_i64(episode.episode_number.as_deref()) else {
            continue;
        };
        let mut episode_json = serde_json::json!({ "number": number });
        if !remove_event(req) {
            if let Some(occurred_at) = req.occurred_at.clone() {
                episode_json["collected_at"] = serde_json::Value::String(occurred_at);
            }
            let episode_media_metadata = episode_media_metadata(raw_episode, &req.media_files);
            apply_episode_media_metadata(
                &mut episode_json,
                episode_media_metadata
                    .as_ref()
                    .or(common_media_metadata.as_ref()),
            );
        }
        by_season.entry(season).or_default().push(episode_json);
    }

    by_season
        .into_iter()
        .map(|(number, episodes)| {
            serde_json::json!({
                "number": number,
                "episodes": episodes,
            })
        })
        .collect()
}

fn episode_media_metadata(
    raw_episode: Option<&serde_json::Value>,
    media_files: &[PluginNotificationMediaFile],
) -> Option<TraktMediaMetadata> {
    let raw_episode = raw_episode?;
    if let Some(media_file_id) = string_value(raw_episode, "media_file_id")
        && let Some(media_file) = media_files
            .iter()
            .find(|media_file| media_file.id.as_deref() == Some(media_file_id.as_str()))
    {
        return Some(media_metadata(media_file));
    }

    if let Some(media_file_path) = string_value(raw_episode, "media_file_path")
        && let Some(media_file) = media_files
            .iter()
            .find(|media_file| media_file.path == media_file_path)
    {
        return Some(media_metadata(media_file));
    }

    None
}

fn apply_episode_media_metadata(
    episode_json: &mut serde_json::Value,
    metadata: Option<&TraktMediaMetadata>,
) {
    let Some(metadata) = metadata else {
        return;
    };

    set_string_field(episode_json, "resolution", metadata.resolution.clone());
    set_string_field(episode_json, "hdr", metadata.hdr.clone());
    set_string_field(episode_json, "media_type", metadata.media_type.clone());
    set_string_field(
        episode_json,
        "audio_channels",
        metadata.audio_channels.clone(),
    );
    set_string_field(episode_json, "audio", metadata.audio.clone());
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraktMediaMetadata {
    resolution: Option<String>,
    hdr: Option<String>,
    media_type: Option<String>,
    audio_channels: Option<String>,
    audio: Option<String>,
}

fn common_media_metadata(
    media_files: &[PluginNotificationMediaFile],
) -> Option<TraktMediaMetadata> {
    let mut iter = media_files.iter().map(media_metadata);
    let first = iter.next()?;
    iter.all(|metadata| metadata == first).then_some(first)
}

fn media_metadata(media_file: &PluginNotificationMediaFile) -> TraktMediaMetadata {
    TraktMediaMetadata {
        resolution: map_resolution(media_file),
        hdr: map_hdr(media_file),
        media_type: map_media_type(media_file),
        audio_channels: media_file.audio_channels.clone(),
        audio: map_audio(media_file),
    }
}

fn set_string_field(episode_json: &mut serde_json::Value, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        episode_json[key] = serde_json::Value::String(value);
    }
}

fn map_resolution(media_file: &PluginNotificationMediaFile) -> Option<String> {
    match media_file.video_height? {
        height if height >= 2160 => Some("uhd_4k".to_string()),
        1080 => Some("hd_1080p".to_string()),
        720 => Some("hd_720p".to_string()),
        576 => Some("sd_576p".to_string()),
        480 => Some("sd_480p".to_string()),
        _ => None,
    }
}

fn map_hdr(media_file: &PluginNotificationMediaFile) -> Option<String> {
    let normalized = media_file
        .video_hdr_format
        .as_ref()?
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");

    if normalized.contains("dolbyvision") || normalized == "dv" {
        Some("dolby_vision".to_string())
    } else if normalized.contains("hdr10plus") || normalized.contains("hdr10+") {
        Some("hdr10_plus".to_string())
    } else if normalized.contains("hdr10") {
        Some("hdr10".to_string())
    } else if normalized.contains("hlg") {
        Some("hlg".to_string())
    } else {
        None
    }
}

fn map_media_type(media_file: &PluginNotificationMediaFile) -> Option<String> {
    let quality = media_file.quality.as_ref()?.to_ascii_lowercase();
    if quality.contains("web") {
        Some("digital".to_string())
    } else if quality.contains("blu") || quality.contains("bd") {
        Some("bluray".to_string())
    } else if quality.contains("dvd") {
        Some("dvd".to_string())
    } else if quality.contains("hdtv") || quality.contains("tv") {
        Some("vhs".to_string())
    } else {
        None
    }
}

fn map_audio(media_file: &PluginNotificationMediaFile) -> Option<String> {
    let normalized = media_file.audio_codec.as_ref()?.trim().to_ascii_uppercase();
    match normalized.as_str() {
        value if value.contains("EAC3") && value.contains("ATMOS") => {
            Some("dolby_digital_plus_atmos".to_string())
        }
        value if value.contains("TRUEHD") && value.contains("ATMOS") => {
            Some("dolby_atmos".to_string())
        }
        "AC3" => Some("dolby_digital".to_string()),
        "EAC3" => Some("dolby_digital_plus".to_string()),
        "TRUEHD" => Some("dolby_truehd".to_string()),
        "DTS" | "DTS-ES" => Some("dts".to_string()),
        "DTS-HD MA" => Some("dts_ma".to_string()),
        "DTS-HD HRA" => Some("dts_hr".to_string()),
        "DTS-X" | "DTS:X" => Some("dts_x".to_string()),
        "MP3" => Some("mp3".to_string()),
        "MP2" => Some("mp2".to_string()),
        "VORBIS" => Some("ogg".to_string()),
        "WMA" => Some("wma".to_string()),
        "AAC" => Some("aac".to_string()),
        "PCM" => Some("lpcm".to_string()),
        "FLAC" => Some("flac".to_string()),
        "OPUS" => Some("ogg_opus".to_string()),
        _ => None,
    }
}

fn remove_event(req: &PluginNotificationRequest) -> bool {
    matches!(
        req.event_type,
        NotificationEventType::FileDeleted
            | NotificationEventType::FileDeletedForUpgrade
            | NotificationEventType::TitleDeleted
    )
}

fn parse_i64(value: Option<&str>) -> Option<i64> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i64>().ok())
}

fn action_name(request: &serde_json::Value) -> Option<String> {
    string_value(request, "action")
        .or_else(|| string_value(request, "name"))
        .or_else(|| string_value(request, "providerAction"))
}

fn required_action_param(request: &serde_json::Value, key: &str) -> Result<String, Error> {
    action_param(request, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg(format!("QueryParam {key} invalid.")))
}

fn action_param(request: &serde_json::Value, key: &str) -> Option<String> {
    request
        .get("query")
        .and_then(|query| string_value(query, key))
        .or_else(|| {
            request
                .get("query_params")
                .and_then(|query| string_value(query, key))
        })
        .or_else(|| string_value(request, key))
}

fn string_value(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| match value {
            serde_json::Value::String(value) => Some(value.trim().to_string()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            serde_json::Value::Bool(value) => Some(value.to_string()),
            _ => None,
        })
        .filter(|value| !value.is_empty())
}

fn expires_after_seconds(seconds: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_rfc3339(now.saturating_add(seconds))
}

fn format_unix_rfc3339(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };

    (year, month, day)
}

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per Extism entry point this plugin used to export — and this channel
/// exported three, so `action` is a real operation here rather than the in-band
/// `Unsupported` most channels answer with. Its OAuth handlers are unchanged;
/// only the envelope around them moved.
///
/// A failure in either operation was a `FnResult` hard fault under Extism: the
/// host saw a string and a generic ABI error, and could not tell a
/// misconfigured channel from a broken one. Both are now typed
/// `PluginResult::Err`, which also means a failed OAuth exchange no longer
/// takes the component instance down with it.
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
        PluginNotificationCommand::Action(request) => {
            PluginNotificationCommandResult::Action(match handle_action(&request) {
                Ok(payload) => PluginResult::Ok(PluginActionResponse { payload }),
                Err(error) => PluginResult::Err(config_error(error)),
            })
        }
    }
}

/// Rebuild the JSON document the action handlers have always read.
///
/// Under Extism, `scryer_notification_action` received one opaque JSON string:
/// the action name alongside a `query` object of parameters. The command
/// envelope splits those into `PluginActionRequest::action` and `::payload`,
/// and the host fills the payload with exactly `{"query": {..}}`. Re-joining
/// them here keeps `action_name`, `action_param` and every handler below
/// byte-for-byte unchanged, so the OAuth flows are not re-derived as part of a
/// transport migration.
fn action_request_value(request: &PluginActionRequest) -> serde_json::Value {
    let mut value = match request.payload.clone() {
        value @ serde_json::Value::Object(_) => value,
        other => serde_json::json!({ "query": other }),
    };
    value["action"] = serde_json::Value::String(request.action.clone());
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        episodes: Vec<serde_json::Value>,
        media_files: Vec<serde_json::Value>,
    ) -> PluginNotificationRequest {
        serde_json::from_value(serde_json::json!({
            "schema_version": 3,
            "event_type": "import_complete",
            "occurred_at": "2026-06-10T12:00:00Z",
            "summary_title": "Imported",
            "summary_message": "Imported",
            "app": {
                "name": "Scryer",
                "version": "0.16.0"
            },
            "episodes": episodes,
            "media_files": media_files
        }))
        .unwrap()
    }

    fn episode(season: &str, number: &str) -> serde_json::Value {
        serde_json::json!({
            "season_number": season,
            "episode_number": number
        })
    }

    fn media_file(id: &str, path: &str, height: i32) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "path": path,
            "quality": if height >= 1080 { "Bluray" } else { "WEBDL" },
            "video_height": height,
            "audio_codec": "AC3",
            "audio_channels": "5.1"
        })
    }

    #[test]
    fn seasons_payload_uses_associated_media_file_metadata() {
        let req = request(
            vec![episode("1", "1"), episode("1", "2")],
            vec![
                media_file("file-720", "/show/s01e01.mkv", 720),
                media_file("file-1080", "/show/s01e02.mkv", 1080),
            ],
        );
        let raw_req = serde_json::json!({
            "episodes": [
                { "media_file_id": "file-720" },
                { "media_file_path": "/show/s01e02.mkv" }
            ]
        });

        let seasons = seasons_payload(&req, &raw_req);
        let episodes = seasons[0]["episodes"].as_array().unwrap();

        assert_eq!(episodes[0]["resolution"], "hd_720p");
        assert_eq!(episodes[0]["media_type"], "digital");
        assert_eq!(episodes[1]["resolution"], "hd_1080p");
        assert_eq!(episodes[1]["media_type"], "bluray");
    }

    #[test]
    fn seasons_payload_keeps_single_file_metadata_fallback() {
        let req = request(
            vec![episode("1", "1")],
            vec![media_file("file-1080", "/show/s01e01.mkv", 1080)],
        );

        let seasons = seasons_payload(&req, &serde_json::json!({}));
        let episode = &seasons[0]["episodes"][0];

        assert_eq!(episode["resolution"], "hd_1080p");
        assert_eq!(episode["audio_channels"], "5.1");
    }

    #[test]
    fn seasons_payload_does_not_guess_differing_multi_file_metadata() {
        let req = request(
            vec![episode("1", "1"), episode("1", "2")],
            vec![
                media_file("file-720", "/show/s01e01.mkv", 720),
                media_file("file-1080", "/show/s01e02.mkv", 1080),
            ],
        );

        let seasons = seasons_payload(&req, &serde_json::json!({}));
        let episodes = seasons[0]["episodes"].as_array().unwrap();

        assert!(episodes[0].get("resolution").is_none());
        assert!(episodes[1].get("resolution").is_none());
    }
}
