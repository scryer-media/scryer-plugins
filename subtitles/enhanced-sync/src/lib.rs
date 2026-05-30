use alass_core::{NoProgressHandler, TimeDelta, TimePoint, TimeSpan};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extism_pdk::*;
use scryer_plugin_sdk::{
    AudioStreamSelector as SdkAudioStreamSelector, AudioTranscodeCodec as SdkAudioTranscodeCodec,
    EXPORT_SUBSYNC_ALIGN, PluginDescriptor, PluginError, PluginErrorCode, PluginResult,
    ProviderDescriptor, SDK_VERSION, SubtitleCapabilities, SubtitleDescriptor,
    SubtitlePluginGenerateRequest, SubtitlePluginGenerateResponse,
    SubtitlePluginValidateConfigRequest, SubtitlePluginValidateConfigResponse,
    SubtitleProviderMode, SubtitleQueryMediaKind, SubtitleSyncAlignRequest,
    SubtitleSyncAlignResponse, SubtitleSyncAlignSkipReason, SubtitleTimingSpan,
    SubtitleValidateConfigStatus, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod ffmpeg_backend;

const PLUGIN_ID: &str = "enhanced-subtitle-sync";
const PLUGIN_NAME: &str = "Enhanced Subtitle Sync";
const DECODER_BACKEND: &str = "vendored-ffmpeg-wasm";
const SYMPHONIA_BACKEND: &str = "plugin-symphonia";
const HYBRID_BACKEND: &str = "vendored-ffmpeg-wasm+symphonia";
const MAX_DECODE_INPUT_BYTES: usize = 64 * 1024 * 1024;
const SYNC_SCRATCH_FLAC_PATH: &str = "/scratch/subsync-reference.flac";
const SPLIT_PENALTY: f64 = 7.0;
const MIN_REFERENCE_SPANS: usize = 3;
const MIN_EFFECTIVE_OFFSET_MS: i64 = 50;
const DELTA_CONSISTENCY_TOLERANCE_MS: i64 = 350;
const MIN_CONSISTENT_DELTA_RATIO: f64 = 0.5;
const WINDOW_MS: i64 = 10;
const VAD_START_THRESHOLD_MIN: f64 = 500.0;
const VAD_STOP_THRESHOLD_MIN: f64 = 250.0;
const VAD_START_MULTIPLIER: f64 = 3.0;
const VAD_STOP_MULTIPLIER: f64 = 1.8;
const VAD_NOISE_SMOOTHING: f64 = 0.05;
const VAD_MIN_SILENCE_WINDOWS: usize = 3;

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

#[plugin_fn]
pub fn scryer_subsync_align(input: String) -> FnResult<String> {
    let request: SubtitleSyncAlignRequest = serde_json::from_str(&input)?;
    let response = align_impl(&request);
    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
            EXPORT_SUBSYNC_ALIGN.to_string(),
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

#[derive(Debug, Clone, Copy)]
struct AlignmentSummary {
    offset_ms: i64,
    consistency_ratio: f64,
    nosplit_score: f64,
    split_score: f64,
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
    let subtitle_spans = request
        .subtitle_spans
        .iter()
        .filter_map(wire_span_to_alass)
        .collect::<Vec<_>>();
    let (backend, warnings, reference_spans) = match extract_reference_spans(request) {
        Ok(result) => result,
        Err(message) => {
            return skipped_align_response(
                SubtitleSyncAlignSkipReason::AudioDecodeFailed,
                request_backend_label(request).to_string(),
                Vec::new(),
                SkippedAlignmentDetails {
                    message: Some(message),
                    ..Default::default()
                },
            );
        }
    };

    if reference_spans.len() < MIN_REFERENCE_SPANS {
        return skipped_align_response(
            SubtitleSyncAlignSkipReason::NotEnoughReferenceSpans,
            backend,
            warnings,
            SkippedAlignmentDetails {
                message: Some(format!(
                    "decoded only {} reference speech spans",
                    reference_spans.len()
                )),
                ..Default::default()
            },
        );
    }

    let alignment = compute_alignment(&reference_spans, &subtitle_spans);
    if alignment.nosplit_score <= 0.0 || alignment.split_score <= 0.0 {
        return skipped_align_response(
            SubtitleSyncAlignSkipReason::WeakAlignment,
            backend,
            warnings,
            SkippedAlignmentDetails {
                offset_ms: alignment.offset_ms,
                consistency_ratio: Some(alignment.consistency_ratio),
                nosplit_score: Some(alignment.nosplit_score),
                split_score: Some(alignment.split_score),
                message: Some("alignment score too weak".to_string()),
            },
        );
    }

    if alignment.consistency_ratio < MIN_CONSISTENT_DELTA_RATIO {
        return skipped_align_response(
            SubtitleSyncAlignSkipReason::LowAlignmentConsistency,
            backend,
            warnings,
            SkippedAlignmentDetails {
                offset_ms: alignment.offset_ms,
                consistency_ratio: Some(alignment.consistency_ratio),
                nosplit_score: Some(alignment.nosplit_score),
                split_score: Some(alignment.split_score),
                message: Some("alignment consistency below threshold".to_string()),
            },
        );
    }

    if alignment.offset_ms.unsigned_abs() > (request.max_offset_seconds as u64 * 1000) {
        return skipped_align_response(
            SubtitleSyncAlignSkipReason::OffsetExceedsMaximum,
            backend,
            warnings,
            SkippedAlignmentDetails {
                offset_ms: alignment.offset_ms,
                consistency_ratio: Some(alignment.consistency_ratio),
                nosplit_score: Some(alignment.nosplit_score),
                split_score: Some(alignment.split_score),
                message: Some("alignment offset exceeds configured maximum".to_string()),
            },
        );
    }

    if alignment.offset_ms.unsigned_abs() < MIN_EFFECTIVE_OFFSET_MS as u64 {
        return skipped_align_response(
            SubtitleSyncAlignSkipReason::OffsetTooSmall,
            backend,
            warnings,
            SkippedAlignmentDetails {
                offset_ms: alignment.offset_ms,
                consistency_ratio: Some(alignment.consistency_ratio),
                nosplit_score: Some(alignment.nosplit_score),
                split_score: Some(alignment.split_score),
                message: Some("alignment offset too small to apply".to_string()),
            },
        );
    }

    SubtitleSyncAlignResponse {
        applied: true,
        offset_ms: alignment.offset_ms,
        consistency_ratio: Some(alignment.consistency_ratio),
        nosplit_score: Some(alignment.nosplit_score),
        split_score: Some(alignment.split_score),
        skipped_reason: None,
        backend,
        warnings,
        message: None,
    }
}

fn extract_reference_spans(
    request: &SubtitleSyncAlignRequest,
) -> Result<(String, Vec<String>, Vec<TimeSpan>), String> {
    let selector = request
        .selector
        .as_ref()
        .map(map_sdk_selector)
        .unwrap_or(AudioStreamSelector::Default);
    if let Some(expected_codec) = request.expected_codec {
        let transcoded = ffmpeg_backend::transcode_sync_flac(
            &request.input.path,
            Path::new(SYNC_SCRATCH_FLAC_PATH),
            Some(map_sdk_audio_codec(expected_codec)),
            &selector,
        )
        .map_err(|error| match error {
            ffmpeg_backend::TranscodeFailure::Unsupported { message }
            | ffmpeg_backend::TranscodeFailure::Error { message } => message,
        })?;
        let spans = decode_audio_to_speech_spans(
            Path::new(SYNC_SCRATCH_FLAC_PATH),
            &AudioStreamSelector::Default,
        )?;
        return Ok((HYBRID_BACKEND.to_string(), transcoded.warnings, spans));
    }

    Ok((
        SYMPHONIA_BACKEND.to_string(),
        Vec::new(),
        decode_audio_to_speech_spans(&request.input.path, &selector)?,
    ))
}

fn request_backend_label(request: &SubtitleSyncAlignRequest) -> &'static str {
    if request.expected_codec.is_some() {
        HYBRID_BACKEND
    } else {
        SYMPHONIA_BACKEND
    }
}

fn wire_span_to_alass(span: &SubtitleTimingSpan) -> Option<TimeSpan> {
    (span.end_ms > span.start_ms)
        .then(|| TimeSpan::new(TimePoint::from(span.start_ms), TimePoint::from(span.end_ms)))
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

fn map_sdk_audio_codec(codec: SdkAudioTranscodeCodec) -> AudioTranscodeCodec {
    match codec {
        SdkAudioTranscodeCodec::Ac3 => AudioTranscodeCodec::Ac3,
        SdkAudioTranscodeCodec::Eac3 => AudioTranscodeCodec::Eac3,
        SdkAudioTranscodeCodec::Dts => AudioTranscodeCodec::Dts,
        SdkAudioTranscodeCodec::DtsHdMaCore => AudioTranscodeCodec::DtsHdMaCore,
        SdkAudioTranscodeCodec::TrueHd => AudioTranscodeCodec::TrueHd,
    }
}

fn compute_alignment(
    reference_spans: &[TimeSpan],
    subtitle_spans: &[TimeSpan],
) -> AlignmentSummary {
    let (offset, nosplit_score) = alass_core::align_nosplit(
        reference_spans,
        subtitle_spans,
        alass_core::standard_scoring,
        NoProgressHandler,
    );
    let (split_deltas, split_score) = alass_core::align(
        reference_spans,
        subtitle_spans,
        SPLIT_PENALTY,
        None,
        alass_core::standard_scoring,
        NoProgressHandler,
    );

    let offset_ms = offset.as_i64();
    let consistency_ratio = delta_consistency_ratio(&split_deltas, offset_ms);
    AlignmentSummary {
        offset_ms,
        consistency_ratio,
        nosplit_score,
        split_score,
    }
}

fn delta_consistency_ratio(deltas: &[TimeDelta], offset_ms: i64) -> f64 {
    if deltas.is_empty() {
        return 0.0;
    }

    let consistent = deltas
        .iter()
        .filter(|delta| (delta.as_i64() - offset_ms).abs() <= DELTA_CONSISTENCY_TOLERANCE_MS)
        .count();
    consistent as f64 / deltas.len() as f64
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
) -> Result<Vec<TimeSpan>, String> {
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
    let mut detector = SpeechSpanDetector::new(sample_rate);
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

struct SpeechSpanDetector {
    samples_per_window: usize,
    frames_in_window: usize,
    window_energy_sum: f64,
    current_window_start_ms: i64,
    noise_floor: f64,
    noise_floor_initialized: bool,
    below_threshold_windows: usize,
    in_speech: bool,
    speech_start_ms: i64,
    spans: Vec<TimeSpan>,
}

impl SpeechSpanDetector {
    fn new(sample_rate: u32) -> Self {
        Self {
            samples_per_window: (sample_rate / 100).max(1) as usize,
            frames_in_window: 0,
            window_energy_sum: 0.0,
            current_window_start_ms: 0,
            noise_floor: 0.0,
            noise_floor_initialized: false,
            below_threshold_windows: 0,
            in_speech: false,
            speech_start_ms: 0,
            spans: Vec::new(),
        }
    }

    fn push_interleaved_i16(&mut self, samples: &[i16], channels: usize) {
        let channels = channels.max(1);
        for frame in samples.chunks_exact(channels) {
            let mean_sq = frame
                .iter()
                .map(|sample| {
                    let sample = *sample as f64;
                    sample * sample
                })
                .sum::<f64>()
                / channels as f64;
            self.push_frame_energy(mean_sq);
        }
    }

    fn push_frame_energy(&mut self, mean_sq: f64) {
        self.window_energy_sum += mean_sq;
        self.frames_in_window += 1;

        if self.frames_in_window >= self.samples_per_window {
            let rms = (self.window_energy_sum / self.frames_in_window as f64).sqrt();
            self.process_window(rms);
            self.frames_in_window = 0;
            self.window_energy_sum = 0.0;
        }
    }

    fn process_window(&mut self, rms: f64) {
        if !self.noise_floor_initialized {
            self.noise_floor = rms.clamp(1.0, VAD_START_THRESHOLD_MIN / VAD_START_MULTIPLIER);
            self.noise_floor_initialized = true;
        } else if !self.in_speech || rms < self.noise_floor * VAD_START_MULTIPLIER {
            self.noise_floor =
                (1.0 - VAD_NOISE_SMOOTHING) * self.noise_floor + VAD_NOISE_SMOOTHING * rms.max(1.0);
        }

        let start_threshold =
            (self.noise_floor * VAD_START_MULTIPLIER).max(VAD_START_THRESHOLD_MIN);
        let stop_threshold = (self.noise_floor * VAD_STOP_MULTIPLIER).max(VAD_STOP_THRESHOLD_MIN);
        let window_start_ms = self.current_window_start_ms;

        if rms > start_threshold {
            self.below_threshold_windows = 0;
            if !self.in_speech {
                self.in_speech = true;
                self.speech_start_ms = window_start_ms;
            }
        } else if self.in_speech && rms <= stop_threshold {
            self.below_threshold_windows += 1;
            if self.below_threshold_windows >= VAD_MIN_SILENCE_WINDOWS {
                let end_ms = window_start_ms - ((VAD_MIN_SILENCE_WINDOWS as i64 - 1) * WINDOW_MS);
                self.push_span(self.speech_start_ms, end_ms);
                self.in_speech = false;
                self.below_threshold_windows = 0;
            }
        } else if self.in_speech {
            self.below_threshold_windows = 0;
        }

        self.current_window_start_ms += WINDOW_MS;
    }

    fn finish(mut self) -> Vec<TimeSpan> {
        if self.frames_in_window > 0 {
            let rms = (self.window_energy_sum / self.frames_in_window as f64).sqrt();
            self.process_window(rms);
            self.frames_in_window = 0;
            self.window_energy_sum = 0.0;
        }

        if self.in_speech {
            self.push_span(self.speech_start_ms, self.current_window_start_ms);
            self.in_speech = false;
        }

        self.spans
    }

    fn push_span(&mut self, start_ms: i64, end_ms: i64) {
        if end_ms <= start_ms {
            return;
        }

        if let Some(last) = self.spans.last_mut() {
            let last_end = last.end.as_i64();
            if start_ms - last_end <= WINDOW_MS {
                *last = TimeSpan::new(last.start, TimePoint::from(end_ms));
                return;
            }
        }

        self.spans.push(TimeSpan::new(
            TimePoint::from(start_ms),
            TimePoint::from(end_ms),
        ));
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
                .contains(&EXPORT_SUBSYNC_ALIGN.to_string())
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
            assert!(transcoded.samples_written > 0, "{env_name}");

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
