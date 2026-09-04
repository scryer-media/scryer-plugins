//! Plugin-internal PAR2 verification, placement and repair.
//!
//! PAR2 is deliberately absent from the archive plugin contract: there is no
//! `verify-par2` operation and no PAR2 report on the response. Recovery sets
//! are instead handled *data-driven* — every `Inspect` and `ExtractArchive`
//! looks at the source directory it was given, and if a `.par2` set is sitting
//! there it is used. The host asks for an extraction; whether that extraction
//! needed a repair first is this plugin's business.
//!
//! ## The sandbox shapes the design
//!
//! The source preopen is READ-ONLY, so nothing here ever writes beside the
//! inputs. Corrected bytes are materialized in one of two writable places:
//!
//! * the private `TMPDIR` scratch, when the recovery set protects an archive —
//!   the extractor then reads its volumes from the scratch copy, and the
//!   scratch is removed when the invocation ends; or
//! * the output directory, when the recovery set protects plain (non-archive)
//!   files — those repaired files ARE the deliverable, and the host's import
//!   pass picks them up from there.
//!
//! ## Behaviour, and where it comes from
//!
//! The semantics are ported from Scryer's own host-side implementation
//! (`crates/scryer-application/src/import/archive_extractor.rs`), so moving the
//! work into the plugin does not move the behaviour:
//!
//! * **Placement.** A set whose files are misnamed or swapped on disk is
//!   matched by content hash, not by name, and the canonical names are restored
//!   in the staging copy. Ambiguous placement (`conflicts`) is refused rather
//!   than guessed.
//! * **Repair before extraction.** A damaged-but-repairable set is staged,
//!   repaired, and then RE-VERIFIED; a repair that does not verify clean is a
//!   failure, never a silent pass-through.
//! * **Archive resolution from metadata.** The archive to open is chosen from
//!   the PAR2 metadata (first RAR volume of the identified group, or the single
//!   file with the requested extension), with the caller's `archive_path` used
//!   only as a disambiguating hint. That is what lets an obfuscated download
//!   extract at all.
//! * **Unrepairable is terminal.** Insufficient recovery data fails with a
//!   clear message. A verification that hits par2-rs's own resource limits
//!   degrades to "extract without PAR2" rather than failing, matching the
//!   host's warn-and-continue.
//! * **Absent is a no-op.** No `.par2` files means the extraction runs exactly
//!   as it did before this module existed.
//!
//! Memory stays bounded: files are copied with a fixed streaming buffer, never
//! read whole, and par2-rs's own repair budget bounds the reconstruction
//! workspace. The guest is single-threaded.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use par2_rs::disk::PlacementFileAccess;
use par2_rs::{
    DiskFileAccess, Par2FileSet, RepairOptions, Repairability, execute_repair_with_options,
    placement::PlacementPlan, plan_repair, scan_placement, verify::verify_all,
};
use scryer_plugin_sdk::{
    ArchivePluginExtractedFile, ArchivePluginFormat, ArchivePluginProcessResponse,
    ArchivePluginStatus,
};

use crate::{
    MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_EXPANDED_BYTES, copy_limited, empty_response, failed_message,
    failed_response,
};

/// Recovery-volume ceiling for one set. Real sets run to tens of volumes; this
/// only stops a directory stuffed with `.par2` names from turning the initial
/// scan into the attack.
const MAX_PAR2_RECOVERY_FILES: usize = 4_096;

/// Protected-file ceiling, matching the archive entry ceiling: both bound the
/// number of paths one request can be made to touch.
const MAX_PAR2_PROTECTED_FILES: usize = MAX_ARCHIVE_ENTRIES;

/// Total bytes this plugin will stage or emit for one recovery set.
const MAX_PAR2_STAGED_BYTES: u64 = MAX_ARCHIVE_EXPANDED_BYTES;

/// Guest path of the private per-invocation scratch dir, and the fallback when
/// `TMPDIR` is absent. The host preopens it read-write and points `TMPDIR` at
/// it; the fallback keeps native unit tests honest if the variable is unset.
const SCRATCH_ENV: &str = "TMPDIR";
const SCRATCH_FALLBACK: &str = "/tmp";

/// What the caller should do after PAR2 handling.
pub(crate) enum Par2Plan {
    /// No recovery set (or one that could not be evaluated within par2-rs's
    /// resource limits). Extract exactly as requested.
    NoRecoverySet,
    /// A verified input set. Extract `archive_path` out of `source_dir`.
    Prepared(Par2Inputs),
    /// Terminal: the response is complete and must be returned as-is. Carries
    /// both the plain-file emission success and every PAR2 failure.
    Complete(Box<ArchivePluginProcessResponse>),
}

/// A verified archive input, plus the scratch directory that holds it.
///
/// Only the archive path is carried: the RAR volume scan derives its directory
/// from the archive's own parent, so a staged set is found wherever the archive
/// was materialized without anything else having to be told about it.
pub(crate) struct Par2Inputs {
    pub(crate) archive_path: PathBuf,
    /// Removed by [`Par2Inputs::cleanup`] once extraction has finished with it.
    staging_dir: Option<PathBuf>,
}

impl Par2Inputs {
    /// Drop the staging copy. Called after extraction, success or failure: the
    /// staged bytes are a duplicate of inputs the host still owns.
    pub(crate) fn cleanup(&self) {
        if let Some(staging_dir) = &self.staging_dir {
            let _ = fs::remove_dir_all(staging_dir);
        }
    }
}

/// A verification pass over a recovery set, with the on-disk location of every
/// protected file — which is not its canonical name when the set is misnamed.
struct Placement {
    /// Canonical PAR2 filename -> where those bytes actually live right now.
    actual_by_canonical: HashMap<String, PathBuf>,
    state: State,
    /// Renames plus swaps: non-zero means the on-disk names are not canonical,
    /// so extraction cannot read the set in place.
    move_count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Verified,
    Repairable,
    InsufficientRecoveryData,
    ResourceLimited,
}

/// What a recovery set turns out to protect.
enum Target {
    /// An archive of the requested format; the value is its canonical name
    /// (the first volume, for a multi-volume RAR set).
    Archive(String),
    /// No archive in any format this plugin extracts — the set protects plain
    /// files, which are themselves the deliverable.
    PlainFiles,
}

/// Run PAR2 handling for an extraction request.
///
/// `output_dir` is where repaired plain files are emitted; it is not touched
/// when the set protects an archive.
pub(crate) fn prepare_for_extraction(
    source_dir: &Path,
    archive_hint: &Path,
    format: ArchivePluginFormat,
    output_dir: &Path,
) -> Par2Plan {
    let recovery_files = match find_recovery_files(source_dir) {
        Ok(files) if files.is_empty() => return Par2Plan::NoRecoverySet,
        Ok(files) => files,
        Err(response) => return Par2Plan::Complete(response),
    };

    let set = match load_set(&recovery_files) {
        Ok(set) => set,
        Err(response) => return Par2Plan::Complete(response),
    };

    let placement = match scan(source_dir, &set) {
        Ok(placement) => placement,
        Err(response) => return Par2Plan::Complete(response),
    };

    match placement.state {
        // par2-rs could not finish within its own budget. The host warns and
        // extracts anyway rather than failing an archive that may well be
        // intact; do the same.
        State::ResourceLimited => return Par2Plan::NoRecoverySet,
        State::InsufficientRecoveryData => {
            return Par2Plan::Complete(Box::new(failed_message(
                "par2_insufficient_recovery",
                "PAR2 set does not have enough recovery data to repair these files",
            )));
        }
        State::Verified | State::Repairable => {}
    }

    let target = match resolve_target(&set, &placement, format, archive_hint) {
        Ok(target) => target,
        Err(response) => return Par2Plan::Complete(response),
    };

    match target {
        Target::PlainFiles => emit_plain_files(&set, &placement, output_dir),
        Target::Archive(canonical) => prepare_archive(&set, &placement, &canonical, source_dir),
    }
}

/// Run PAR2 handling for an inspection request.
///
/// `Inspect` gets no output preopen, so this reports what the set says without
/// materializing anything. `None` means there is no recovery set and the caller
/// should answer for itself.
pub(crate) fn inspect(source_dir: &Path) -> Option<ArchivePluginProcessResponse> {
    let recovery_files = match find_recovery_files(source_dir) {
        Ok(files) if files.is_empty() => return None,
        Ok(files) => files,
        Err(response) => return Some(*response),
    };
    let set = match load_set(&recovery_files) {
        Ok(set) => set,
        Err(response) => return Some(*response),
    };
    let placement = match scan(source_dir, &set) {
        Ok(placement) => placement,
        Err(response) => return Some(*response),
    };

    let mut files = placement
        .actual_by_canonical
        .keys()
        .map(|canonical| ArchivePluginExtractedFile {
            relative_path: canonical.clone(),
            size: None,
            checksum: None,
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let (status, message) = match placement.state {
        State::Verified if placement.move_count == 0 => (
            ArchivePluginStatus::Ok,
            format!("PAR2 set verified {} file(s)", files.len()),
        ),
        State::Verified => (
            ArchivePluginStatus::Ok,
            format!(
                "PAR2 set verified {} file(s); {} are misnamed and will be placed before extraction",
                files.len(),
                placement.move_count
            ),
        ),
        State::Repairable => (
            ArchivePluginStatus::Ok,
            "PAR2 set is damaged but repairable; extraction will repair it first".to_string(),
        ),
        State::InsufficientRecoveryData => {
            return Some(failed_message(
                "par2_insufficient_recovery",
                "PAR2 set does not have enough recovery data to repair these files",
            ));
        }
        State::ResourceLimited => (
            ArchivePluginStatus::Ok,
            "PAR2 verification exceeded its resource limits and was not completed".to_string(),
        ),
    };

    Some(ArchivePluginProcessResponse {
        status,
        files,
        message: Some(message),
        ..empty_response()
    })
}

// ---------------------------------------------------------------------------
// Discovery and verification
// ---------------------------------------------------------------------------

fn find_recovery_files(dir: &Path) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    let Ok(entries) = fs::read_dir(dir) else {
        // An unreadable source directory is the extractor's problem to report,
        // not PAR2's: fall through to "no recovery set".
        return Ok(Vec::new());
    };

    let mut recovery_files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_par2 = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("par2"));
        if !is_par2 {
            continue;
        }
        if recovery_files.len() >= MAX_PAR2_RECOVERY_FILES {
            return Err(Box::new(failed_message(
                "too_many_par2_files",
                &format!(
                    "source directory contains more than {MAX_PAR2_RECOVERY_FILES} PAR2 recovery files"
                ),
            )));
        }
        recovery_files.push(path);
    }
    recovery_files.sort();
    Ok(recovery_files)
}

fn load_set(recovery_files: &[PathBuf]) -> Result<Par2FileSet, Box<ArchivePluginProcessResponse>> {
    let set = Par2FileSet::from_paths(recovery_files).map_err(|error| {
        Box::new(failed_response(
            "par2_load",
            "failed to read the PAR2 recovery set",
            error,
        ))
    })?;
    if set.files.len() > MAX_PAR2_PROTECTED_FILES {
        return Err(Box::new(failed_message(
            "too_many_par2_files",
            &format!("PAR2 set protects more than {MAX_PAR2_PROTECTED_FILES} files"),
        )));
    }
    validate_file_names(&set)?;
    Ok(set)
}

/// Every canonical filename in the set must be a safe relative path, and no two
/// may collide. This runs before any filesystem access, so a hostile set cannot
/// aim a staged write outside the scratch directory.
fn validate_file_names(set: &Par2FileSet) -> Result<(), Box<ArchivePluginProcessResponse>> {
    let mut seen = HashSet::new();
    for description in set.files.values() {
        let relative = safe_relative_path(&description.filename)?;
        if !seen.insert(relative) {
            return Err(Box::new(failed_message(
                "par2_duplicate_path",
                &format!(
                    "PAR2 metadata contains duplicate file path '{}'",
                    description.filename
                ),
            )));
        }
    }
    Ok(())
}

fn scan(
    source_dir: &Path,
    set: &Par2FileSet,
) -> Result<Placement, Box<ArchivePluginProcessResponse>> {
    let plan = scan_placement(source_dir, set).map_err(|error| {
        Box::new(failed_response(
            "par2_placement",
            "failed to scan PAR2 file placement",
            error,
        ))
    })?;
    if !plan.conflicts.is_empty() {
        return Err(Box::new(failed_message(
            "par2_ambiguous_placement",
            &format!(
                "PAR2 placement is ambiguous for {} file(s); refusing to guess the archive order",
                plan.conflicts.len()
            ),
        )));
    }

    let access = PlacementFileAccess::from_plan(source_dir.to_path_buf(), set, &plan);
    let verification = verify_all(set, &access);
    let state = match verification.repairable {
        Repairability::NotNeeded => State::Verified,
        Repairability::Repairable { .. } => State::Repairable,
        Repairability::Insufficient { .. } => State::InsufficientRecoveryData,
        Repairability::ResourceLimited { .. } => State::ResourceLimited,
    };

    Ok(Placement {
        actual_by_canonical: actual_paths_by_canonical(source_dir, set, &plan),
        state,
        move_count: plan.renames.len() + plan.swaps.len().saturating_mul(2),
    })
}

/// Map each canonical PAR2 filename to where its bytes are right now.
///
/// Files sitting at their canonical name map to themselves; renames and swaps
/// override that with the name the file currently wears. This is what makes an
/// obfuscated download extractable without renaming anything in the read-only
/// source directory.
fn actual_paths_by_canonical(
    source_dir: &Path,
    set: &Par2FileSet,
    plan: &PlacementPlan,
) -> HashMap<String, PathBuf> {
    let mut actual = HashMap::new();
    for description in set.files.values() {
        actual.insert(
            description.filename.clone(),
            source_dir.join(safe_relative_path_lossy(&description.filename)),
        );
    }
    for (left, right) in &plan.swaps {
        actual.insert(
            left.correct_name.clone(),
            source_dir.join(safe_relative_path_lossy(&left.current_name)),
        );
        actual.insert(
            right.correct_name.clone(),
            source_dir.join(safe_relative_path_lossy(&right.current_name)),
        );
    }
    for entry in &plan.renames {
        actual.insert(
            entry.correct_name.clone(),
            source_dir.join(safe_relative_path_lossy(&entry.current_name)),
        );
    }
    actual
}

// ---------------------------------------------------------------------------
// Archive resolution
// ---------------------------------------------------------------------------

fn resolve_target(
    set: &Par2FileSet,
    placement: &Placement,
    format: ArchivePluginFormat,
    hint: &Path,
) -> Result<Target, Box<ArchivePluginProcessResponse>> {
    let hint = canonical_hint(placement, hint);
    let resolved = match format {
        ArchivePluginFormat::Rar => rar_first_volume(placement, &hint),
        ArchivePluginFormat::SevenZip => single_archive(placement, &hint, &["7z"]),
        ArchivePluginFormat::Zip => single_archive(placement, &hint, &["zip"]),
        ArchivePluginFormat::Xz => single_archive(placement, &hint, &["xz", "txz"]),
    };

    match resolved {
        Ok(canonical) => Ok(Target::Archive(canonical)),
        // The set describes no archive of the requested format. When it
        // describes no archive at ALL, it is protecting plain media and the
        // repaired files are the deliverable. When it does describe archives,
        // just not these, the request and the recovery set disagree and
        // guessing would be worse than failing.
        Err(response) if !describes_any_archive(set) => {
            let _ = response;
            Ok(Target::PlainFiles)
        }
        Err(response) => Err(response),
    }
}

/// Whether the set protects anything this plugin knows how to extract.
fn describes_any_archive(set: &Par2FileSet) -> bool {
    set.files.values().any(|description| {
        let name = file_name_of(&description.filename).unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        lower.ends_with(".7z")
            || lower.ends_with(".zip")
            || lower.ends_with(".xz")
            || lower.ends_with(".txz")
            || rar_volume_info(&lower).is_some()
    })
}

/// Translate the caller's `archive_path` hint into the canonical name of the
/// same bytes, so an obfuscated request still identifies its own set.
fn canonical_hint(placement: &Placement, hint: &Path) -> PathBuf {
    let hint_name = hint.file_name().and_then(|name| name.to_str());
    for (canonical, actual_path) in &placement.actual_by_canonical {
        let matches = actual_path == hint
            || hint_name.is_some_and(|hint_name| {
                actual_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|actual_name| actual_name.eq_ignore_ascii_case(hint_name))
            });
        if matches {
            return PathBuf::from(canonical);
        }
    }
    hint.to_path_buf()
}

struct RarVolume {
    group: String,
    index: usize,
    canonical_name: String,
}

/// Choose the first volume of the RAR set the request identifies.
///
/// Extraction must start at volume 1 even when the caller named a later volume
/// (a hint of `foo.part3.rar` still opens `foo.part1.rar`), and a set that maps
/// two files onto the same volume index is refused rather than half-extracted.
fn rar_first_volume(
    placement: &Placement,
    hint: &Path,
) -> Result<String, Box<ArchivePluginProcessResponse>> {
    let mut candidates = Vec::new();
    for canonical in placement.actual_by_canonical.keys() {
        let Some(canonical_name) = file_name_of(canonical) else {
            continue;
        };
        let Some((group, index)) = rar_volume_info(&canonical_name.to_ascii_lowercase()) else {
            continue;
        };
        candidates.push(RarVolume {
            group,
            index,
            canonical_name: canonical.clone(),
        });
    }
    if candidates.is_empty() {
        return Err(Box::new(failed_message(
            "par2_no_archive",
            "PAR2 metadata does not describe a RAR archive",
        )));
    }

    let group = hint_group(hint, &candidates)
        .or_else(|| single_group(&candidates))
        .ok_or_else(|| {
            Box::new(failed_message(
                "par2_ambiguous_archive",
                "PAR2 metadata describes multiple RAR archive sets and the requested archive did not identify one",
            ))
        })?;

    let mut selected = candidates
        .into_iter()
        .filter(|candidate| candidate.group == group)
        .collect::<Vec<_>>();
    selected.sort_by(|left, right| {
        left.index
            .cmp(&right.index)
            .then_with(|| left.canonical_name.cmp(&right.canonical_name))
    });

    let first = selected.first().ok_or_else(|| {
        Box::new(failed_message(
            "par2_no_archive",
            "PAR2 metadata did not identify a RAR archive set",
        ))
    })?;
    if first.index != 0 {
        return Err(Box::new(failed_message(
            "par2_no_first_volume",
            "PAR2 metadata did not identify the first RAR volume",
        )));
    }
    let mut previous = None;
    for candidate in &selected {
        if previous == Some(candidate.index) {
            return Err(Box::new(failed_message(
                "par2_duplicate_volume",
                "PAR2 metadata maps multiple files to the same RAR volume index",
            )));
        }
        previous = Some(candidate.index);
    }
    Ok(first.canonical_name.clone())
}

fn hint_group(hint: &Path, candidates: &[RarVolume]) -> Option<String> {
    let hint_name = hint.file_name().and_then(|name| name.to_str())?;
    candidates
        .iter()
        .find(|candidate| {
            file_name_of(&candidate.canonical_name)
                .is_some_and(|name| name.eq_ignore_ascii_case(hint_name))
        })
        .map(|candidate| candidate.group.clone())
}

fn single_group(candidates: &[RarVolume]) -> Option<String> {
    let mut groups = candidates
        .iter()
        .map(|candidate| candidate.group.clone())
        .collect::<Vec<_>>();
    groups.sort();
    groups.dedup();
    match groups.as_slice() {
        [group] => Some(group.clone()),
        _ => None,
    }
}

/// Resolve a single-file format (7z / zip / xz): the hint wins when it names a
/// covered file, otherwise the set must describe exactly one.
fn single_archive(
    placement: &Placement,
    hint: &Path,
    extensions: &[&str],
) -> Result<String, Box<ArchivePluginProcessResponse>> {
    let label = extensions.first().copied().unwrap_or("archive");
    let hint_name = hint
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase());

    let mut candidates = placement
        .actual_by_canonical
        .keys()
        .filter_map(|canonical| {
            let canonical_name = file_name_of(canonical)?;
            let lower = canonical_name.to_ascii_lowercase();
            extensions
                .iter()
                .any(|extension| lower.ends_with(&format!(".{extension}")))
                .then_some((canonical_name, canonical.clone()))
        })
        .collect::<Vec<_>>();

    if let Some(hint_name) = &hint_name
        && let Some((_, canonical)) = candidates
            .iter()
            .find(|(canonical_name, _)| canonical_name.eq_ignore_ascii_case(hint_name))
    {
        return Ok(canonical.clone());
    }

    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    match candidates.as_slice() {
        [(_, canonical)] => Ok(canonical.clone()),
        [] => Err(Box::new(failed_message(
            "par2_no_archive",
            &format!("PAR2 metadata does not describe a {label} archive"),
        ))),
        _ => Err(Box::new(failed_message(
            "par2_ambiguous_archive",
            &format!(
                "PAR2 metadata describes multiple {label} archives and the requested path did not identify one"
            ),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Staging, repair and emission
// ---------------------------------------------------------------------------

/// Make the recovery set's archive readable at its canonical names.
///
/// The fast path is a clean set already sitting at its canonical names: nothing
/// is copied and extraction reads the source directory directly. Anything else
/// — misnamed files, or damage — is materialized into the private scratch dir
/// and re-verified there before extraction is allowed to start.
fn prepare_archive(
    set: &Par2FileSet,
    placement: &Placement,
    canonical_archive: &str,
    source_dir: &Path,
) -> Par2Plan {
    if placement.state == State::Verified && placement.move_count == 0 {
        let relative = match safe_relative_path(canonical_archive) {
            Ok(relative) => relative,
            Err(response) => return Par2Plan::Complete(response),
        };
        return Par2Plan::Prepared(Par2Inputs {
            archive_path: source_dir.join(relative),
            staging_dir: None,
        });
    }

    let staging_dir = match create_scratch_dir("par2") {
        Ok(dir) => dir,
        Err(response) => return Par2Plan::Complete(response),
    };
    let inputs = (|| {
        stage_protected_files(placement, &staging_dir)?;
        if placement.state == State::Repairable {
            repair_in_place(set, &staging_dir)?;
        }
        confirm_verified(set, &staging_dir)?;
        let relative = safe_relative_path(canonical_archive)?;
        Ok(Par2Inputs {
            archive_path: staging_dir.join(relative),
            staging_dir: Some(staging_dir.clone()),
        })
    })();

    match inputs {
        Ok(inputs) => Par2Plan::Prepared(inputs),
        Err(response) => {
            let _ = fs::remove_dir_all(&staging_dir);
            Par2Plan::Complete(response)
        }
    }
}

/// Emit a recovery set that protects plain files.
///
/// The output directory is the writable one AND the deliverable, so the files
/// are copied straight there under their canonical names and repaired in place
/// — no scratch round trip, and no second copy of media-sized payloads.
fn emit_plain_files(set: &Par2FileSet, placement: &Placement, output_dir: &Path) -> Par2Plan {
    let emitted = (|| {
        fs::create_dir_all(output_dir).map_err(|error| {
            Box::new(failed_response(
                "create_output",
                "failed to create archive output directory",
                error,
            ))
        })?;
        let staged = stage_protected_files(placement, output_dir)?;
        if placement.state == State::Repairable {
            repair_in_place(set, output_dir)?;
        }
        confirm_verified(set, output_dir)?;

        let mut files = Vec::new();
        let mut emitted_bytes = 0_u64;
        for canonical in staged {
            let relative = safe_relative_path(&canonical)?;
            let size = fs::metadata(output_dir.join(&relative))
                .map(|meta| meta.len())
                .ok();
            emitted_bytes = emitted_bytes.saturating_add(size.unwrap_or(0));
            files.push(ArchivePluginExtractedFile {
                relative_path: relative.to_string_lossy().replace('\\', "/"),
                size,
                checksum: None,
            });
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

        Ok(ArchivePluginProcessResponse {
            status: ArchivePluginStatus::Ok,
            files,
            copied_bytes: Some(emitted_bytes),
            message: Some(
                "PAR2 set protects plain files; verified copies were written to the output directory"
                    .to_string(),
            ),
            ..empty_response()
        })
    })();

    match emitted {
        Ok(response) => Par2Plan::Complete(Box::new(response)),
        Err(response) => Par2Plan::Complete(response),
    }
}

/// Copy every protected file that exists into `destination` under its CANONICAL
/// name. This is both the placement fix (a misnamed file lands at its correct
/// name) and the writability fix (repair needs to modify its inputs, and the
/// source preopen is read-only).
///
/// Returns the canonical names actually staged. A file that is entirely missing
/// is skipped, not an error: reconstructing it from recovery data is exactly
/// what the repair step is for.
fn stage_protected_files(
    placement: &Placement,
    destination: &Path,
) -> Result<Vec<String>, Box<ArchivePluginProcessResponse>> {
    let mut staged = Vec::new();
    let mut staged_bytes = 0_u64;

    let mut canonicals = placement
        .actual_by_canonical
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    canonicals.sort();

    for canonical in canonicals {
        let relative = safe_relative_path(&canonical)?;
        let Some(actual_path) = placement.actual_by_canonical.get(&canonical) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(actual_path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err(Box::new(failed_message(
                "par2_symlink_input",
                &format!("PAR2 staging refuses the symbolic link '{canonical}'"),
            )));
        }
        if !metadata.is_file() {
            continue;
        }
        staged_bytes = staged_bytes.saturating_add(metadata.len());
        if staged_bytes > MAX_PAR2_STAGED_BYTES {
            return Err(Box::new(failed_message(
                "par2_too_large",
                &format!("PAR2 inputs exceed {MAX_PAR2_STAGED_BYTES} bytes"),
            )));
        }

        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Box::new(failed_response(
                    "par2_stage",
                    "failed to create a PAR2 staging directory",
                    error,
                ))
            })?;
        }
        copy_file(actual_path, &target)?;
        staged.push(canonical);
    }

    Ok(staged)
}

/// Stream one file across, so a media-sized volume never has to fit in the
/// guest's linear memory.
fn copy_file(source: &Path, destination: &Path) -> Result<(), Box<ArchivePluginProcessResponse>> {
    let mut input = fs::File::open(source).map_err(|error| {
        Box::new(failed_response(
            "par2_stage",
            "failed to open a PAR2 input file",
            error,
        ))
    })?;
    let mut output = fs::File::create(destination).map_err(|error| {
        Box::new(failed_response(
            "par2_stage",
            "failed to create a staged PAR2 file",
            error,
        ))
    })?;
    copy_limited(&mut input, &mut output, MAX_PAR2_STAGED_BYTES).map_err(|error| {
        let _ = fs::remove_file(destination);
        Box::new(failed_response(
            "par2_stage",
            "failed to stage a PAR2 input file",
            error,
        ))
    })?;
    Ok(())
}

/// Reconstruct the damaged slices of a staged set, in place.
fn repair_in_place(set: &Par2FileSet, dir: &Path) -> Result<(), Box<ArchivePluginProcessResponse>> {
    let mut access = DiskFileAccess::new(dir.to_path_buf(), set);
    let verification = verify_all(set, &access);
    match &verification.repairable {
        Repairability::NotNeeded => return Ok(()),
        Repairability::Insufficient { .. } => {
            return Err(Box::new(failed_message(
                "par2_insufficient_recovery",
                "PAR2 set does not have enough recovery data to repair these files",
            )));
        }
        Repairability::ResourceLimited { reason } => {
            return Err(Box::new(failed_message(
                "par2_repair_resource_limit",
                &format!("PAR2 repair exceeded its resource limits: {reason}"),
            )));
        }
        Repairability::Repairable { .. } => {}
    }

    let plan = plan_repair(set, &verification).map_err(|error| {
        Box::new(failed_response(
            "par2_repair",
            "failed to plan the PAR2 repair",
            error,
        ))
    })?;
    execute_repair_with_options(&plan, set, &mut access, &RepairOptions::default())
        .map_err(|error| Box::new(failed_response("par2_repair", "PAR2 repair failed", error)))?;
    Ok(())
}

/// Re-verify after staging or repair. A repair that "succeeded" but does not
/// verify clean must never reach the extractor.
fn confirm_verified(
    set: &Par2FileSet,
    dir: &Path,
) -> Result<(), Box<ArchivePluginProcessResponse>> {
    let access = DiskFileAccess::new(dir.to_path_buf(), set);
    let verification = verify_all(set, &access);
    if verification.needs_repair() {
        return Err(Box::new(failed_message(
            "par2_repair_incomplete",
            "PAR2 inputs still require repair after reconstruction",
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Create a private scratch directory under `TMPDIR`.
///
/// The host preopens `TMPDIR` read-write and discards it when the invocation
/// ends, so this is where corrected archive inputs belong: writable, private,
/// and never confused with the output the host imports.
fn create_scratch_dir(purpose: &str) -> Result<PathBuf, Box<ArchivePluginProcessResponse>> {
    let root = PathBuf::from(
        std::env::var("TMPDIR")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| SCRATCH_FALLBACK.to_string()),
    );
    let _ = SCRATCH_ENV;
    for attempt in 0..64_u32 {
        let candidate = root.join(format!(".scryer-{purpose}-{attempt}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Box::new(failed_response(
                    "par2_scratch",
                    "failed to create the PAR2 scratch directory",
                    error,
                )));
            }
        }
    }
    Err(Box::new(failed_message(
        "par2_scratch",
        "failed to create a unique PAR2 scratch directory",
    )))
}

fn file_name_of(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
}

/// A PAR2 filename reduced to a safe relative path, or a rejection.
///
/// Absolute paths and any `..` component are refused outright; `.` components
/// are dropped. Every staged or emitted write goes through this, so a hostile
/// recovery set cannot address anything outside the directory it was handed.
fn safe_relative_path(path: &str) -> Result<PathBuf, Box<ArchivePluginProcessResponse>> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(Box::new(failed_message(
            "par2_unsafe_path",
            &format!("PAR2 filename '{path}' is absolute"),
        )));
    }
    let mut relative = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Box::new(failed_message(
                    "par2_unsafe_path",
                    &format!("PAR2 filename '{path}' is unsafe"),
                )));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(Box::new(failed_message(
            "par2_unsafe_path",
            "PAR2 metadata contains an empty filename",
        )));
    }
    Ok(relative)
}

/// The lossy form used only to build the canonical->actual MAP, which is read
/// for comparisons and then re-validated by [`safe_relative_path`] before any
/// write. Flattening an unsafe name here keeps the map total without ever
/// letting an unsafe name reach the filesystem.
fn safe_relative_path_lossy(path: &str) -> PathBuf {
    match safe_relative_path(path) {
        Ok(relative) => relative,
        Err(_) => PathBuf::from(path.replace(['/', '\\'], "_")),
    }
}

/// Split a RAR volume filename into `(group, zero-based volume index)`.
///
/// Handles both naming schemes: modern `name.partN.rar` and the legacy
/// `name.rar` / `name.r00` / `name.s00` families, whose ordering is `.rar`
/// first and then `r00..r99`, `s00..s99`, and so on.
fn rar_volume_info(file_name: &str) -> Option<(String, usize)> {
    if let Some(stem) = file_name.strip_suffix(".rar") {
        if let Some((group, part)) = stem.rsplit_once(".part")
            && let Ok(part_index) = part.parse::<usize>()
            && part_index > 0
        {
            return Some((group.to_string(), part_index - 1));
        }
        return Some((stem.to_string(), 0));
    }

    let (group, extension) = file_name.rsplit_once('.')?;
    if !is_legacy_volume_extension(extension) {
        return None;
    }
    let mut characters = extension.chars();
    let family = characters.next()?;
    let number = characters.as_str().parse::<usize>().ok()?;
    let family_offset = (family as u8).checked_sub(b'r')? as usize;
    Some((group.to_string(), family_offset * 100 + number + 1))
}

fn is_legacy_volume_extension(extension: &str) -> bool {
    let mut characters = extension.chars();
    matches!(characters.next(), Some('r'..='z'))
        && extension.len() >= 3
        && characters.all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rar_volume_info_orders_modern_and_legacy_schemes() {
        assert_eq!(
            rar_volume_info("show.part1.rar"),
            Some(("show".to_string(), 0))
        );
        assert_eq!(
            rar_volume_info("show.part12.rar"),
            Some(("show".to_string(), 11))
        );
        assert_eq!(rar_volume_info("show.rar"), Some(("show".to_string(), 0)));
        assert_eq!(rar_volume_info("show.r00"), Some(("show".to_string(), 1)));
        assert_eq!(rar_volume_info("show.s00"), Some(("show".to_string(), 101)));
        assert_eq!(rar_volume_info("show.mkv"), None);
    }

    #[test]
    fn safe_relative_path_rejects_escapes_before_any_filesystem_access() {
        assert!(safe_relative_path("../escape.rar").is_err());
        assert!(safe_relative_path("/etc/passwd").is_err());
        assert!(safe_relative_path("").is_err());
        assert!(safe_relative_path("./nested/./file.rar").is_ok());
        assert_eq!(
            safe_relative_path("nested/file.rar").unwrap(),
            PathBuf::from("nested/file.rar")
        );
    }
}
