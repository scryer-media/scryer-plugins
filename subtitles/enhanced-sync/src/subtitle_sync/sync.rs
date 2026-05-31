use super::aligners::{best_candidate, offset_ms};
use super::subtitles::{SubtitleFormat, parse_document, rewrite_document};
use super::transformers::{
    DEFAULT_MAX_SUBTITLE_MS, Span, SubtitleTransformOptions, filter_reference_spans,
    max_cue_time_seconds, preprocess_cues, spans_to_timeline, subtitle_speech_spans,
};

#[derive(Debug, Clone)]
pub(crate) struct SyncOptions {
    pub max_offset_seconds: i64,
    pub min_effective_offset_ms: i64,
    pub start_seconds: u32,
    pub max_subtitle_duration_ms: i64,
    pub precise_framerate_search: bool,
    pub output_encoding: String,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            max_offset_seconds: 60,
            min_effective_offset_ms: 50,
            start_seconds: 0,
            max_subtitle_duration_ms: DEFAULT_MAX_SUBTITLE_MS,
            precise_framerate_search: true,
            output_encoding: "same".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SyncOutcome {
    pub rewritten_content: Vec<u8>,
    pub output_format: String,
    pub offset_ms: i64,
    pub score: f64,
    pub selected_framerate_ratio: f64,
    pub reference_span_count: usize,
    pub subtitle_span_count: usize,
    pub subtitle_max_time_seconds: f64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum SyncError {
    Parse(String),
    NotEnoughReferenceSpans {
        count: usize,
    },
    NotEnoughSubtitleSpans {
        count: usize,
    },
    WeakAlignment {
        offset_ms: i64,
        score: f64,
    },
    OffsetTooSmall {
        offset_ms: i64,
        score: f64,
        ratio: f64,
    },
    OffsetExceedsMaximum {
        offset_ms: i64,
        score: f64,
        ratio: f64,
    },
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => f.write_str(message),
            Self::NotEnoughReferenceSpans { count } => {
                write!(f, "decoded only {count} reference speech spans")
            }
            Self::NotEnoughSubtitleSpans { count } => {
                write!(f, "parsed only {count} subtitle speech spans")
            }
            Self::WeakAlignment { .. } => f.write_str("alignment score too weak"),
            Self::OffsetTooSmall { .. } => f.write_str("alignment offset too small to apply"),
            Self::OffsetExceedsMaximum { .. } => {
                f.write_str("alignment offset exceeds configured maximum")
            }
        }
    }
}

pub(crate) fn sync_subtitle(
    reference_spans: &[Span],
    subtitle_bytes: &[u8],
    subtitle_format: &str,
    encoding_hint: Option<&str>,
    options: &SyncOptions,
) -> Result<SyncOutcome, SyncError> {
    let transform_options = SubtitleTransformOptions {
        start_seconds: options.start_seconds,
        max_subtitle_duration_ms: options.max_subtitle_duration_ms,
    };
    let reference_spans = filter_reference_spans(reference_spans, options.start_seconds);
    if reference_spans.len() < 3 {
        return Err(SyncError::NotEnoughReferenceSpans {
            count: reference_spans.len(),
        });
    }

    let format = SubtitleFormat::parse(subtitle_format).map_err(SyncError::Parse)?;
    let (document, mut warnings) =
        parse_document(format, subtitle_bytes, encoding_hint).map_err(SyncError::Parse)?;
    let processed_cues = preprocess_cues(&document.cues, transform_options);
    let speech_spans = subtitle_speech_spans(&processed_cues, transform_options);
    if speech_spans.len() < 3 {
        return Err(SyncError::NotEnoughSubtitleSpans {
            count: speech_spans.len(),
        });
    }

    let reference_timeline = spans_to_timeline(&reference_spans, 1.0);
    let Some(best) = best_candidate(
        &reference_timeline,
        &speech_spans,
        options.max_offset_seconds,
        options.precise_framerate_search,
    ) else {
        return Err(SyncError::WeakAlignment {
            offset_ms: 0,
            score: f64::NEG_INFINITY,
        });
    };

    let offset_ms = offset_ms(&best);
    let max_offset_ms = options.max_offset_seconds.saturating_abs() * 1000;
    if offset_ms.abs() > max_offset_ms {
        return Err(SyncError::OffsetExceedsMaximum {
            offset_ms,
            score: best.alignment.score,
            ratio: best.ratio,
        });
    }
    if best.alignment.score <= 0.0 {
        return Err(SyncError::WeakAlignment {
            offset_ms,
            score: best.alignment.score,
        });
    }
    if offset_ms.abs() < options.min_effective_offset_ms && (best.ratio - 1.0).abs() < 0.0001 {
        return Err(SyncError::OffsetTooSmall {
            offset_ms,
            score: best.alignment.score,
            ratio: best.ratio,
        });
    }

    let (rewritten_content, encode_warnings) = rewrite_document(
        &document,
        best.ratio,
        offset_ms,
        options.output_encoding.as_str(),
    )
    .map_err(SyncError::Parse)?;
    warnings.extend(encode_warnings);

    Ok(SyncOutcome {
        rewritten_content,
        output_format: format.label().to_string(),
        offset_ms,
        score: best.alignment.score,
        selected_framerate_ratio: best.ratio,
        reference_span_count: reference_spans.len(),
        subtitle_span_count: speech_spans.len(),
        subtitle_max_time_seconds: max_cue_time_seconds(&processed_cues, options.start_seconds),
        warnings,
    })
}

pub(crate) fn subtitle_reference_spans(
    subtitle_bytes: &[u8],
    subtitle_format: &str,
    encoding_hint: Option<&str>,
    options: &SyncOptions,
) -> Result<(Vec<Span>, Vec<String>), SyncError> {
    let transform_options = SubtitleTransformOptions {
        start_seconds: options.start_seconds,
        max_subtitle_duration_ms: options.max_subtitle_duration_ms,
    };
    let format = SubtitleFormat::parse(subtitle_format).map_err(SyncError::Parse)?;
    let (document, warnings) =
        parse_document(format, subtitle_bytes, encoding_hint).map_err(SyncError::Parse)?;
    let processed_cues = preprocess_cues(&document.cues, transform_options);
    Ok((
        subtitle_speech_spans(&processed_cues, transform_options),
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle_sync::subtitles::{format_srt_ts, parse_srt_ts};
    use crate::subtitle_sync::transformers::SAMPLE_RATE;

    const THREE_CUE_SRT: &[u8] = b"1
00:00:01,000 --> 00:00:02,000
one

2
00:00:04,000 --> 00:00:05,000
two

3
00:00:07,000 --> 00:00:08,000
three
";

    const THREE_CUE_WITH_END_METADATA_SRT: &[u8] = b"1
00:00:01,000 --> 00:00:02,000
one

2
00:00:04,000 --> 00:00:05,000
two

3
00:00:07,000 --> 00:00:08,000
three

4
00:00:10,000 --> 00:00:12,000
[music]
";

    fn spans(offset_ms: i64) -> Vec<Span> {
        [
            (1_000 + offset_ms, 2_000 + offset_ms),
            (4_000 + offset_ms, 5_000 + offset_ms),
            (7_000 + offset_ms, 8_000 + offset_ms),
        ]
        .into_iter()
        .filter_map(|(start, end)| Span::new(start, end))
        .collect()
    }

    #[test]
    fn sync_rewrites_positive_negative_and_zero_offsets() {
        for (reference_offset, expected) in [(1_000, 1_000), (-1_000, -1_000)] {
            let outcome = sync_subtitle(
                &spans(reference_offset),
                THREE_CUE_SRT,
                "srt",
                Some("utf-8"),
                &SyncOptions {
                    precise_framerate_search: false,
                    ..SyncOptions::default()
                },
            )
            .expect("sync");
            assert_eq!(outcome.offset_ms, expected);
        }

        let zero = sync_subtitle(
            &spans(0),
            THREE_CUE_SRT,
            "srt",
            Some("utf-8"),
            &SyncOptions {
                precise_framerate_search: false,
                ..SyncOptions::default()
            },
        )
        .expect_err("zero offset should be too small");
        assert!(matches!(zero, SyncError::OffsetTooSmall { .. }));
    }

    #[test]
    fn max_offset_constrains_large_shift_to_search_window() {
        let outcome = sync_subtitle(
            &spans(20_000),
            THREE_CUE_SRT,
            "srt",
            Some("utf-8"),
            &SyncOptions {
                max_offset_seconds: 2,
                precise_framerate_search: false,
                ..SyncOptions::default()
            },
        )
        .expect("constrained sync");

        assert!(outcome.offset_ms.abs() <= 2_000);
    }

    #[test]
    fn generated_integration_timestamps_roughly_match_reference() {
        let reference = spans(1_500);
        let outcome = sync_subtitle(
            &reference,
            THREE_CUE_SRT,
            "srt",
            Some("utf-8"),
            &SyncOptions {
                precise_framerate_search: false,
                ..SyncOptions::default()
            },
        )
        .expect("sync");
        let rewritten = String::from_utf8(outcome.rewritten_content).unwrap();
        let starts = rewritten
            .lines()
            .filter_map(|line| {
                line.split_once("-->")
                    .and_then(|(start, _)| parse_srt_ts(start))
            })
            .collect::<Vec<_>>();

        assert_eq!(starts, vec![2_500, 5_500, 8_500]);
        assert_eq!(format_srt_ts(starts[0]), "00:00:02,500");
    }

    #[test]
    fn subtitle_max_time_uses_sample_rate_compatible_units() {
        let outcome = sync_subtitle(
            &spans(1_000),
            THREE_CUE_WITH_END_METADATA_SRT,
            "srt",
            Some("utf-8"),
            &SyncOptions {
                precise_framerate_search: false,
                ..SyncOptions::default()
            },
        )
        .expect("sync");

        assert_eq!(SAMPLE_RATE, 100);
        assert_eq!(outcome.subtitle_span_count, 3);
        assert!((outcome.subtitle_max_time_seconds - 12.0).abs() < 0.001);
    }
}
