//! Guest-side [`RepairSolver`] that dispatches the PAR2 Reed-Solomon reconstruct
//! to the native host via the `scryer_par2_reconstruct` import (RFC 123 WP2.5).
//!
//! A `wasm32-wasip1` guest cannot run weaver-par2's parallel GF(2^16) matmul
//! (wasip1 has no rayon worker pool). Instead this solver serializes the repair
//! problem's index arrays and `(ptr, len)` region tables into the guest's own
//! linear memory as the frozen "PAR2 v2" descriptor and calls the host, which
//! rebuilds the repair matrix on a native stack and runs the matmul across host
//! threads, zero-copy over the guest's memory. The bulk slice regions are
//! referenced in place: in wasm32 a `&[u8]`'s pointer value *is* its byte offset
//! into linear memory, so `slice.as_ptr() as u64` is exactly the offset the host
//! needs, with `len == word_count * 2`.
//!
//! Everything host-ABI lives in this module; the rest of the plugin only sees
//! the `weaver_par2::RepairSolver` trait.

use scryer_plugin_sdk::par2_reconstruct::{
    self, DESC_HEADER_LEN, Par2ReconstructHeaderFields, Par2ReconstructStatus, TABLE_ENTRY_LEN,
};
use weaver_par2::{RepairProblem, RepairSolver, SolverError};

// The host import, under the same namespace as the frozen crypto ABI (RFC §5).
// `desc_ptr`/`desc_len` are the byte offset and length of the descriptor in the
// guest's exported `"memory"`. The return is `0` on success or a negative
// `Par2ReconstructStatus` code; the host never traps on these.
#[cfg(target_family = "wasm")]
#[link(wasm_import_module = "extism:host/user")]
unsafe extern "C" {
    fn scryer_par2_reconstruct(desc_ptr: i64, desc_len: i64) -> i64;
}

/// Native stub so the crate type-checks off-wasm (unit tests, host `cargo check`
/// / `clippy`). The plugin only ever executes as wasm, where the import above is
/// the sole real path; a native build never dispatches a reconstruct.
#[cfg(not(target_family = "wasm"))]
unsafe fn scryer_par2_reconstruct(_desc_ptr: i64, _desc_len: i64) -> i64 {
    Par2ReconstructStatus::Ok.code()
}

/// A [`RepairSolver`] that marshals each whole-reconstruct to the host function.
pub struct HostDispatchSolver;

impl RepairSolver for HostDispatchSolver {
    fn reconstruct(&self, problem: &mut RepairProblem<'_>) -> Result<(), SolverError> {
        let n_out = problem.outputs.len();
        if n_out == 0 {
            return Ok(());
        }
        let n_avail = problem.available_indices.len();

        // Contract sanity the host also re-validates: weaver-par2 hands one
        // missing index and one recovery exponent per output row, and sources
        // ordered `[avail..., recovery...]`.
        if problem.missing_indices.len() != n_out {
            return Err(SolverError::Dimensions(format!(
                "{n_out} outputs but {} missing indices",
                problem.missing_indices.len()
            )));
        }
        if problem.recovery_exponents.len() != n_out {
            return Err(SolverError::Dimensions(format!(
                "{n_out} outputs but {} recovery exponents",
                problem.recovery_exponents.len()
            )));
        }
        if problem.sources.len() != n_avail + n_out {
            return Err(SolverError::Dimensions(format!(
                "{} sources but n_avail({n_avail}) + n_out({n_out})",
                problem.sources.len()
            )));
        }

        // Snapshot each region's `(linear-memory offset, len)`. In wasm32 a
        // slice's pointer value is its byte offset into linear memory, which is
        // precisely what the host needs to address the region zero-copy. The
        // referenced bytes live in the caller's `input_buffers`/`repaired_slices`
        // and stay put for the whole synchronous host call.
        let src_regions: Vec<(u64, u64)> = problem
            .sources
            .iter()
            .map(|s| (s.as_ptr() as u64, s.len() as u64))
            .collect();
        let out_regions: Vec<(u64, u64)> = problem
            .outputs
            .iter()
            .map(|o| (o.as_ptr() as u64, o.len() as u64))
            .collect();

        let input = DescriptorInput {
            total_inputs: problem.total_inputs,
            word_count: problem.word_count,
            missing_indices: problem.missing_indices,
            available_indices: problem.available_indices,
            recovery_exponents: problem.recovery_exponents,
            src_regions: &src_regions,
            out_regions: &out_regions,
        };

        // One contiguous scratch buffer holds header + index arrays + region
        // tables. It is allocated at full size (no realloc) and moving the `Vec`
        // out of `build_descriptor` does not move its heap bytes, so every
        // pointer baked into it stays valid across the host call below.
        let desc = build_descriptor(&input)?;
        let code = unsafe { scryer_par2_reconstruct(desc.as_ptr() as i64, desc.len() as i64) };
        // Keep `desc` (and thus its pointers) alive until after the host returns.
        drop(desc);

        map_return_code(code)
    }
}

/// The scalar inputs and region tables for one reconstruct descriptor.
struct DescriptorInput<'a> {
    total_inputs: usize,
    word_count: usize,
    missing_indices: &'a [usize],
    available_indices: &'a [usize],
    recovery_exponents: &'a [u32],
    /// `(ptr, len)` of each source region: `n_avail` available slices (avail
    /// order) then `n_out` recovery blocks (exponent order).
    src_regions: &'a [(u64, u64)],
    /// `(ptr, len)` of each of the `n_out` missing-output regions.
    out_regions: &'a [(u64, u64)],
}

/// Byte offsets, within a single descriptor buffer, of each section.
struct DescLayout {
    missing_off: usize,
    avail_off: usize,
    exp_off: usize,
    src_table_off: usize,
    out_table_off: usize,
    total_len: usize,
}

impl DescLayout {
    fn new(n_avail: usize, n_out: usize) -> Self {
        let n_src = n_avail + n_out;
        let missing_off = DESC_HEADER_LEN;
        let avail_off = missing_off + n_out * 4;
        let exp_off = avail_off + n_avail * 4;
        let src_table_off = exp_off + n_out * 4;
        let out_table_off = src_table_off + n_src * TABLE_ENTRY_LEN;
        let total_len = out_table_off + n_out * TABLE_ENTRY_LEN;
        Self {
            missing_off,
            avail_off,
            exp_off,
            src_table_off,
            out_table_off,
            total_len,
        }
    }
}

/// Allocate and fully serialize a PAR2-v2 descriptor for `input`. The returned
/// buffer's own linear-memory address is used as the pointer base, so the
/// header's `*_ptr` fields are absolute offsets the host resolves directly.
fn build_descriptor(input: &DescriptorInput<'_>) -> Result<Vec<u8>, SolverError> {
    let layout = DescLayout::new(input.available_indices.len(), input.recovery_exponents.len());
    let mut desc = vec![0u8; layout.total_len];
    let base = desc.as_ptr() as u64;
    write_descriptor(&mut desc, base, input)?;
    Ok(desc)
}

/// Serialize the descriptor into `out` (header + index arrays + region tables),
/// with `base` the linear-memory offset of `out[0]`. Split from
/// [`build_descriptor`] so the byte layout is unit-testable with `base = 0`.
fn write_descriptor(
    out: &mut [u8],
    base: u64,
    input: &DescriptorInput<'_>,
) -> Result<(), SolverError> {
    let n_out = input.recovery_exponents.len();
    let n_avail = input.available_indices.len();
    let layout = DescLayout::new(n_avail, n_out);
    if out.len() < layout.total_len {
        return Err(SolverError::Dimensions(format!(
            "descriptor buffer {} < required {}",
            out.len(),
            layout.total_len
        )));
    }

    // Index arrays (little-endian u32).
    for (i, &m) in input.missing_indices.iter().enumerate() {
        put(par2_reconstruct::write_u32_array_entry(
            out,
            layout.missing_off,
            i,
            m as u32,
        ))?;
    }
    for (i, &a) in input.available_indices.iter().enumerate() {
        put(par2_reconstruct::write_u32_array_entry(
            out,
            layout.avail_off,
            i,
            a as u32,
        ))?;
    }
    for (i, &e) in input.recovery_exponents.iter().enumerate() {
        put(par2_reconstruct::write_u32_array_entry(
            out,
            layout.exp_off,
            i,
            e,
        ))?;
    }

    // Region tables: `(u64 ptr, u64 len)` per entry. `sources` is already ordered
    // `[avail..., recovery...]` by weaver-par2, matching the repair-matrix columns.
    for (i, &(ptr, len)) in input.src_regions.iter().enumerate() {
        put(par2_reconstruct::write_table_entry(
            out,
            layout.src_table_off,
            i,
            ptr,
            len,
        ))?;
    }
    for (i, &(ptr, len)) in input.out_regions.iter().enumerate() {
        put(par2_reconstruct::write_table_entry(
            out,
            layout.out_table_off,
            i,
            ptr,
            len,
        ))?;
    }

    // Header. `*_ptr` fields are absolute (`base` + in-buffer offset); the host
    // recomputes `constants` from `total_inputs`, so none are sent. `flags` = 0.
    let fields = Par2ReconstructHeaderFields {
        total_inputs: input.total_inputs as u32,
        n_out: n_out as u32,
        n_avail: n_avail as u32,
        word_count: input.word_count as u32,
        flags: 0,
        missing_idx_ptr: base + layout.missing_off as u64,
        avail_idx_ptr: base + layout.avail_off as u64,
        exponent_ptr: base + layout.exp_off as u64,
        src_table_ptr: base + layout.src_table_off as u64,
        out_table_ptr: base + layout.out_table_off as u64,
    };
    put(par2_reconstruct::write_header(out, &fields))?;
    Ok(())
}

/// Turn an SDK writer's bounds-check `Option` into a [`SolverError`]. `None` can
/// only occur if the buffer was mis-sized, which [`build_descriptor`] prevents.
#[inline]
fn put(result: Option<()>) -> Result<(), SolverError> {
    result.ok_or_else(|| SolverError::Dimensions("descriptor region out of bounds".to_string()))
}

/// Map the host return code: `>= 0` is success; every negative is fatal and
/// surfaces as a [`SolverError`] (which weaver-par2 turns into a repair failure).
fn map_return_code(code: i64) -> Result<(), SolverError> {
    if code >= 0 {
        return Ok(());
    }
    match Par2ReconstructStatus::from_code(code) {
        Some(Par2ReconstructStatus::Singular) => Err(SolverError::Singular { bad_row: None }),
        _ => Err(SolverError::Host(format!(
            "scryer_par2_reconstruct returned {code} ({})",
            Par2ReconstructStatus::describe(code)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::par2_reconstruct::{parse_header, read_table_entry, read_u32_array};

    #[test]
    fn descriptor_round_trips_through_the_sdk_host_readers() {
        let total_inputs = 12usize;
        let word_count = 64usize;
        let slice_bytes = (word_count * 2) as u64;
        let missing = [2usize, 5, 9];
        let avail = [0usize, 1, 3, 4, 6, 7, 8, 10, 11];
        let exps = [0u32, 1, 2];

        let src_regions: Vec<(u64, u64)> = (0..avail.len() + missing.len())
            .map(|i| (0x1_0000 + i as u64 * slice_bytes, slice_bytes))
            .collect();
        let out_regions: Vec<(u64, u64)> = (0..missing.len())
            .map(|i| (0x9_0000 + i as u64 * slice_bytes, slice_bytes))
            .collect();

        let input = DescriptorInput {
            total_inputs,
            word_count,
            missing_indices: &missing,
            available_indices: &avail,
            recovery_exponents: &exps,
            src_regions: &src_regions,
            out_regions: &out_regions,
        };

        let layout = DescLayout::new(avail.len(), missing.len());
        let mut buf = vec![0u8; layout.total_len];
        // base = 0 so the header's absolute `*_ptr` values equal in-buffer
        // offsets and are directly addressable within `buf` by the host readers.
        write_descriptor(&mut buf, 0, &input).unwrap();

        let header = parse_header(&buf, 0).expect("valid header parses");
        assert_eq!(header.total_inputs, total_inputs);
        assert_eq!(header.n_out, missing.len());
        assert_eq!(header.n_avail, avail.len());
        assert_eq!(header.word_count, word_count);
        assert_eq!(header.slice_bytes, word_count * 2);
        assert_eq!(header.flags, 0);
        assert_eq!(header.n_src(), avail.len() + missing.len());

        assert_eq!(
            read_u32_array(&buf, header.missing_idx_ptr, header.n_out).unwrap(),
            missing.iter().map(|&x| x as u32).collect::<Vec<_>>()
        );
        assert_eq!(
            read_u32_array(&buf, header.avail_idx_ptr, header.n_avail).unwrap(),
            avail.iter().map(|&x| x as u32).collect::<Vec<_>>()
        );
        assert_eq!(
            read_u32_array(&buf, header.exponent_ptr, header.n_out).unwrap(),
            exps.to_vec()
        );

        for (i, &(ptr, len)) in src_regions.iter().enumerate() {
            assert_eq!(
                read_table_entry(&buf, header.src_table_ptr, i),
                Some((ptr as usize, len as usize))
            );
        }
        for (i, &(ptr, len)) in out_regions.iter().enumerate() {
            assert_eq!(
                read_table_entry(&buf, header.out_table_ptr, i),
                Some((ptr as usize, len as usize))
            );
        }
    }

    #[test]
    fn negative_codes_map_to_solver_errors() {
        assert!(map_return_code(0).is_ok());
        assert!(map_return_code(1).is_ok());
        assert_eq!(
            map_return_code(Par2ReconstructStatus::Singular.code()),
            Err(SolverError::Singular { bad_row: None })
        );
        // A non-singular negative is an opaque host failure.
        match map_return_code(Par2ReconstructStatus::Alias.code()) {
            Err(SolverError::Host(_)) => {}
            other => panic!("expected Host error, got {other:?}"),
        }
        match map_return_code(Par2ReconstructStatus::DeadlineExceeded.code()) {
            Err(SolverError::Host(_)) => {}
            other => panic!("expected Host error, got {other:?}"),
        }
    }
}
