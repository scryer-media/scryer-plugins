use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use super::simd;
use super::transformers::{SAMPLE_RATE, Span, ms_to_sample, samples_to_ms, scale_ms};

pub(crate) const FRAMERATE_RATIOS: [f64; 3] = [24.0 / 23.976, 25.0 / 23.976, 25.0 / 24.0];
const MIN_FRAMERATE_RATIO: f64 = 0.9;
const MAX_FRAMERATE_RATIO: f64 = 1.1;
const GSS_TOLERANCE: f64 = 1e-4;
const RATIO_EPSILON: f64 = 1e-6;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Alignment {
    pub score: f64,
    pub offset_samples: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub ratio: f64,
    pub alignment: Alignment,
}

pub(crate) fn best_candidate(
    reference_timeline: &[f64],
    subtitle_spans: &[Span],
    max_offset_seconds: i64,
    precise_framerate_search: bool,
) -> Option<Candidate> {
    let max_offset_samples = Some(max_offset_seconds.saturating_abs() * i64::from(SAMPLE_RATE));
    let mut ratios = vec![1.0];
    for ratio in FRAMERATE_RATIOS {
        ratios.push(ratio);
        ratios.push(1.0 / ratio);
    }

    let base_subtitle_timeline = subtitle_spans_to_timeline(subtitle_spans, 1.0);
    if let (Some(reference_frames), Some(subtitle_frames)) = (
        nonzero_frame_span(reference_timeline),
        nonzero_frame_span(&base_subtitle_timeline),
    ) && subtitle_frames > 0
    {
        ratios.push(reference_frames as f64 / subtitle_frames as f64);
    }

    let mut candidates = Vec::new();
    for ratio in dedupe_ratios(ratios) {
        if (MIN_FRAMERATE_RATIO..=MAX_FRAMERATE_RATIO).contains(&ratio) {
            if let Some(candidate) = score_candidate(
                reference_timeline,
                subtitle_spans,
                max_offset_samples,
                ratio,
            ) {
                candidates.push(candidate);
            }
        }
    }

    if precise_framerate_search
        && let Some(candidate) = golden_section_candidate(
            reference_timeline,
            subtitle_spans,
            max_offset_samples,
            MIN_FRAMERATE_RATIO,
            MAX_FRAMERATE_RATIO,
        )
    {
        candidates.push(candidate);
    }

    choose_best(candidates)
}

pub(crate) fn subtitle_spans_to_timeline(spans: &[Span], ratio: f64) -> Vec<f64> {
    let max_sample = spans
        .iter()
        .map(|span| {
            let start = ms_to_sample(scale_ms(span.start_ms, ratio));
            let duration = ms_to_sample(scale_ms(span.end_ms - span.start_ms, ratio));
            start + duration
        })
        .max()
        .unwrap_or(0);
    let mut timeline = vec![0.0; max_sample + 2];
    for span in spans {
        let start = ms_to_sample(scale_ms(span.start_ms, ratio)).min(timeline.len());
        let duration = ms_to_sample(scale_ms(span.end_ms - span.start_ms, ratio));
        let end = (start + duration).min(timeline.len());
        if end > start {
            let fill_value = (1.0 / ratio).min(1.0);
            timeline[start..end].fill(fill_value);
        }
    }
    timeline
}

pub(crate) fn fft_align(
    reference_timeline: &[f64],
    subtitle_timeline: &[f64],
    max_offset_samples: Option<i64>,
) -> Alignment {
    if reference_timeline.is_empty() || subtitle_timeline.is_empty() {
        return Alignment {
            score: f64::NEG_INFINITY,
            offset_samples: 0,
        };
    }

    let reference = simd::center_binary(reference_timeline);
    let subtitle = simd::center_binary(subtitle_timeline);
    let total_length = (reference.len() + subtitle.len()).next_power_of_two();
    let extra_zeros = total_length - reference.len() - subtitle.len();
    let mut sub_input = vec![Complex::new(0.0, 0.0); total_length];
    for (index, value) in subtitle.iter().enumerate() {
        sub_input[extra_zeros + reference.len() + index].re = *value;
    }
    let mut ref_input = vec![Complex::new(0.0, 0.0); total_length];
    for (index, value) in reference.iter().rev().enumerate() {
        ref_input[subtitle.len() + extra_zeros + index].re = *value;
    }

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(total_length);
    fft.process(&mut sub_input);
    fft.process(&mut ref_input);
    for (left, right) in sub_input.iter_mut().zip(ref_input.iter()) {
        *left *= *right;
    }
    let ifft = planner.plan_fft_inverse(total_length);
    ifft.process(&mut sub_input);

    let scale = total_length as f64;
    let scores = simd::scaled_complex_reals(&sub_input, scale);
    let Some((best_idx, best_score)) =
        simd::masked_argmax_offset(&scores, subtitle.len(), max_offset_samples)
    else {
        return Alignment {
            score: f64::NEG_INFINITY,
            offset_samples: 0,
        };
    };
    let offset_samples = total_length as i64 - 1 - best_idx as i64 - subtitle.len() as i64;
    Alignment {
        score: best_score,
        offset_samples,
    }
}

pub(crate) fn offset_ms(candidate: &Candidate) -> i64 {
    samples_to_ms(candidate.alignment.offset_samples)
}

#[allow(unused_assignments)]
fn golden_section_candidate(
    reference_timeline: &[f64],
    subtitle_spans: &[Span],
    max_offset_samples: Option<i64>,
    mut a: f64,
    mut b: f64,
) -> Option<Candidate> {
    let invphi = (5.0_f64.sqrt() - 1.0) / 2.0;
    let invphi2 = (3.0 - 5.0_f64.sqrt()) / 2.0;
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let mut h = b - a;
    if h <= GSS_TOLERANCE {
        return score_candidate(
            reference_timeline,
            subtitle_spans,
            max_offset_samples,
            (a + b) / 2.0,
        );
    }

    let n = ((GSS_TOLERANCE / h).ln() / invphi.ln()).ceil() as usize;
    let mut c = a + invphi2 * h;
    let mut d = a + invphi * h;
    let mut yc = -score_candidate(reference_timeline, subtitle_spans, max_offset_samples, c)?
        .alignment
        .score;
    let mut yd = -score_candidate(reference_timeline, subtitle_spans, max_offset_samples, d)?
        .alignment
        .score;
    let mut best = choose_better(
        score_candidate(reference_timeline, subtitle_spans, max_offset_samples, c),
        score_candidate(reference_timeline, subtitle_spans, max_offset_samples, d),
    );

    for _ in 0..n.saturating_sub(1) {
        if yc < yd {
            b = d;
            d = c;
            yd = yc;
            h *= invphi;
            c = a + invphi2 * h;
            if let Some(candidate) =
                score_candidate(reference_timeline, subtitle_spans, max_offset_samples, c)
            {
                yc = -candidate.alignment.score;
                best = choose_better(best, Some(candidate));
            }
        } else {
            a = c;
            c = d;
            yc = yd;
            h *= invphi;
            d = a + invphi * h;
            if let Some(candidate) =
                score_candidate(reference_timeline, subtitle_spans, max_offset_samples, d)
            {
                yd = -candidate.alignment.score;
                best = choose_better(best, Some(candidate));
            }
        }
    }
    best
}

fn score_candidate(
    reference_timeline: &[f64],
    subtitle_spans: &[Span],
    max_offset_samples: Option<i64>,
    ratio: f64,
) -> Option<Candidate> {
    if !(MIN_FRAMERATE_RATIO..=MAX_FRAMERATE_RATIO).contains(&ratio) {
        return None;
    }
    let timeline = subtitle_spans_to_timeline(subtitle_spans, ratio);
    let alignment = fft_align(reference_timeline, &timeline, max_offset_samples);
    alignment
        .score
        .is_finite()
        .then_some(Candidate { ratio, alignment })
}

fn choose_best(candidates: impl IntoIterator<Item = Candidate>) -> Option<Candidate> {
    let mut best: Option<Candidate> = None;
    for candidate in candidates {
        if !candidate.alignment.score.is_finite() {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|current| candidate.alignment.score > current.alignment.score)
        {
            best = Some(candidate);
        }
    }
    best
}

fn choose_better(left: Option<Candidate>, right: Option<Candidate>) -> Option<Candidate> {
    choose_best(left.into_iter().chain(right))
}

fn dedupe_ratios(ratios: Vec<f64>) -> Vec<f64> {
    let mut deduped: Vec<f64> = Vec::new();
    for ratio in ratios {
        if ratio.is_finite()
            && !deduped
                .iter()
                .any(|existing| (*existing - ratio).abs() < RATIO_EPSILON)
        {
            deduped.push(ratio);
        }
    }
    deduped
}

fn nonzero_frame_span(timeline: &[f64]) -> Option<usize> {
    let first = timeline.iter().position(|value| *value > 0.0)?;
    let last = timeline.iter().rposition(|value| *value > 0.0)?;
    Some(last.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_alignment_matches_upstream_cases() {
        let cases = [
            ("111001", "11001", -1),
            ("1001", "1001", 0),
            ("10010", "01001", 1),
        ];
        for (substring, reference, expected_offset) in cases {
            let reference = reference
                .chars()
                .map(|ch| if ch == '1' { 1.0 } else { 0.0 })
                .collect::<Vec<_>>();
            let substring = substring
                .chars()
                .map(|ch| if ch == '1' { 1.0 } else { 0.0 })
                .collect::<Vec<_>>();
            assert_eq!(
                fft_align(&reference, &substring, None).offset_samples,
                expected_offset
            );
            assert_eq!(
                best_candidate(
                    &reference,
                    &[Span::new(0, substring.len() as i64 * 10).unwrap()],
                    60,
                    false
                )
                .is_some(),
                true
            );
        }
    }

    #[test]
    fn max_offset_rejects_large_shift() {
        let reference = vec![1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let subtitle = vec![0.0, 0.0, 0.0, 1.0, 1.0];
        let unconstrained = fft_align(&reference, &subtitle, None);
        let constrained = fft_align(&reference, &subtitle, Some(1));

        assert_ne!(unconstrained.offset_samples, constrained.offset_samples);
        assert!(constrained.offset_samples.abs() <= 1);
    }

    #[test]
    fn max_score_selection_constrains_each_candidate_offset() {
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
        let reference = super::super::transformers::spans_to_timeline(&shifted, 1.0);
        let constrained = best_candidate(&reference, &subtitle_spans, 1, false).expect("candidate");

        assert!(constrained.alignment.offset_samples.abs() <= i64::from(SAMPLE_RATE));
    }

    #[test]
    fn golden_section_search_finds_nonstandard_ratio() {
        let subtitle_spans = (0..18)
            .filter_map(|index| {
                let start = 2_000 + index * 8_000;
                Span::new(start, start + 1_400)
            })
            .collect::<Vec<_>>();
        let true_ratio = 0.943;
        let reference = super::super::transformers::spans_to_timeline(&subtitle_spans, true_ratio);
        let candidate =
            golden_section_candidate(&reference, &subtitle_spans, Some(6_000), 0.9, 1.1)
                .expect("candidate");

        assert!((candidate.ratio - true_ratio).abs() < 0.01);
        assert!(candidate.alignment.offset_samples.abs() <= 1);
    }
}
