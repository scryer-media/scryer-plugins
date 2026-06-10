use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "mailgun",
        "Mailgun",
        env!("CARGO_PKG_VERSION"),
        "mailgun",
        vec![NotificationDeliveryMode::Email],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.mailgun.net", "api.eu.mailgun.net"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "use_eu_endpoint",
            "Use EU Endpoint",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "from",
            "From Address",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "sender_domain",
            "Sender Domain",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "recipients",
            "Recipients",
            ConfigFieldType::String,
            true,
            None,
            Some("Comma, semicolon, or newline separated recipient email addresses."),
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let (title, message) = title_and_body(&req);
    let base_url = if config_bool("use_eu_endpoint") {
        "https://api.eu.mailgun.net/v3"
    } else {
        "https://api.mailgun.net/v3"
    };
    let domain = required_config("sender_domain")?;
    let mut params = vec![
        ("from".to_string(), required_config("from")?),
        ("subject".to_string(), title),
        ("text".to_string(), message),
    ];
    for recipient in config_csv("recipients") {
        params.push(("to".to_string(), recipient));
    }
    let headers = [(
        "Authorization",
        basic_auth_header("api", &required_config("api_key")?),
    )];
    let response = send_form(
        &format!("{}/{}/messages", base_url, domain.trim_matches('/')),
        "POST",
        &headers,
        &params,
    );
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
