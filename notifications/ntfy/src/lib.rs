use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
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
    Ok(serde_json::to_string(&descriptor)?)
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

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let server = config_value("server_url").unwrap_or_else(|| "https://ntfy.sh".to_string());
    let topics = config_csv("topics");
    if topics.is_empty() {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "topics is not configured",
            None,
        )))?);
    }

    let (title, message) = title_and_body(&req);
    let priority = config_i64("priority", 3).to_string();
    let tags = config_csv("tags").join(",");
    let click = config_value("click_url");
    let mut headers = configured_headers();
    if let Some(token) = config_value("access_token") {
        headers.push(("Authorization", format!("Bearer {token}")));
    } else if let (Some(username), Some(password)) =
        (config_value("username"), config_value("password"))
    {
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

    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
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

fn leak_header_key(key: &str) -> &'static str {
    Box::leak(key.trim().to_string().into_boxed_str())
}
