use extism_pdk::*;
use notify_common::*;

const PLEX_CLIENT_IDENTIFIER: &str = "scryer";
const PLEX_PRODUCT: &str = "Scryer";
const PLEX_PLATFORM: &str = "Scryer";
const PLEX_PLATFORM_VERSION: &str = "0";
const PLEX_VERSION: &str = "0";
const PLEX_PINS_URL: &str = "https://plex.tv/api/v2/pins";
const PLEX_RESOURCES_URL: &str = "https://plex.tv/api/v2/resources";
const PLEX_SIGN_IN_URL: &str = "https://app.plex.tv/auth/#!";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "plex",
        "Plex Media Server",
        env!("CARGO_PKG_VERSION"),
        "plex",
        vec![NotificationDeliveryMode::MediaServerUpdate],
        vec![NotificationPayloadFormat::StructuredJson],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["app.plex.tv", "plex.tv"]);
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
            Some("32400"),
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
            "auth_token",
            "Auth Token",
            ConfigFieldType::Password,
            false,
            None,
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
            "section_ids",
            "Section IDs",
            ConfigFieldType::String,
            false,
            None,
            Some(
                "Optional comma, semicolon, or newline separated Plex library section IDs. Leave blank to discover TV libraries like Sonarr.",
            ),
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
    if !config_bool("update_library") || !should_update_library(&req) {
        return Ok(serde_json::to_string(&PluginResult::Ok(ok_response()))?);
    }
    let path = update_path(&req).map(|path| map_path(&path));
    let targets = match refresh_targets(path.as_deref()) {
        Ok(targets) => targets,
        Err(error) => {
            return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                error.to_string(),
                None,
            )))?);
        }
    };
    let mut responses = Vec::new();
    for target in targets {
        responses.push(refresh_section(&target.section_id, target.path.as_deref()));
    }
    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
}

#[plugin_fn]
pub fn scryer_notification_action(input: String) -> FnResult<String> {
    let request: serde_json::Value = serde_json::from_str(&input)?;
    let response = match action_name(&request).as_deref() {
        Some("startOAuth") => start_oauth(),
        Some("continueOAuth") => continue_oauth(&request)?,
        Some("pollOAuth") => poll_oauth(&request)?,
        Some("getOAuthToken") => get_oauth_token(&request)?,
        Some("servers") => servers()?,
        _ => serde_json::json!({}),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn start_oauth() -> serde_json::Value {
    serde_json::json!({
        "poll": true,
        "url": append_query(PLEX_PINS_URL, &plex_query_params([("strong", "true".to_string())])),
        "method": "POST",
        "headers": {
            "Accept": "application/json",
        },
    })
}

fn continue_oauth(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let callback_url = required_action_param(request, "callbackUrl")?;
    let pin_id = required_action_param(request, "id")?;
    let pin_code = required_action_param(request, "code")?;
    let oauth_url = append_query(
        PLEX_SIGN_IN_URL,
        &plex_query_params([
            ("clientID", PLEX_CLIENT_IDENTIFIER.to_string()),
            ("forwardUrl", callback_url),
            ("code", pin_code),
        ]),
    );

    Ok(serde_json::json!({
        "oauthUrl": oauth_url,
        "pinId": parse_i64(&pin_id)?,
    }))
}

fn poll_oauth(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let pin_id = required_action_param(request, "pinId")?;
    let auth_token = plex_auth_token(parse_i64(&pin_id)?)?;
    Ok(serde_json::json!({
        "success": !auth_token.trim().is_empty(),
    }))
}

fn get_oauth_token(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let pin_id = required_action_param(request, "pinId")?;
    let auth_token = plex_auth_token(parse_i64(&pin_id)?)?;
    Ok(serde_json::json!({
        "authToken": auth_token,
    }))
}

fn servers() -> Result<serde_json::Value, Error> {
    let Some(auth_token) = config_value("auth_token") else {
        return Ok(serde_json::json!({}));
    };
    let url = append_query(
        PLEX_RESOURCES_URL,
        &plex_query_params([
            ("includeHttps", "1".to_string()),
            ("clientID", PLEX_CLIENT_IDENTIFIER.to_string()),
            ("X-Plex-Token", auth_token),
        ]),
    );
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-plex-plugin/0.1")
        .with_header("Accept", "application/json");
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Plex server discovery failed: HTTP {status}: {body}"
        )));
    }

    let resources: serde_json::Value = serde_json::from_slice(&response.body())?;
    let options = resources
        .as_array()
        .into_iter()
        .flatten()
        .filter(|resource| bool_member(resource, "owned"))
        .filter(|resource| {
            string_member(resource, &["provides"])
                .map(|provides| provides.split(',').any(|value| value.trim() == "server"))
                .unwrap_or(false)
        })
        .flat_map(server_options)
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "options": options,
    }))
}

fn plex_auth_token(pin_id: i64) -> Result<String, Error> {
    let url = append_query(
        &format!("{PLEX_PINS_URL}/{pin_id}"),
        &plex_query_params(std::iter::empty::<(&str, String)>()),
    );
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-plex-plugin/0.1")
        .with_header("Accept", "application/json");
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Plex pin lookup failed: HTTP {status}: {body}"
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&response.body())?;
    Ok(string_member(&value, &["authToken", "auth_token"]).unwrap_or_default())
}

fn refresh_section(section_id: &str, path: Option<&str>) -> PluginNotificationResponse {
    let mut params = vec![
        ("X-Plex-Client-Identifier", "scryer".to_string()),
        ("X-Plex-Product", "Scryer".to_string()),
        ("X-Plex-Platform", "Scryer".to_string()),
        ("X-Plex-Device-Name", "Scryer".to_string()),
        ("X-Plex-Version", "0".to_string()),
    ];
    if let Some(token) = config_value("auth_token") {
        params.push(("X-Plex-Token", token));
    }
    if let Some(path) = path {
        params.push(("path", path.to_string()));
    }
    let url = append_query(
        &format!("{}/library/sections/{section_id}/refresh", base_url()),
        &params,
    );
    send_bytes(&url, "GET", &[], Vec::new())
}

fn base_url() -> String {
    let scheme = if config_bool("use_ssl") {
        "https"
    } else {
        "http"
    };
    let host = required_config("host").unwrap_or_default();
    let port = config_i64("port", 32400);
    let url_base = config_value("url_base").unwrap_or_default();
    if url_base.is_empty() {
        format!("{scheme}://{host}:{port}")
    } else {
        format!("{scheme}://{host}:{port}/{}", url_base.trim_matches('/'))
    }
}

#[derive(Debug, Clone)]
struct PlexRefreshTarget {
    section_id: String,
    path: Option<String>,
}

#[derive(Debug, Clone)]
struct PlexSection {
    id: String,
    locations: Vec<String>,
}

fn refresh_targets(path: Option<&str>) -> Result<Vec<PlexRefreshTarget>, Error> {
    let ids = config_csv("section_ids");
    if !ids.is_empty() && !ids.iter().any(|id| id.eq_ignore_ascii_case("all")) {
        return Ok(ids
            .into_iter()
            .map(|section_id| PlexRefreshTarget {
                section_id,
                path: path.map(str::to_string),
            })
            .collect());
    }

    let sections = tv_sections()?;
    if sections.is_empty() {
        return Err(Error::msg("Plex returned no TV library sections"));
    }

    if let Some(path) = path {
        let matching = sections
            .iter()
            .filter(|section| section.matches_path(path))
            .map(|section| PlexRefreshTarget {
                section_id: section.id.clone(),
                path: Some(path.to_string()),
            })
            .collect::<Vec<_>>();
        if !matching.is_empty() {
            return Ok(matching);
        }
    }

    Ok(sections
        .into_iter()
        .map(|section| PlexRefreshTarget {
            section_id: section.id,
            path: None,
        })
        .collect())
}

fn tv_sections() -> Result<Vec<PlexSection>, Error> {
    let url = append_query(
        &format!("{}/library/sections", base_url()),
        &plex_server_query_params(),
    );
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-plex-plugin/0.1")
        .with_header("Accept", "application/json");
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Plex library section lookup failed: HTTP {status}: {body}"
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&response.body())?;
    Ok(parse_tv_sections(&value))
}

fn parse_tv_sections(value: &serde_json::Value) -> Vec<PlexSection> {
    let section_values = value
        .pointer("/MediaContainer/Directory")
        .or_else(|| value.pointer("/MediaContainer/_children"))
        .or_else(|| value.get("Directory"))
        .or_else(|| value.get("_children"))
        .and_then(|sections| sections.as_array());

    section_values
        .into_iter()
        .flatten()
        .filter(|section| {
            string_member(section, &["type"])
                .is_some_and(|section_type| section_type.eq_ignore_ascii_case("show"))
        })
        .filter_map(|section| {
            let id = string_member(section, &["key", "id"])?;
            Some(PlexSection {
                id,
                locations: plex_section_locations(section),
            })
        })
        .collect()
}

fn plex_section_locations(section: &serde_json::Value) -> Vec<String> {
    section
        .get("Location")
        .or_else(|| section.get("_children"))
        .and_then(|locations| locations.as_array())
        .into_iter()
        .flatten()
        .filter_map(|location| string_member(location, &["path"]))
        .collect()
}

impl PlexSection {
    fn matches_path(&self, path: &str) -> bool {
        let path = normalize_media_path(path);
        self.locations
            .iter()
            .map(|location| normalize_media_path(location))
            .any(|location| {
                path == location
                    || path
                        .strip_prefix(&location)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
    }
}

fn normalize_media_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn update_path(req: &PluginNotificationRequest) -> Option<String> {
    req.file
        .as_ref()
        .and_then(|file| file.primary_path.clone())
        .or_else(|| req.title.as_ref().and_then(|title| title.path.clone()))
}

fn map_path(path: &str) -> String {
    match (config_value("map_from"), config_value("map_to")) {
        (Some(from), Some(to)) if path.starts_with(&from) => path.replacen(&from, &to, 1),
        _ => path.to_string(),
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

fn plex_query_params(
    extra: impl IntoIterator<Item = (&'static str, String)>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        (
            "X-Plex-Client-Identifier",
            PLEX_CLIENT_IDENTIFIER.to_string(),
        ),
        ("X-Plex-Product", PLEX_PRODUCT.to_string()),
        ("X-Plex-Platform", PLEX_PLATFORM.to_string()),
        ("X-Plex-Platform-Version", PLEX_PLATFORM_VERSION.to_string()),
        ("X-Plex-Device-Name", PLEX_PRODUCT.to_string()),
        ("X-Plex-Version", PLEX_VERSION.to_string()),
    ];
    params.extend(extra);
    params
}

fn plex_server_query_params() -> Vec<(&'static str, String)> {
    let mut params = plex_query_params(std::iter::empty::<(&str, String)>());
    if let Some(token) = config_value("auth_token") {
        params.push(("X-Plex-Token", token));
    }
    params
}

fn server_options(resource: &serde_json::Value) -> Vec<serde_json::Value> {
    let name = string_member(resource, &["name"]).unwrap_or_else(|| "Plex".to_string());
    resource
        .get("connections")
        .and_then(|connections| connections.as_array())
        .into_iter()
        .flatten()
        .flat_map(|connection| connection_options(&name, connection))
        .collect()
}

fn connection_options(name: &str, connection: &serde_json::Value) -> Vec<serde_json::Value> {
    let uri = string_member(connection, &["uri"]).unwrap_or_default();
    let protocol = string_member(connection, &["protocol"]).unwrap_or_default();
    let address = string_member(connection, &["address"]).unwrap_or_default();
    let port = integer_member(connection, "port").unwrap_or(32400);
    let local = bool_member(connection, "local");
    let is_secure = protocol == "https";
    let host = host_from_uri(&uri).unwrap_or_else(|| address.clone());
    let hint = if is_secure {
        format!("{}, Secure", locality_hint(local))
    } else {
        locality_hint(local).to_string()
    };
    let mut options = vec![serde_json::json!({
        "value": uri,
        "name": format!("{name} ({host})"),
        "hint": hint,
        "additionalProperties": {
            "host": host,
            "port": port,
            "useSsl": is_secure,
        },
    })];

    if is_secure && !address.trim().is_empty() {
        options.push(serde_json::json!({
            "value": format!("http://{address}:{port}"),
            "name": format!("{name} ({address})"),
            "hint": locality_hint(local),
            "additionalProperties": {
                "host": address,
                "port": port,
                "useSsl": false,
            },
        }));
    }

    options
}

fn action_name(request: &serde_json::Value) -> Option<String> {
    string_member(request, &["action", "name", "providerAction"])
}

fn required_action_param(request: &serde_json::Value, key: &str) -> Result<String, Error> {
    action_param(request, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg(format!("QueryParam {key} invalid.")))
}

fn action_param(request: &serde_json::Value, key: &str) -> Option<String> {
    request
        .get("query")
        .and_then(|query| string_member(query, &[key]))
        .or_else(|| {
            request
                .get("query_params")
                .and_then(|query| string_member(query, &[key]))
        })
        .or_else(|| string_member(request, &[key]))
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

fn integer_member(value: &serde_json::Value, key: &str) -> Option<i64> {
    value
        .get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn bool_member(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(|value| {
            value.as_bool().or_else(|| {
                let value = value.as_str()?.to_ascii_lowercase();
                Some(matches!(value.as_str(), "1" | "true" | "yes" | "on"))
            })
        })
        .unwrap_or(false)
}

fn parse_i64(value: &str) -> Result<i64, Error> {
    value
        .parse::<i64>()
        .map_err(|_| Error::msg("QueryParam id invalid."))
}

fn host_from_uri(uri: &str) -> Option<String> {
    let host = uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(uri)
        .split('/')
        .next()?
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or_else(|| {
            uri.split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(uri)
                .split('/')
                .next()
                .unwrap_or(uri)
        })
        .split(':')
        .next()?
        .trim();

    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn locality_hint(local: bool) -> &'static str {
    if local { "Local" } else { "Remote" }
}
