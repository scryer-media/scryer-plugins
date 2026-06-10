use extism_pdk::*;
use notify_common::*;
use std::time::{SystemTime, UNIX_EPOCH};

const TRAKT_API_URL: &str = "https://api.trakt.tv";
const TRAKT_OAUTH_URL: &str = "https://trakt.tv/oauth/authorize";
const TRAKT_REDIRECT_URI: &str = "https://auth.servarr.com/v1/trakt_sonarr/auth";
const TRAKT_RENEW_URL: &str = "https://auth.servarr.com/v1/trakt_sonarr/renew";
const TRAKT_CLIENT_ID: &str = "d44ba57cab40c31eb3f797dcfccd203500796539125b333883ec1d94aa62ed4c";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
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
    Ok(serde_json::to_string(&descriptor)?)
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
            false,
            None,
            Some("Trakt OAuth refresh token. Used to renew the access token before sync."),
        ),
        field(
            "expires",
            "Expires",
            ConfigFieldType::String,
            false,
            None,
            Some("Sonarr-compatible token expiry value retained for import parity."),
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let Some(body) = trakt_payload(&req) else {
        return Ok(serde_json::to_string(&PluginResult::Ok(ok_response()))?);
    };
    let endpoint = if remove_event(&req) {
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
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_notification_action(input: String) -> FnResult<String> {
    let request: serde_json::Value = serde_json::from_str(&input)?;
    let response = match action_name(&request).as_deref() {
        Some("startOAuth") => start_oauth(&request)?,
        Some("getOAuthToken") => get_oauth_token(&request)?,
        _ => serde_json::json!({}),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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

fn trakt_payload(req: &PluginNotificationRequest) -> Option<serde_json::Value> {
    let title = req.title.as_ref()?;
    let mut show = serde_json::json!({
        "title": title.name,
        "year": title.year,
        "ids": {
            "imdb": title.external_ids.imdb_id,
            "tvdb": parse_i64(title.external_ids.tvdb_id.as_deref()),
        },
    });

    let seasons = seasons_payload(req);
    if !seasons.is_empty() {
        show["seasons"] = serde_json::Value::Array(seasons);
    }

    Some(serde_json::json!({
        "shows": [show],
    }))
}

fn seasons_payload(req: &PluginNotificationRequest) -> Vec<serde_json::Value> {
    let mut by_season: std::collections::BTreeMap<i64, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    let episodes = if req.episodes.is_empty() {
        req.episode.iter().cloned().collect::<Vec<_>>()
    } else {
        req.episodes.clone()
    };

    for episode in episodes {
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
