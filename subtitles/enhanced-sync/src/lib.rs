use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};
use scryer_plugin_sdk::{
    PluginDescriptor, PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitlePluginGenerateRequest,
    SubtitlePluginGenerateResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};
use truehd::process::{decode, extract, parse};

const PLUGIN_ID: &str = "enhanced-subtitle-sync";
const PLUGIN_NAME: &str = "Enhanced Subtitle Sync";
const DECODER_BACKEND: &str = "rust-decoders-with-ffmpeg-source-snapshot";
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

fn scryer_subsync_decode_window_json(input: String) -> FnResult<String> {
    let request: SubtitleSyncDecodeWindowRequest = serde_json::from_str(&input)?;
    let response = decode_window_impl(&request).map_err(Error::msg)?;
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
        ],
        capabilities: SubtitleSyncCapabilities {
            backend: DECODER_BACKEND.to_string(),
            decode_status: DecodeBackendStatus::Partial,
            supported_codecs: AudioCodec::all().to_vec(),
            decoded_codecs: vec![AudioCodec::Ac3, AudioCodec::Eac3, AudioCodec::TrueHd],
            pending_codecs: vec![AudioCodec::Dts],
            output_sample_format: "f32le".to_string(),
            supports_mono_mixdown: true,
        },
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
        notes.push("codec is routed to the FFmpeg-derived decoder backend".to_string());
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
        return Ok(SubtitleSyncDecodeWindowResponse {
            status: DecodeWindowStatus::Unsupported,
            codec: Some(observed),
            sample_rate_hz: None,
            channels: None,
            samples_decoded: 0,
            pcm_f32le_base64: None,
            message: Some("requested codec did not match packet sync probe".to_string()),
        });
    }

    let routed_codec = request.codec.or(observed_codec);
    let Some(routed_codec) = routed_codec else {
        return Ok(SubtitleSyncDecodeWindowResponse {
            status: DecodeWindowStatus::Unsupported,
            codec: None,
            sample_rate_hz: None,
            channels: None,
            samples_decoded: 0,
            pcm_f32le_base64: None,
            message: Some("no supported codec could be identified".to_string()),
        });
    };

    let decoded = match routed_codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => {
            decode_ac3_family(routed_codec, &packets, request.mixdown_mono)?
        }
        AudioCodec::TrueHd => decode_truehd(&packets, request.mixdown_mono)?,
        AudioCodec::Dts => {
            return Ok(SubtitleSyncDecodeWindowResponse {
                status: DecodeWindowStatus::Unsupported,
                codec: Some(AudioCodec::Dts),
                sample_rate_hz: None,
                channels: None,
                samples_decoded: 0,
                pcm_f32le_base64: None,
                message: Some(
                    "DTS packet routing is implemented, but the DTS PCM backend still needs the vendored FFmpeg decoder port"
                        .to_string(),
                ),
            });
        }
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
        pcm_f32le_base64: Some(BASE64.encode(f32_samples_as_le_bytes(&decoded.pcm_f32))),
        message,
    })
}

fn decode_ac3_family(
    codec: AudioCodec,
    packets: &[DecodedPacket],
    mixdown_mono: bool,
) -> Result<DecodedPcm, String> {
    let codec_id = match codec {
        AudioCodec::Ac3 => "ac3",
        AudioCodec::Eac3 => "eac3",
        _ => return Err(format!("{codec:?} is not an AC-3 family codec")),
    };
    let mut params = CodecParameters::audio(CodecId::new(codec_id));
    params.channels = mixdown_mono.then_some(1);
    params.sample_format = Some(SampleFormat::S16);

    let mut decoder = match codec {
        AudioCodec::Ac3 => oxideav_ac3::decoder::make_decoder(&params),
        AudioCodec::Eac3 => oxideav_ac3::decoder::make_eac3_decoder(&params),
        _ => unreachable!("guarded above"),
    }
    .map_err(|error| format!("{codec_id} decoder init failed: {error}"))?;

    let mut output = DecodedPcm::new(codec);
    for packet in packets {
        let packet_sample_rate_hz = packet_sample_rate(codec, &packet.data);
        let time_base = TimeBase::new(1, packet_sample_rate_hz.unwrap_or(48_000) as i64);
        let mut packet_value = Packet::new(0, time_base, packet.data.clone());
        packet_value.pts = packet.pts_ms;
        decoder
            .send_packet(&packet_value)
            .map_err(|error| format!("{codec_id} packet decode failed: {error}"))?;
        receive_oxideav_frames(
            codec_id,
            codec,
            packet_sample_rate_hz,
            decoder.as_mut(),
            &mut output,
        )?;
    }

    if output.samples_decoded == 0 {
        return Err(format!("{codec_id} decoder produced no PCM samples"));
    }
    Ok(output)
}

fn receive_oxideav_frames(
    codec_id: &str,
    codec: AudioCodec,
    packet_sample_rate_hz: Option<u32>,
    decoder: &mut dyn oxideav_core::Decoder,
    output: &mut DecodedPcm,
) -> Result<(), String> {
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(frame)) => {
                let data = frame
                    .data
                    .first()
                    .ok_or_else(|| format!("{codec_id} decoder returned an empty audio frame"))?;
                let channels = infer_interleaved_s16_channels(data, frame.samples)?;
                let sample_rate_hz = packet_sample_rate_hz
                    .or(output.sample_rate_hz)
                    .or_else(|| codec.default_sample_rate());
                output.append_s16le_frame(codec, sample_rate_hz, channels, frame.samples, data)?;
            }
            Ok(_) => return Err(format!("{codec_id} decoder returned a non-audio frame")),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(error) => return Err(format!("{codec_id} frame receive failed: {error}")),
        }
    }
    Ok(())
}

fn decode_truehd(packets: &[DecodedPacket], mixdown_mono: bool) -> Result<DecodedPcm, String> {
    let mut extractor = extract::Extractor::default();
    let mut parser = parse::Parser::default();
    let mut decoder = decode::Decoder::default();
    let mut output = DecodedPcm::new(AudioCodec::TrueHd);

    for packet in packets {
        extractor.push_bytes(&packet.data);
        for frame in extractor.by_ref() {
            let frame = match frame {
                Ok(frame) => frame,
                Err(truehd::utils::errors::ExtractError::InsufficientData) => break,
                Err(error) => return Err(format!("truehd frame extraction failed: {error}")),
            };
            let access_unit = parser
                .parse(&frame)
                .map_err(|error| format!("truehd frame parse failed: {error}"))?;
            let decoded = decoder
                .decode_presentation(&access_unit, 0)
                .map_err(|error| format!("truehd PCM decode failed: {error}"))?;
            if decoded.is_duplicate || decoded.sample_length == 0 {
                continue;
            }
            output.append_truehd_frame(&decoded, mixdown_mono)?;
        }
    }

    if output.samples_decoded == 0 {
        return Err("truehd decoder produced no PCM samples".to_string());
    }
    Ok(output)
}

fn infer_interleaved_s16_channels(data: &[u8], samples: u32) -> Result<u16, String> {
    let sample_count = samples as usize;
    if sample_count == 0 || !data.len().is_multiple_of(2) {
        return Err("decoder returned invalid S16 frame shape".to_string());
    }
    let channels = data.len() / 2 / sample_count;
    u16::try_from(channels)
        .ok()
        .filter(|channels| *channels > 0)
        .ok_or_else(|| "decoder returned invalid channel count".to_string())
}

fn packet_sample_rate(codec: AudioCodec, packet: &[u8]) -> Option<u32> {
    match codec {
        AudioCodec::Ac3 => (packet.len() >= 5)
            .then(|| ac3_sample_rate(packet[4] >> 6))
            .flatten(),
        AudioCodec::Eac3 => eac3_sample_rate(packet),
        AudioCodec::Dts | AudioCodec::TrueHd => None,
    }
}

fn eac3_sample_rate(packet: &[u8]) -> Option<u32> {
    let header = *packet.get(4)?;
    let fscod = header >> 6;
    if fscod == 3 {
        match (header >> 4) & 0b11 {
            0 => Some(24_000),
            1 => Some(22_050),
            2 => Some(16_000),
            _ => None,
        }
    } else {
        ac3_sample_rate(fscod)
    }
}

fn f32_samples_as_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
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
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AudioCodec {
    Ac3,
    Eac3,
    Dts,
    #[serde(rename = "truehd")]
    TrueHd,
}

impl AudioCodec {
    fn all() -> &'static [Self] {
        &[Self::Ac3, Self::Eac3, Self::Dts, Self::TrueHd]
    }

    fn default_sample_rate(self) -> Option<u32> {
        match self {
            Self::Ac3 | Self::Eac3 => Some(48_000),
            Self::Dts | Self::TrueHd => None,
        }
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
struct DecodedPacket {
    pts_ms: Option<i64>,
    data: Vec<u8>,
}

#[derive(Debug)]
struct DecodedPcm {
    codec: AudioCodec,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    samples_decoded: u64,
    pcm_f32: Vec<f32>,
}

impl DecodedPcm {
    fn new(codec: AudioCodec) -> Self {
        Self {
            codec,
            sample_rate_hz: None,
            channels: None,
            samples_decoded: 0,
            pcm_f32: Vec::new(),
        }
    }

    fn append_s16le_frame(
        &mut self,
        codec: AudioCodec,
        sample_rate_hz: Option<u32>,
        channels: u16,
        samples: u32,
        data: &[u8],
    ) -> Result<(), String> {
        if self.codec != codec {
            return Err("decoded PCM codec changed across frames".to_string());
        }
        self.set_shape(sample_rate_hz, channels)?;
        let expected_bytes = samples as usize * channels as usize * 2;
        if data.len() != expected_bytes {
            return Err(format!(
                "decoder returned {} bytes, expected {expected_bytes}",
                data.len()
            ));
        }
        for sample in data.chunks_exact(2) {
            let value = i16::from_le_bytes([sample[0], sample[1]]);
            self.pcm_f32.push(value as f32 / 32768.0);
        }
        self.samples_decoded += samples as u64;
        Ok(())
    }

    fn append_truehd_frame(
        &mut self,
        decoded: &decode::DecodedAccessUnit,
        mixdown_mono: bool,
    ) -> Result<(), String> {
        let source_channels = u16::try_from(decoded.channel_count)
            .map_err(|_| "truehd channel count does not fit u16".to_string())?;
        let channels = if mixdown_mono { 1 } else { source_channels };
        self.set_shape(Some(decoded.sampling_frequency), channels)?;

        for row in decoded.pcm_data.iter().take(decoded.sample_length) {
            if mixdown_mono {
                let mut sum = 0.0f32;
                for sample in row.iter().take(source_channels as usize) {
                    sum += truehd_sample_to_f32(*sample);
                }
                self.pcm_f32.push(sum / source_channels.max(1) as f32);
            } else {
                for sample in row.iter().take(source_channels as usize) {
                    self.pcm_f32.push(truehd_sample_to_f32(*sample));
                }
            }
        }
        self.samples_decoded += decoded.sample_length as u64;
        Ok(())
    }

    fn set_shape(&mut self, sample_rate_hz: Option<u32>, channels: u16) -> Result<(), String> {
        if let (Some(existing), Some(next)) = (self.sample_rate_hz, sample_rate_hz)
            && existing != next
        {
            return Err(format!(
                "decoder sample rate changed from {existing}Hz to {next}Hz"
            ));
        }
        if let Some(existing) = self.channels
            && existing != channels
        {
            return Err(format!(
                "decoder channel count changed from {existing} to {channels}"
            ));
        }

        self.sample_rate_hz = self.sample_rate_hz.or(sample_rate_hz);
        self.channels = Some(channels);
        Ok(())
    }
}

fn truehd_sample_to_f32(sample: i32) -> f32 {
    (sample as f32 / 8_388_608.0).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::codecs::audio::well_known::{
        CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
    };
    use symphonia::core::codecs::audio::{AudioCodecId, AudioCodecParameters};
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
    fn descriptor_advertises_experimental_sync_contract() {
        let descriptor = sync_descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.plugin_type, "subtitle_sync");
        assert_eq!(
            descriptor.capabilities.supported_codecs.as_slice(),
            AudioCodec::all()
        );
        assert_eq!(
            descriptor.capabilities.decoded_codecs,
            vec![AudioCodec::Ac3, AudioCodec::Eac3, AudioCodec::TrueHd]
        );
        assert_eq!(
            descriptor.capabilities.pending_codecs,
            vec![AudioCodec::Dts]
        );
        assert!(
            descriptor
                .exports
                .contains(&"scryer_subsync_probe".to_string())
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
    fn decode_window_reports_dts_backend_gap() {
        let request = SubtitleSyncDecodeWindowRequest {
            codec: Some(AudioCodec::Dts),
            packets: vec![AudioPacket {
                pts_ms: Some(0),
                data_base64: BASE64.encode([0x7f, 0xfe, 0x80, 0x01]),
            }],
            target_sample_rate_hz: Some(16_000),
            mixdown_mono: true,
        };

        let response = decode_window_impl(&request).expect("decode window response");
        assert_eq!(response.status, DecodeWindowStatus::Unsupported);
        assert_eq!(response.codec, Some(AudioCodec::Dts));
        assert_eq!(response.sample_rate_hz, None);
        assert_eq!(response.channels, None);
        assert!(response.pcm_f32le_base64.is_none());
    }

    #[test]
    fn decode_window_decodes_ac3_fixture() {
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

    #[test]
    fn decode_window_decodes_truehd_example_data() {
        let request = SubtitleSyncDecodeWindowRequest {
            codec: Some(AudioCodec::TrueHd),
            packets: vec![AudioPacket {
                pts_ms: Some(0),
                data_base64: BASE64.encode(truehd::process::EXAMPLE_DATA),
            }],
            target_sample_rate_hz: None,
            mixdown_mono: true,
        };

        let response = decode_window_impl(&request).expect("decode window response");
        assert_eq!(response.status, DecodeWindowStatus::Decoded);
        assert_eq!(response.codec, Some(AudioCodec::TrueHd));
        assert!(response.sample_rate_hz.is_some());
        assert_eq!(response.channels, Some(1));
        assert!(response.samples_decoded > 0);
        assert!(response.pcm_f32le_base64.is_some());
    }
}
