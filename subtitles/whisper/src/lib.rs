use std::fs;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use extism_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, PluginDescriptor, PluginResult,
    ProviderDescriptor, SubtitleCapabilities, SubtitleDescriptor, SubtitlePluginGenerateRequest,
    SubtitlePluginGenerateResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus, SDK_VERSION,
};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_RETRY_AFTER_SECONDS: i64 = 10;

#[derive(Clone)]
struct WhisperConfig {
    api_key: String,
    model: String,
    prompt: Option<String>,
}

struct MultipartBody {
    content_type: String,
    body: Vec<u8>,
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    let response = match WhisperConfig::from_extism() {
        Ok(config) => match validate_api_key(&config) {
            Ok(()) => SubtitlePluginValidateConfigResponse {
                status: SubtitleValidateConfigStatus::Valid,
                message: None,
                retry_after_seconds: None,
            },
            Err(error) => validation_error_response(&error),
        },
        Err(error) => SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error),
            retry_after_seconds: None,
        },
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subtitle_generate(input: String) -> FnResult<String> {
    let request: SubtitlePluginGenerateRequest = serde_json::from_str(&input)?;
    let config = WhisperConfig::from_extism().map_err(Error::msg)?;
    let response = generate_subtitle_impl(&config, &request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

impl WhisperConfig {
    fn from_extism() -> Result<Self, String> {
        Ok(Self {
            api_key: config_required_string("api_key")?,
            model: config_string_with_default("model", "whisper-1")?,
            prompt: config_optional_string("prompt")?,
        })
    }
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "whisper".to_string(),
        name: "Whisper".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: "whisper".to_string(),
            provider_aliases: Vec::new(),
            config_fields: vec![
                ConfigFieldDef {
                    key: "api_key".to_string(),
                    label: "API Key".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    default_value: None,
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: Vec::new(),
                    help_text: Some(
                        "OpenAI API key used for Whisper transcription requests.".to_string(),
                    ),
                },
                ConfigFieldDef {
                    key: "model".to_string(),
                    label: "Model".to_string(),
                    field_type: ConfigFieldType::String,
                    required: true,
                    default_value: Some("whisper-1".to_string()),
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: Vec::new(),
                    help_text: Some("Transcription model to use.".to_string()),
                },
                ConfigFieldDef {
                    key: "prompt".to_string(),
                    label: "Prompt".to_string(),
                    field_type: ConfigFieldType::Multiline,
                    required: false,
                    default_value: None,
                    value_source: ConfigFieldValueSource::User,
                    host_binding: None,
                    options: Vec::new(),
                    help_text: Some(
                        "Optional prompt to improve terminology or formatting for the transcription."
                            .to_string(),
                    ),
                },
            ],
            default_base_url: Some(OPENAI_API_BASE.to_string()),
            allowed_hosts: vec!["api.openai.com".to_string()],
            capabilities: SubtitleCapabilities {
            mode: SubtitleProviderMode::Generator,
            supported_media_kinds: vec![
                SubtitleQueryMediaKind::Movie,
                SubtitleQueryMediaKind::Episode,
            ],
            recommended_facets: vec![
                "movie".to_string(),
                "series".to_string(),
                "anime".to_string(),
            ],
            supports_hash_lookup: false,
            supports_forced: false,
            supports_hearing_impaired: false,
            supports_ai_translated: false,
            supports_machine_translated: false,
            supported_languages: Vec::new(),
            },
        }),
    }
}

fn validate_api_key(config: &WhisperConfig) -> Result<(), String> {
    let request = HttpRequest::new(format!("{OPENAI_API_BASE}/models"))
        .with_method("GET")
        .with_header("Authorization", format!("Bearer {}", config.api_key));
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| format!("Whisper validation request failed: {error}"))?;
    if response.status_code() >= 400 {
        return Err(http_error("validate", &response));
    }
    Ok(())
}

fn generate_subtitle_impl(
    config: &WhisperConfig,
    request: &SubtitlePluginGenerateRequest,
) -> Result<SubtitlePluginGenerateResponse, String> {
    let input_path = request.input.path.to_string_lossy().to_string();
    let audio_bytes = fs::read(&request.input.path).map_err(|error| {
        format!(
            "failed to read staged generator input '{}': {error}",
            input_path
        )
    })?;
    let file_name = request
        .input
        .path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("audio.flac");
    let multipart = build_transcription_multipart_body(
        file_name,
        &request.input.mime_type,
        &audio_bytes,
        config,
    );

    let request = HttpRequest::new(format!("{OPENAI_API_BASE}/audio/transcriptions"))
        .with_method("POST")
        .with_header("Authorization", format!("Bearer {}", config.api_key))
        .with_header("Content-Type", multipart.content_type.as_str());
    let response = http::request::<Vec<u8>>(&request, Some(multipart.body))
        .map_err(|error| format!("Whisper transcription request failed: {error}"))?;
    if response.status_code() >= 400 {
        return Err(http_error("generate subtitle", &response));
    }

    Ok(SubtitlePluginGenerateResponse {
        content_base64: BASE64.encode(response.body()),
        format: "srt".to_string(),
    })
}

fn build_transcription_multipart_body(
    file_name: &str,
    mime_type: &str,
    audio_bytes: &[u8],
    config: &WhisperConfig,
) -> MultipartBody {
    let boundary = "----scryer-whisper-boundary";
    let mut body = Vec::new();

    append_multipart_text(&mut body, boundary, "model", Some(config.model.as_str()));
    append_multipart_text(&mut body, boundary, "response_format", Some("srt"));
    append_multipart_text(&mut body, boundary, "prompt", config.prompt.as_deref());

    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n",
            escape_quotes(file_name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(audio_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    MultipartBody {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
    }
}

fn append_multipart_text(body: &mut Vec<u8>, boundary: &str, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", key).as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
}

fn validation_error_response(error: &str) -> SubtitlePluginValidateConfigResponse {
    if error.contains("authentication failed") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::AuthFailed,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else if error.contains("rate limited") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::RateLimited,
            message: Some(error.to_string()),
            retry_after_seconds: parse_retry_after_seconds(error),
        }
    } else if error.contains("required") || error.contains("missing") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::InvalidConfig,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else if error.contains("request failed") {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Unreachable,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    } else {
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Unsupported,
            message: Some(error.to_string()),
            retry_after_seconds: None,
        }
    }
}

fn http_error(action: &str, response: &HttpResponse) -> String {
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    match status {
        401 | 403 => "Whisper authentication failed".to_string(),
        429 => {
            let retry_after = retry_after_seconds(response).unwrap_or(DEFAULT_RETRY_AFTER_SECONDS);
            format!("Whisper rate limited — retry after {retry_after}s")
        }
        500..=599 => format!(
            "Whisper {action} failed with HTTP {status}: {}",
            compact_error_body(&body)
        ),
        _ => format!(
            "Whisper {action} returned HTTP {status}: {}",
            compact_error_body(&body)
        ),
    }
}

fn retry_after_seconds(response: &HttpResponse) -> Option<i64> {
    response
        .headers()
        .get("retry-after")
        .or_else(|| response.headers().get("x-retry-after"))
        .and_then(|value| value.parse::<i64>().ok())
}

fn parse_retry_after_seconds(error: &str) -> Option<i64> {
    let marker = "retry after ";
    let (_, tail) = error.split_once(marker)?;
    tail.trim_end_matches('s').trim().parse::<i64>().ok()
}

fn compact_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response".to_string()
    } else if trimmed.len() > 240 {
        format!("{}...", &trimmed[..240])
    } else {
        trimmed.to_string()
    }
}

fn escape_quotes(value: &str) -> String {
    value.replace('"', "")
}

fn config_required_string(key: &str) -> Result<String, String> {
    let value = config::get(key)
        .map_err(|error| format!("failed to read config field '{key}': {error}"))?
        .unwrap_or_default()
        .trim()
        .to_string();
    if value.is_empty() {
        Err(format!("missing required config field '{key}'"))
    } else {
        Ok(value)
    }
}

fn config_optional_string(key: &str) -> Result<Option<String>, String> {
    Ok(config::get(key)
        .map_err(|error| format!("failed to read config field '{key}': {error}"))?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn config_string_with_default(key: &str, default: &str) -> Result<String, String> {
    Ok(config_optional_string(key)?.unwrap_or_else(|| default.to_string()))
}
