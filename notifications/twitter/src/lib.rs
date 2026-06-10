use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use hmac::{Hmac, Mac};
use notify_common::*;
use sha1::Sha1;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

const UPDATE_STATUS_URL: &str = "https://api.twitter.com/1.1/statuses/update.json";
const DIRECT_MESSAGE_URL: &str = "https://api.twitter.com/1.1/direct_messages/new.json";
const REQUEST_TOKEN_URL: &str = "https://api.twitter.com/oauth/request_token";
const ACCESS_TOKEN_URL: &str = "https://api.twitter.com/oauth/access_token";
const AUTHORIZE_URL: &str = "https://api.twitter.com/oauth/authorize";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "twitter",
        "Twitter",
        env!("CARGO_PKG_VERSION"),
        "twitter",
        vec![
            NotificationDeliveryMode::Push,
            NotificationDeliveryMode::Chat,
        ],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.twitter.com"]);
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "consumer_key",
            "Consumer Key",
            ConfigFieldType::Password,
            true,
            None,
            Some("Twitter application consumer key."),
        ),
        field(
            "consumer_secret",
            "Consumer Secret",
            ConfigFieldType::Password,
            true,
            None,
            Some("Twitter application consumer secret."),
        ),
        field(
            "access_token",
            "Access Token",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "access_token_secret",
            "Access Token Secret",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "mention",
            "Mention",
            ConfigFieldType::String,
            false,
            None,
            Some("Twitter username used for direct messages or appended to public status updates."),
        ),
        field(
            "direct_message",
            "Direct Message",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            Some("Send direct messages instead of public status updates."),
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    let settings = TwitterSettings::from_config()?;
    let mut message = twitter_message(&req);
    let response = if settings.direct_message {
        let mention = required_config("mention")?;
        signed_post(
            DIRECT_MESSAGE_URL,
            &settings,
            &[
                ("text".to_string(), rfc3986_encode(&message)),
                ("screenname".to_string(), rfc3986_encode(&mention)),
            ],
        )
    } else {
        if let Some(mention) = config_value("mention") {
            message.push_str(" @");
            message.push_str(&mention);
        }

        signed_post(
            UPDATE_STATUS_URL,
            &settings,
            &[("status".to_string(), rfc3986_encode(&message))],
        )
    };

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

struct TwitterSettings {
    consumer_key: String,
    consumer_secret: String,
    access_token: String,
    access_token_secret: String,
    direct_message: bool,
}

impl TwitterSettings {
    fn from_config() -> Result<Self, Error> {
        Ok(Self {
            consumer_key: required_config("consumer_key")?,
            consumer_secret: required_config("consumer_secret")?,
            access_token: required_config("access_token")?,
            access_token_secret: required_config("access_token_secret")?,
            direct_message: config_bool("direct_message"),
        })
    }
}

struct TwitterConsumerSettings {
    consumer_key: String,
    consumer_secret: String,
}

impl TwitterConsumerSettings {
    fn from_config() -> Result<Self, Error> {
        Ok(Self {
            consumer_key: required_config("consumer_key")?,
            consumer_secret: required_config("consumer_secret")?,
        })
    }
}

fn twitter_message(req: &PluginNotificationRequest) -> String {
    let title = notification_title(req);
    let body = notification_body(req);
    match (title.is_empty(), body.is_empty()) {
        (true, true) => "Scryer notification".to_string(),
        (true, false) => body,
        (false, true) => title,
        (false, false) => format!("{title}: {body}"),
    }
}

fn signed_post(
    url: &str,
    settings: &TwitterSettings,
    body_params: &[(String, String)],
) -> PluginNotificationResponse {
    let oauth_params = oauth_parameters(settings);
    let signature = oauth_signature(
        "POST",
        url,
        &oauth_params,
        body_params,
        &settings.consumer_secret,
        &settings.access_token_secret,
    );
    let mut signed_oauth_params = oauth_params;
    signed_oauth_params.push(("oauth_signature".to_string(), signature));
    let headers = vec![
        ("Authorization", authorization_header(&signed_oauth_params)),
        (
            "Content-Type",
            "application/x-www-form-urlencoded".to_string(),
        ),
    ];
    send_bytes(url, "POST", &headers, encoded_form_body(body_params))
}

fn start_oauth(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let settings = TwitterConsumerSettings::from_config()?;
    let callback_url = required_action_param(request, "callbackUrl")?;
    let oauth_params = oauth_request_token_parameters(&settings, callback_url);
    let body = signed_oauth_get(
        REQUEST_TOKEN_URL,
        &oauth_params,
        &settings.consumer_secret,
        "",
    )?;
    let values = parse_form_response(&body);
    let oauth_token = values
        .get("oauth_token")
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg("Twitter request-token response did not include oauth_token"))?;

    Ok(serde_json::json!({
        "oauthUrl": format!("{AUTHORIZE_URL}?oauth_token={}", rfc3986_encode(&oauth_token)),
    }))
}

fn get_oauth_token(request: &serde_json::Value) -> Result<serde_json::Value, Error> {
    let settings = TwitterConsumerSettings::from_config()?;
    let oauth_token = required_action_param(request, "oauth_token")?;
    let oauth_verifier = required_action_param(request, "oauth_verifier")?;
    let oauth_params = oauth_access_token_parameters(&settings, oauth_token, oauth_verifier);
    let body = signed_oauth_get(
        ACCESS_TOKEN_URL,
        &oauth_params,
        &settings.consumer_secret,
        "",
    )?;
    let values = parse_form_response(&body);
    let access_token = values
        .get("oauth_token")
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg("Twitter access-token response did not include oauth_token"))?;
    let access_token_secret = values
        .get("oauth_token_secret")
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::msg("Twitter access-token response did not include oauth_token_secret")
        })?;

    Ok(serde_json::json!({
        "accessToken": access_token,
        "accessTokenSecret": access_token_secret,
    }))
}

fn signed_oauth_get(
    url: &str,
    oauth_params: &[(String, String)],
    consumer_secret: &str,
    token_secret: &str,
) -> Result<Vec<u8>, Error> {
    let signature = oauth_signature("GET", url, oauth_params, &[], consumer_secret, token_secret);
    let mut signed_oauth_params = oauth_params.to_vec();
    signed_oauth_params.push(("oauth_signature".to_string(), signature));
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-twitter-plugin/0.1")
        .with_header("Authorization", authorization_header(&signed_oauth_params));
    let response = http::request::<Vec<u8>>(&request, None)?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "Twitter OAuth request failed: HTTP {status}: {body}"
        )));
    }

    Ok(response.body())
}

fn oauth_parameters(settings: &TwitterSettings) -> Vec<(String, String)> {
    vec![
        (
            "oauth_consumer_key".to_string(),
            settings.consumer_key.clone(),
        ),
        ("oauth_nonce".to_string(), oauth_nonce()),
        (
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        ),
        ("oauth_timestamp".to_string(), oauth_timestamp()),
        ("oauth_token".to_string(), settings.access_token.clone()),
        ("oauth_version".to_string(), "1.0".to_string()),
    ]
}

fn oauth_base_parameters(consumer_key: &str) -> Vec<(String, String)> {
    vec![
        ("oauth_consumer_key".to_string(), consumer_key.to_string()),
        ("oauth_nonce".to_string(), oauth_nonce()),
        (
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        ),
        ("oauth_timestamp".to_string(), oauth_timestamp()),
        ("oauth_version".to_string(), "1.0".to_string()),
    ]
}

fn oauth_request_token_parameters(
    settings: &TwitterConsumerSettings,
    callback_url: String,
) -> Vec<(String, String)> {
    let mut params = oauth_base_parameters(&settings.consumer_key);
    params.push(("oauth_callback".to_string(), callback_url));
    params
}

fn oauth_access_token_parameters(
    settings: &TwitterConsumerSettings,
    oauth_token: String,
    oauth_verifier: String,
) -> Vec<(String, String)> {
    let mut params = oauth_base_parameters(&settings.consumer_key);
    params.push(("oauth_token".to_string(), oauth_token));
    params.push(("oauth_verifier".to_string(), oauth_verifier));
    params
}

fn oauth_signature(
    method: &str,
    url: &str,
    oauth_params: &[(String, String)],
    body_params: &[(String, String)],
    consumer_secret: &str,
    token_secret: &str,
) -> String {
    let mut parameters = Vec::new();
    parameters.extend_from_slice(oauth_params);
    parameters.extend_from_slice(body_params);
    let normalized = normalize_parameters(&parameters);
    let signature_base = format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        rfc3986_encode(&normalized_request_url(url)),
        rfc3986_encode(&normalized),
    );
    let signing_key = format!(
        "{}&{}",
        rfc3986_encode(consumer_secret),
        rfc3986_encode(token_secret),
    );
    let mut mac = HmacSha1::new_from_slice(signing_key.as_bytes())
        .expect("HMAC accepts signing keys of any size");
    mac.update(signature_base.as_bytes());
    rfc3986_encode(&STANDARD.encode(mac.finalize().into_bytes()))
}

fn normalize_parameters(parameters: &[(String, String)]) -> String {
    let mut copy = parameters
        .iter()
        .filter(|(key, _)| key != "oauth_signature")
        .map(|(key, value)| (key.clone(), rfc3986_encode_preserving_percent(value)))
        .collect::<Vec<_>>();
    copy.sort_by(|left, right| match left.0.cmp(&right.0) {
        std::cmp::Ordering::Equal => left.1.cmp(&right.1),
        ordering => ordering,
    });
    copy.into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn authorization_header(parameters: &[(String, String)]) -> String {
    let mut oauth = parameters
        .iter()
        .filter(|(key, value)| key.starts_with("oauth_") && !value.trim().is_empty())
        .collect::<Vec<_>>();
    oauth.sort_by(|left, right| left.0.cmp(&right.0));
    let pairs = oauth
        .into_iter()
        .map(|(key, value)| format!("{key}=\"{}\"", rfc3986_encode_preserving_percent(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("OAuth {pairs}")
}

fn encoded_form_body(params: &[(String, String)]) -> Vec<u8> {
    params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

fn normalized_request_url(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

fn oauth_timestamp() -> String {
    now_since_epoch().as_secs().to_string()
}

fn oauth_nonce() -> String {
    let now = now_since_epoch();
    format!("{:x}{:x}", now.as_secs(), now.subsec_nanos())
}

fn now_since_epoch() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn rfc3986_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn rfc3986_encode_preserving_percent(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'%' => {
                encoded.push(*byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded.replace("%%", "%25%")
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

fn parse_form_response(body: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((form_decode(key), form_decode(value)))
        })
        .collect()
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let Some(byte) = hex_byte(bytes[index + 1], bytes[index + 2]) {
                    decoded.push(byte);
                    index += 3;
                } else {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).to_string()
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_value(high)? * 16 + hex_value(low)?)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
