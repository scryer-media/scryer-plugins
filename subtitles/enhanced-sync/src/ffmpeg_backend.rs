use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::slice;

use crate::{AudioCodec, AudioStreamSelector, AudioTranscodeCodec, DecodedPacket, DecodedPcm};

#[repr(C)]
struct FfiDecodeResult {
    status_code: i32,
    sample_rate_hz: u32,
    channels: u16,
    samples_decoded: u64,
    pcm_f32le: *mut u8,
    pcm_f32le_len: usize,
    message: [c_char; 256],
}

#[repr(C)]
struct FfiTranscodeResult {
    status_code: i32,
    stream_index: u32,
    codec: u32,
    sample_rate_hz: u32,
    channels: u16,
    samples_written: u64,
    duration_ms: i64,
    timeline_start_ms: i64,
    used_core_fallback: i32,
    source_codec_name: [c_char; 64],
    source_profile: [c_char; 64],
    language: [c_char; 32],
    message: [c_char; 256],
    warnings: [c_char; 512],
}

unsafe extern "C" {
    fn scryer_ffmpeg_decode_window(
        codec: u32,
        packet_data: *const *const u8,
        packet_lens: *const usize,
        pts_ms: *const i64,
        packet_count: usize,
        mixdown_mono: c_int,
        out: *mut FfiDecodeResult,
    ) -> i32;
    fn scryer_ffmpeg_transcode_sync_flac(
        input_path: *const c_char,
        output_path: *const c_char,
        requested_stream_index: c_int,
        language: *const c_char,
        expected_codec: u32,
        max_output_samples: u64,
        out: *mut FfiTranscodeResult,
    ) -> i32;
    fn scryer_ffmpeg_free(ptr: *mut c_void);
}

#[derive(Debug, Clone)]
pub(crate) struct TranscodedFlac {
    pub stream_index: u32,
    pub codec: AudioTranscodeCodec,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub samples_written: u64,
    pub duration_ms: i64,
    pub timeline_start_ms: i64,
    pub used_core_fallback: bool,
    pub source_codec_name: Option<String>,
    pub source_profile: Option<String>,
    pub language: Option<String>,
    pub warnings: Vec<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum TranscodeFailure {
    Unsupported { message: String },
    Error { message: String },
}

pub(crate) fn decode_window(
    codec: AudioCodec,
    packets: &[DecodedPacket],
    mixdown_mono: bool,
) -> Result<DecodedPcm, String> {
    let packet_data = packets
        .iter()
        .map(|packet| packet.data.as_ptr())
        .collect::<Vec<_>>();
    let packet_lens = packets
        .iter()
        .map(|packet| packet.data.len())
        .collect::<Vec<_>>();
    let pts_ms = packets
        .iter()
        .map(|packet| packet.pts_ms.unwrap_or(i64::MIN))
        .collect::<Vec<_>>();
    let pts_ptr = if pts_ms.iter().any(|pts| *pts != i64::MIN) {
        pts_ms.as_ptr()
    } else {
        ptr::null()
    };

    let mut result = FfiDecodeResult {
        status_code: 0,
        sample_rate_hz: 0,
        channels: 0,
        samples_decoded: 0,
        pcm_f32le: ptr::null_mut(),
        pcm_f32le_len: 0,
        message: [0; 256],
    };

    let status = unsafe {
        scryer_ffmpeg_decode_window(
            codec as u32,
            packet_data.as_ptr(),
            packet_lens.as_ptr(),
            pts_ptr,
            packets.len(),
            i32::from(mixdown_mono),
            &mut result,
        )
    };
    let message = ffi_message(&result);

    if status != 0 {
        if !result.pcm_f32le.is_null() {
            unsafe {
                scryer_ffmpeg_free(result.pcm_f32le.cast::<c_void>());
            }
        }
        return Err(message);
    }
    if result.pcm_f32le.is_null() || result.pcm_f32le_len == 0 {
        return Err("FFmpeg decoder returned an empty PCM buffer".to_string());
    }

    let pcm_f32le =
        unsafe { slice::from_raw_parts(result.pcm_f32le, result.pcm_f32le_len) }.to_vec();
    unsafe {
        scryer_ffmpeg_free(result.pcm_f32le.cast::<c_void>());
    }

    Ok(DecodedPcm {
        codec,
        sample_rate_hz: Some(result.sample_rate_hz),
        channels: Some(result.channels),
        samples_decoded: result.samples_decoded,
        pcm_f32le,
    })
}

fn ffi_message(result: &FfiDecodeResult) -> String {
    let message = ffi_string(&result.message)
        .to_string_lossy()
        .trim()
        .to_string();
    if message.is_empty() {
        "FFmpeg decoder failed without a message".to_string()
    } else {
        message
    }
}

pub(crate) fn transcode_sync_flac(
    input_path: &Path,
    output_path: &Path,
    expected_codec: Option<AudioTranscodeCodec>,
    selector: &AudioStreamSelector,
) -> Result<TranscodedFlac, TranscodeFailure> {
    transcode_sync_flac_inner(input_path, output_path, expected_codec, selector, 0)
}

#[cfg(test)]
pub(crate) fn transcode_sync_flac_with_sample_limit(
    input_path: &Path,
    output_path: &Path,
    expected_codec: Option<AudioTranscodeCodec>,
    selector: &AudioStreamSelector,
    max_output_samples: u64,
) -> Result<TranscodedFlac, TranscodeFailure> {
    transcode_sync_flac_inner(
        input_path,
        output_path,
        expected_codec,
        selector,
        max_output_samples,
    )
}

fn transcode_sync_flac_inner(
    input_path: &Path,
    output_path: &Path,
    expected_codec: Option<AudioTranscodeCodec>,
    selector: &AudioStreamSelector,
    max_output_samples: u64,
) -> Result<TranscodedFlac, TranscodeFailure> {
    let input_path = c_path(input_path).map_err(|message| TranscodeFailure::Error { message })?;
    let output_path = c_path(output_path).map_err(|message| TranscodeFailure::Error { message })?;
    let language_holder = match selector {
        AudioStreamSelector::Language { language: value } => Some(
            CString::new(value.as_str()).map_err(|_| TranscodeFailure::Error {
                message: "audio stream language selector contained a NUL byte".to_string(),
            })?,
        ),
        _ => None,
    };
    let (requested_stream_index, language_ptr) = match selector {
        AudioStreamSelector::Default => (-1, ptr::null()),
        AudioStreamSelector::StreamIndex { index } => {
            let index = i32::try_from(*index).map_err(|_| TranscodeFailure::Error {
                message: format!("stream index {index} exceeds supported range"),
            })?;
            (index, ptr::null())
        }
        AudioStreamSelector::Language { .. } => {
            (-1, language_holder.as_ref().expect("language set").as_ptr())
        }
    };

    let mut result = FfiTranscodeResult {
        status_code: 2,
        stream_index: 0,
        codec: u32::MAX,
        sample_rate_hz: 0,
        channels: 0,
        samples_written: 0,
        duration_ms: 0,
        timeline_start_ms: 0,
        used_core_fallback: 0,
        source_codec_name: [0; 64],
        source_profile: [0; 64],
        language: [0; 32],
        message: [0; 256],
        warnings: [0; 512],
    };

    let status = unsafe {
        scryer_ffmpeg_transcode_sync_flac(
            input_path.as_ptr(),
            output_path.as_ptr(),
            requested_stream_index,
            language_ptr,
            expected_codec.map_or(u32::MAX, |codec| codec as u32),
            max_output_samples,
            &mut result,
        )
    };
    let message = ffi_transcode_message(&result);

    match status {
        0 => {
            let Some(codec) = AudioTranscodeCodec::from_ffi(result.codec) else {
                return Err(TranscodeFailure::Error {
                    message: "FFmpeg transcoder returned an unknown codec id".to_string(),
                });
            };
            Ok(TranscodedFlac {
                stream_index: result.stream_index,
                codec,
                sample_rate_hz: result.sample_rate_hz,
                channels: result.channels,
                samples_written: result.samples_written,
                duration_ms: result.duration_ms,
                timeline_start_ms: result.timeline_start_ms,
                used_core_fallback: result.used_core_fallback != 0,
                source_codec_name: non_empty_ffi_string(&result.source_codec_name),
                source_profile: non_empty_ffi_string(&result.source_profile),
                language: non_empty_ffi_string(&result.language),
                warnings: split_warnings(&result.warnings),
                message: Some(message),
            })
        }
        1 => Err(TranscodeFailure::Unsupported { message }),
        _ => Err(TranscodeFailure::Error { message }),
    }
}

fn c_path(path: &Path) -> Result<CString, String> {
    CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| format!("path '{}' contained a NUL byte", path.display()))
}

fn ffi_transcode_message(result: &FfiTranscodeResult) -> String {
    non_empty_ffi_string(&result.message)
        .unwrap_or_else(|| "FFmpeg transcoder failed without a message".to_string())
}

fn non_empty_ffi_string<const N: usize>(value: &[c_char; N]) -> Option<String> {
    let value = ffi_string(value).to_string_lossy().trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn split_warnings(value: &[c_char; 512]) -> Vec<String> {
    non_empty_ffi_string(value)
        .map(|warnings| {
            warnings
                .split(';')
                .map(str::trim)
                .filter(|warning| !warning.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn ffi_string(value: &[c_char]) -> &CStr {
    unsafe { CStr::from_ptr(value.as_ptr()) }
}
