use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use scryer_plugin_sdk::{
    AudioStreamSelector as SdkAudioStreamSelector, EXPORT_SUBSYNC_ALIGN, PluginDescriptor,
    PluginError, PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
    SubtitleCapabilities, SubtitleDescriptor, SubtitlePluginGenerateRequest,
    SubtitlePluginGenerateResponse, SubtitlePluginValidateConfigRequest,
    SubtitlePluginValidateConfigResponse, SubtitleProviderMode, SubtitleQueryMediaKind,
    SubtitleSyncAlignRequest, SubtitleSyncAlignResponse, SubtitleSyncAlignSkipReason,
    SubtitleSyncAudioCodec as SdkSubtitleSyncAudioCodec, SubtitleSyncRewrittenSubtitle,
    SubtitleValidateConfigStatus, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

mod ffmpeg_backend;
mod subtitle_sync;

const PLUGIN_ID: &str = "enhanced-subtitle-sync";
const PLUGIN_NAME: &str = "Enhanced Subtitle Sync";
const DECODER_BACKEND: &str = "vendored-ffmpeg-wasm";
const SYMPHONIA_BACKEND: &str = "plugin-symphonia";
const FFMPEG_SYNC_BACKEND: &str = "vendored-ffmpeg-wasm";
const REFERENCE_SUBTITLE_BACKEND: &str = "reference-subtitle";
const SUBTITLE_SYNC_BACKEND: &str = "subtitle-sync-rust";
const MAX_DECODE_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MIN_EFFECTIVE_OFFSET_MS: i64 = 50;

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

#[plugin_fn]
pub fn scryer_subsync_align(input: String) -> FnResult<String> {
    scryer_subsync_align_json(input)
}

fn scryer_subsync_align_json(input: String) -> FnResult<String> {
    let request: SubtitleSyncAlignRequest = serde_json::from_str(&input)?;
    let response = align_impl(&request);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
            EXPORT_SUBSYNC_ALIGN.to_string(),
        ],
        capabilities: SubtitleSyncCapabilities {
            backend: format!(
                "{DECODER_BACKEND}+{SUBTITLE_SYNC_BACKEND}+{}",
                subtitle_sync::WEBRTC_BACKEND_LABEL
            ),
            decode_status: DecodeBackendStatus::Complete,
            supported_codecs: AudioCodec::all().to_vec(),
            decoded_codecs: AudioCodec::all().to_vec(),
            pending_codecs: vec![],
            output_sample_format: "f32le".to_string(),
            supports_mono_mixdown: true,
        },
    }
}

#[derive(Debug, Clone, Default)]
struct SkippedAlignmentDetails {
    offset_ms: i64,
    consistency_ratio: Option<f64>,
    nosplit_score: Option<f64>,
    split_score: Option<f64>,
    message: Option<String>,
}

fn align_impl(request: &SubtitleSyncAlignRequest) -> SubtitleSyncAlignResponse {
    let subtitle_content = match BASE64.decode(&request.subtitle.content_base64) {
        Ok(content) => content,
        Err(error) => {
            return skipped_align_response(
                SubtitleSyncAlignSkipReason::WeakAlignment,
                request_backend_label(request).to_string(),
                Vec::new(),
                SkippedAlignmentDetails {
                    message: Some(format!("invalid subtitle content_base64: {error}")),
                    ..Default::default()
                },
            );
        }
    };
    let request_sync_options = request.sync_options.clone().unwrap_or_default();
    let sync_options = subtitle_sync::SyncOptions {
        max_offset_seconds: request.max_offset_seconds,
        min_effective_offset_ms: MIN_EFFECTIVE_OFFSET_MS,
        start_seconds: request_sync_options.start_seconds,
        max_subtitle_duration_ms: request_sync_options.max_subtitle_duration_ms as i64,
        precise_framerate_search: request_sync_options.precise_framerate_search,
        output_encoding: request_sync_options.output_encoding,
    };

    let media_failure = match extract_reference_spans(request) {
        Ok((backend, warnings, reference_spans)) => {
            match align_with_reference_spans(
                backend,
                warnings,
                reference_spans,
                &subtitle_content,
                request,
                &sync_options,
            ) {
                Ok(response) => return response,
                Err(failure) if !should_try_reference_subtitle_fallback(&failure) => {
                    return failure.into_response();
                }
                Err(failure) => Some(failure),
            }
        }
        Err(message) => Some(AlignmentFailure {
            skipped_reason: SubtitleSyncAlignSkipReason::AudioDecodeFailed,
            backend: request_backend_label(request).to_string(),
            warnings: Vec::new(),
            details: SkippedAlignmentDetails {
                message: Some(message),
                ..Default::default()
            },
        }),
    };

    let Some(reference_subtitle) = request.reference_subtitle.as_ref() else {
        return media_failure.expect("media failure set").into_response();
    };
    let mut fallback_warnings = media_failure
        .as_ref()
        .map(|failure| failure.warnings.clone())
        .unwrap_or_default();
    if let Some(failure) = media_failure.as_ref() {
        fallback_warnings.push(format!(
            "media reference alignment failed on {}: {}",
            failure.backend,
            failure
                .details
                .message
                .as_deref()
                .unwrap_or("alignment was not usable")
        ));
    }
    match align_with_reference_subtitle(
        reference_subtitle,
        fallback_warnings,
        &subtitle_content,
        request,
        &sync_options,
    ) {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    }
}

struct AlignmentFailure {
    skipped_reason: SubtitleSyncAlignSkipReason,
    backend: String,
    warnings: Vec<String>,
    details: SkippedAlignmentDetails,
}

impl AlignmentFailure {
    fn into_response(self) -> SubtitleSyncAlignResponse {
        skipped_align_response(
            self.skipped_reason,
            self.backend,
            self.warnings,
            self.details,
        )
    }
}

fn align_with_reference_spans(
    backend: String,
    mut warnings: Vec<String>,
    reference_spans: Vec<subtitle_sync::Span>,
    subtitle_content: &[u8],
    request: &SubtitleSyncAlignRequest,
    sync_options: &subtitle_sync::SyncOptions,
) -> Result<SubtitleSyncAlignResponse, AlignmentFailure> {
    let sync = match subtitle_sync::sync_subtitle(
        &reference_spans,
        &subtitle_content,
        &request.subtitle.format,
        request.subtitle.encoding_hint.as_deref(),
        sync_options,
    ) {
        Ok(sync) => sync,
        Err(error) => {
            return Err(AlignmentFailure {
                skipped_reason: skipped_reason_for_sync_error(&error),
                backend,
                warnings,
                details: skipped_details_for_sync_error(&error),
            });
        }
    };
    warnings.extend(sync.warnings);

    Ok(SubtitleSyncAlignResponse {
        applied: true,
        offset_ms: sync.offset_ms,
        rewritten_subtitle: Some(SubtitleSyncRewrittenSubtitle {
            content_base64: BASE64.encode(&sync.rewritten_content),
            format: sync.output_format,
        }),
        score: Some(sync.score),
        selected_framerate_ratio: Some(sync.selected_framerate_ratio),
        consistency_ratio: Some(1.0),
        nosplit_score: Some(sync.score),
        split_score: None,
        skipped_reason: None,
        backend: format!("{backend}+{}", subtitle_sync::simd::backend_suffix()),
        warnings,
        message: Some(format!(
            "aligned {} reference speech spans and {} subtitle spans over {:.3}s at framerate ratio {:.6}",
            sync.reference_span_count,
            sync.subtitle_span_count,
            sync.subtitle_max_time_seconds,
            sync.selected_framerate_ratio
        )),
    })
}

fn align_with_reference_subtitle(
    reference_subtitle: &scryer_plugin_sdk::SubtitleSyncReferenceSubtitle,
    mut warnings: Vec<String>,
    subtitle_content: &[u8],
    request: &SubtitleSyncAlignRequest,
    sync_options: &subtitle_sync::SyncOptions,
) -> Result<SubtitleSyncAlignResponse, AlignmentFailure> {
    let content = BASE64
        .decode(&reference_subtitle.content_base64)
        .map_err(|error| AlignmentFailure {
            skipped_reason: SubtitleSyncAlignSkipReason::WeakAlignment,
            backend: REFERENCE_SUBTITLE_BACKEND.to_string(),
            warnings: warnings.clone(),
            details: SkippedAlignmentDetails {
                message: Some(format!(
                    "invalid reference_subtitle content_base64: {error}"
                )),
                ..Default::default()
            },
        })?;
    let (reference_spans, reference_warnings) = subtitle_sync::subtitle_reference_spans(
        &content,
        &reference_subtitle.format,
        reference_subtitle.encoding_hint.as_deref(),
        sync_options,
    )
    .map_err(|error| AlignmentFailure {
        skipped_reason: skipped_reason_for_sync_error(&error),
        backend: REFERENCE_SUBTITLE_BACKEND.to_string(),
        warnings: warnings.clone(),
        details: skipped_details_for_sync_error(&error),
    })?;
    warnings.extend(reference_warnings);
    align_with_reference_spans(
        REFERENCE_SUBTITLE_BACKEND.to_string(),
        warnings,
        reference_spans,
        subtitle_content,
        request,
        sync_options,
    )
}

fn should_try_reference_subtitle_fallback(failure: &AlignmentFailure) -> bool {
    !matches!(
        failure.skipped_reason,
        SubtitleSyncAlignSkipReason::OffsetTooSmall
    )
}

fn skipped_reason_for_sync_error(error: &subtitle_sync::SyncError) -> SubtitleSyncAlignSkipReason {
    match error {
        subtitle_sync::SyncError::NotEnoughReferenceSpans { .. } => {
            SubtitleSyncAlignSkipReason::NotEnoughReferenceSpans
        }
        subtitle_sync::SyncError::OffsetTooSmall { .. } => {
            SubtitleSyncAlignSkipReason::OffsetTooSmall
        }
        subtitle_sync::SyncError::OffsetExceedsMaximum { .. } => {
            SubtitleSyncAlignSkipReason::OffsetExceedsMaximum
        }
        subtitle_sync::SyncError::WeakAlignment { .. }
        | subtitle_sync::SyncError::Parse(_)
        | subtitle_sync::SyncError::NotEnoughSubtitleSpans { .. } => {
            SubtitleSyncAlignSkipReason::WeakAlignment
        }
    }
}

fn skipped_details_for_sync_error(error: &subtitle_sync::SyncError) -> SkippedAlignmentDetails {
    let (offset_ms, score, ratio) = match error {
        subtitle_sync::SyncError::OffsetTooSmall {
            offset_ms,
            score,
            ratio,
        }
        | subtitle_sync::SyncError::OffsetExceedsMaximum {
            offset_ms,
            score,
            ratio,
        } => (*offset_ms, Some(*score), Some(*ratio)),
        subtitle_sync::SyncError::WeakAlignment { offset_ms, score } => {
            (*offset_ms, Some(*score), None)
        }
        _ => (0, None, None),
    };
    let mut details = SkippedAlignmentDetails {
        offset_ms,
        nosplit_score: score,
        message: Some(error.to_string()),
        ..Default::default()
    };
    if let Some(ratio) = ratio {
        details.message = Some(format!("{} (framerate ratio {ratio:.6})", error));
    }
    details
}

fn extract_reference_spans(
    request: &SubtitleSyncAlignRequest,
) -> Result<(String, Vec<String>, Vec<subtitle_sync::Span>), String> {
    let selector = request
        .selector
        .as_ref()
        .map(map_sdk_selector)
        .unwrap_or(AudioStreamSelector::Default);
    if let Some(expected_codec) = request.expected_codec {
        let (metadata, detection) = decode_ffmpeg_sync_audio_to_speech_spans(
            &request.input.path,
            map_sdk_audio_codec(expected_codec),
            &selector,
        )?;
        let mut warnings = metadata.warnings;
        warnings.extend(detection.warnings);
        return Ok((
            format!("{FFMPEG_SYNC_BACKEND}+{}", detection.backend),
            warnings,
            detection.spans,
        ));
    }

    let detection = decode_audio_to_speech_spans(&request.input.path, &selector)?;
    Ok((
        format!("{SYMPHONIA_BACKEND}+{}", detection.backend),
        detection.warnings,
        detection.spans,
    ))
}

fn request_backend_label(request: &SubtitleSyncAlignRequest) -> &'static str {
    if request.expected_codec.is_some() {
        FFMPEG_SYNC_BACKEND
    } else {
        SYMPHONIA_BACKEND
    }
}

fn decode_ffmpeg_sync_audio_to_speech_spans(
    path: &Path,
    expected_codec: SyncAudioCodec,
    selector: &AudioStreamSelector,
) -> Result<
    (
        ffmpeg_backend::DecodedSyncAudio,
        subtitle_sync::SpeechDetection,
    ),
    String,
> {
    let mut detector = None;
    let metadata = ffmpeg_backend::decode_sync_audio(
        path,
        Some(expected_codec),
        selector,
        0,
        |samples, sample_rate_hz, channels| {
            let detector = detector
                .get_or_insert_with(|| subtitle_sync::SpeechSpanDetector::new(sample_rate_hz));
            detector.push_interleaved_i16(samples, usize::from(channels));
            Ok(())
        },
    )
    .map_err(|error| match error {
        ffmpeg_backend::DecodeFailure::Unsupported { message }
        | ffmpeg_backend::DecodeFailure::Error { message } => message,
    })?;
    let detection = detector
        .map(subtitle_sync::SpeechSpanDetector::finish)
        .ok_or_else(|| "vendored FFmpeg produced no sync PCM chunks".to_string())?;
    Ok((metadata, detection))
}

fn map_sdk_selector(selector: &SdkAudioStreamSelector) -> AudioStreamSelector {
    match selector {
        SdkAudioStreamSelector::Default => AudioStreamSelector::Default,
        SdkAudioStreamSelector::StreamIndex { index } => {
            AudioStreamSelector::StreamIndex { index: *index }
        }
        SdkAudioStreamSelector::Language { language } => AudioStreamSelector::Language {
            language: language.clone(),
        },
    }
}

fn map_sdk_audio_codec(codec: SdkSubtitleSyncAudioCodec) -> SyncAudioCodec {
    match codec {
        SdkSubtitleSyncAudioCodec::Ac3 => SyncAudioCodec::Ac3,
        SdkSubtitleSyncAudioCodec::Eac3 => SyncAudioCodec::Eac3,
        SdkSubtitleSyncAudioCodec::Dts => SyncAudioCodec::Dts,
        SdkSubtitleSyncAudioCodec::DtsHdMaCore => SyncAudioCodec::DtsHdMaCore,
        SdkSubtitleSyncAudioCodec::TrueHd => SyncAudioCodec::TrueHd,
    }
}

fn skipped_align_response(
    skipped_reason: SubtitleSyncAlignSkipReason,
    backend: String,
    warnings: Vec<String>,
    details: SkippedAlignmentDetails,
) -> SubtitleSyncAlignResponse {
    SubtitleSyncAlignResponse {
        applied: false,
        offset_ms: details.offset_ms,
        rewritten_subtitle: None,
        score: details.nosplit_score,
        selected_framerate_ratio: None,
        consistency_ratio: details.consistency_ratio,
        nosplit_score: details.nosplit_score,
        split_score: details.split_score,
        skipped_reason: Some(skipped_reason),
        backend,
        warnings,
        message: details.message,
    }
}

struct SelectedAudioTrack {
    id: u32,
    sample_rate: u32,
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
}

fn decode_audio_to_speech_spans(
    path: &Path,
    selector: &AudioStreamSelector,
) -> Result<subtitle_sync::SpeechDetection, String> {
    use symphonia::core::codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
    use symphonia::core::formats::{FormatOptions, TrackType, probe::Hint};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open media for subtitle sync: {error}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("audio probe failed: {error}"))?;

    let mut format = probed;
    let default_track_id = format.default_track(TrackType::Audio).map(|track| track.id);
    let mut best_track: Option<(i32, SelectedAudioTrack)> = None;

    for (track_index, track) in format.tracks().iter().enumerate() {
        let Some(priority) =
            track_selection_priority(track, track_index as u32, default_track_id, selector)
        else {
            continue;
        };

        let Some(audio_params) = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
        else {
            continue;
        };

        if audio_params.codec == CODEC_ID_NULL_AUDIO {
            continue;
        }

        let Ok(decoder) = symphonia::default::get_codecs()
            .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
        else {
            continue;
        };

        let sample_rate = audio_params.sample_rate.unwrap_or(44_100);
        let channel_count = audio_params
            .channels
            .as_ref()
            .map(|channels| channels.count())
            .unwrap_or(1);
        let priority = priority + channel_count as i32 + sample_rate as i32 / 1000;
        let selected = SelectedAudioTrack {
            id: track.id,
            sample_rate,
            decoder,
        };

        match &best_track {
            Some((best_priority, _)) if *best_priority >= priority => {}
            _ => best_track = Some((priority, selected)),
        }
    }

    let SelectedAudioTrack {
        id: track_id,
        sample_rate,
        mut decoder,
    } = best_track
        .map(|(_, track)| track)
        .ok_or_else(|| selector_unavailable_message(selector))?;
    let mut detector = subtitle_sync::SpeechSpanDetector::new(sample_rate);
    let mut decoded_samples = Vec::<i16>::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };

        let channels = decoded.spec().channels().count().max(1);
        decoded.copy_to_vec_interleaved(&mut decoded_samples);
        detector.push_interleaved_i16(&decoded_samples, channels);
    }

    Ok(detector.finish())
}

fn track_selection_priority(
    track: &symphonia::core::formats::Track,
    track_index: u32,
    default_track_id: Option<u32>,
    selector: &AudioStreamSelector,
) -> Option<i32> {
    if !track_matches_selector(track, track_index, selector) {
        return None;
    }

    let mut priority = 0;
    if Some(track.id) == default_track_id {
        priority += 10_000;
    }
    if track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .and_then(|params| params.sample_rate)
        .is_some()
    {
        priority += 1_000;
    }
    if track.language.is_some() {
        priority += 10;
    }
    Some(priority)
}

fn track_matches_selector(
    track: &symphonia::core::formats::Track,
    track_index: u32,
    selector: &AudioStreamSelector,
) -> bool {
    match selector {
        AudioStreamSelector::Default => true,
        AudioStreamSelector::StreamIndex { index } => *index == track_index,
        AudioStreamSelector::Language { language } => track
            .language
            .as_ref()
            .is_some_and(|track_language| track_language.eq_ignore_ascii_case(language)),
    }
}

fn selector_unavailable_message(selector: &AudioStreamSelector) -> String {
    match selector {
        AudioStreamSelector::Default => "no decodable audio track found".to_string(),
        AudioStreamSelector::StreamIndex { index } => {
            format!("requested audio stream index {index} was not decodable")
        }
        AudioStreamSelector::Language { language } => {
            format!("requested audio stream language '{language}' was not decodable")
        }
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

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SyncAudioCodec {
    Ac3 = 0,
    Eac3 = 1,
    Dts = 2,
    DtsHdMaCore = 3,
    TrueHd = 4,
}

impl SyncAudioCodec {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AudioStreamSelector {
    Default,
    StreamIndex { index: u32 },
    Language { language: String },
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
    use std::path::PathBuf;
    use symphonia::core::codecs::audio::well_known::{
        CODEC_ID_AC3, CODEC_ID_DCA, CODEC_ID_EAC3, CODEC_ID_TRUEHD,
    };
    use symphonia::core::codecs::audio::{AudioCodecId, AudioCodecParameters};
    use symphonia::core::packet::Packet as SymphoniaPacket;
    use symphonia::core::units::{Duration, Timestamp};

    const TEST_DATA_FIXTURE_ROOT: &str = "tests/fixtures/test-data";
    const TEST_DATA_AAC_MEDIA: &str = "media/test-data-aac.mp4";

    #[derive(Debug, Clone, Copy)]
    struct SubtitleFixtureCase {
        name: &'static str,
        max_start_delta_ms: i64,
        min_ratio: Option<f64>,
        max_ratio: Option<f64>,
    }

    #[derive(Debug, Clone, Copy)]
    struct ParsedCue {
        start_ms: i64,
        end_ms: i64,
    }

    #[derive(Debug, Clone, Copy)]
    struct CodecFixtureCase {
        media_path: &'static str,
        expected_codec: SdkSubtitleSyncAudioCodec,
    }

    const SUBTITLE_CASES: [SubtitleFixtureCase; 3] = [
        SubtitleFixtureCase {
            name: "late_1750",
            max_start_delta_ms: 450,
            min_ratio: None,
            max_ratio: None,
        },
        SubtitleFixtureCase {
            name: "early_2200",
            max_start_delta_ms: 450,
            min_ratio: None,
            max_ratio: None,
        },
        SubtitleFixtureCase {
            name: "stretch_25_24",
            max_start_delta_ms: 700,
            min_ratio: Some(1.02),
            max_ratio: Some(1.06),
        },
    ];

    const CODEC_CASES: [CodecFixtureCase; 4] = [
        CodecFixtureCase {
            media_path: "media/test-data-ac3.mkv",
            expected_codec: SdkSubtitleSyncAudioCodec::Ac3,
        },
        CodecFixtureCase {
            media_path: "media/test-data-eac3.mkv",
            expected_codec: SdkSubtitleSyncAudioCodec::Eac3,
        },
        CodecFixtureCase {
            media_path: "media/test-data-dts.mkv",
            expected_codec: SdkSubtitleSyncAudioCodec::Dts,
        },
        CodecFixtureCase {
            media_path: "media/test-data-truehd.mkv",
            expected_codec: SdkSubtitleSyncAudioCodec::TrueHd,
        },
    ];

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

    fn test_data_fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(TEST_DATA_FIXTURE_ROOT)
            .join(relative)
    }

    fn test_data_subtitle_path(format: &str, case: &str) -> PathBuf {
        test_data_fixture_path(&format!("subtitles/{format}/{case}.{format}"))
    }

    fn align_test_data_fixture(
        media_relative_path: &str,
        subtitle_format: &str,
        subtitle_case: &str,
        expected_codec: Option<SdkSubtitleSyncAudioCodec>,
    ) -> SubtitleSyncAlignResponse {
        let subtitle_path = test_data_subtitle_path(subtitle_format, subtitle_case);
        let subtitle_content = std::fs::read(&subtitle_path).expect("read subtitle fixture");
        let request = SubtitleSyncAlignRequest {
            input: scryer_plugin_sdk::SubtitleSyncAlignInputRef {
                path: test_data_fixture_path(media_relative_path),
            },
            subtitle: scryer_plugin_sdk::SubtitleSyncInputSubtitle {
                content_base64: BASE64.encode(&subtitle_content),
                format: subtitle_format.to_string(),
                file_name: subtitle_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
                encoding_hint: Some("utf-8".to_string()),
            },
            reference_subtitle: None,
            subtitle_spans: Vec::new(),
            max_offset_seconds: 8,
            sync_options: Some(scryer_plugin_sdk::SubtitleSyncOptions {
                start_seconds: 0,
                max_subtitle_duration_ms: 10_000,
                precise_framerate_search: true,
                output_encoding: "same".to_string(),
            }),
            selector: Some(SdkAudioStreamSelector::Default),
            expected_codec,
        };
        align_request(request)
    }

    fn align_request(request: SubtitleSyncAlignRequest) -> SubtitleSyncAlignResponse {
        let raw_response =
            scryer_subsync_align_json(serde_json::to_string(&request).expect("encode request"))
                .expect("align export response");
        let PluginResult::Ok(response) =
            serde_json::from_str::<PluginResult<SubtitleSyncAlignResponse>>(&raw_response)
                .expect("decode plugin result")
        else {
            panic!("align export returned plugin error");
        };
        response
    }

    fn rewritten_subtitle_bytes(response: &SubtitleSyncAlignResponse) -> Vec<u8> {
        assert!(
            response.applied,
            "expected alignment to apply; response={response:?}"
        );
        let rewritten = response
            .rewritten_subtitle
            .as_ref()
            .expect("rewritten subtitle");
        BASE64
            .decode(&rewritten.content_base64)
            .expect("decode rewritten subtitle")
    }

    fn assert_rewrite_matches_authoritative(
        subtitle_format: &str,
        fixture_case: SubtitleFixtureCase,
        response: &SubtitleSyncAlignResponse,
    ) {
        let rewritten = rewritten_subtitle_bytes(response);
        let authoritative = std::fs::read(test_data_subtitle_path(subtitle_format, "aligned"))
            .expect("read aligned");
        let actual = parse_fixture_cues(subtitle_format, &rewritten);
        let expected = parse_fixture_cues(subtitle_format, &authoritative);

        assert_eq!(
            actual.len(),
            expected.len(),
            "{subtitle_format} {} cue count mismatch\n{}",
            fixture_case.name,
            String::from_utf8_lossy(&rewritten)
        );
        for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (actual.start_ms - expected.start_ms).abs() <= fixture_case.max_start_delta_ms,
                "{subtitle_format} {} cue {} start {} not within {}ms of {}",
                fixture_case.name,
                index + 1,
                actual.start_ms,
                fixture_case.max_start_delta_ms,
                expected.start_ms
            );
            assert!(
                (actual.end_ms - expected.end_ms).abs() <= fixture_case.max_start_delta_ms + 250,
                "{subtitle_format} {} cue {} end {} not within {}ms of {}",
                fixture_case.name,
                index + 1,
                actual.end_ms,
                fixture_case.max_start_delta_ms + 250,
                expected.end_ms
            );
        }

        match fixture_case.name {
            "late_1750" => assert!(
                response.offset_ms < -500,
                "expected negative offset for late subtitles: {response:?}"
            ),
            "early_2200" => assert!(
                response.offset_ms > 500,
                "expected positive offset for early subtitles: {response:?}"
            ),
            _ => {}
        }
        if let Some(min_ratio) = fixture_case.min_ratio {
            let ratio = response.selected_framerate_ratio.expect("selected ratio");
            assert!(
                ratio >= min_ratio,
                "ratio {ratio} below {min_ratio}: {response:?}"
            );
        }
        if let Some(max_ratio) = fixture_case.max_ratio {
            let ratio = response.selected_framerate_ratio.expect("selected ratio");
            assert!(
                ratio <= max_ratio,
                "ratio {ratio} above {max_ratio}: {response:?}"
            );
        }
    }

    fn parse_fixture_cues(format: &str, bytes: &[u8]) -> Vec<ParsedCue> {
        let content = String::from_utf8_lossy(bytes);
        match format {
            "srt" | "vtt" => content
                .lines()
                .filter_map(parse_arrow_timing_line)
                .collect(),
            "ass" | "ssa" => content
                .lines()
                .filter_map(parse_ass_fixture_timing_line)
                .collect(),
            other => panic!("unsupported fixture format {other}"),
        }
    }

    fn parse_arrow_timing_line(line: &str) -> Option<ParsedCue> {
        let (start, rest) = line.split_once("-->")?;
        let end = rest
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or_default();
        Some(ParsedCue {
            start_ms: parse_fixture_ts(start.trim())?,
            end_ms: parse_fixture_ts(end)?,
        })
    }

    fn parse_ass_fixture_timing_line(line: &str) -> Option<ParsedCue> {
        let trimmed = line.trim_start();
        if !trimmed
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Dialogue:"))
        {
            return None;
        }
        let (_, body) = trimmed.split_once(':')?;
        let fields = body.trim_start().splitn(4, ',').collect::<Vec<_>>();
        if fields.len() < 3 {
            return None;
        }
        Some(ParsedCue {
            start_ms: parse_fixture_ts(fields[1].trim())?,
            end_ms: parse_fixture_ts(fields[2].trim())?,
        })
    }

    fn parse_fixture_ts(value: &str) -> Option<i64> {
        let (time, fraction) = value.trim().split_once([',', '.'])?;
        let parts = time.split(':').collect::<Vec<_>>();
        let (hours, minutes, seconds) = match parts.as_slice() {
            [minutes, seconds] => (
                0,
                minutes.parse::<i64>().ok()?,
                seconds.parse::<i64>().ok()?,
            ),
            [hours, minutes, seconds] => (
                hours.parse::<i64>().ok()?,
                minutes.parse::<i64>().ok()?,
                seconds.parse::<i64>().ok()?,
            ),
            _ => return None,
        };
        let digits = fraction
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .take(3)
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        let value = digits.parse::<i64>().ok()?;
        let millis = match digits.len() {
            1 => value * 100,
            2 => value * 10,
            _ => value,
        };
        Some((((hours * 60 + minutes) * 60 + seconds) * 1000) + millis)
    }

    #[test]
    fn descriptor_advertises_vendored_ffmpeg_sync_contract() {
        let descriptor = sync_descriptor();
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.plugin_type, "subtitle_sync");
        assert_eq!(
            descriptor.capabilities.backend,
            format!(
                "{DECODER_BACKEND}+{SUBTITLE_SYNC_BACKEND}+{}",
                subtitle_sync::WEBRTC_BACKEND_LABEL
            )
        );
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
                .contains(&EXPORT_SUBSYNC_ALIGN.to_string())
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
    fn ffmpeg_sync_decoder_streams_ac3_fixture_to_pcm_chunks() {
        let input_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine440_stereo.ac3");
        let mut chunks = 0usize;
        let mut sample_count = 0usize;
        let decoded = ffmpeg_backend::decode_sync_audio(
            &input_path,
            Some(SyncAudioCodec::Ac3),
            &AudioStreamSelector::Default,
            20 * 16_000,
            |samples, sample_rate_hz, channels| {
                assert_eq!(sample_rate_hz, 16_000);
                assert_eq!(channels, 1);
                chunks += 1;
                sample_count += samples.len();
                Ok(())
            },
        )
        .expect("stream sync PCM");

        assert_eq!(decoded.sample_rate_hz, 16_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.codec, SyncAudioCodec::Ac3);
        assert_eq!(decoded.samples_decoded, sample_count as u64);
        assert!(decoded.duration_ms > 0);
        assert_eq!(decoded.timeline_start_ms, 0);
        assert!(decoded.source_codec_name.is_some());
        assert!(decoded.language.is_none());
        assert!(decoded.message.is_some());
        assert!(chunks > 0);
    }

    #[test]
    #[ignore = "requires local media files; set SCRYER_ENHANCED_SYNC_ACCEPT_* env vars"]
    fn local_media_streams_targeted_codecs_to_sync_pcm() {
        let cases = [
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_AC3",
                SyncAudioCodec::Ac3,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_EAC3",
                SyncAudioCodec::Eac3,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_DTS",
                SyncAudioCodec::Dts,
                false,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_DTS_HD_MA",
                SyncAudioCodec::DtsHdMaCore,
                true,
            ),
            (
                "SCRYER_ENHANCED_SYNC_ACCEPT_TRUEHD",
                SyncAudioCodec::TrueHd,
                false,
            ),
        ];

        let mut ran = 0usize;
        for (env_name, expected_codec, expect_core_fallback) in cases {
            let Ok(input_path) = std::env::var(env_name) else {
                eprintln!("skipping {env_name}; env var not set");
                continue;
            };
            ran += 1;

            let mut chunks = 0usize;
            let mut sample_count = 0usize;
            let decoded = ffmpeg_backend::decode_sync_audio(
                &PathBuf::from(&input_path),
                Some(expected_codec),
                &AudioStreamSelector::Default,
                20 * 16_000,
                |samples, sample_rate_hz, channels| {
                    assert_eq!(sample_rate_hz, 16_000, "{env_name}");
                    assert_eq!(channels, 1, "{env_name}");
                    chunks += 1;
                    sample_count += samples.len();
                    Ok(())
                },
            )
            .unwrap_or_else(|error| {
                panic!("{env_name} failed for {input_path}: {error:?}");
            });

            assert_eq!(decoded.sample_rate_hz, 16_000, "{env_name}");
            assert_eq!(decoded.channels, 1, "{env_name}");
            assert_eq!(decoded.samples_decoded, sample_count as u64, "{env_name}");
            assert!(chunks > 0, "{env_name}");

            assert_eq!(decoded.codec, expected_codec, "{env_name}");
            assert_eq!(
                decoded.used_core_fallback, expect_core_fallback,
                "{env_name}"
            );

            eprintln!(
                "{env_name}: decoded {:?} from stream {} into {} samples ({:?})",
                decoded.codec,
                decoded.stream_index,
                decoded.samples_decoded,
                decoded.source_profile
            );
        }

        assert!(
            ran > 0,
            "set at least one SCRYER_ENHANCED_SYNC_ACCEPT_* env var"
        );
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
    fn test_data_aac_aligns_all_subtitle_formats_and_desync_cases() {
        for subtitle_format in ["srt", "vtt", "ass", "ssa"] {
            for fixture_case in SUBTITLE_CASES {
                let response = align_test_data_fixture(
                    TEST_DATA_AAC_MEDIA,
                    subtitle_format,
                    fixture_case.name,
                    None,
                );
                assert!(
                    response.backend.contains(SYMPHONIA_BACKEND),
                    "{subtitle_format} {} backend: {}",
                    fixture_case.name,
                    response.backend
                );
                assert!(
                    response
                        .backend
                        .contains(subtitle_sync::WEBRTC_BACKEND_LABEL),
                    "{subtitle_format} {} backend: {}",
                    fixture_case.name,
                    response.backend
                );
                assert_eq!(
                    response
                        .rewritten_subtitle
                        .as_ref()
                        .map(|subtitle| subtitle.format.as_str()),
                    Some(subtitle_format)
                );
                assert_rewrite_matches_authoritative(subtitle_format, fixture_case, &response);
            }
        }
    }

    #[test]
    fn test_data_hard_codecs_align_through_streamed_ffmpeg_pcm() {
        let fixture_case = SUBTITLE_CASES[0];
        for codec_case in CODEC_CASES {
            let response = align_test_data_fixture(
                codec_case.media_path,
                "srt",
                fixture_case.name,
                Some(codec_case.expected_codec),
            );
            assert!(
                response.backend.contains(FFMPEG_SYNC_BACKEND),
                "{} backend: {}",
                codec_case.media_path,
                response.backend
            );
            assert!(
                response
                    .backend
                    .contains(subtitle_sync::WEBRTC_BACKEND_LABEL),
                "{} backend: {}",
                codec_case.media_path,
                response.backend
            );
            assert_rewrite_matches_authoritative("srt", fixture_case, &response);
        }
    }

    #[test]
    fn reference_subtitle_fallback_aligns_srt_and_vtt_when_media_fails() {
        for subtitle_format in ["srt", "vtt"] {
            let subtitle_path = test_data_subtitle_path(subtitle_format, "late_1750");
            let reference_path = test_data_subtitle_path(subtitle_format, "aligned");
            let subtitle_content = std::fs::read(&subtitle_path).expect("read subtitle fixture");
            let reference_content = std::fs::read(&reference_path).expect("read reference fixture");
            let request = SubtitleSyncAlignRequest {
                input: scryer_plugin_sdk::SubtitleSyncAlignInputRef {
                    path: test_data_fixture_path("media/missing-reference.mp4"),
                },
                subtitle: scryer_plugin_sdk::SubtitleSyncInputSubtitle {
                    content_base64: BASE64.encode(&subtitle_content),
                    format: subtitle_format.to_string(),
                    file_name: subtitle_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string),
                    encoding_hint: Some("utf-8".to_string()),
                },
                reference_subtitle: Some(scryer_plugin_sdk::SubtitleSyncReferenceSubtitle {
                    content_base64: BASE64.encode(&reference_content),
                    format: subtitle_format.to_string(),
                    file_name: reference_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string),
                    encoding_hint: Some("utf-8".to_string()),
                }),
                subtitle_spans: Vec::new(),
                max_offset_seconds: 8,
                sync_options: Some(scryer_plugin_sdk::SubtitleSyncOptions::default()),
                selector: Some(SdkAudioStreamSelector::Default),
                expected_codec: None,
            };

            let response = align_request(request);
            assert!(
                response.backend.contains(REFERENCE_SUBTITLE_BACKEND),
                "{subtitle_format} backend: {}",
                response.backend
            );
            assert!(
                response
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("media reference alignment failed")),
                "{subtitle_format} warnings: {:?}",
                response.warnings
            );
            assert_rewrite_matches_authoritative(subtitle_format, SUBTITLE_CASES[0], &response);
        }
    }

    #[test]
    fn missing_media_without_reference_subtitle_skips_alignment() {
        let response = align_request(SubtitleSyncAlignRequest {
            input: scryer_plugin_sdk::SubtitleSyncAlignInputRef {
                path: test_data_fixture_path("media/missing-reference.mp4"),
            },
            subtitle: scryer_plugin_sdk::SubtitleSyncInputSubtitle {
                content_base64: BASE64.encode(b"1\n00:00:01,000 --> 00:00:02,000\nHello\n"),
                format: "srt".to_string(),
                file_name: Some("subtitle.srt".to_string()),
                encoding_hint: Some("utf-8".to_string()),
            },
            reference_subtitle: None,
            subtitle_spans: Vec::new(),
            max_offset_seconds: 8,
            sync_options: Some(scryer_plugin_sdk::SubtitleSyncOptions::default()),
            selector: Some(SdkAudioStreamSelector::Default),
            expected_codec: None,
        });

        assert!(!response.applied);
        assert_eq!(
            response.skipped_reason,
            Some(SubtitleSyncAlignSkipReason::AudioDecodeFailed)
        );
    }

    #[test]
    fn generated_wav_decodes_to_webrtc_vad_speech_spans() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let input_path = temp_dir.path().join("generated-dialogue.wav");
        write_generated_voice_wav(&input_path, 48_000, 2);

        let detection = decode_audio_to_speech_spans(&input_path, &AudioStreamSelector::Default)
            .expect("decode generated wav");

        assert_eq!(detection.backend, subtitle_sync::WEBRTC_BACKEND_LABEL);
        assert!(detection.warnings.is_empty());
        assert!(detection.spans.len() >= 2, "{:?}", detection.spans);
        assert_generated_span_near(detection.spans[0], 400, 1_400, 220);
        assert_generated_span_near(detection.spans[1], 1_900, 2_800, 220);
    }

    fn write_generated_voice_wav(path: &std::path::Path, sample_rate_hz: u32, channels: u16) {
        let segments = [
            (400usize, false),
            (1_000usize, true),
            (500usize, false),
            (900usize, true),
            (400usize, false),
        ];
        let mut pcm = Vec::<i16>::new();
        let mut absolute_sample = 0usize;
        for (duration_ms, voiced) in segments {
            let sample_count = sample_rate_hz as usize * duration_ms / 1000;
            for _ in 0..sample_count {
                let sample = if voiced {
                    generated_voice_sample(absolute_sample, sample_rate_hz)
                } else {
                    0
                };
                absolute_sample += 1;
                for channel in 0..channels {
                    pcm.push(if channel == 0 { sample } else { sample / 2 });
                }
            }
        }

        let data_bytes = (pcm.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
        wav.extend_from_slice(&(sample_rate_hz * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in pcm {
            wav.extend_from_slice(&sample.to_le_bytes());
        }

        std::fs::write(path, wav).expect("write generated wav");
    }

    fn generated_voice_sample(absolute_sample: usize, sample_rate_hz: u32) -> i16 {
        let t = absolute_sample as f32 / sample_rate_hz as f32;
        let f0 = 115.0 + 20.0 * (std::f32::consts::TAU * 3.0 * t).sin();
        let mut value = 0.0;
        for harmonic in 1..80 {
            let frequency = f0 * harmonic as f32;
            if frequency > 3_600.0 {
                break;
            }

            let mut amplitude = 0.02 / harmonic as f32;
            for (center, bandwidth, gain) in [
                (730.0, 80.0, 1.0),
                (1_090.0, 90.0, 0.7),
                (2_440.0, 120.0, 0.45),
                (3_300.0, 180.0, 0.25),
            ] {
                let distance = (frequency - center) / bandwidth;
                amplitude += gain / (1.0 + distance * distance) / harmonic as f32;
            }

            value += amplitude * (std::f32::consts::TAU * frequency * t).sin();
        }

        (value.tanh() * 10_000.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }

    fn assert_generated_span_near(
        span: subtitle_sync::Span,
        expected_start_ms: i64,
        expected_end_ms: i64,
        tolerance_ms: i64,
    ) {
        assert!(
            (span.start_ms - expected_start_ms).abs() <= tolerance_ms,
            "start {} not within {tolerance_ms}ms of {expected_start_ms}",
            span.start_ms
        );
        assert!(
            (span.end_ms - expected_end_ms).abs() <= tolerance_ms,
            "end {} not within {tolerance_ms}ms of {expected_end_ms}",
            span.end_ms
        );
    }
}
