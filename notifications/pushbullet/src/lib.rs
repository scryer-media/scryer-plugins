use notify_common::*;

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

const PUSHBULLET_URL: &str = "https://api.pushbullet.com/v2/pushes";
const PUSHBULLET_DEVICES_URL: &str = "https://api.pushbullet.com/v2/devices";

fn build_descriptor() -> PluginDescriptor {
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
    descriptor
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

fn send_notification(req: &PluginNotificationRequest) -> FnResult<PluginNotificationResponse> {
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
    Ok(merge_responses(responses))
}

fn handle_action(action: &PluginActionRequest) -> FnResult<serde_json::Value> {
    let request = action_request_value(action);
    let response = match action_name(&request).as_deref() {
        Some("getDevices") => get_devices()?,
        _ => serde_json::json!({}),
    };

    Ok(response)
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
