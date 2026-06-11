use extism_pdk::*;
use notify_common::*;

const SENDGRID_BASE_URL: &str = "https://api.sendgrid.com/v3";

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
    let from = required_config("from")?;
    if !valid_email_address(&from) {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "sendgrid from address must be a valid email address",
            Some("invalid_from".to_string()),
        )))?);
    }

    let recipient_addresses = config_csv("recipients");
    if recipient_addresses.is_empty() {
        return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
            "sendgrid recipients is not configured",
            None,
        )))?);
    }
    for recipient in &recipient_addresses {
        if !valid_email_address(recipient) {
            return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                format!("sendgrid recipient must be a valid email address: {recipient}"),
                Some("invalid_recipient".to_string()),
            )))?);
        }
    }

    let recipients = recipient_addresses
        .into_iter()
        .map(|email| serde_json::json!({ "email": email }))
        .collect::<Vec<_>>();
    let body = serde_json::json!({
        "from": { "email": from },
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
        &format!("{SENDGRID_BASE_URL}/mail/send"),
        "POST",
        &headers,
        body,
    );
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn valid_email_address(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }

    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !domain.contains('@')
}
