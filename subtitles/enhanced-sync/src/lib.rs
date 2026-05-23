use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use scryer_plugin_sdk::{
    PluginDescriptor, PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitlePluginGenerateRequest,
    SubtitlePluginGenerateResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod ffmpeg_backend;

const PLUGIN_ID: &str = "enhanced-subtitle-sync";
const PLUGIN_NAME: &str = "Enhanced Subtitle Sync";
const DECODER_BACKEND: &str = "vendored-ffmpeg-wasm";
const MAX_DECODE_INPUT_BYTES: usize = 64 * 1024 * 1024;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&descriptor())?)
}

#[plugin_fn]
pub fn scryer_subsync_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&sync_descriptor())?)
}

#[plugin_fn]
pub fn scryer_audio_transcode_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&audio_transcode_descriptor())?)
}

#[plugin_fn]
pub fn scryer_validate_config(input: String) -> FnResult<String> {
    let _: SubtitlePluginValidateConfigRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_subtitle_generate(input: String) -> FnResult<String> {
    let _: SubtitlePluginGenerateRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&PluginResult::<
        SubtitlePluginGenerateResponse,
    >::Err(PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: "Enhanced subtitle sync exposes decoder exports, not subtitle generation"
            .to_string(),
        debug_message: None,
        retry_after_seconds: None,
    }))?)
}

#[plugin_fn]
pub fn scryer_subsync_probe(input: String) -> FnResult<String> {
    let request: SubtitleSyncProbeRequest = serde_json::from_str(&input)?;
    let response = probe_impl(&request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

#[plugin_fn]
pub fn scryer_subsync_decode_window(input: String) -> FnResult<String> {
    scryer_subsync_decode_window_json(input)
}

#[plugin_fn]
pub fn scryer_audio_transcode(input: String) -> FnResult<String> {
    scryer_audio_transcode_json(input)
}

fn scryer_subsync_decode_window_json(input: String) -> FnResult<String> {
    let request: SubtitleSyncDecodeWindowRequest = serde_json::from_str(&input)?;
    let response = decode_window_impl(&request).map_err(Error::msg)?;
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn scryer_audio_transcode_json(input: String) -> FnResult<String> {
    let request: AudioTranscodeRequest = serde_json::from_str(&input)?;
    let response = audio_transcode_impl(&request);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::Subtitle(SubtitleDescriptor {
            provider_type: PLUGIN_ID.to_string(),
            provider_aliases: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
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
                supported_languages: vec![],
            },
        }),
    }
}

fn sync_descriptor() -> EnhancedSubtitleSyncDescriptor {
    EnhancedSubtitleSyncDescriptor {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        plugin_type: "subtitle_sync".to_string(),
        exports: vec![
            "scryer_subsync_describe".to_string(),
            "scryer_subsync_probe".to_string(),
            "scryer_subsync_decode_window".to_string(),
            "scryer_audio_transcode_describe".to_string(),
            "scryer_audio_transcode".to_string(),
        ],
        capabilities: SubtitleSyncCapabilities {
            backend: DECODER_BACKEND.to_string(),
            decode_status: DecodeBackendStatus::Complete,
            supported_codecs: AudioCodec::all().to_vec(),
            decoded_codecs: AudioCodec::all().to_vec(),
            pending_codecs: vec![],
            output_sample_format: "f32le".to_string(),
            supports_mono_mixdown: true,
        },
    }
}

fn audio_transcode_descriptor() -> AudioTranscodeDescriptor {
    AudioTranscodeDescriptor {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        supported_profiles: vec![AudioTranscodeProfile::SyncFlac],
        supported_input_codecs: AudioTranscodeCodec::all().to_vec(),
    }
}

fn audio_transcode_impl(request: &AudioTranscodeRequest) -> AudioTranscodeResponse {
    let AudioTranscodeProfile::SyncFlac = request.profile;
    let selector = request
        .selector
        .as_ref()
        .unwrap_or(&AudioStreamSelector::Default);
    match ffmpeg_backend::transcode_sync_flac(
        &request.input.path,
        &request.output.path,
        request.expected_codec,
        selector,
    ) {
        Ok(transcoded) => AudioTranscodeResponse {
            status: AudioTranscodeStatus::Decoded,
            output: Some(request.output.clone()),
            stream: Some(AudioTranscodeStreamMetadata {
                index: transcoded.stream_index,
                language: transcoded.language,
                codec: transcoded.codec,
                source_codec_name: transcoded.source_codec_name,
                source_profile: transcoded.source_profile,
                used_core_fallback: transcoded.used_core_fallback,
            }),
            sample_rate_hz: Some(transcoded.sample_rate_hz),
            channels: Some(transcoded.channels),
            samples_written: Some(transcoded.samples_written),
            duration_ms: Some(transcoded.duration_ms),
            timeline_start_ms: Some(transcoded.timeline_start_ms),
            warnings: transcoded.warnings,
            message: transcoded.message,
        },
        Err(ffmpeg_backend::TranscodeFailure::Unsupported { message }) => {
            audio_transcode_failure(AudioTranscodeStatus::UnsupportedCodec, message)
        }
        Err(ffmpeg_backend::TranscodeFailure::Error { message }) => {
            audio_transcode_failure(AudioTranscodeStatus::Error, message)
        }
    }
}

fn audio_transcode_failure(
    status: AudioTranscodeStatus,
    message: impl Into<String>,
) -> AudioTranscodeResponse {
    AudioTranscodeResponse {
        status,
        output: None,
        stream: None,
        sample_rate_hz: None,
        channels: None,
        samples_written: None,
        duration_ms: None,
        timeline_start_ms: None,
        warnings: Vec::new(),
        message: Some(message.into()),
    }
}

fn probe_impl(request: &SubtitleSyncProbeRequest) -> Result<SubtitleSyncProbeResponse, String> {
    let explicit_codec = request.codec.or_else(|| {
        request
            .codec_label
            .as_deref()
            .and_then(AudioCodec::from_label)
    });
    let packet = decode_optional_base64(request.packet_base64.as_deref())?;
    let detected = packet.as_deref().and_then(detect_packet_codec);
    let codec = explicit_codec.or(detected.as_ref().map(|detected| detected.codec));
    let confidence = if explicit_codec.is_some() {
        1.0
    } else {
        detected
            .as_ref()
            .map_or(0.0, |detected| detected.confidence)
    };

    let mut notes = Vec::new();
    if explicit_codec.is_some()
        && detected
            .as_ref()
            .is_some_and(|detected| Some(detected.codec) != explicit_codec)
    {
        notes.push("explicit codec did not match packet sync probe".to_string());
    }
    if codec.is_some() {
        notes.push("codec is routed to the vendored FFmpeg decoder backend".to_string());
    } else {
        notes.push("no supported codec could be identified".to_string());
    }

    Ok(SubtitleSyncProbeResponse {
        codec,
        supported: codec.is_some(),
        backend: DECODER_BACKEND.to_string(),
        confidence,
        sample_rate_hz: detected.and_then(|detected| detected.sample_rate_hz),
        notes,
    })
}

fn decode_window_impl(
    request: &SubtitleSyncDecodeWindowRequest,
) -> Result<SubtitleSyncDecodeWindowResponse, String> {
    if request.packets.is_empty() {
        return Err("decode window must contain at least one packet".to_string());
    }

    let mut observed_codec = None;
    let mut packets = Vec::with_capacity(request.packets.len());
    let mut input_bytes = 0usize;
    for packet in &request.packets {
        let bytes = BASE64
            .decode(&packet.data_base64)
            .map_err(|error| format!("invalid packet base64: {error}"))?;
        input_bytes = input_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| "decode input byte count overflowed".to_string())?;
        if input_bytes > MAX_DECODE_INPUT_BYTES {
            return Err(format!(
                "decode window input exceeds {} bytes",
                MAX_DECODE_INPUT_BYTES
            ));
        }
        if observed_codec.is_none() {
            observed_codec = detect_packet_codec(&bytes).map(|detected| detected.codec);
        }
        packets.push(DecodedPacket {
            pts_ms: packet.pts_ms,
            data: bytes,
        });
    }

    if let (Some(requested), Some(observed)) = (request.codec, observed_codec)
        && requested != observed
    {
        return Ok(unsupported_response(
            Some(observed),
            "requested codec did not match packet sync probe",
        ));
    }

    let Some(routed_codec) = request.codec.or(observed_codec) else {
        return Ok(unsupported_response(
            None,
            "no supported codec could be identified",
        ));
    };

    let decoded = match ffmpeg_backend::decode_window(routed_codec, &packets, request.mixdown_mono)
    {
        Ok(decoded) => decoded,
        Err(message) => return Ok(unsupported_response(Some(routed_codec), &message)),
    };
    let sample_rate_hz = decoded
        .sample_rate_hz
        .ok_or_else(|| format!("{routed_codec:?} decoder did not report a sample rate"))?;
    let channels = decoded
        .channels
        .ok_or_else(|| format!("{routed_codec:?} decoder did not report a channel count"))?;
    let message = request
        .target_sample_rate_hz
        .filter(|target_sample_rate_hz| *target_sample_rate_hz != sample_rate_hz)
        .map(|target_sample_rate_hz| {
            format!(
                "decoded at {sample_rate_hz}Hz; requested {target_sample_rate_hz}Hz resampling is not implemented in the plugin"
            )
        });

    Ok(SubtitleSyncDecodeWindowResponse {
        status: DecodeWindowStatus::Decoded,
        codec: Some(decoded.codec),
        sample_rate_hz: Some(sample_rate_hz),
        channels: Some(channels),
        samples_decoded: decoded.samples_decoded,
        pcm_f32le_base64: Some(BASE64.encode(&decoded.pcm_f32le)),
        message,
    })
}

fn unsupported_response(
    codec: Option<AudioCodec>,
    message: impl Into<String>,
) -> SubtitleSyncDecodeWindowResponse {
    SubtitleSyncDecodeWindowResponse {
        status: DecodeWindowStatus::Unsupported,
        codec,
        sample_rate_hz: None,
        channels: None,
        samples_decoded: 0,
        pcm_f32le_base64: None,
        message: Some(message.into()),
    }
}

fn decode_optional_base64(value: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| {
            BASE64
                .decode(value)
                .map_err(|error| format!("invalid packet base64: {error}"))
        })
        .transpose()
}

fn detect_packet_codec(packet: &[u8]) -> Option<DetectedCodec> {
    if contains_truehd_major_sync(packet) {
        return Some(DetectedCodec {
            codec: AudioCodec::TrueHd,
            confidence: 0.95,
            sample_rate_hz: None,
        });
    }

    if contains_dts_sync(packet) {
        return Some(DetectedCodec {
            codec: AudioCodec::Dts,
            confidence: 0.95,
            sample_rate_hz: None,
        });
    }

    if packet.len() >= 6 && packet[0] == 0x0b && packet[1] == 0x77 {
        let bitstream_id = packet[5] >> 3;
        let codec = if (11..=16).contains(&bitstream_id) {
            AudioCodec::Eac3
        } else {
            AudioCodec::Ac3
        };
        return Some(DetectedCodec {
            codec,
            confidence: 0.9,
            sample_rate_hz: ac3_sample_rate(packet[4] >> 6),
        });
    }

    None
}

fn ac3_sample_rate(fscod: u8) -> Option<u32> {
    match fscod {
        0 => Some(48_000),
        1 => Some(44_100),
        2 => Some(32_000),
        _ => None,
    }
}

fn contains_dts_sync(packet: &[u8]) -> bool {
    const DTS_SYNCS: [[u8; 4]; 6] = [
        [0x7f, 0xfe, 0x80, 0x01],
        [0xfe, 0x7f, 0x01, 0x80],
        [0x1f, 0xff, 0xe8, 0x00],
        [0xff, 0x1f, 0x00, 0xe8],
        [0x64, 0x58, 0x20, 0x25],
        [0x41, 0xa2, 0x95, 0x47],
    ];
    packet
        .windows(4)
        .any(|window| DTS_SYNCS.iter().any(|sync| window == sync))
}

fn contains_truehd_major_sync(packet: &[u8]) -> bool {
    packet
        .windows(4)
        .any(|window| window == [0xf8, 0x72, 0x6f, 0xba] || window == [0xf8, 0x72, 0x6f, 0xbb])
}

#[derive(Debug, Clone, Serialize)]
struct EnhancedSubtitleSyncDescriptor {
    id: String,
    name: String,
    version: String,
    sdk_version: String,
    sdk_constraint: String,
    plugin_type: String,
    exports: Vec<String>,
    capabilities: SubtitleSyncCapabilities,
}

#[derive(Debug, Clone, Serialize)]
struct SubtitleSyncCapabilities {
    backend: String,
    decode_status: DecodeBackendStatus,
    supported_codecs: Vec<AudioCodec>,
    decoded_codecs: Vec<AudioCodec>,
    pending_codecs: Vec<AudioCodec>,
    output_sample_format: String,
    supports_mono_mixdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DecodeBackendStatus {
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeDescriptor {
    id: String,
    name: String,
    version: String,
    sdk_version: String,
    sdk_constraint: String,
    supported_profiles: Vec<AudioTranscodeProfile>,
    supported_input_codecs: Vec<AudioTranscodeCodec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AudioTranscodeProfile {
    SyncFlac,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AudioTranscodeCodec {
    Ac3 = 0,
    Eac3 = 1,
    Dts = 2,
    DtsHdMaCore = 3,
    TrueHd = 4,
}

impl AudioTranscodeCodec {
    fn all() -> &'static [Self] {
        &[
            Self::Ac3,
            Self::Eac3,
            Self::Dts,
            Self::DtsHdMaCore,
            Self::TrueHd,
        ]
    }

    fn from_ffi(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Ac3),
            1 => Some(Self::Eac3),
            2 => Some(Self::Dts),
            3 => Some(Self::DtsHdMaCore),
            4 => Some(Self::TrueHd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AudioTranscodeStatus {
    Decoded,
    UnsupportedCodec,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AudioStreamSelector {
    Default,
    StreamIndex { index: u32 },
    Language { language: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeInputRef {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeOutputRef {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeRequest {
    input: AudioTranscodeInputRef,
    output: AudioTranscodeOutputRef,
    profile: AudioTranscodeProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selector: Option<AudioStreamSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_codec: Option<AudioTranscodeCodec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeStreamMetadata {
    index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    codec: AudioTranscodeCodec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_codec_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_profile: Option<String>,
    #[serde(default)]
    used_core_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioTranscodeResponse {
    status: AudioTranscodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<AudioTranscodeOutputRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<AudioTranscodeStreamMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample_rate_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    samples_written: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline_start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AudioCodec {
    Ac3 = 0,
    Eac3 = 1,
    Dts = 2,
    #[serde(rename = "truehd")]
    TrueHd = 3,
}

impl AudioCodec {
    fn all() -> &'static [Self] {
        &[Self::Ac3, Self::Eac3, Self::Dts, Self::TrueHd]
    }

    fn from_label(label: &str) -> Option<Self> {
        let normalized = label
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(char::to_uppercase)
            .collect::<String>();
        match normalized.as_str() {
            "AC3" | "DOLBYDIGITAL" => Some(Self::Ac3),
            "EAC3" | "EC3" | "DDP" | "DDPLUS" | "DOLBYDIGITALPLUS" => Some(Self::Eac3),
            "DTS" | "DTSHD" | "DTSHDMA" | "DTSX" | "DCA" => Some(Self::Dts),
            "TRUEHD" | "DOLBYTRUEHD" | "MLP" => Some(Self::TrueHd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitleSyncProbeRequest {
    #[serde(default)]
    codec: Option<AudioCodec>,
    #[serde(default)]
    codec_label: Option<String>,
    #[serde(default)]
    packet_base64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitleSyncProbeResponse {
    codec: Option<AudioCodec>,
    supported: bool,
    backend: String,
    confidence: f32,
    sample_rate_hz: Option<u32>,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitleSyncDecodeWindowRequest {
    codec: Option<AudioCodec>,
    packets: Vec<AudioPacket>,
    #[serde(default)]
    target_sample_rate_hz: Option<u32>,
    #[serde(default)]
    mixdown_mono: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AudioPacket {
    #[serde(default)]
    pts_ms: Option<i64>,
    data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubtitleSyncDecodeWindowResponse {
    status: DecodeWindowStatus,
    codec: Option<AudioCodec>,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    samples_decoded: u64,
    pcm_f32le_base64: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DecodeWindowStatus {
    Decoded,
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
struct DetectedCodec {
    codec: AudioCodec,
    confidence: f32,
    sample_rate_hz: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedPacket {
    pts_ms: Option<i64>,
    data: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct DecodedPcm {
    codec: AudioCodec,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    samples_decoded: u64,
    pcm_f32le: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::codecs::audio::well_known::{
        CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
    };
    use symphonia::core::codecs::audio::{AudioCodecId, AudioCodecParameters, AudioDecoderOptions};
    use symphonia::core::formats::{FormatOptions, TrackType, probe::Hint};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::packet::Packet as SymphoniaPacket;
    use symphonia::core::units::{Duration, Timestamp};

    fn symphonia_codec_to_plugin(codec: AudioCodecId) -> Option<AudioCodec> {
        match codec {
            CODEC_ID_AC3 => Some(AudioCodec::Ac3),
            CODEC_ID_EAC3 => Some(AudioCodec::Eac3),
            CODEC_ID_DCA => Some(AudioCodec::Dts),
            CODEC_ID_TRUEHD => Some(AudioCodec::TrueHd),
            _ => None,
        }
    }

    fn symphonia_packet_to_abi_packet(
        packet: &SymphoniaPacket,
        sample_rate_hz: u32,
    ) -> AudioPacket {
        AudioPacket {
            pts_ms: packet
                .pts
                .get()
                .checked_mul(1000)
                .map(|pts| pts / i64::from(sample_rate_hz)),
            data_base64: BASE64.encode(&packet.data),
        }
    }

    #[test]
    fn descriptor_advertises_vendored_ffmpeg_sync_contract() {
        let descriptor = sync_descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.plugin_type, "subtitle_sync");
        assert_eq!(descriptor.capabilities.backend, DECODER_BACKEND);
        assert_eq!(
            descriptor.capabilities.decode_status,
            DecodeBackendStatus::Complete
        );
        assert_eq!(
            descriptor.capabilities.supported_codecs.as_slice(),
            AudioCodec::all()
        );
        assert_eq!(
            descriptor.capabilities.decoded_codecs.as_slice(),
            AudioCodec::all()
        );
        assert!(descriptor.capabilities.pending_codecs.is_empty());
        assert!(
            descriptor
                .exports
                .contains(&"scryer_subsync_probe".to_string())
        );
        assert!(
            descriptor
                .exports
                .contains(&"scryer_audio_transcode".to_string())
        );
    }

    #[test]
    fn audio_transcode_descriptor_advertises_targeted_flac_contract() {
        let descriptor = audio_transcode_descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(
            descriptor.supported_profiles,
            vec![AudioTranscodeProfile::SyncFlac]
        );
        assert_eq!(
            descriptor.supported_input_codecs,
            AudioTranscodeCodec::all()
        );
    }

    #[test]
    fn sdk_descriptor_stays_loadable_until_sync_provider_abi_exists() {
        let descriptor = descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.plugin_type(), "subtitle_provider");
        assert!(matches!(
            descriptor.provider,
            ProviderDescriptor::Subtitle(SubtitleDescriptor {
                capabilities: SubtitleCapabilities {
                    mode: SubtitleProviderMode::Generator,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn codec_labels_route_to_supported_decoders() {
        assert_eq!(AudioCodec::from_label("AC-3"), Some(AudioCodec::Ac3));
        assert_eq!(AudioCodec::from_label("E-AC-3"), Some(AudioCodec::Eac3));
        assert_eq!(AudioCodec::from_label("DTS-HD MA"), Some(AudioCodec::Dts));
        assert_eq!(
            AudioCodec::from_label("Dolby TrueHD"),
            Some(AudioCodec::TrueHd)
        );
    }

    #[test]
    fn probes_ac3_sync_and_sample_rate() {
        let detected =
            detect_packet_codec(&[0x0b, 0x77, 0x00, 0x00, 0x00, 0x50]).expect("detect ac3");
        assert_eq!(detected.codec, AudioCodec::Ac3);
        assert_eq!(detected.sample_rate_hz, Some(48_000));
    }

    #[test]
    fn probes_eac3_from_bitstream_id() {
        let detected =
            detect_packet_codec(&[0x0b, 0x77, 0x00, 0x00, 0x40, 0x78]).expect("detect eac3");
        assert_eq!(detected.codec, AudioCodec::Eac3);
        assert_eq!(detected.sample_rate_hz, Some(44_100));
    }

    #[test]
    fn probes_dts_and_truehd_sync_words() {
        let dts = detect_packet_codec(&[0xaa, 0x7f, 0xfe, 0x80, 0x01, 0xbb]).expect("detect dts");
        assert_eq!(dts.codec, AudioCodec::Dts);

        let truehd =
            detect_packet_codec(&[0x00, 0x00, 0xf8, 0x72, 0x6f, 0xba]).expect("detect truehd");
        assert_eq!(truehd.codec, AudioCodec::TrueHd);
    }

    #[test]
    fn decode_window_decodes_ac3_fixture_with_vendored_ffmpeg() {
        let request = SubtitleSyncDecodeWindowRequest {
            codec: Some(AudioCodec::Ac3),
            packets: vec![AudioPacket {
                pts_ms: Some(0),
                data_base64: BASE64.encode(include_bytes!("../tests/fixtures/sine440_stereo.ac3")),
            }],
            target_sample_rate_hz: None,
            mixdown_mono: true,
        };

        let raw_response = scryer_subsync_decode_window_json(
            serde_json::to_string(&request).expect("encode request"),
        )
        .expect("decode window export response");
        let PluginResult::Ok(response) =
            serde_json::from_str::<PluginResult<SubtitleSyncDecodeWindowResponse>>(&raw_response)
                .expect("decode plugin result")
        else {
            panic!("decode window export returned plugin error");
        };
        assert_eq!(response.status, DecodeWindowStatus::Decoded);
        assert_eq!(response.codec, Some(AudioCodec::Ac3));
        assert_eq!(response.sample_rate_hz, Some(48_000));
        assert_eq!(response.channels, Some(1));
        assert!(response.samples_decoded > 0);
        assert!(response.pcm_f32le_base64.is_some());
    }

    #[test]
    fn audio_transcode_decodes_ac3_fixture_to_symphonia_readable_flac() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let output_path = temp_dir.path().join("sync.flac");
        let request = AudioTranscodeRequest {
            input: AudioTranscodeInputRef {
                path: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/sine440_stereo.ac3"),
            },
            output: AudioTranscodeOutputRef {
                path: output_path.clone(),
            },
            profile: AudioTranscodeProfile::SyncFlac,
            selector: Some(AudioStreamSelector::Default),
            expected_codec: Some(AudioTranscodeCodec::Ac3),
        };

        let raw_response =
            scryer_audio_transcode_json(serde_json::to_string(&request).expect("encode request"))
                .expect("transcode export response");
        let PluginResult::Ok(response) =
            serde_json::from_str::<PluginResult<AudioTranscodeResponse>>(&raw_response)
                .expect("decode plugin result")
        else {
            panic!("audio transcode export returned plugin error");
        };
        assert_eq!(response.status, AudioTranscodeStatus::Decoded);
        assert_eq!(response.sample_rate_hz, Some(16_000));
        assert_eq!(response.channels, Some(1));
        assert!(response.samples_written.unwrap_or_default() > 0);
        assert_eq!(
            response.stream.as_ref().map(|stream| stream.codec),
            Some(AudioTranscodeCodec::Ac3)
        );
        assert_eq!(
            std::fs::read(&output_path).expect("read flac")[..4],
            *b"fLaC"
        );

        let (sample_rate_hz, channels, frames) = decode_flac_with_symphonia(&output_path);
        assert_eq!(sample_rate_hz, 16_000);
        assert_eq!(channels, 1);
        assert!(frames > 0);
    }

    #[test]
    #[ignore = "requires local media files; set SCRYER_ENHANCED_SYNC_ACCEPT_* env vars"]
    fn local_media_transcodes_targeted_codecs_to_symphonia_readable_flac() {
        let cases = [
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_AC3",
                AudioTranscodeCodec::Ac3,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_EAC3",
                AudioTranscodeCodec::Eac3,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_DTS",
                AudioTranscodeCodec::Dts,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_DTS_HD_MA",
                AudioTranscodeCodec::DtsHdMaCore,
                true,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_TRUEHD",
                AudioTranscodeCodec::TrueHd,
                false,
            ),
        ];

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut ran = 0usize;
        for (env_name, expected_codec, expect_core_fallback) in cases {
            let Ok(input_path) = std::env::var(env_name) else {
                eprintln!("skipping {env_name}; env var not set");
                continue;
            };
            ran += 1;

            let output_path = temp_dir
                .path()
                .join(format!("{}.flac", env_name.to_ascii_lowercase()));
            let transcoded = ffmpeg_backend::transcode_sync_flac_with_sample_limit(
                &PathBuf::from(&input_path),
                &output_path,
                Some(expected_codec),
                &AudioStreamSelector::Default,
                20 * 16_000,
            )
            .unwrap_or_else(|error| {
                panic!("{env_name} failed for {input_path}: {error:?}");
            });

            assert_eq!(transcoded.sample_rate_hz, 16_000, "{env_name}");
            assert_eq!(transcoded.channels, 1, "{env_name}");
            assert!(
                transcoded.samples_written > 0,
                "{env_name}"
            );

            assert_eq!(transcoded.codec, expected_codec, "{env_name}");
            assert_eq!(
                transcoded.used_core_fallback, expect_core_fallback,
                "{env_name}"
            );

            assert_eq!(
                std::fs::read(&output_path).expect("read flac")[..4],
                *b"fLaC",
                "{env_name}"
            );
            let (sample_rate_hz, channels, frames) = decode_flac_with_symphonia(&output_path);
            assert_eq!(sample_rate_hz, 16_000, "{env_name}");
            assert_eq!(channels, 1, "{env_name}");
            assert!(frames > 0, "{env_name}");

            eprintln!(
                "{env_name}: decoded {:?} from stream {} into {} samples ({:?})",
                transcoded.codec, transcoded.stream_index, frames, transcoded.source_profile
            );
        }

        assert!(ran > 0, "set at least one SCRYER_ENHANCED_SYNC_ACCEPT_* env var");
    }

    #[test]
    fn symphonia_packet_payload_decodes_through_plugin_abi_shape() {
        let codec_params = AudioCodecParameters {
            codec: CODEC_ID_AC3,
            sample_rate: Some(48_000),
            ..Default::default()
        };
        let packet = SymphoniaPacket::new(
            17,
            Timestamp::new(96_000),
            Duration::new(1536),
            include_bytes!("../tests/fixtures/sine440_stereo.ac3").to_vec(),
        );

        let abi_packet =
            symphonia_packet_to_abi_packet(&packet, codec_params.sample_rate.expect("sample rate"));
        assert_eq!(abi_packet.pts_ms, Some(2000));

        let request = SubtitleSyncDecodeWindowRequest {
            codec: symphonia_codec_to_plugin(codec_params.codec),
            packets: vec![abi_packet],
            target_sample_rate_hz: None,
            mixdown_mono: true,
        };

        let response = decode_window_impl(&request).expect("decode window response");
        assert_eq!(response.status, DecodeWindowStatus::Decoded);
        assert_eq!(response.codec, Some(AudioCodec::Ac3));
        assert_eq!(response.sample_rate_hz, Some(48_000));
        assert_eq!(response.channels, Some(1));
        assert!(response.samples_decoded > 0);
        assert!(response.pcm_f32le_base64.is_some());
    }

    fn decode_flac_with_symphonia(path: &std::path::Path) -> (u32, usize, u64) {
        let file = std::fs::File::open(path).expect("open flac");
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("flac");
        let probed = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .expect("probe flac");
        let mut format = probed;
        let track = format
            .default_track(TrackType::Audio)
            .expect("default flac track");
        let track_id = track.id;
        let codec_params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .expect("flac audio codec params")
            .clone();
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
            .expect("make flac decoder");
        let mut sample_rate_hz = 0;
        let mut channels = 0;
        let mut frames = 0;

        loop {
            let packet = match format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(symphonia::core::errors::Error::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(_) => break,
            };
            if packet.track_id != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).expect("decode flac packet");
            sample_rate_hz = decoded.spec().rate();
            channels = decoded.spec().channels().count();
            frames += decoded.frames() as u64;
        }

        (sample_rate_hz, channels, frames)
    }
}
