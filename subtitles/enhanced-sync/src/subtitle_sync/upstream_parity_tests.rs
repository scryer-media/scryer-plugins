use super::aligners::{FRAMERATE_RATIOS, best_candidate, fft_align, subtitle_spans_to_timeline};
use super::subtitles::{
    SubtitleCue, SubtitleFormat, format_srt_ts, parse_document, parse_srt_ts, rewrite_document,
};
use super::sync::{SyncOptions, sync_subtitle};
use super::transformers::{
    Span, SubtitleTransformOptions, max_cue_time_seconds, max_time_seconds, merge_cues,
    preprocess_cues, spans_to_timeline, spans_to_timeline_at_rate, subtitle_speech_spans,
};

const ODD_TIMESTAMP_SRT: &[u8] = b"1
00:00:00,178 --> 00:00:01,1416
<i>Previously on \"Your favorite TV show...\"</i>

2
00:00:01,1828 --> 00:00:04,549
Oh hi, Mark.

3
00:00:04,653 --> 00:00:03,3062
You are tearing me apart, Lisa!
";

const OFFSET_SAFE_SRT: &[u8] = b"1
00:00:05,000 --> 00:00:06,000
one

2
00:00:08,000 --> 00:00:09,000
two

3
00:00:11,000 --> 00:00:12,000
three
";

const TRUTH_CUES: [FixtureCue; 4] = [
    FixtureCue::new(2_000, 2_900, "alpha"),
    FixtureCue::new(6_500, 7_400, "beta"),
    FixtureCue::new(11_000, 11_900, "gamma"),
    FixtureCue::new(15_500, 16_400, "delta"),
];

#[derive(Debug, Clone, Copy)]
struct FixtureCue {
    start_ms: i64,
    end_ms: i64,
    text: &'static str,
}

impl FixtureCue {
    const fn new(start_ms: i64, end_ms: i64, text: &'static str) -> Self {
        Self {
            start_ms,
            end_ms,
            text,
        }
    }
}

#[test]
fn alignment_unit_cases_match_upstream_expectations() {
    let cases = [
        ("111001", "11001", -1),
        ("1001", "1001", 0),
        ("10010", "01001", 1),
    ];

    for (substring, reference, expected_offset) in cases {
        let reference = bits(reference);
        let substring = bits(substring);
        assert_eq!(
            fft_align(&reference, &substring, None).offset_samples,
            expected_offset
        );

        let subtitle_spans = spans_from_bits(&substring, 10);
        let candidate = best_candidate(&reference, &subtitle_spans, 60, false).expect("candidate");
        assert_eq!(candidate.alignment.offset_samples, expected_offset);
    }
}

#[test]
fn start_seconds_filter_matches_upstream_parser_cases() {
    let (doc, _) = parse_document(SubtitleFormat::Srt, ODD_TIMESTAMP_SRT, Some("utf-8")).unwrap();
    let zero = preprocess_cues(
        &doc.cues,
        SubtitleTransformOptions {
            start_seconds: 0,
            max_subtitle_duration_ms: 10_000,
        },
    );

    for start_seconds in [0, 2, 4, 6] {
        let filtered = preprocess_cues(
            &doc.cues,
            SubtitleTransformOptions {
                start_seconds,
                max_subtitle_duration_ms: 10_000,
            },
        );
        let expected = zero
            .iter()
            .filter(|cue| cue.start_ms >= i64::from(start_seconds) * 1000)
            .collect::<Vec<_>>();

        assert_eq!(filtered.len(), expected.len());
        for (actual, expected) in filtered.iter().zip(expected) {
            assert_eq!(actual.start_ms, expected.start_ms);
            assert_eq!(actual.end_ms, expected.end_ms);
            assert_eq!(actual.content, expected.content);
        }
    }
}

#[test]
fn max_subtitle_duration_clamps_like_upstream_parser_cases() {
    let (doc, _) = parse_document(SubtitleFormat::Srt, ODD_TIMESTAMP_SRT, Some("utf-8")).unwrap();

    for max_subtitle_duration_ms in [1_000, 1_500, 2_000, 2_500] {
        let clamped = preprocess_cues(
            &doc.cues,
            SubtitleTransformOptions {
                start_seconds: 0,
                max_subtitle_duration_ms,
            },
        );

        assert!(clamped.iter().all(|cue| {
            cue.end_ms - cue.start_ms <= max_subtitle_duration_ms
                && cue.end_ms <= cue.start_ms + max_subtitle_duration_ms
        }));
    }
}

#[test]
fn same_and_explicit_encoding_behaviors_cover_upstream_cases() {
    for encoding in ["utf-8", "ascii", "latin-1"] {
        let (doc, _) =
            parse_document(SubtitleFormat::Srt, ODD_TIMESTAMP_SRT, Some(encoding)).unwrap();
        let (same, same_warnings) = rewrite_document(&doc, 1.0, 1_000, "same").unwrap();
        let (utf8, utf8_warnings) = rewrite_document(&doc, 1.0, 1_000, "utf-8").unwrap();

        assert!(same_warnings.is_empty() || encoding != "utf-8");
        assert!(utf8_warnings.is_empty());
        assert!(String::from_utf8_lossy(&same).contains("00:00:01,178"));
        assert!(String::from_utf8(utf8).unwrap().contains("00:00:01,178"));
    }

    let latin1 = b"1\n00:00:00,000 --> 00:00:01,000\ncaf\xe9\n";
    let (doc, _) = parse_document(SubtitleFormat::Srt, latin1, Some("latin-1")).unwrap();
    let (same, _) = rewrite_document(&doc, 1.0, 1_000, "same").unwrap();
    assert!(same.contains(&0xe9));
}

#[test]
fn offset_rewrite_matches_upstream_shifter_cases() {
    let (doc, _) = parse_document(SubtitleFormat::Srt, OFFSET_SAFE_SRT, Some("utf-8")).unwrap();

    for offset_ms in [1_000, 1_500, -2_300] {
        let (rewritten, _) = rewrite_document(&doc, 1.0, offset_ms, "same").unwrap();
        let (shifted, _) = parse_document(SubtitleFormat::Srt, &rewritten, Some("utf-8")).unwrap();

        for (original, shifted) in doc.cues.iter().zip(&shifted.cues) {
            assert_eq!(shifted.start_ms - original.start_ms, offset_ms);
            assert_eq!(shifted.end_ms - original.end_ms, offset_ms);
        }
    }
}

#[test]
fn subtitle_speech_extraction_matches_upstream_sample_rate_cases() {
    let (doc, _) = parse_document(SubtitleFormat::Srt, ODD_TIMESTAMP_SRT, Some("utf-8")).unwrap();

    for sample_rate in [10, 20, 100, 300] {
        for start_seconds in [0, 2, 4, 6] {
            let options = SubtitleTransformOptions {
                start_seconds,
                max_subtitle_duration_ms: 10_000,
            };
            let cues = preprocess_cues(&doc.cues, options);
            let spans = subtitle_speech_spans(&cues, options);
            let timeline = spans_to_timeline_at_rate(&spans, 1.0, sample_rate);

            for span in spans {
                let start = rounded_sample(span.start_ms, sample_rate);
                let duration = rounded_sample(span.end_ms - span.start_ms, sample_rate);
                let active = timeline[start..start + duration]
                    .iter()
                    .filter(|value| **value > 0.5)
                    .count();

                assert_eq!(active, duration);
            }
        }
    }
}

#[test]
fn max_time_matches_upstream_subtitle_speech_transformer_case() {
    let (doc, _) = parse_document(SubtitleFormat::Srt, ODD_TIMESTAMP_SRT, Some("utf-8")).unwrap();
    let spans = subtitle_speech_spans(&doc.cues, SubtitleTransformOptions::default());

    assert!((max_time_seconds(&spans) - 6.062).abs() < 0.001);
    assert!((max_cue_time_seconds(&doc.cues, 0) - 6.062).abs() < 0.001);
}

#[test]
fn generated_integration_timestamps_roughly_match_after_sync() {
    let truth_srt = render_srt(&TRUTH_CUES);
    let unsynced_srt = desync_srt_from_truth(&TRUTH_CUES, 1.0, 1_250);
    let truth = parse_document(SubtitleFormat::Srt, &truth_srt, Some("utf-8"))
        .unwrap()
        .0;
    let reference_spans = truth
        .cues
        .iter()
        .filter_map(|cue| Span::new(cue.start_ms, cue.end_ms))
        .collect::<Vec<_>>();

    let outcome = sync_subtitle(
        &reference_spans,
        &unsynced_srt,
        "srt",
        Some("utf-8"),
        &SyncOptions {
            precise_framerate_search: false,
            ..SyncOptions::default()
        },
    )
    .expect("sync");

    assert_eq!(outcome.offset_ms, 1_250);
    assert!(timestamps_roughly_match(
        &outcome.rewritten_content,
        &truth_srt
    ));
}

#[test]
fn generated_integration_timestamps_roughly_match_after_stretch() {
    let truth_srt = render_srt(&TRUTH_CUES);
    let shifted_and_stretched = desync_srt_from_truth(&TRUTH_CUES, 1.1, 900);
    let truth = parse_document(SubtitleFormat::Srt, &truth_srt, Some("utf-8"))
        .unwrap()
        .0;
    let reference_spans = truth
        .cues
        .iter()
        .filter_map(|cue| Span::new(cue.start_ms, cue.end_ms))
        .collect::<Vec<_>>();

    let outcome = sync_subtitle(
        &reference_spans,
        &shifted_and_stretched,
        "srt",
        Some("utf-8"),
        &SyncOptions {
            precise_framerate_search: true,
            ..SyncOptions::default()
        },
    )
    .expect("sync");

    let rewritten = String::from_utf8(outcome.rewritten_content.clone()).unwrap();
    assert!((outcome.selected_framerate_ratio - 1.1).abs() < 0.02);
    assert!(
        timestamps_roughly_match(&outcome.rewritten_content, &truth_srt),
        "ratio={} offset={} rewritten=\n{}",
        outcome.selected_framerate_ratio,
        outcome.offset_ms,
        rewritten
    );
}

#[test]
fn standard_framerate_ratios_match_upstream_defaults() {
    assert_eq!(FRAMERATE_RATIOS.len(), 3);
    assert!((FRAMERATE_RATIOS[0] - 24.0 / 23.976).abs() < 1e-12);
    assert!((FRAMERATE_RATIOS[1] - 25.0 / 23.976).abs() < 1e-12);
    assert!((FRAMERATE_RATIOS[2] - 25.0 / 24.0).abs() < 1e-12);
}

#[test]
fn subtitle_timeline_uses_upstream_duration_rounding_and_ratio_fill() {
    let spans = vec![Span::new(178, 2_416).unwrap()];
    let timeline = subtitle_spans_to_timeline(&spans, 1.25);
    let start = rounded_sample((178.0_f64 * 1.25).round() as i64, 100);
    let duration = rounded_sample((2_238.0_f64 * 1.25).round() as i64, 100);

    assert_eq!(
        timeline[start..start + duration]
            .iter()
            .filter(|value| **value > 0.0)
            .count(),
        duration
    );
    assert!((timeline[start] - 0.8).abs() < 1e-12);
}

#[test]
fn max_offset_window_is_applied_during_score_selection() {
    let subtitle_spans = [
        Span::new(1_000, 1_700).unwrap(),
        Span::new(4_000, 4_800).unwrap(),
        Span::new(9_000, 9_900).unwrap(),
        Span::new(14_000, 14_900).unwrap(),
    ];
    let shifted = subtitle_spans
        .iter()
        .filter_map(|span| Span::new(span.start_ms + 5_000, span.end_ms + 5_000))
        .collect::<Vec<_>>();
    let reference = spans_to_timeline(&shifted, 1.0);
    let candidate = best_candidate(&reference, &subtitle_spans, 1, false).expect("candidate");

    assert!(candidate.alignment.offset_samples.abs() <= 100);
}

#[test]
fn metadata_suppression_matches_source_rules() {
    let cues = vec![
        cue(0, 1_000, "English subtitles"),
        cue(2_000, 3_000, "<i>HTML-tagged speech survives</i>"),
        cue(4_000, 5_000, "[music]"),
        cue(6_000, 7_000, "Name - Title"),
    ];
    let spans = subtitle_speech_spans(&cues, SubtitleTransformOptions::default());

    assert_eq!(spans, vec![Span::new(2_000, 3_000).unwrap()]);
}

#[test]
fn malformed_srt_blocks_are_tolerated_like_upstream_default_parser() {
    let malformed = b"not an index
still bad

1
00:00:01,000 --> 00:00:02,000
valid

bad timing
00:00:xx,000 --> 00:00:04,000
invalid

2
00:00:05,000 --> 00:00:06,000
also valid
";
    let (doc, _) = parse_document(SubtitleFormat::Srt, malformed, Some("utf-8")).unwrap();

    assert_eq!(doc.cues.len(), 2);
    assert_eq!(doc.cues[0].content, "valid");
    assert_eq!(doc.cues[1].content, "also valid");
}

#[test]
fn ass_and_ssa_timed_events_sync_and_rewrite() {
    let cues = [
        FixtureCue::new(1_000, 2_000, "alpha"),
        FixtureCue::new(4_000, 5_000, "beta"),
        FixtureCue::new(7_000, 8_000, "gamma, with comma"),
    ];
    let reference = vec![
        Span::new(2_000, 3_000).unwrap(),
        Span::new(5_000, 6_000).unwrap(),
        Span::new(8_000, 9_000).unwrap(),
    ];

    for (format, generated, section_marker) in [
        ("ass", render_ass(&cues), "[V4+ Styles]"),
        ("ssa", render_ssa(&cues), "[V4 Styles]"),
    ] {
        let outcome = sync_subtitle(
            &reference,
            generated.as_bytes(),
            format,
            Some("utf-8"),
            &SyncOptions {
                precise_framerate_search: false,
                ..SyncOptions::default()
            },
        )
        .expect("sync");
        let rewritten = String::from_utf8(outcome.rewritten_content).unwrap();

        assert_eq!(outcome.offset_ms, 1_000);
        assert!(rewritten.contains(section_marker));
        assert!(rewritten.contains("0:00:02.00,0:00:03.00,Default,alpha"));
        assert!(rewritten.contains("0:00:05.00,0:00:06.00,Default,beta"));
        assert!(rewritten.contains("gamma, with comma"));
    }
}

#[test]
fn line_endings_and_timing_suffixes_survive_srt_rewrite() {
    let srt = b"1\r\n00:00:01,000 --> 00:00:02,000  X1:99\r\nalpha\r\n\r\n2\r\n00:00:04,000 --> 00:00:05,000\r\nbeta\r\n";
    let (doc, _) = parse_document(SubtitleFormat::Srt, srt, Some("utf-8")).unwrap();
    let (rewritten, _) = rewrite_document(&doc, 1.0, 1_000, "same").unwrap();
    let rewritten = String::from_utf8(rewritten).unwrap();

    assert!(rewritten.contains("\r\n"));
    assert!(rewritten.contains("00:00:02,000 --> 00:00:03,000  X1:99"));
}

#[test]
fn reference_merge_library_matches_source_ordering_rules() {
    let reference = vec![cue(0, 1_000, "A"), cue(10_000, 11_000, "C")];
    let output = vec![cue(9_000, 10_000, "B")];
    let merged = merge_cues(&reference, &output, true);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].content, "C\nB");

    let reference = vec![cue(1_000, 2_000, "reference")];
    let output = vec![cue(1_000, 2_000, "output")];
    assert_eq!(
        merge_cues(&reference, &output, true)[0].content,
        "reference\noutput"
    );
    assert_eq!(
        merge_cues(&reference, &output, false)[0].content,
        "output\nreference"
    );
}

#[test]
fn rough_timestamp_match_rejects_unsynced_fixture() {
    let truth_srt = render_srt(&TRUTH_CUES);
    let unsynced_srt = desync_srt_from_truth(&TRUTH_CUES, 1.0, 1_250);

    assert!(!timestamps_roughly_match(&unsynced_srt, &truth_srt));
}

fn bits(value: &str) -> Vec<f64> {
    value
        .chars()
        .map(|ch| if ch == '1' { 1.0 } else { 0.0 })
        .collect()
}

fn render_srt(cues: &[FixtureCue]) -> Vec<u8> {
    let mut output = String::new();
    for (index, cue) in cues.iter().enumerate() {
        output.push_str(&(index + 1).to_string());
        output.push('\n');
        output.push_str(&format_srt_ts(cue.start_ms));
        output.push_str(" --> ");
        output.push_str(&format_srt_ts(cue.end_ms));
        output.push('\n');
        output.push_str(cue.text);
        output.push_str("\n\n");
    }
    output.into_bytes()
}

fn desync_srt_from_truth(cues: &[FixtureCue], ratio: f64, offset_ms: i64) -> Vec<u8> {
    let shifted = cues
        .iter()
        .map(|cue| {
            FixtureCue::new(
                inverse_transform_ms(cue.start_ms, ratio, offset_ms),
                inverse_transform_ms(cue.end_ms, ratio, offset_ms),
                cue.text,
            )
        })
        .collect::<Vec<_>>();
    render_srt(&shifted)
}

fn render_ass(cues: &[FixtureCue]) -> String {
    let mut output = String::from(
        "[Script Info]\nTitle: generated parity\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour\nStyle: Default,Arial,20,&H00FFFFFF\n\n[Events]\nFormat: Layer, Start, End, Style, Text\n",
    );
    for (index, cue) in cues.iter().enumerate() {
        let event = if index == 1 { "Comment" } else { "Dialogue" };
        output.push_str(&format!(
            "{event}: 0,{},{},Default,{}\n",
            format_ass_ts(cue.start_ms),
            format_ass_ts(cue.end_ms),
            cue.text
        ));
    }
    output
}

fn render_ssa(cues: &[FixtureCue]) -> String {
    let mut output = String::from(
        "[Script Info]\nTitle: generated parity\n\n[V4 Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour\nStyle: Default,Arial,20,&H00FFFFFF\n\n[Events]\nFormat: Marked, Start, End, Style, Text\n",
    );
    for (index, cue) in cues.iter().enumerate() {
        let event = if index == 1 { "Comment" } else { "Dialogue" };
        output.push_str(&format!(
            "{event}: Marked=0,{},{},Default,{}\n",
            format_ass_ts(cue.start_ms),
            format_ass_ts(cue.end_ms),
            cue.text
        ));
    }
    output
}

fn format_ass_ts(ms: i64) -> String {
    let total_cs = (ms.max(0) + 5) / 10;
    let total_seconds = total_cs / 100;
    format!(
        "{}:{:02}:{:02}.{:02}",
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
        total_cs % 100
    )
}

fn inverse_transform_ms(ms: i64, ratio: f64, offset_ms: i64) -> i64 {
    (((ms - offset_ms) as f64) / ratio).round() as i64
}

fn spans_from_bits(bits: &[f64], sample_ms: i64) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut start = None;
    for (index, value) in bits.iter().enumerate() {
        if *value > 0.5 && start.is_none() {
            start = Some(index as i64 * sample_ms);
        } else if *value <= 0.5
            && let Some(start_ms) = start.take()
        {
            spans.push(Span::new(start_ms, index as i64 * sample_ms).unwrap());
        }
    }
    if let Some(start_ms) = start {
        spans.push(Span::new(start_ms, bits.len() as i64 * sample_ms).unwrap());
    }
    spans
}

fn cue(start_ms: i64, end_ms: i64, content: &str) -> SubtitleCue {
    SubtitleCue {
        start_ms,
        end_ms,
        content: content.to_string(),
    }
}

fn rounded_sample(ms: i64, sample_rate: u32) -> usize {
    ((ms.max(0) as f64 * sample_rate as f64) / 1000.0).round() as usize
}

fn timestamps_roughly_match(left: &[u8], right: &[u8]) -> bool {
    let left_doc = parse_document(SubtitleFormat::Srt, left, Some("utf-8"))
        .unwrap()
        .0;
    let right_doc = parse_document(SubtitleFormat::Srt, right, Some("utf-8"))
        .unwrap()
        .0;
    let left_spans = left_doc
        .cues
        .iter()
        .filter_map(|cue| Span::new(cue.start_ms, cue.end_ms))
        .collect::<Vec<_>>();
    let right_spans = right_doc
        .cues
        .iter()
        .filter_map(|cue| Span::new(cue.start_ms, cue.end_ms))
        .collect::<Vec<_>>();
    let left_timeline = spans_to_timeline(&left_spans, 1.0);
    let right_timeline = spans_to_timeline(&right_spans, 1.0);
    let len = left_timeline.len().max(right_timeline.len()).max(1);
    let matches = (0..len)
        .filter(|index| {
            let left = left_timeline.get(*index).copied().unwrap_or(0.0) > 0.5;
            let right = right_timeline.get(*index).copied().unwrap_or(0.0) > 0.5;
            left == right
        })
        .count();

    matches as f64 / len as f64 >= 0.99
}

#[test]
fn parse_rewritten_timestamp_helper_handles_offset_output() {
    assert_eq!(parse_srt_ts("00:00:06,000"), Some(6_000));
}
