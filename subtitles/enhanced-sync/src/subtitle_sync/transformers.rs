use super::subtitles::SubtitleCue;

pub(crate) const SAMPLE_RATE: u32 = 100;
pub(crate) const DEFAULT_MAX_SUBTITLE_MS: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Span {
    pub(crate) fn new(start_ms: i64, end_ms: i64) -> Option<Self> {
        (end_ms > start_ms).then_some(Self { start_ms, end_ms })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SubtitleTransformOptions {
    pub start_seconds: u32,
    pub max_subtitle_duration_ms: i64,
}

impl Default for SubtitleTransformOptions {
    fn default() -> Self {
        Self {
            start_seconds: 0,
            max_subtitle_duration_ms: DEFAULT_MAX_SUBTITLE_MS,
        }
    }
}

pub(crate) fn filter_reference_spans(spans: &[Span], start_seconds: u32) -> Vec<Span> {
    let start_ms = i64::from(start_seconds) * 1000;
    spans
        .iter()
        .filter_map(|span| {
            let start = span.start_ms.max(start_ms) - start_ms;
            let end = span.end_ms - start_ms;
            Span::new(start, end)
        })
        .collect()
}

pub(crate) fn preprocess_cues(
    cues: &[SubtitleCue],
    options: SubtitleTransformOptions,
) -> Vec<SubtitleCue> {
    let start_ms = i64::from(options.start_seconds) * 1000;
    cues.iter()
        .filter(|cue| cue.start_ms >= start_ms)
        .map(|cue| {
            let mut cue = cue.clone();
            cue.end_ms = cue
                .end_ms
                .min(cue.start_ms + options.max_subtitle_duration_ms.max(1));
            cue
        })
        .collect()
}

pub(crate) fn subtitle_speech_spans(
    cues: &[SubtitleCue],
    options: SubtitleTransformOptions,
) -> Vec<Span> {
    let start_ms = i64::from(options.start_seconds) * 1000;
    cues.iter()
        .enumerate()
        .filter(|(index, cue)| !is_metadata(&cue.content, *index == 0 || *index + 1 == cues.len()))
        .filter_map(|(_, cue)| Span::new(cue.start_ms - start_ms, cue.end_ms - start_ms))
        .collect()
}

pub(crate) fn spans_to_timeline(spans: &[Span], ratio: f64) -> Vec<f64> {
    spans_to_timeline_at_rate(spans, ratio, SAMPLE_RATE)
}

pub(crate) fn spans_to_timeline_at_rate(spans: &[Span], ratio: f64, sample_rate: u32) -> Vec<f64> {
    let max_sample = spans
        .iter()
        .map(|span| {
            let start = ms_to_sample_at_rate(scale_ms(span.start_ms, ratio), sample_rate);
            let duration =
                ms_to_sample_at_rate(scale_ms(span.end_ms - span.start_ms, ratio), sample_rate);
            start + duration
        })
        .max()
        .unwrap_or(0);
    let mut timeline = vec![0.0; max_sample + 2];
    for span in spans {
        let start =
            ms_to_sample_at_rate(scale_ms(span.start_ms, ratio), sample_rate).min(timeline.len());
        let duration =
            ms_to_sample_at_rate(scale_ms(span.end_ms - span.start_ms, ratio), sample_rate);
        let end = (start + duration).min(timeline.len());
        if end > start {
            timeline[start..end].fill(1.0);
        }
    }
    timeline
}

#[cfg(test)]
pub(crate) fn max_time_seconds(spans: &[Span]) -> f64 {
    spans.iter().map(|span| span.end_ms).max().unwrap_or(0) as f64 / 1000.0
}

pub(crate) fn max_cue_time_seconds(cues: &[SubtitleCue], start_seconds: u32) -> f64 {
    let start_ms = i64::from(start_seconds) * 1000;
    cues.iter()
        .map(|cue| cue.end_ms)
        .max()
        .map(|end_ms| (end_ms - start_ms).max(0) as f64 / 1000.0)
        .unwrap_or(0.0)
}

#[allow(dead_code)]
pub(crate) fn merge_cues(
    reference: &[SubtitleCue],
    output: &[SubtitleCue],
    reference_first: bool,
) -> Vec<SubtitleCue> {
    use std::collections::VecDeque;

    let mut first = if reference_first { reference } else { output }
        .iter()
        .cloned()
        .collect::<VecDeque<_>>();
    let mut second = if reference_first { output } else { reference }
        .iter()
        .cloned()
        .collect::<VecDeque<_>>();
    let mut cur_a = first.pop_front();
    let mut cur_b = second.pop_front();
    let mut merged = Vec::new();

    loop {
        match (&cur_a, &cur_b) {
            (None, None) => return merged,
            (None, Some(_)) => {
                while let Some(cue) = cur_b.take() {
                    merged.push(cue);
                    cur_b = second.pop_front();
                }
                return merged;
            }
            (Some(_), None) => {
                while let Some(cue) = cur_a.take() {
                    merged.push(cue);
                    cur_a = first.pop_front();
                }
                return merged;
            }
            (Some(a), Some(b)) => {
                let swapped = a.start_ms >= b.start_ms;
                if swapped {
                    std::mem::swap(&mut first, &mut second);
                    std::mem::swap(&mut cur_a, &mut cur_b);
                }

                let mut prev_a = cur_a.clone();
                while let (Some(current_a), Some(next_b)) = (&cur_a, &cur_b) {
                    if current_a.start_ms >= next_b.start_ms {
                        break;
                    }
                    cur_a = first.pop_front();
                    if cur_a
                        .as_ref()
                        .is_none_or(|next_a| next_a.start_ms < next_b.start_ms)
                    {
                        merged.push(prev_a.take().expect("previous cue"));
                        prev_a = cur_a.clone();
                    } else {
                        break;
                    }
                }

                let Some(previous) = prev_a.take() else {
                    while let Some(cue) = cur_b.take() {
                        merged.push(cue);
                        cur_b = second.pop_front();
                    }
                    return merged;
                };
                let Some(next_a) = cur_a.clone() else {
                    merged.push(previous);
                    while let Some(cue) = cur_b.take() {
                        merged.push(cue);
                        cur_b = second.pop_front();
                    }
                    return merged;
                };
                let current_b = cur_b.clone().expect("current second cue");

                if current_b.start_ms - previous.start_ms < next_a.start_ms - current_b.start_ms {
                    if swapped {
                        merged.push(current_b.merge_with(&previous));
                        std::mem::swap(&mut first, &mut second);
                        std::mem::swap(&mut cur_a, &mut cur_b);
                        cur_a = first.pop_front();
                    } else {
                        merged.push(previous.merge_with(&current_b));
                        cur_b = second.pop_front();
                    }
                } else if swapped {
                    merged.push(current_b.merge_with(&next_a));
                    std::mem::swap(&mut first, &mut second);
                    std::mem::swap(&mut cur_a, &mut cur_b);
                    cur_a = first.pop_front();
                    cur_b = second.pop_front();
                } else {
                    merged.push(next_a.merge_with(&current_b));
                    cur_a = first.pop_front();
                    cur_b = second.pop_front();
                }
            }
        }
    }
}

pub(crate) fn ms_to_sample(ms: i64) -> usize {
    ms_to_sample_at_rate(ms, SAMPLE_RATE)
}

pub(crate) fn ms_to_sample_at_rate(ms: i64, sample_rate: u32) -> usize {
    (((ms.max(0) as f64) * sample_rate as f64) / 1000.0).round() as usize
}

pub(crate) fn samples_to_ms(samples: i64) -> i64 {
    ((samples as f64 * 1000.0) / SAMPLE_RATE as f64).round() as i64
}

pub(crate) fn scale_ms(ms: i64, ratio: f64) -> i64 {
    ((ms as f64) * ratio).round() as i64
}

fn is_metadata(content: &str, is_beginning_or_end: bool) -> bool {
    let content = content.trim();
    if content.is_empty() {
        return true;
    }
    if let Some(end) = paired_nester_end(content.chars().next().unwrap())
        && content.ends_with(end)
    {
        return true;
    }
    is_beginning_or_end
        && (content.to_ascii_lowercase().contains("english") || content.contains(" - "))
}

fn paired_nester_end(ch: char) -> Option<char> {
    match ch {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: i64, end_ms: i64, content: &str) -> SubtitleCue {
        SubtitleCue {
            start_ms,
            end_ms,
            content: content.to_string(),
        }
    }

    #[test]
    fn filters_start_and_clamps_duration() {
        let cues = vec![
            cue(500, 1_500, "before"),
            cue(2_000, 7_000, "long"),
            cue(8_000, 9_000, "after"),
        ];
        let filtered = preprocess_cues(
            &cues,
            SubtitleTransformOptions {
                start_seconds: 2,
                max_subtitle_duration_ms: 1_500,
            },
        );

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].start_ms, 2_000);
        assert_eq!(filtered[0].end_ms, 3_500);
    }

    #[test]
    fn speech_timeline_uses_rounding_across_sample_rates() {
        let spans = vec![
            Span::new(178, 2_416).unwrap(),
            Span::new(1_828, 4_549).unwrap(),
            Span::new(4_653, 6_062).unwrap(),
        ];

        for sample_rate in [10, 20, 100, 300] {
            let timeline = spans_to_timeline_at_rate(&spans, 1.0, sample_rate);
            for span in &spans {
                let start = ms_to_sample_at_rate(span.start_ms, sample_rate);
                let duration = ms_to_sample_at_rate(span.end_ms - span.start_ms, sample_rate);
                let active = timeline[start..start + duration]
                    .iter()
                    .filter(|value| **value > 0.5)
                    .count();

                assert_eq!(active, duration);
            }
        }
    }

    #[test]
    fn suppresses_metadata_in_speech_spans() {
        let cues = vec![
            cue(0, 1_000, "English subtitles"),
            cue(2_000, 3_000, "<i>tagged speech</i>"),
            cue(4_000, 5_000, "[music]"),
        ];
        let spans = subtitle_speech_spans(&cues, SubtitleTransformOptions::default());

        assert_eq!(spans, vec![Span::new(2_000, 3_000).unwrap()]);
    }

    #[test]
    fn merges_nearby_reference_and_output_cues() {
        let reference = vec![cue(0, 1_000, "A"), cue(10_000, 11_000, "C")];
        let output = vec![cue(9_000, 10_000, "B")];
        let merged = merge_cues(&reference, &output, true);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content, "C\nB");
    }

    #[test]
    fn merge_preserves_requested_content_order_for_equal_starts() {
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
}
