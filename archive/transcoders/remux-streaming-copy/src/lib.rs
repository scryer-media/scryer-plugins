use extism_pdk::*;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, ConfigFieldValueSource, PluginError,
    PluginErrorCode, PluginResult, SDK_VERSION, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const PLUGIN_ID: &str = "remux-streaming-copy";
const PLUGIN_NAME: &str = "REMUX Streaming Copy";
const PLUGIN_TYPE: &str = "media_transcoder";
const EXPORT_DESCRIBE: &str = "scryer_transcode_describe";
const EXPORT_PLAN: &str = "scryer_transcode_plan";
const EXPORT_RUN: &str = "scryer_transcode_run";
const EXPORT_STATUS: &str = "scryer_transcode_status";
const BACKEND_LABEL: &str = "future-ffmpeg-wasm+x264+x265+rav1e+libaom";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_transcode_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let request: TranscodeValidateConfigRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&validate_config(request))?)
}

#[plugin_fn]
pub fn scryer_transcode_plan(input: String) -> FnResult<String> {
    let request: TranscodePlanRequest = serde_json::from_str(&input)?;
    let response = plan_transcode(request);
    Ok(serde_json::to_string(&response)?)
}

#[plugin_fn]
pub fn scryer_transcode_run(input: String) -> FnResult<String> {
    let _: TranscodeRunRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&unsupported_runtime::<
        TranscodeRunResponse,
    >())?)
}

#[plugin_fn]
pub fn scryer_transcode_status(input: String) -> FnResult<String> {
    let _: TranscodeStatusRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&unsupported_runtime::<
        TranscodeStatusResponse,
    >())?)
}

fn descriptor() -> FutureTranscoderDescriptor {
    FutureTranscoderDescriptor {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: Vec::new(),
        plugin_type: PLUGIN_TYPE.to_string(),
        status: "experimental".to_string(),
        exports: vec![
            EXPORT_DESCRIBE.to_string(),
            EXPORT_PLAN.to_string(),
            EXPORT_RUN.to_string(),
            EXPORT_STATUS.to_string(),
        ],
        provider: FutureTranscoderProviderDescriptor {
            provider_type: PLUGIN_ID.to_string(),
            provider_aliases: vec![
                "remux-transcode".to_string(),
                "streaming-copy".to_string(),
                "video-transcode".to_string(),
            ],
            backend: BACKEND_LABEL.to_string(),
            config_fields: config_fields(),
            capabilities: TranscoderCapabilities {
                source_media_kinds: vec![
                    "movie".to_string(),
                    "episode".to_string(),
                    "anime".to_string(),
                    "video_file".to_string(),
                ],
                source_containers: vec![
                    "mkv".to_string(),
                    "mp4".to_string(),
                    "m2ts".to_string(),
                    "ts".to_string(),
                    "mov".to_string(),
                    "avi".to_string(),
                    "vob".to_string(),
                ],
                target_containers: TargetContainer::all()
                    .iter()
                    .map(|container| container.as_str().to_string())
                    .collect(),
                target_codecs: TargetVideoCodec::all()
                    .iter()
                    .map(|codec| codec.as_str().to_string())
                    .collect(),
                audio_modes: AudioMode::all()
                    .iter()
                    .map(|mode| mode.as_str().to_string())
                    .collect(),
                subtitle_modes: SubtitleMode::all()
                    .iter()
                    .map(|mode| mode.as_str().to_string())
                    .collect(),
                supports_chapter_copy: true,
                supports_hdr_preservation: true,
                supports_tonemap_to_sdr: true,
                supports_multi_audio: true,
                supports_progress: true,
                host_requirements: HostRequirements {
                    wasm_features: vec![
                        "simd128".to_string(),
                        "relaxed-simd".to_string(),
                        "exception-handling".to_string(),
                    ],
                    host_capabilities: vec![
                        "media_io.read_stream".to_string(),
                        "media_io.write_stream".to_string(),
                        "media_io.scratch_space".to_string(),
                        "job.progress_events".to_string(),
                        "job.cancellation".to_string(),
                    ],
                },
                encoder_backends: vec![
                    EncoderBackend {
                        codec: TargetVideoCodec::Av1,
                        implementation: "rav1e".to_string(),
                        status: "planned".to_string(),
                        required_features: vec!["simd128".to_string(), "relaxed-simd".to_string()],
                        notes: Some(
                            "Cargo-native AV1 path for reproducible CI builds; quality knobs are intentionally conservative."
                                .to_string(),
                        ),
                    },
                    EncoderBackend {
                        codec: TargetVideoCodec::Av1,
                        implementation: "libaom".to_string(),
                        status: "planned".to_string(),
                        required_features: vec![
                            "simd128".to_string(),
                            "relaxed-simd".to_string(),
                            "exception-handling".to_string(),
                        ],
                        notes: Some(
                            "High-quality AV1 path once Scryer enables modern WASM exception handling for SJLJ."
                                .to_string(),
                        ),
                    },
                    EncoderBackend {
                        codec: TargetVideoCodec::H264,
                        implementation: "x264".to_string(),
                        status: "planned".to_string(),
                        required_features: vec!["simd128".to_string(), "relaxed-simd".to_string()],
                        notes: Some("Required compatibility target.".to_string()),
                    },
                    EncoderBackend {
                        codec: TargetVideoCodec::Hevc,
                        implementation: "x265".to_string(),
                        status: "planned".to_string(),
                        required_features: vec!["simd128".to_string(), "relaxed-simd".to_string()],
                        notes: Some(
                            "HEVC remains experimental because WASI C++ and 10-bit x265 builds need pinned patches."
                                .to_string(),
                        ),
                    },
                ],
            },
        },
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        select_field(
            "target_codec",
            "Target codec",
            true,
            Some(TargetVideoCodec::Av1.as_str()),
            TargetVideoCodec::all()
                .iter()
                .map(|codec| option(codec.as_str(), codec.label()))
                .collect(),
            Some("Default AV1 is intended for high-quality smaller streaming copies."),
        ),
        select_field(
            "container",
            "Container",
            true,
            Some(TargetContainer::Matroska.as_str()),
            TargetContainer::all()
                .iter()
                .map(|container| option(container.as_str(), container.label()))
                .collect(),
            None,
        ),
        select_field(
            "quality_mode",
            "Quality mode",
            true,
            Some(QualityMode::Archival.as_str()),
            QualityMode::all()
                .iter()
                .map(|mode| option(mode.as_str(), mode.label()))
                .collect(),
            Some(
                "Archival keeps high-quality REMUX sources visually transparent first, size second.",
            ),
        ),
        number_field(
            "crf",
            "CRF",
            false,
            None,
            Some("Leave blank to use the plugin default for the selected codec and quality mode."),
        ),
        string_field(
            "encoder_preset",
            "Encoder preset",
            false,
            Some("slow"),
            Some(
                "Maps to x264/x265 preset, rav1e speed, or libaom cpu-used depending on target codec.",
            ),
        ),
        number_field(
            "max_width",
            "Max width",
            false,
            None,
            Some("Optional downscale width. Blank preserves source dimensions."),
        ),
        number_field(
            "max_height",
            "Max height",
            false,
            None,
            Some("Optional downscale height. Blank preserves source dimensions."),
        ),
        select_field(
            "audio_mode",
            "Audio mode",
            true,
            Some(AudioMode::Compatibility.as_str()),
            AudioMode::all()
                .iter()
                .map(|mode| option(mode.as_str(), mode.label()))
                .collect(),
            None,
        ),
        select_field(
            "subtitle_mode",
            "Subtitle mode",
            true,
            Some(SubtitleMode::CopyText.as_str()),
            SubtitleMode::all()
                .iter()
                .map(|mode| option(mode.as_str(), mode.label()))
                .collect(),
            None,
        ),
        bool_field(
            "preserve_hdr",
            "Preserve HDR metadata",
            false,
            Some("true"),
            Some("Keeps HDR signaling when the selected encoder/container path supports it."),
        ),
        bool_field("copy_chapters", "Copy chapters", false, Some("true"), None),
    ]
}

fn validate_config(
    request: TranscodeValidateConfigRequest,
) -> PluginResult<TranscodeValidateConfigResponse> {
    match options_from_config(&request.config, None) {
        Ok(options) => PluginResult::Ok(TranscodeValidateConfigResponse {
            status: "valid".to_string(),
            message: Some(format!(
                "configured {} {} streaming-copy profile",
                options.quality_mode.label(),
                options.target_codec.label()
            )),
            warnings: validation_warnings(&options),
        }),
        Err(message) => PluginResult::Err(PluginError {
            code: PluginErrorCode::InvalidConfig,
            public_message: message,
            debug_message: None,
            retry_after_seconds: None,
        }),
    }
}

fn plan_transcode(request: TranscodePlanRequest) -> PluginResult<TranscodePlanResponse> {
    let options = match options_from_config(&request.config, request.options) {
        Ok(options) => options,
        Err(message) => {
            return PluginResult::Err(PluginError {
                code: PluginErrorCode::InvalidConfig,
                public_message: message,
                debug_message: None,
                retry_after_seconds: None,
            });
        }
    };
    let encoder = encoder_for(options.target_codec, options.quality_mode);
    let video_args = video_args(&options, &encoder);
    let audio_args = audio_args(options.audio_mode);
    let subtitle_args = subtitle_args(options.subtitle_mode);
    let filter_args = filter_args(&options);
    let container = options.container;
    let output_extension = container.extension();
    let output_path = request
        .output
        .path
        .unwrap_or_else(|| derive_output_path(&request.input.path, output_extension));
    let mut warnings = validation_warnings(&options);
    if options.target_codec == TargetVideoCodec::Hevc {
        warnings
            .push("HEVC is planned but remains the least portable WASM encoder path.".to_string());
    }
    if options.target_codec == TargetVideoCodec::Av1 && encoder.implementation == "rav1e" {
        warnings.push(
            "rav1e keeps the build Cargo-native but does not expose every libaom archival-quality knob."
                .to_string(),
        );
    }

    PluginResult::Ok(TranscodePlanResponse {
        backend: BACKEND_LABEL.to_string(),
        input_path: request.input.path,
        output_path,
        target_codec: options.target_codec,
        container,
        encoder,
        ffmpeg_args: FfmpegPlan {
            global: vec!["-hide_banner".to_string(), "-nostdin".to_string()],
            input: vec!["-i".to_string(), "$SCRYER_INPUT".to_string()],
            filters: filter_args,
            video: video_args,
            audio: audio_args,
            subtitles: subtitle_args,
            muxer: muxer_args(container, &options),
            output: vec!["$SCRYER_OUTPUT".to_string()],
        },
        estimated_passes: 1,
        requires_future_host_capabilities: descriptor().provider.capabilities.host_requirements,
        warnings,
    })
}

fn options_from_config(
    config: &BTreeMap<String, String>,
    request_options: Option<TranscodeOptions>,
) -> Result<TranscodeOptions, String> {
    let mut options = TranscodeOptions {
        target_codec: parse_config_enum(config, "target_codec", TargetVideoCodec::Av1)?,
        container: parse_config_enum(config, "container", TargetContainer::Matroska)?,
        quality_mode: parse_config_enum(config, "quality_mode", QualityMode::Archival)?,
        crf: parse_optional_u8(config, "crf")?,
        encoder_preset: config
            .get("encoder_preset")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "slow".to_string()),
        max_width: parse_optional_u32(config, "max_width")?,
        max_height: parse_optional_u32(config, "max_height")?,
        audio_mode: parse_config_enum(config, "audio_mode", AudioMode::Compatibility)?,
        subtitle_mode: parse_config_enum(config, "subtitle_mode", SubtitleMode::CopyText)?,
        preserve_hdr: parse_optional_bool(config, "preserve_hdr")?.unwrap_or(true),
        copy_chapters: parse_optional_bool(config, "copy_chapters")?.unwrap_or(true),
    };

    if let Some(overrides) = request_options {
        options = options.merge(overrides);
    }
    options.validate()?;
    Ok(options)
}

fn video_args(options: &TranscodeOptions, encoder: &EncoderSelection) -> Vec<String> {
    let mut args = match options.target_codec {
        TargetVideoCodec::Av1 if encoder.implementation == "libaom" => vec![
            "-c:v".to_string(),
            "libaom-av1".to_string(),
            "-crf".to_string(),
            options.effective_crf().to_string(),
            "-b:v".to_string(),
            "0".to_string(),
            "-cpu-used".to_string(),
            libaom_cpu_used(&options.encoder_preset).to_string(),
        ],
        TargetVideoCodec::Av1 => vec![
            "-c:v".to_string(),
            "librav1e".to_string(),
            "-qp".to_string(),
            rav1e_qp(options.effective_crf()).to_string(),
            "-speed".to_string(),
            rav1e_speed(&options.encoder_preset).to_string(),
        ],
        TargetVideoCodec::H264 => vec![
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            options.encoder_preset.clone(),
            "-crf".to_string(),
            options.effective_crf().to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ],
        TargetVideoCodec::Hevc => vec![
            "-c:v".to_string(),
            "libx265".to_string(),
            "-preset".to_string(),
            options.encoder_preset.clone(),
            "-crf".to_string(),
            options.effective_crf().to_string(),
        ],
    };
    if options.preserve_hdr
        && matches!(
            options.target_codec,
            TargetVideoCodec::Av1 | TargetVideoCodec::Hevc
        )
    {
        args.push("-color_primaries".to_string());
        args.push("copy".to_string());
        args.push("-color_trc".to_string());
        args.push("copy".to_string());
        args.push("-colorspace".to_string());
        args.push("copy".to_string());
    }
    args
}

fn audio_args(mode: AudioMode) -> Vec<String> {
    match mode {
        AudioMode::Copy => vec!["-c:a".to_string(), "copy".to_string()],
        AudioMode::Compatibility => vec![
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "384k".to_string(),
        ],
        AudioMode::Opus => vec![
            "-c:a".to_string(),
            "libopus".to_string(),
            "-b:a".to_string(),
            "256k".to_string(),
        ],
    }
}

fn subtitle_args(mode: SubtitleMode) -> Vec<String> {
    match mode {
        SubtitleMode::Copy => vec!["-c:s".to_string(), "copy".to_string()],
        SubtitleMode::CopyText => vec!["-c:s".to_string(), "mov_text".to_string()],
        SubtitleMode::Discard => vec!["-sn".to_string()],
    }
}

fn filter_args(options: &TranscodeOptions) -> Vec<String> {
    let Some((width, height)) = options.max_dimensions() else {
        return Vec::new();
    };
    vec![
        "-vf".to_string(),
        format!("scale='min(iw,{width})':'min(ih,{height})':force_original_aspect_ratio=decrease"),
    ]
}

fn muxer_args(container: TargetContainer, options: &TranscodeOptions) -> Vec<String> {
    let mut args = match container {
        TargetContainer::Matroska => vec!["-f".to_string(), "matroska".to_string()],
        TargetContainer::Mp4 => vec![
            "-f".to_string(),
            "mp4".to_string(),
            "-movflags".to_string(),
            "+faststart".to_string(),
        ],
    };
    if options.copy_chapters {
        args.push("-map_chapters".to_string());
        args.push("0".to_string());
    }
    args
}

fn encoder_for(codec: TargetVideoCodec, quality_mode: QualityMode) -> EncoderSelection {
    let implementation = match (codec, quality_mode) {
        (TargetVideoCodec::Av1, QualityMode::Archival) => "libaom",
        (TargetVideoCodec::Av1, _) => "rav1e",
        (TargetVideoCodec::H264, _) => "x264",
        (TargetVideoCodec::Hevc, _) => "x265",
    };
    EncoderSelection {
        codec,
        implementation: implementation.to_string(),
        quality_mode,
    }
}

fn validation_warnings(options: &TranscodeOptions) -> Vec<String> {
    let mut warnings = Vec::new();
    if options.container == TargetContainer::Mp4 && options.subtitle_mode == SubtitleMode::Copy {
        warnings.push("MP4 cannot carry every subtitle format; use text conversion or MKV for broad subtitle preservation.".to_string());
    }
    if options.target_codec == TargetVideoCodec::H264 && options.preserve_hdr {
        warnings.push("H.264 streaming copies generally require SDR output; HDR preservation is best-effort only.".to_string());
    }
    warnings
}

fn derive_output_path(input_path: &str, extension: &str) -> String {
    let trimmed = input_path.trim_end_matches('/');
    let Some((prefix, _)) = trimmed.rsplit_once('.') else {
        return format!("{trimmed}.streaming-copy.{extension}");
    };
    format!("{prefix}.streaming-copy.{extension}")
}

fn unsupported_runtime<T>() -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: "Scryer does not expose the media transcoder SDK/runtime hooks required by this unpublished plugin yet".to_string(),
        debug_message: Some(
            "required future hooks: media_io.read_stream, media_io.write_stream, media_io.scratch_space, job.progress_events, job.cancellation, and WASM exception-handling feature selection"
                .to_string(),
        ),
        retry_after_seconds: None,
    })
}

fn parse_config_enum<T>(
    config: &BTreeMap<String, String>,
    key: &str,
    default: T,
) -> Result<T, String>
where
    T: ParseConfigValue,
{
    config
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(T::parse_config)
        .transpose()?
        .map_or(Ok(default), Ok)
}

fn parse_optional_u8(config: &BTreeMap<String, String>, key: &str) -> Result<Option<u8>, String> {
    config
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u8>()
                .map_err(|_| format!("`{key}` must be a positive integer"))
        })
        .transpose()
}

fn parse_optional_u32(config: &BTreeMap<String, String>, key: &str) -> Result<Option<u32>, String> {
    config
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| format!("`{key}` must be a positive integer"))
        })
        .transpose()
}

fn parse_optional_bool(
    config: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<bool>, String> {
    config
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(format!("`{key}` must be true or false")),
        })
        .transpose()
}

fn rav1e_qp(crf: u8) -> u8 {
    crf.saturating_add(60).min(180)
}

fn rav1e_speed(preset: &str) -> u8 {
    match preset.trim().to_ascii_lowercase().as_str() {
        "veryslow" | "placebo" => 1,
        "slower" => 2,
        "slow" => 3,
        "medium" => 4,
        "fast" => 6,
        "faster" | "veryfast" => 8,
        _ => 4,
    }
}

fn libaom_cpu_used(preset: &str) -> u8 {
    match preset.trim().to_ascii_lowercase().as_str() {
        "veryslow" | "placebo" => 1,
        "slower" => 2,
        "slow" => 3,
        "medium" => 4,
        "fast" => 5,
        "faster" | "veryfast" => 6,
        _ => 3,
    }
}

fn option(value: &str, label: &str) -> ConfigFieldOption {
    ConfigFieldOption {
        value: value.to_string(),
        label: label.to_string(),
    }
}

fn select_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    options: Vec<ConfigFieldOption>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Select,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options,
        help_text: help_text.map(str::to_string),
    }
}

fn string_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::String,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options: Vec::new(),
        help_text: help_text.map(str::to_string),
    }
}

fn number_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Number,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options: Vec::new(),
        help_text: help_text.map(str::to_string),
    }
}

fn bool_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Bool,
        required,
        default_value: default_value.map(str::to_string),
        value_source: ConfigFieldValueSource::User,
        role: None,
        host_binding: None,
        options: Vec::new(),
        help_text: help_text.map(str::to_string),
    }
}

trait ParseConfigValue: Sized {
    fn parse_config(value: &str) -> Result<Self, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FutureTranscoderDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub sdk_version: String,
    pub sdk_constraint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub socket_permissions: Vec<String>,
    pub plugin_type: String,
    pub status: String,
    pub exports: Vec<String>,
    pub provider: FutureTranscoderProviderDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FutureTranscoderProviderDescriptor {
    pub provider_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_aliases: Vec<String>,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_fields: Vec<ConfigFieldDef>,
    pub capabilities: TranscoderCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscoderCapabilities {
    pub source_media_kinds: Vec<String>,
    pub source_containers: Vec<String>,
    pub target_containers: Vec<String>,
    pub target_codecs: Vec<String>,
    pub audio_modes: Vec<String>,
    pub subtitle_modes: Vec<String>,
    pub supports_chapter_copy: bool,
    pub supports_hdr_preservation: bool,
    pub supports_tonemap_to_sdr: bool,
    pub supports_multi_audio: bool,
    pub supports_progress: bool,
    pub host_requirements: HostRequirements,
    pub encoder_backends: Vec<EncoderBackend>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostRequirements {
    pub wasm_features: Vec<String>,
    pub host_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncoderBackend {
    pub codec: TargetVideoCodec,
    pub implementation: String,
    pub status: String,
    pub required_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeValidateConfigRequest {
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodeValidateConfigResponse {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscodePlanRequest {
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    pub input: TranscodeInputRef,
    #[serde(default)]
    pub output: TranscodeOutputRef,
    #[serde(default)]
    pub options: Option<TranscodeOptions>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeRunRequest {
    pub job_id: String,
    #[serde(flatten)]
    pub plan_request: TranscodePlanRequest,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeStatusRequest {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodeRunResponse {
    pub job_id: String,
    pub output_path: String,
    pub state: TranscodeJobState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodeStatusResponse {
    pub job_id: String,
    pub state: TranscodeJobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TranscodeProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodePlanResponse {
    pub backend: String,
    pub input_path: String,
    pub output_path: String,
    pub target_codec: TargetVideoCodec,
    pub container: TargetContainer,
    pub encoder: EncoderSelection,
    pub ffmpeg_args: FfmpegPlan,
    pub estimated_passes: u8,
    pub requires_future_host_capabilities: HostRequirements,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FfmpegPlan {
    pub global: Vec<String>,
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
    pub video: Vec<String>,
    pub audio: Vec<String>,
    pub subtitles: Vec<String>,
    pub muxer: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeInputRef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranscodeOutputRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetVideoCodec {
    Av1,
    H264,
    Hevc,
}

impl TargetVideoCodec {
    fn all() -> &'static [Self] {
        &[Self::Av1, Self::H264, Self::Hevc]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Av1 => "AV1",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

impl ParseConfigValue for TargetVideoCodec {
    fn parse_config(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "av1" => Ok(Self::Av1),
            "h264" | "h.264" | "x264" => Ok(Self::H264),
            "hevc" | "h265" | "h.265" | "x265" => Ok(Self::Hevc),
            _ => Err("`target_codec` must be av1, h264, or hevc".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetContainer {
    Matroska,
    Mp4,
}

impl TargetContainer {
    fn all() -> &'static [Self] {
        &[Self::Matroska, Self::Mp4]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Matroska => "Matroska",
            Self::Mp4 => "MP4",
        }
    }

    fn extension(self) -> &'static str {
        self.as_str()
    }
}

impl ParseConfigValue for TargetContainer {
    fn parse_config(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mkv" | "matroska" => Ok(Self::Matroska),
            "mp4" => Ok(Self::Mp4),
            _ => Err("`container` must be mkv or mp4".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QualityMode {
    Archival,
    Balanced,
    Small,
}

impl QualityMode {
    fn all() -> &'static [Self] {
        &[Self::Archival, Self::Balanced, Self::Small]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Archival => "archival",
            Self::Balanced => "balanced",
            Self::Small => "small",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Archival => "archival",
            Self::Balanced => "balanced",
            Self::Small => "small",
        }
    }
}

impl ParseConfigValue for QualityMode {
    fn parse_config(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "archival" | "transparent" | "remux" => Ok(Self::Archival),
            "balanced" => Ok(Self::Balanced),
            "small" | "compact" => Ok(Self::Small),
            _ => Err("`quality_mode` must be archival, balanced, or small".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    Copy,
    Compatibility,
    Opus,
}

impl AudioMode {
    fn all() -> &'static [Self] {
        &[Self::Copy, Self::Compatibility, Self::Opus]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Compatibility => "compatibility",
            Self::Opus => "opus",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy source audio",
            Self::Compatibility => "AAC compatibility track",
            Self::Opus => "Opus streaming track",
        }
    }
}

impl ParseConfigValue for AudioMode {
    fn parse_config(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "copy" => Ok(Self::Copy),
            "compatibility" | "aac" => Ok(Self::Compatibility),
            "opus" => Ok(Self::Opus),
            _ => Err("`audio_mode` must be copy, compatibility, or opus".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    Copy,
    CopyText,
    Discard,
}

impl SubtitleMode {
    fn all() -> &'static [Self] {
        &[Self::Copy, Self::CopyText, Self::Discard]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::CopyText => "copy_text",
            Self::Discard => "discard",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy subtitles",
            Self::CopyText => "Convert text subtitles",
            Self::Discard => "Discard subtitles",
        }
    }
}

impl ParseConfigValue for SubtitleMode {
    fn parse_config(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "copy" => Ok(Self::Copy),
            "copy_text" | "text" | "convert_text" => Ok(Self::CopyText),
            "discard" | "none" => Ok(Self::Discard),
            _ => Err("`subtitle_mode` must be copy, copy_text, or discard".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodeOptions {
    pub target_codec: TargetVideoCodec,
    pub container: TargetContainer,
    pub quality_mode: QualityMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crf: Option<u8>,
    pub encoder_preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    pub audio_mode: AudioMode,
    pub subtitle_mode: SubtitleMode,
    pub preserve_hdr: bool,
    pub copy_chapters: bool,
}

impl TranscodeOptions {
    fn merge(mut self, overrides: Self) -> Self {
        self.target_codec = overrides.target_codec;
        self.container = overrides.container;
        self.quality_mode = overrides.quality_mode;
        self.crf = overrides.crf.or(self.crf);
        if !overrides.encoder_preset.trim().is_empty() {
            self.encoder_preset = overrides.encoder_preset;
        }
        self.max_width = overrides.max_width.or(self.max_width);
        self.max_height = overrides.max_height.or(self.max_height);
        self.audio_mode = overrides.audio_mode;
        self.subtitle_mode = overrides.subtitle_mode;
        self.preserve_hdr = overrides.preserve_hdr;
        self.copy_chapters = overrides.copy_chapters;
        self
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(crf) = self.crf {
            let max = match self.target_codec {
                TargetVideoCodec::Av1 => 63,
                TargetVideoCodec::H264 | TargetVideoCodec::Hevc => 51,
            };
            if crf > max {
                return Err(format!(
                    "`crf` must be 0..={max} for {}",
                    self.target_codec.label()
                ));
            }
        }
        if self.max_width == Some(0) || self.max_height == Some(0) {
            return Err("max dimensions must be greater than zero".to_string());
        }
        if (self.max_width.is_some() && self.max_height.is_none())
            || (self.max_width.is_none() && self.max_height.is_some())
        {
            return Err("max_width and max_height must be configured together".to_string());
        }
        Ok(())
    }

    fn effective_crf(&self) -> u8 {
        self.crf
            .unwrap_or(match (self.target_codec, self.quality_mode) {
                (TargetVideoCodec::Av1, QualityMode::Archival) => 18,
                (TargetVideoCodec::Av1, QualityMode::Balanced) => 24,
                (TargetVideoCodec::Av1, QualityMode::Small) => 30,
                (TargetVideoCodec::H264, QualityMode::Archival) => 16,
                (TargetVideoCodec::H264, QualityMode::Balanced) => 20,
                (TargetVideoCodec::H264, QualityMode::Small) => 24,
                (TargetVideoCodec::Hevc, QualityMode::Archival) => 18,
                (TargetVideoCodec::Hevc, QualityMode::Balanced) => 22,
                (TargetVideoCodec::Hevc, QualityMode::Small) => 27,
            })
    }

    fn max_dimensions(&self) -> Option<(u32, u32)> {
        self.max_width.zip(self.max_height)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncoderSelection {
    pub codec: TargetVideoCodec,
    pub implementation: String,
    pub quality_mode: QualityMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscodeJobState {
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscodeProgress {
    pub processed_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(dead_code)]
mod extism_host_stubs {
    #[unsafe(no_mangle)]
    pub extern "C" fn alloc(_len: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn error_set(_ptr: u64) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn input_length() -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn input_load_u64(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn input_load_u8(_offset: u64) -> u8 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn output_set(_offset: u64, _len: u64) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u64(_offset: u64, _value: u64) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u8(_offset: u64, _value: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> TranscodePlanRequest {
        TranscodePlanRequest {
            config: BTreeMap::new(),
            input: TranscodeInputRef {
                path: "/media/Movie.Remux.mkv".to_string(),
                media_kind: Some("movie".to_string()),
                container_hint: Some("mkv".to_string()),
            },
            output: TranscodeOutputRef::default(),
            options: None,
        }
    }

    #[test]
    fn descriptor_is_future_transcoder_and_unambiguous() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.plugin_type, PLUGIN_TYPE);
        assert_eq!(descriptor.status, "experimental");
        assert!(descriptor.exports.contains(&EXPORT_RUN.to_string()));
        assert!(
            descriptor
                .provider
                .capabilities
                .target_codecs
                .contains(&"av1".to_string())
        );
        assert!(
            descriptor
                .provider
                .capabilities
                .target_codecs
                .contains(&"h264".to_string())
        );
        assert!(
            descriptor
                .provider
                .capabilities
                .target_codecs
                .contains(&"hevc".to_string())
        );
        assert!(
            descriptor
                .provider
                .capabilities
                .host_requirements
                .host_capabilities
                .contains(&"media_io.read_stream".to_string())
        );
    }

    #[test]
    fn default_plan_targets_archival_av1_libaom() {
        let PluginResult::Ok(plan) = plan_transcode(base_request()) else {
            panic!("expected plan");
        };
        assert_eq!(plan.target_codec, TargetVideoCodec::Av1);
        assert_eq!(plan.container, TargetContainer::Matroska);
        assert_eq!(plan.encoder.implementation, "libaom");
        assert_eq!(plan.output_path, "/media/Movie.Remux.streaming-copy.mkv");
        assert!(plan.ffmpeg_args.video.contains(&"libaom-av1".to_string()));
        assert!(
            plan.requires_future_host_capabilities
                .wasm_features
                .contains(&"exception-handling".to_string())
        );
    }

    #[test]
    fn h264_plan_keeps_required_compatibility_target() {
        let mut request = base_request();
        request
            .config
            .insert("target_codec".to_string(), "h264".to_string());
        request
            .config
            .insert("container".to_string(), "mp4".to_string());

        let PluginResult::Ok(plan) = plan_transcode(request) else {
            panic!("expected plan");
        };
        assert_eq!(plan.target_codec, TargetVideoCodec::H264);
        assert_eq!(plan.container, TargetContainer::Mp4);
        assert_eq!(plan.encoder.implementation, "x264");
        assert!(plan.ffmpeg_args.video.contains(&"libx264".to_string()));
        assert!(plan.ffmpeg_args.muxer.contains(&"+faststart".to_string()));
    }

    #[test]
    fn hevc_plan_is_present_but_warns() {
        let mut request = base_request();
        request
            .config
            .insert("target_codec".to_string(), "hevc".to_string());

        let PluginResult::Ok(plan) = plan_transcode(request) else {
            panic!("expected plan");
        };
        assert_eq!(plan.target_codec, TargetVideoCodec::Hevc);
        assert_eq!(plan.encoder.implementation, "x265");
        assert!(plan.ffmpeg_args.video.contains(&"libx265".to_string()));
        assert!(plan.warnings.iter().any(|warning| warning.contains("HEVC")));
    }

    #[test]
    fn max_dimensions_must_be_configured_as_a_pair() {
        let mut request = base_request();
        request
            .config
            .insert("max_width".to_string(), "1920".to_string());

        let PluginResult::Err(error) = plan_transcode(request) else {
            panic!("expected invalid config");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert!(
            error
                .public_message
                .contains("max_width and max_height must be configured together")
        );
    }

    #[test]
    fn run_reports_future_sdk_requirement() {
        let result = unsupported_runtime::<TranscodeRunResponse>();
        let PluginResult::Err(error) = result else {
            panic!("expected unsupported");
        };
        assert_eq!(error.code, PluginErrorCode::Unsupported);
        assert!(error.public_message.contains("does not expose"));
    }
}
