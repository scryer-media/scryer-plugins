use rustfft::num_complex::Complex;

pub(crate) fn backend_suffix() -> &'static str {
    if cfg!(all(target_arch = "wasm32", target_feature = "simd128")) {
        "subtitle-sync-rust-simd"
    } else {
        "subtitle-sync-rust"
    }
}

pub(crate) fn mean_square_i16(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    sum_squares_i16(samples) / samples.len() as f64
}

pub(crate) fn mean_i16(samples: &[i16]) -> i16 {
    if samples.is_empty() {
        return 0;
    }

    let mean = (sum_i16(samples) / samples.len() as f64).trunc() as i32;
    mean.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

pub(crate) fn center_binary(values: &[f64]) -> Vec<f64> {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { center_binary_simd(values) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        values.iter().map(|value| 2.0 * *value - 1.0).collect()
    }
}

pub(crate) fn masked_argmax_offset(
    values: &[f64],
    substring_len: usize,
    max_offset_samples: Option<i64>,
) -> Option<(usize, f64)> {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { masked_argmax_offset_simd(values, substring_len, max_offset_samples) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        masked_argmax_offset_scalar(values, substring_len, max_offset_samples)
    }
}

pub(crate) fn scaled_complex_reals(values: &[Complex<f64>], scale: f64) -> Vec<f64> {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { scaled_complex_reals_simd(values, scale) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        scaled_complex_reals_scalar(values, scale)
    }
}

pub(crate) fn fill_f64(values: &mut [f64], value: f64) {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        unsafe { fill_f64_simd(values, value) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        values.fill(value);
    }
}

pub(crate) fn transform_ms_pair(
    start_ms: i64,
    end_ms: i64,
    ratio: f64,
    offset_ms: i64,
) -> (i64, i64) {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { transform_ms_pair_simd(start_ms, end_ms, ratio, offset_ms) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        (
            transform_one_ms(start_ms, ratio, offset_ms),
            transform_one_ms(end_ms, ratio, offset_ms),
        )
    }
}

#[cfg(any(test, not(all(target_arch = "wasm32", target_feature = "simd128"))))]
fn masked_argmax_offset_scalar(
    values: &[f64],
    substring_len: usize,
    max_offset_samples: Option<i64>,
) -> Option<(usize, f64)> {
    let convolve_len = values.len();
    let mut best: Option<(usize, f64)> = None;
    for (index, score) in values.iter().copied().enumerate() {
        if max_offset_samples
            .is_some_and(|max| !index_within_offset(index, convolve_len, substring_len, max))
        {
            continue;
        }
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((index, score));
        }
    }
    best
}

#[cfg(any(test, not(all(target_arch = "wasm32", target_feature = "simd128"))))]
fn scaled_complex_reals_scalar(values: &[Complex<f64>], scale: f64) -> Vec<f64> {
    values.iter().map(|value| value.re / scale).collect()
}

fn sum_squares_i16(samples: &[i16]) -> f64 {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { sum_squares_i16_simd(samples) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        samples
            .iter()
            .map(|sample| {
                let sample = *sample as f64;
                sample * sample
            })
            .sum()
    }
}

fn sum_i16(samples: &[i16]) -> f64 {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return unsafe { sum_i16_simd(samples) };
    }

    #[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
    {
        samples.iter().map(|sample| f64::from(*sample)).sum()
    }
}

#[cfg(any(test, not(all(target_arch = "wasm32", target_feature = "simd128"))))]
fn transform_one_ms(ms: i64, ratio: f64, offset_ms: i64) -> i64 {
    ((ms as f64) * ratio).round() as i64 + offset_ms
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn sum_squares_i16_simd(samples: &[i16]) -> f64 {
    use core::arch::wasm32::{
        f64x2_add, f64x2_extract_lane, f64x2_mul, f64x2_replace_lane, f64x2_splat,
    };

    let mut acc = f64x2_splat(0.0);
    let mut chunks = samples.chunks_exact(2);
    for chunk in &mut chunks {
        let lanes = f64x2_replace_lane::<1>(
            f64x2_replace_lane::<0>(f64x2_splat(0.0), chunk[0] as f64),
            chunk[1] as f64,
        );
        acc = f64x2_add(acc, f64x2_mul(lanes, lanes));
    }

    let mut sum = f64x2_extract_lane::<0>(acc) + f64x2_extract_lane::<1>(acc);
    for sample in chunks.remainder() {
        let sample = *sample as f64;
        sum += sample * sample;
    }
    sum
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn sum_i16_simd(samples: &[i16]) -> f64 {
    use core::arch::wasm32::{f64x2_add, f64x2_extract_lane, f64x2_replace_lane, f64x2_splat};

    let mut acc = f64x2_splat(0.0);
    let mut chunks = samples.chunks_exact(2);
    for chunk in &mut chunks {
        let lanes = f64x2_replace_lane::<1>(
            f64x2_replace_lane::<0>(f64x2_splat(0.0), chunk[0] as f64),
            chunk[1] as f64,
        );
        acc = f64x2_add(acc, lanes);
    }

    let mut sum = f64x2_extract_lane::<0>(acc) + f64x2_extract_lane::<1>(acc);
    for sample in chunks.remainder() {
        sum += *sample as f64;
    }
    sum
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn scaled_complex_reals_simd(values: &[Complex<f64>], scale: f64) -> Vec<f64> {
    use core::arch::wasm32::{f64x2_extract_lane, f64x2_mul, f64x2_replace_lane, f64x2_splat};

    let inv_scale = f64x2_splat(1.0 / scale);
    let mut out = Vec::with_capacity(values.len());
    let mut chunks = values.chunks_exact(2);
    for chunk in &mut chunks {
        let lanes = f64x2_replace_lane::<1>(
            f64x2_replace_lane::<0>(f64x2_splat(0.0), chunk[0].re),
            chunk[1].re,
        );
        let scaled = f64x2_mul(lanes, inv_scale);
        out.push(f64x2_extract_lane::<0>(scaled));
        out.push(f64x2_extract_lane::<1>(scaled));
    }
    out.extend(chunks.remainder().iter().map(|value| value.re / scale));
    out
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn fill_f64_simd(values: &mut [f64], value: f64) {
    use core::arch::wasm32::{f64x2_splat, v128_store};

    let lanes = f64x2_splat(value);
    let mut chunks = values.chunks_exact_mut(2);
    for chunk in &mut chunks {
        unsafe { v128_store(chunk.as_mut_ptr().cast(), lanes) };
    }
    chunks.into_remainder().fill(value);
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn transform_ms_pair_simd(
    start_ms: i64,
    end_ms: i64,
    ratio: f64,
    offset_ms: i64,
) -> (i64, i64) {
    use core::arch::wasm32::{
        f64x2_add, f64x2_extract_lane, f64x2_mul, f64x2_replace_lane, f64x2_splat,
    };

    let lanes = f64x2_replace_lane::<1>(
        f64x2_replace_lane::<0>(f64x2_splat(0.0), start_ms as f64),
        end_ms as f64,
    );
    let transformed = f64x2_add(
        f64x2_mul(lanes, f64x2_splat(ratio)),
        f64x2_splat(offset_ms as f64),
    );
    (
        f64x2_extract_lane::<0>(transformed).round() as i64,
        f64x2_extract_lane::<1>(transformed).round() as i64,
    )
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn masked_argmax_offset_simd(
    values: &[f64],
    substring_len: usize,
    max_offset_samples: Option<i64>,
) -> Option<(usize, f64)> {
    use core::arch::wasm32::{f64x2_extract_lane, v128_load};

    let convolve_len = values.len();
    let mut best: Option<(usize, f64)> = None;
    let mut chunks = values.chunks_exact(2);
    for (chunk_index, chunk) in chunks.by_ref().enumerate() {
        let lanes = unsafe { v128_load(chunk.as_ptr().cast()) };
        let base = chunk_index * 2;
        let left = (base, f64x2_extract_lane::<0>(lanes));
        let right = (base + 1, f64x2_extract_lane::<1>(lanes));
        for (index, score) in [left, right] {
            if max_offset_samples
                .is_some_and(|max| !index_within_offset(index, convolve_len, substring_len, max))
            {
                continue;
            }
            if best.is_none_or(|(_, best_score)| score > best_score) {
                best = Some((index, score));
            }
        }
    }
    let base = values.len() - chunks.remainder().len();
    for (offset, score) in chunks.remainder().iter().copied().enumerate() {
        let index = base + offset;
        if max_offset_samples
            .is_some_and(|max| !index_within_offset(index, convolve_len, substring_len, max))
        {
            continue;
        }
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((index, score));
        }
    }
    best
}

fn index_within_offset(
    index: usize,
    convolve_len: usize,
    substring_len: usize,
    max_offset_samples: i64,
) -> bool {
    let offset = convolve_len as i64 - 1 - index as i64 - substring_len as i64;
    offset.abs() <= max_offset_samples
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn center_binary_simd(values: &[f64]) -> Vec<f64> {
    use core::arch::wasm32::{
        f64x2_add, f64x2_extract_lane, f64x2_mul, f64x2_replace_lane, f64x2_splat,
    };

    let mut out = Vec::with_capacity(values.len());
    let two = f64x2_splat(2.0);
    let minus_one = f64x2_splat(-1.0);
    let mut chunks = values.chunks_exact(2);
    for chunk in &mut chunks {
        let lanes = f64x2_replace_lane::<1>(
            f64x2_replace_lane::<0>(f64x2_splat(0.0), chunk[0]),
            chunk[1],
        );
        let centered = f64x2_add(f64x2_mul(lanes, two), minus_one);
        out.push(f64x2_extract_lane::<0>(centered));
        out.push(f64x2_extract_lane::<1>(centered));
    }
    out.extend(chunks.remainder().iter().map(|value| 2.0 * *value - 1.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_kernels_match_expected_values() {
        assert_eq!(mean_square_i16(&[2, -2, 4, -4]), 10.0);
        assert_eq!(mean_i16(&[10, -5, 6]), 3);
        assert_eq!(mean_i16(&[-10, 5]), -2);
        assert_eq!(center_binary(&[0.0, 1.0, 0.5]), vec![-1.0, 1.0, 0.0]);
        assert_eq!(
            masked_argmax_offset(&[1.0, 4.0, 4.0, 3.0], 1, None),
            Some((1, 4.0))
        );
        assert_eq!(
            masked_argmax_offset(&[1.0, 4.0, 2.0, 9.0], 1, Some(0)),
            Some((2, 2.0))
        );
        assert_eq!(
            scaled_complex_reals(
                &[
                    Complex::new(2.0, 99.0),
                    Complex::new(-4.0, 0.0),
                    Complex::new(1.0, -1.0)
                ],
                2.0
            ),
            vec![1.0, -2.0, 0.5]
        );
        let mut values = vec![0.0, 1.0, 2.0];
        fill_f64(&mut values[1..], 9.0);
        assert_eq!(values, vec![0.0, 9.0, 9.0]);
        assert_eq!(
            transform_ms_pair(1_001, 2_002, 25.0 / 24.0, -250),
            (793, 1835)
        );
    }
}
