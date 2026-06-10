use extism_pdk::*;
use notify_common::*;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "sendgrid",
        "SendGrid",
        env!("CARGO_PKG_VERSION"),
        "sendgrid",
        vec![NotificationDeliveryMode::Email],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.sendgrid.com"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        connection_field(
            "base_url",
            "Base URL",
            false,
            Some("https://api.sendgrid.com/v3/"),
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
            "from",
            "From Address",
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
    let base_url =
        config_value("base_url").unwrap_or_else(|| "https://api.sendgrid.com/v3/".to_string());
    let recipients = config_csv("recipients")
        .into_iter()
        .map(|email| serde_json::json!({ "email": email }))
        .collect::<Vec<_>>();
    let body = serde_json::json!({
        "from": { "email": required_config("from")? },
        "personalizations": [{
            "subject": title,
            "to": recipients,
        }],
        "content": [{
            "type": "text/plain",
            "value": message,
        }],
    });
    let headers = [(
        "Authorization",
        format!("Bearer {}", required_config("api_key")?),
    )];
    let response = send_json(
        &format!("{}/mail/send", base_url.trim_end_matches('/')),
        "POST",
        &headers,
        body,
    );
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}
