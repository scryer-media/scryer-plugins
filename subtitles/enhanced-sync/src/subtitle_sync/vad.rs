use std::ptr::NonNull;

use super::{Span, simd};

pub(crate) const WEBRTC_BACKEND_LABEL: &str = "webrtc-vad";
pub(crate) const RMS_BACKEND_LABEL: &str = "rms-vad";

const WEBRTC_FALLBACK_SAMPLE_RATE_HZ: u32 = 16_000;
const WEBRTC_FRAME_MS: i64 = 10;
const WEBRTC_MODE: i32 = 3;
const WEBRTC_MIN_SILENCE_FRAMES: usize = 3;
const MIN_ALIGNMENT_SPANS: usize = 3;

const RMS_WINDOW_MS: i64 = 10;
const RMS_START_THRESHOLD_MIN: f64 = 500.0;
const RMS_STOP_THRESHOLD_MIN: f64 = 250.0;
const RMS_START_MULTIPLIER: f64 = 3.0;
const RMS_STOP_MULTIPLIER: f64 = 1.8;
const RMS_NOISE_SMOOTHING: f64 = 0.05;
const RMS_MIN_SILENCE_WINDOWS: usize = 3;

#[derive(Debug, Clone)]
pub(crate) struct SpeechDetection {
    pub spans: Vec<Span>,
    pub backend: &'static str,
    pub warnings: Vec<String>,
}

pub(crate) struct SpeechSpanDetector {
    primary: Option<WebRtcSpeechDetector>,
    fallback: RmsSpeechDetector,
    warnings: Vec<String>,
}

impl SpeechSpanDetector {
    pub(crate) fn new(sample_rate_hz: u32) -> Self {
        let mut warnings = Vec::new();
        let primary = match WebRtcSpeechDetector::new(sample_rate_hz) {
            Ok(detector) => Some(detector),
            Err(error) => {
                warnings.push(format!(
                    "{WEBRTC_BACKEND_LABEL} initialization failed: {error}"
                ));
                None
            }
        };

        Self {
            primary,
            fallback: RmsSpeechDetector::new(sample_rate_hz),
            warnings,
        }
    }

    pub(crate) fn push_interleaved_i16(&mut self, samples: &[i16], channels: usize) {
        if let Some(primary) = &mut self.primary {
            primary.push_interleaved_i16(samples, channels);
        }
        self.fallback.push_interleaved_i16(samples, channels);
    }

    pub(crate) fn finish(mut self) -> SpeechDetection {
        let primary = self.primary.take().map(WebRtcSpeechDetector::finish);
        let fallback = self.fallback.finish();
        select_speech_detection(primary, fallback, self.warnings)
    }
}

fn select_speech_detection(
    primary: Option<WebRtcSpeechDetection>,
    fallback: Vec<Span>,
    mut warnings: Vec<String>,
) -> SpeechDetection {
    let Some(primary) = primary else {
        warnings.push(format!("used {RMS_BACKEND_LABEL} fallback"));
        return SpeechDetection {
            spans: fallback,
            backend: RMS_BACKEND_LABEL,
            warnings,
        };
    };

    if primary.spans.len() < MIN_ALIGNMENT_SPANS && fallback.len() >= MIN_ALIGNMENT_SPANS {
        warnings.push(format!(
            "{WEBRTC_BACKEND_LABEL} produced {} speech spans; used {RMS_BACKEND_LABEL} fallback",
            primary.spans.len()
        ));
        return SpeechDetection {
            spans: fallback,
            backend: RMS_BACKEND_LABEL,
            warnings,
        };
    }

    warnings.extend(primary.warnings);
    SpeechDetection {
        spans: primary.spans,
        backend: WEBRTC_BACKEND_LABEL,
        warnings,
    }
}

struct WebRtcSpeechDetection {
    spans: Vec<Span>,
    warnings: Vec<String>,
}

struct WebRtcSpeechDetector {
    vad: WebRtcVad,
    resampler: Option<LinearResampler>,
    pending_frame: Vec<i16>,
    frame_samples: usize,
    spans: FrameSpanAccumulator,
    invalid_frames: usize,
}

impl WebRtcSpeechDetector {
    fn new(input_sample_rate_hz: u32) -> Result<Self, WebRtcVadError> {
        let vad_sample_rate = if is_valid_webrtc_sample_rate(input_sample_rate_hz) {
            input_sample_rate_hz
        } else {
            WEBRTC_FALLBACK_SAMPLE_RATE_HZ
        };
        let vad = WebRtcVad::new(vad_sample_rate, WEBRTC_MODE)?;
        let resampler = (!is_valid_webrtc_sample_rate(input_sample_rate_hz))
            .then(|| LinearResampler::new(input_sample_rate_hz.max(1), vad_sample_rate));

        Ok(Self {
            vad,
            resampler,
            pending_frame: Vec::with_capacity(frame_samples_for_rate(vad_sample_rate)),
            frame_samples: frame_samples_for_rate(vad_sample_rate),
            spans: FrameSpanAccumulator::new(WEBRTC_FRAME_MS, WEBRTC_MIN_SILENCE_FRAMES),
            invalid_frames: 0,
        })
    }

    fn push_interleaved_i16(&mut self, samples: &[i16], channels: usize) {
        let channels = channels.max(1);
        let mut resampled = Vec::new();
        for frame in samples.chunks_exact(channels) {
            let mono = downmix_interleaved_frame(frame);
            if let Some(resampler) = &mut self.resampler {
                resampler.push_sample(mono, &mut resampled);
                for sample in resampled.drain(..) {
                    self.push_mono_sample(sample);
                }
            } else {
                self.push_mono_sample(mono);
            }
        }
    }

    fn push_mono_sample(&mut self, sample: i16) {
        self.pending_frame.push(sample);
        if self.pending_frame.len() == self.frame_samples {
            self.process_pending_frame();
        }
    }

    fn process_pending_frame(&mut self) {
        match self.vad.process(&self.pending_frame) {
            Ok(is_speech) => self.spans.push(is_speech),
            Err(WebRtcVadError::InvalidFrame) => {
                self.invalid_frames += 1;
                self.spans.push(false);
            }
            Err(WebRtcVadError::AllocationFailed)
            | Err(WebRtcVadError::InvalidMode)
            | Err(WebRtcVadError::InvalidSampleRate) => {
                self.invalid_frames += 1;
                self.spans.push(false);
            }
        }
        self.pending_frame.clear();
    }

    fn finish(mut self) -> WebRtcSpeechDetection {
        if !self.pending_frame.is_empty() {
            self.pending_frame.resize(self.frame_samples, 0);
            self.process_pending_frame();
        }

        let mut warnings = Vec::new();
        if self.invalid_frames > 0 {
            warnings.push(format!(
                "{WEBRTC_BACKEND_LABEL} skipped {} invalid frames",
                self.invalid_frames
            ));
        }

        WebRtcSpeechDetection {
            spans: self.spans.finish(),
            warnings,
        }
    }
}

struct WebRtcVad {
    ptr: NonNull<Fvad>,
}

impl WebRtcVad {
    fn new(sample_rate_hz: u32, mode: i32) -> Result<Self, WebRtcVadError> {
        let ptr = NonNull::new(unsafe { fvad_new() }).ok_or(WebRtcVadError::AllocationFailed)?;
        let vad = Self { ptr };
        if unsafe { fvad_set_sample_rate(vad.ptr.as_ptr(), sample_rate_hz as i32) } != 0 {
            return Err(WebRtcVadError::InvalidSampleRate);
        }
        if unsafe { fvad_set_mode(vad.ptr.as_ptr(), mode) } != 0 {
            return Err(WebRtcVadError::InvalidMode);
        }
        Ok(vad)
    }

    #[cfg(test)]
    fn reset(&mut self) {
        unsafe { fvad_reset(self.ptr.as_ptr()) };
    }

    fn process(&mut self, frame: &[i16]) -> Result<bool, WebRtcVadError> {
        match unsafe { fvad_process(self.ptr.as_ptr(), frame.as_ptr(), frame.len()) } {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WebRtcVadError::InvalidFrame),
        }
    }
}

impl Drop for WebRtcVad {
    fn drop(&mut self) {
        unsafe { fvad_free(self.ptr.as_ptr()) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebRtcVadError {
    AllocationFailed,
    InvalidMode,
    InvalidSampleRate,
    InvalidFrame,
}

impl std::fmt::Display for WebRtcVadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AllocationFailed => f.write_str("allocation failed"),
            Self::InvalidMode => f.write_str("invalid mode"),
            Self::InvalidSampleRate => f.write_str("invalid sample rate"),
            Self::InvalidFrame => f.write_str("invalid frame"),
        }
    }
}

#[repr(C)]
struct Fvad {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn fvad_new() -> *mut Fvad;
    fn fvad_free(inst: *mut Fvad);
    #[cfg(test)]
    fn fvad_reset(inst: *mut Fvad);
    fn fvad_set_mode(inst: *mut Fvad, mode: i32) -> i32;
    fn fvad_set_sample_rate(inst: *mut Fvad, sample_rate: i32) -> i32;
    fn fvad_process(inst: *mut Fvad, frame: *const i16, length: usize) -> i32;
}

struct LinearResampler {
    step: f64,
    next_output_pos: f64,
    input_index: u64,
    previous: Option<(u64, i16)>,
}

impl LinearResampler {
    fn new(input_sample_rate_hz: u32, output_sample_rate_hz: u32) -> Self {
        Self {
            step: input_sample_rate_hz as f64 / output_sample_rate_hz as f64,
            next_output_pos: 0.0,
            input_index: 0,
            previous: None,
        }
    }

    fn push_sample(&mut self, sample: i16, output: &mut Vec<i16>) {
        let current_index = self.input_index;
        self.input_index += 1;

        if let Some((previous_index, previous_sample)) = self.previous {
            while self.next_output_pos <= current_index as f64 {
                if self.next_output_pos < previous_index as f64 {
                    self.next_output_pos += self.step;
                    continue;
                }

                let distance = (current_index - previous_index).max(1) as f64;
                let fraction =
                    ((self.next_output_pos - previous_index as f64) / distance).clamp(0.0, 1.0);
                let interpolated =
                    previous_sample as f64 + (sample as f64 - previous_sample as f64) * fraction;
                output.push(interpolated.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
                self.next_output_pos += self.step;
            }
        } else {
            while self.next_output_pos <= current_index as f64 {
                output.push(sample);
                self.next_output_pos += self.step;
            }
        }

        self.previous = Some((current_index, sample));
    }
}

struct RmsSpeechDetector {
    samples_per_window: usize,
    frames_in_window: usize,
    window_energy_sum: f64,
    current_window_start_ms: i64,
    noise_floor: f64,
    noise_floor_initialized: bool,
    below_threshold_windows: usize,
    in_speech: bool,
    speech_start_ms: i64,
    spans: Vec<Span>,
}

impl RmsSpeechDetector {
    fn new(sample_rate_hz: u32) -> Self {
        Self {
            samples_per_window: (sample_rate_hz / 100).max(1) as usize,
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
            let mean_sq = super::simd::mean_square_i16(frame);
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
            self.noise_floor = rms.clamp(1.0, RMS_START_THRESHOLD_MIN / RMS_START_MULTIPLIER);
            self.noise_floor_initialized = true;
        } else if !self.in_speech || rms < self.noise_floor * RMS_START_MULTIPLIER {
            self.noise_floor =
                (1.0 - RMS_NOISE_SMOOTHING) * self.noise_floor + RMS_NOISE_SMOOTHING * rms.max(1.0);
        }

        let start_threshold =
            (self.noise_floor * RMS_START_MULTIPLIER).max(RMS_START_THRESHOLD_MIN);
        let stop_threshold = (self.noise_floor * RMS_STOP_MULTIPLIER).max(RMS_STOP_THRESHOLD_MIN);
        let window_start_ms = self.current_window_start_ms;

        if rms > start_threshold {
            self.below_threshold_windows = 0;
            if !self.in_speech {
                self.in_speech = true;
                self.speech_start_ms = window_start_ms;
            }
        } else if self.in_speech && rms <= stop_threshold {
            self.below_threshold_windows += 1;
            if self.below_threshold_windows >= RMS_MIN_SILENCE_WINDOWS {
                let end_ms =
                    window_start_ms - ((RMS_MIN_SILENCE_WINDOWS as i64 - 1) * RMS_WINDOW_MS);
                self.push_span(self.speech_start_ms, end_ms);
                self.in_speech = false;
                self.below_threshold_windows = 0;
            }
        } else if self.in_speech {
            self.below_threshold_windows = 0;
        }

        self.current_window_start_ms += RMS_WINDOW_MS;
    }

    fn finish(mut self) -> Vec<Span> {
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
        push_span(&mut self.spans, start_ms, end_ms, RMS_WINDOW_MS);
    }
}

struct FrameSpanAccumulator {
    frame_ms: i64,
    min_silence_frames: usize,
    current_frame_start_ms: i64,
    below_threshold_frames: usize,
    in_speech: bool,
    speech_start_ms: i64,
    spans: Vec<Span>,
}

impl FrameSpanAccumulator {
    fn new(frame_ms: i64, min_silence_frames: usize) -> Self {
        Self {
            frame_ms,
            min_silence_frames,
            current_frame_start_ms: 0,
            below_threshold_frames: 0,
            in_speech: false,
            speech_start_ms: 0,
            spans: Vec::new(),
        }
    }

    fn push(&mut self, is_speech: bool) {
        let frame_start_ms = self.current_frame_start_ms;

        if is_speech {
            self.below_threshold_frames = 0;
            if !self.in_speech {
                self.in_speech = true;
                self.speech_start_ms = frame_start_ms;
            }
        } else if self.in_speech {
            self.below_threshold_frames += 1;
            if self.below_threshold_frames >= self.min_silence_frames {
                let end_ms =
                    frame_start_ms - ((self.min_silence_frames as i64 - 1) * self.frame_ms);
                push_span(&mut self.spans, self.speech_start_ms, end_ms, self.frame_ms);
                self.in_speech = false;
                self.below_threshold_frames = 0;
            }
        }

        self.current_frame_start_ms += self.frame_ms;
    }

    fn finish(mut self) -> Vec<Span> {
        if self.in_speech {
            push_span(
                &mut self.spans,
                self.speech_start_ms,
                self.current_frame_start_ms,
                self.frame_ms,
            );
            self.in_speech = false;
        }

        self.spans
    }
}

fn is_valid_webrtc_sample_rate(sample_rate_hz: u32) -> bool {
    matches!(sample_rate_hz, 8_000 | 16_000 | 32_000 | 48_000)
}

fn frame_samples_for_rate(sample_rate_hz: u32) -> usize {
    (sample_rate_hz as usize * WEBRTC_FRAME_MS as usize) / 1000
}

fn downmix_interleaved_frame(frame: &[i16]) -> i16 {
    simd::mean_i16(frame)
}

fn push_span(spans: &mut Vec<Span>, start_ms: i64, end_ms: i64, merge_gap_ms: i64) {
    if end_ms <= start_ms {
        return;
    }

    if let Some(last) = spans.last_mut() {
        let last_end = last.end_ms;
        if start_ms - last_end <= merge_gap_ms {
            last.end_ms = end_ms;
            return;
        }
    }

    if let Some(span) = Span::new(start_ms, end_ms) {
        spans.push(span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webrtc_rejects_invalid_frame_lengths() {
        let mut vad = WebRtcVad::new(16_000, WEBRTC_MODE).expect("vad");
        assert_eq!(
            vad.process(&[0; 159]).expect_err("invalid length"),
            WebRtcVadError::InvalidFrame
        );
        vad.reset();
        assert!(!vad.process(&[0; 160]).expect("valid silence frame"));
    }

    #[test]
    fn silence_stays_empty_on_primary_backend() {
        let detection = run_fixture(16_000, 1, &[Segment::silence(2_000)]);

        assert_eq!(detection.backend, WEBRTC_BACKEND_LABEL);
        assert!(detection.spans.is_empty());
        assert!(detection.warnings.is_empty());
    }

    #[test]
    fn webrtc_detects_generated_voice_at_supported_rates() {
        for sample_rate in [8_000, 16_000, 32_000, 48_000] {
            let detection = run_fixture(
                sample_rate,
                1,
                &[
                    Segment::silence(500),
                    Segment::voice(900),
                    Segment::silence(500),
                    Segment::voice(800),
                    Segment::silence(400),
                ],
            );

            assert_eq!(detection.backend, WEBRTC_BACKEND_LABEL, "{sample_rate}");
            assert!(detection.warnings.is_empty(), "{sample_rate}");
            assert!(
                detection.spans.len() >= 2,
                "sample_rate={sample_rate} spans={:?}",
                detection.spans
            );
            assert_span_near(detection.spans[0], 500, 1_400, 250);
            assert_span_near(detection.spans[1], 1_900, 2_700, 250);
        }
    }

    #[test]
    fn webrtc_downmixes_stereo_at_48khz() {
        let detection = run_fixture(
            48_000,
            2,
            &[
                Segment::silence(400),
                Segment::voice(1_000),
                Segment::silence(500),
                Segment::voice(900),
            ],
        );

        assert_eq!(detection.backend, WEBRTC_BACKEND_LABEL);
        assert!(detection.warnings.is_empty());
        assert!(detection.spans.len() >= 2, "{:?}", detection.spans);
        assert_span_near(detection.spans[0], 400, 1_400, 260);
        assert_span_near(detection.spans[1], 1_900, 2_800, 260);
    }

    #[test]
    fn unsupported_sample_rates_are_resampled_without_invalid_frames() {
        let detection = run_fixture(44_100, 1, &[Segment::silence(2_000)]);

        assert_eq!(detection.backend, WEBRTC_BACKEND_LABEL);
        assert!(detection.warnings.is_empty());
        assert!(detection.spans.is_empty());
    }

    #[test]
    fn rms_baseline_remains_available_for_fallback_detection() {
        let mut detector = RmsSpeechDetector::new(16_000);
        detector.push_interleaved_i16(&vec![0; 8_000], 1);
        detector.push_interleaved_i16(&vec![8_000; 16_000], 1);
        detector.push_interleaved_i16(&vec![0; 8_000], 1);
        let spans = detector.finish();

        assert_eq!(spans.len(), 1);
        assert_span_near(spans[0], 500, 1_500, 40);
    }

    #[test]
    fn rms_is_selected_when_primary_is_unavailable() {
        let fallback = test_spans(3);
        let detection = select_speech_detection(None, fallback.clone(), Vec::new());

        assert_eq!(detection.backend, RMS_BACKEND_LABEL);
        assert_eq!(detection.spans, fallback);
        assert!(
            detection
                .warnings
                .iter()
                .any(|warning| warning.contains("rms-vad fallback"))
        );
    }

    #[test]
    fn rms_is_selected_when_primary_is_too_sparse() {
        let primary = WebRtcSpeechDetection {
            spans: test_spans(2),
            warnings: Vec::new(),
        };
        let fallback = test_spans(3);

        let detection = select_speech_detection(Some(primary), fallback.clone(), Vec::new());

        assert_eq!(detection.backend, RMS_BACKEND_LABEL);
        assert_eq!(detection.spans, fallback);
        assert!(
            detection
                .warnings
                .iter()
                .any(|warning| warning.contains("produced 2 speech spans"))
        );
    }

    #[test]
    fn primary_is_selected_when_it_has_enough_spans() {
        let primary_spans = test_spans(3);
        let primary = WebRtcSpeechDetection {
            spans: primary_spans.clone(),
            warnings: vec!["primary warning".to_string()],
        };
        let fallback = test_spans(4);

        let detection = select_speech_detection(Some(primary), fallback, Vec::new());

        assert_eq!(detection.backend, WEBRTC_BACKEND_LABEL);
        assert_eq!(detection.spans, primary_spans);
        assert_eq!(detection.warnings, vec!["primary warning".to_string()]);
    }

    fn run_fixture(sample_rate_hz: u32, channels: usize, segments: &[Segment]) -> SpeechDetection {
        let mut detector = SpeechSpanDetector::new(sample_rate_hz);
        let mut absolute_sample = 0usize;
        for segment in segments {
            let sample_count = sample_rate_hz as usize * segment.duration_ms / 1000;
            let mut samples = Vec::with_capacity(sample_count * channels);
            for _ in 0..sample_count {
                let sample = match segment.kind {
                    SegmentKind::Silence => 0,
                    SegmentKind::Voice => generated_voice_sample(absolute_sample, sample_rate_hz),
                };
                absolute_sample += 1;
                for channel in 0..channels {
                    samples.push(if channel == 0 { sample } else { sample / 2 });
                }
            }
            detector.push_interleaved_i16(&samples, channels);
        }
        detector.finish()
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

    fn test_spans(count: usize) -> Vec<Span> {
        (0..count)
            .filter_map(|index| {
                let start = index as i64 * 1_000;
                Span::new(start, start + 500)
            })
            .collect()
    }

    fn assert_span_near(
        span: Span,
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

    #[derive(Debug, Clone, Copy)]
    struct Segment {
        kind: SegmentKind,
        duration_ms: usize,
    }

    impl Segment {
        fn silence(duration_ms: usize) -> Self {
            Self {
                kind: SegmentKind::Silence,
                duration_ms,
            }
        }

        fn voice(duration_ms: usize) -> Self {
            Self {
                kind: SegmentKind::Voice,
                duration_ms,
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum SegmentKind {
        Silence,
        Voice,
    }
}
