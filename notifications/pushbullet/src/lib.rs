use extism_pdk::*;
use notify_common::*;

const PUSHBULLET_URL: &str = "https://api.pushbullet.com/v2/pushes";
const PUSHBULLET_DEVICES_URL: &str = "https://api.pushbullet.com/v2/devices";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "pushbullet",
        "Pushbullet",
        env!("CARGO_PKG_VERSION"),
        "pushbullet",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.pushbullet.com"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "api_key",
            "Access Token",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "device_ids",
            "Device IDs",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma, semicolon, or newline separated Pushbullet device identifiers."),
        ),
        field(
            "channel_tags",
            "Channel Tags",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma, semicolon, or newline separated Pushbullet channel tags."),
        ),
        field(
            "sender_id",
            "Sender ID",
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
    let targets = push_targets();
    let mut responses = Vec::new();
    for target in targets {
        let (target_key, target_value) = target;
        let mut params = vec![
            ("type".to_string(), "note".to_string()),
            ("title".to_string(), req.summary_title.clone()),
            ("body".to_string(), req.summary_message.clone()),
        ];
        if let Some(value) = target_value {
            params.push((target_key.to_string(), value));
        }
        if let Some(sender_id) = config_value("sender_id") {
            params.push(("source_device_iden".to_string(), sender_id));
        }
        let headers = [(
            "Authorization",
            basic_auth_header(&required_config("api_key")?, ""),
        )];
        responses.push(send_form(PUSHBULLET_URL, "POST", &headers, &params));
    }
    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
}

#[plugin_fn]
pub fn scryer_notification_action(input: String) -> FnResult<String> {
    let request: serde_json::Value = serde_json::from_str(&input)?;
    let response = match action_name(&request).as_deref() {
        Some("getDevices") => get_devices()?,
        _ => serde_json::json!({}),
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn get_devices() -> Result<serde_json::Value, Error> {
    let Some(api_key) = config_value("api_key") else {
        return Ok(serde_json::json!({ "devices": [] }));
    };
    let request = HttpRequest::new(PUSHBULLET_DEVICES_URL)
        .with_method("GET")
        .with_header("User-Agent", "scryer-pushbullet-plugin/0.1")
        .with_header("Authorization", basic_auth_header(&api_key, ""));
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Pushbullet devices request failed: HTTP {status}: {body}"
        )));
    }

    let body: serde_json::Value = serde_json::from_slice(&response.body())?;
    let mut options = body
        .get("devices")
        .and_then(|devices| devices.as_array())
        .into_iter()
        .flatten()
        .filter_map(|device| {
            let id = string_member(device, &["iden", "Iden", "id", "Id"])?;
            let name = string_member(device, &["nickname", "Nickname"])?;
            if name.trim().is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "id": id,
                "name": name,
            }))
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        string_member(left, &["name"])
            .unwrap_or_default()
            .to_ascii_lowercase()
            .cmp(
                &string_member(right, &["name"])
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
            )
    });

    Ok(serde_json::json!({
        "options": options,
    }))
}

fn push_targets() -> Vec<(&'static str, Option<String>)> {
    let channels = config_csv("channel_tags");
    if !channels.is_empty() {
        return channels
            .into_iter()
            .map(|channel| ("channel_tag", Some(channel)))
            .collect();
    }

    let devices = config_csv("device_ids");
    if !devices.is_empty() {
        return devices
            .into_iter()
            .map(|device| {
                if device.parse::<i64>().is_ok() {
                    ("device_id", Some(device))
                } else {
                    ("device_iden", Some(device))
                }
            })
            .collect();
    }

    vec![("device_iden", None)]
}

fn action_name(request: &serde_json::Value) -> Option<String> {
    string_member(request, &["action", "name", "providerAction"])
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
