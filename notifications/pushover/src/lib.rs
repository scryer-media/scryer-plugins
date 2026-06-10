use aes::Aes256;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use extism_pdk::*;
use flate2::{Compression, write::GzEncoder};
use hmac::{Hmac, Mac};
use notify_common::*;
use sha2::Sha256;
use std::io::Write;

const PUSHOVER_URL: &str = "https://api.pushover.net/1/messages.json";
type HmacSha256 = Hmac<Sha256>;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "pushover",
        "Pushover",
        env!("CARGO_PKG_VERSION"),
        "pushover",
        vec![NotificationDeliveryMode::Push],
        vec![NotificationPayloadFormat::PlainText],
        config_fields(),
        false,
        false,
    );
    add_notification_allowed_hosts(&mut descriptor, &["api.pushover.net"]);
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
            "user_key",
            "User Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "devices",
            "Devices",
            ConfigFieldType::String,
            false,
            None,
            Some("Comma, semicolon, or newline separated Pushover device names."),
        ),
        field(
            "priority",
            "Priority",
            ConfigFieldType::Number,
            false,
            Some("0"),
            None,
        ),
        field(
            "retry",
            "Retry",
            ConfigFieldType::Number,
            false,
            Some("0"),
            None,
        ),
        field(
            "expire",
            "Expire",
            ConfigFieldType::Number,
            false,
            Some("0"),
            None,
        ),
        field(
            "ttl",
            "TTL",
            ConfigFieldType::Number,
            false,
            Some("0"),
            None,
        ),
        field("sound", "Sound", ConfigFieldType::String, false, None, None),
        field(
            "encryption_key",
            "Encryption Key",
            ConfigFieldType::Password,
            false,
            None,
            Some("Sonarr-compatible 64-character hex key for encrypted notifications."),
        ),
    ]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;

    let mut title = req.summary_title;
    let mut message = req.summary_message;
    let encrypted = config_value("encryption_key").is_some_and(|key| !key.trim().is_empty());

    if encrypted {
        let key = match encryption_key() {
            Ok(key) => key,
            Err(message) => {
                return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                    message,
                    Some("invalid_encryption_key".to_string()),
                )))?);
            }
        };

        title = match encrypt_field(&title, &key) {
            Ok(value) => value,
            Err(message) => {
                return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                    message,
                    Some("encryption_failed".to_string()),
                )))?);
            }
        };
        message = match encrypt_field(&message, &key) {
            Ok(value) => value,
            Err(message) => {
                return Ok(serde_json::to_string(&PluginResult::Ok(error_response(
                    message,
                    Some("encryption_failed".to_string()),
                )))?);
            }
        };
    }

    let mut params = vec![
        ("token".to_string(), required_config("api_key")?),
        ("user".to_string(), required_config("user_key")?),
        ("device".to_string(), config_csv("devices").join(",")),
        ("title".to_string(), title),
        ("message".to_string(), message),
        (
            "priority".to_string(),
            config_i64("priority", 0).to_string(),
        ),
    ];
    if encrypted {
        params.push(("encrypted".to_string(), "1".to_string()));
    }
    if config_i64("priority", 0) == 2 {
        params.push(("retry".to_string(), config_i64("retry", 30).to_string()));
        params.push(("expire".to_string(), config_i64("expire", 0).to_string()));
    }
    if config_i64("ttl", 0) > 0 {
        params.push(("ttl".to_string(), config_i64("ttl", 0).to_string()));
    }
    if let Some(sound) = config_value("sound") {
        params.push(("sound".to_string(), sound));
    }
    let response = send_form(PUSHOVER_URL, "POST", &[], &params);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn encryption_key() -> Result<[u8; 32], String> {
    let key = config_value("encryption_key").unwrap_or_default();
    let key = key.trim();
    if key.len() != 64 {
        return Err("pushover encryption_key must be a 64-character hex string".to_string());
    }

    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&key[start..start + 2], 16)
            .map_err(|_| "pushover encryption_key must be a 64-character hex string".to_string())?;
    }

    Ok(bytes)
}

fn encrypt_field(plaintext: &str, key: &[u8; 32]) -> Result<String, String> {
    let compressed = gzip_compress(plaintext.as_bytes())?;
    let mut iv = [0u8; 16];
    getrandom::getrandom(&mut iv)
        .map_err(|err| format!("failed to generate pushover encryption IV: {err}"))?;

    let ciphertext = cbc::Encryptor::<Aes256>::new(key.into(), (&iv).into())
        .encrypt_padded_vec_mut::<Pkcs7>(&compressed);

    let mut iv_and_ciphertext = Vec::with_capacity(iv.len() + ciphertext.len());
    iv_and_ciphertext.extend_from_slice(&iv);
    iv_and_ciphertext.extend_from_slice(&ciphertext);

    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|err| format!("failed to sign pushover payload: {err}"))?;
    mac.update(&iv_and_ciphertext);
    let signature = mac.finalize().into_bytes();

    let mut payload = Vec::with_capacity(iv_and_ciphertext.len() + signature.len());
    payload.extend_from_slice(&iv_and_ciphertext);
    payload.extend_from_slice(&signature);

    Ok(BASE64.encode(payload))
}

fn gzip_compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .map_err(|err| format!("failed to gzip pushover field: {err}"))?;
    encoder
        .finish()
        .map_err(|err| format!("failed to finish pushover gzip field: {err}"))
}
