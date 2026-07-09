mod par2_host_solver;

use par2_host_solver::HostDispatchSolver;
use scryer_plugin_pdk::run_archive_plugin;
use scryer_plugin_pdk::{
    ArchivePluginExtractedFile, ArchivePluginFormat, ArchivePluginOperation,
    ArchivePluginProcessRequest, ArchivePluginProcessResponse, ArchivePluginRepairFormat,
    ArchivePluginRepairState, ArchivePluginRepairStatus, ArchivePluginStatus,
};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ArchiveExtractorCapabilities, ArchiveExtractorDescriptor, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION,
};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use weaver_par2::{
    DiskFileAccess, Par2FileSet, RepairOptions, Repairability, disk::PlacementFileAccess,
    execute_repair_with_solver, placement::PlacementPlan, placement::scan_placement,
    plan_repair, verify::VerificationResult, verify_all,
};
use weaver_unrar::{ExtractOptions, RarArchive, RarError};

const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;
const REPAIR_WRITE_PROBE_PREFIX: &str = ".scryer-archive-plugin-write-probe-";

/// Command entry.
///
/// With no arguments — the shipped invocation path (RFC 123 §7.2.5) — this runs
/// the archive command protocol via the PDK: one `ArchivePluginProcessRequest`
/// JSON document on stdin, exactly one `ArchivePluginProcessResponse` JSON
/// document on stdout.
///
/// With a single `describe` argument it writes this plugin's `PluginDescriptor`
/// as JSON to stdout and exits. This is the catalog/packaging descriptor path
/// for a command binary: the Extism `scryer_describe` export no longer exists,
/// so a host runs the wasm as `<plugin> describe` and captures stdout.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("describe") {
        let json = serde_json::to_string(&build_descriptor())
            .expect("descriptor serialization must not fail");
        let mut stdout = io::stdout();
        stdout
            .write_all(json.as_bytes())
            .expect("failed to write descriptor to stdout");
        stdout.flush().expect("failed to flush descriptor");
        return;
    }

    run_archive_plugin(handle_request);
}

fn build_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: "archive-extraction".to_string(),
        name: "archive-extraction".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::ArchiveExtractor(ArchiveExtractorDescriptor {
            provider_type: "archive-extraction".to_string(),
            provider_aliases: vec![],
            config_fields: vec![],
            default_base_url: None,
            allowed_hosts: vec![],
            capabilities: ArchiveExtractorCapabilities {
                formats: vec![ArchivePluginFormat::Rar, ArchivePluginFormat::Zip],
                repair_formats: vec![ArchivePluginRepairFormat::Par2],
            },
        }),
    }
}

/// Archive command-protocol handler.
///
/// Maps the single request into the per-operation logic and returns exactly one
/// response. Operational outcomes are reported in-band via
/// [`ArchivePluginStatus`]; the PDK owns request parsing and response framing.
fn handle_request(request: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse {
    match request.operation {
        ArchivePluginOperation::Inspect { .. } => {
            unsupported_response("archive inspection is not implemented yet")
        }
        ArchivePluginOperation::ExtractArchive {
            archive_path,
            output_dir,
            format,
            password,
        } => extract_archive(&archive_path, &output_dir, format, password.as_deref()),
        ArchivePluginOperation::VerifyRepairSet {
            source_dir,
            par2_path,
        } => verify_par2_set(&source_dir, par2_path.as_deref()),
        ArchivePluginOperation::RepairThenExtract {
            source_dir,
            output_dir,
            format,
            par2_path,
            archive_path,
            password,
        } => repair_then_extract(
            &source_dir,
            &output_dir,
            format,
            par2_path.as_deref(),
            archive_path.as_deref(),
            password.as_deref(),
        ),
    }
}

fn extract_archive(
    archive_path: &str,
    output_dir: &str,
    format: ArchivePluginFormat,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    match format {
        ArchivePluginFormat::Rar => extract_rar(archive_path, output_dir, password),
        ArchivePluginFormat::Zip => extract_zip(archive_path, output_dir, password),
    }
}

fn repair_then_extract(
    source_dir: &str,
    output_dir: &str,
    format: ArchivePluginFormat,
    par2_path: Option<&str>,
    archive_path: Option<&str>,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let Some(archive_path) = archive_path else {
        return failed_message(
            "missing_archive",
            "repair_then_extract requires an archive path after PAR2 verification",
        );
    };

    let source_dir_path = PathBuf::from(source_dir);
    let par2_paths = match par2_paths_for_request(&source_dir_path, par2_path) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };

    match verify_par2_with_placement(source_dir_path.clone(), par2_paths.clone()) {
        Ok(verification) => {
            let mut response = extract_archive_with_par2_placement(
                &verification,
                output_dir,
                format,
                archive_path,
                password,
            );
            response.repair = verification.response.repair;
            response
        }
        Err(response) if response.error_code.as_deref() == Some("par2_repair_required") => {
            if !source_dir_is_writable(&source_dir_path) {
                return response;
            }

            let repair_response = repair_par2_via_host(source_dir_path, par2_paths);
            if !matches!(repair_response.status, ArchivePluginStatus::Ok) {
                return repair_response;
            }

            let mut response = extract_archive(archive_path, output_dir, format, password);
            response.repair = repair_response.repair;
            response
        }
        Err(response) => response,
    }
}

fn source_dir_is_writable(source_dir: &Path) -> bool {
    for attempt in 0..16 {
        let probe_path = source_dir.join(format!("{REPAIR_WRITE_PROBE_PREFIX}{attempt}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)
        {
            Ok(_) => {
                let _ = fs::remove_file(&probe_path);
                return true;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }
    false
}

fn extract_archive_with_par2_placement(
    verification: &Par2PlacementVerification,
    output_dir: &str,
    format: ArchivePluginFormat,
    archive_path: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    match format {
        ArchivePluginFormat::Rar => {
            let volume_paths = match verification.rar_volume_paths(archive_path) {
                Ok(paths) => paths,
                Err(response) => return *response,
            };
            extract_rar_from_ordered_paths(&volume_paths, output_dir, password)
        }
        ArchivePluginFormat::Zip => {
            let archive_path = match verification.zip_archive_path(archive_path) {
                Ok(path) => path,
                Err(response) => return *response,
            };
            extract_zip(&archive_path.to_string_lossy(), output_dir, password)
        }
    }
}

fn open_rar_archive(
    archive_path: &Path,
    password: Option<&str>,
) -> Result<RarArchive, Box<ArchivePluginProcessResponse>> {
    let archive_file = fs::File::open(archive_path).map_err(|error| {
        Box::new(failed_response(
            "open_rar",
            "failed to open RAR archive",
            error,
        ))
    })?;

    match password.filter(|password| !password.is_empty()) {
        Some(password) => RarArchive::open_with_password(archive_file, password).map_err(|error| {
            Box::new(rar_error_response(
                "open_rar",
                "failed to read RAR archive",
                error,
            ))
        }),
        None => RarArchive::open(archive_file).map_err(|error| {
            Box::new(rar_error_response(
                "open_rar",
                "failed to read RAR archive",
                error,
            ))
        }),
    }
}

fn extract_rar_from_ordered_paths(
    volume_paths: &[PathBuf],
    output_dir: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let Some(first_volume) = volume_paths.first() else {
        return failed_message("missing_first_volume", "RAR extraction requires a first volume");
    };

    let mut archive = match open_rar_archive(first_volume, password) {
        Ok(archive) => archive,
        Err(response) => return *response,
    };

    if let Some(password) = password.filter(|password| !password.is_empty()) {
        archive.set_password(password.to_string());
    }

    for (offset, volume_path) in volume_paths.iter().skip(1).enumerate() {
        let volume_file = match fs::File::open(volume_path) {
            Ok(file) => file,
            Err(error) => {
                return failed_response("read_rar_volume", "failed to open RAR volume", error);
            }
        };
        if let Err(error) = archive.add_volume(offset + 1, Box::new(volume_file)) {
            return rar_error_response("read_rar_volume", "failed to read RAR volume", error);
        }
    }

    extract_open_rar_archive(archive, output_dir, password)
}

fn extract_rar(
    archive_path: &str,
    output_dir: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let archive_path = Path::new(archive_path);
    let mut archive = match open_rar_archive(archive_path, password) {
        Ok(archive) => archive,
        Err(response) => return *response,
    };

    if let Some(password) = password.filter(|password| !password.is_empty()) {
        archive.set_password(password.to_string());
    }

    let source_dir = archive_path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = attach_rar_volumes(&mut archive, source_dir, archive_path) {
        return rar_error_response("read_rar_volume", "failed to read RAR volume", error);
    }

    extract_open_rar_archive(archive, output_dir, password)
}

fn extract_open_rar_archive(
    mut archive: RarArchive,
    output_dir: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let output_root = Path::new(output_dir);
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }

    let mut files = Vec::new();
    let mut expanded_bytes = 0_u64;
    let options = ExtractOptions {
        password: password
            .filter(|password| !password.is_empty())
            .map(str::to_string),
        ..ExtractOptions::default()
    };

    let members = archive.indexed_member_infos();
    if members.len() > MAX_ARCHIVE_ENTRIES {
        return failed_message("too_many_entries", "RAR archive contains too many entries");
    }

    for member in members {
        let info = member.info;
        if info.is_symlink || info.is_hardlink || info.is_file_copy {
            return failed_message(
                "link_entry",
                "RAR archive contains a link or file-copy entry",
            );
        }

        let relative_path = match safe_archive_relative_path(&info.name) {
            Ok(path) => path,
            Err(response) => return *response,
        };

        let destination = output_root.join(&relative_path);
        if info.is_directory {
            if let Err(error) = fs::create_dir_all(&destination) {
                return failed_response(
                    "create_directory",
                    "failed to create RAR directory",
                    error,
                );
            }
            continue;
        }

        if !member.extractable {
            return ArchivePluginProcessResponse {
                status: ArchivePluginStatus::RepairRequired,
                repair: Some(ArchivePluginRepairStatus {
                    status: ArchivePluginRepairState::InsufficientRecoveryData,
                    read_bytes: None,
                    written_bytes: None,
                    message: Some(format!(
                        "RAR member '{}' is missing volume(s): {:?}",
                        info.name, member.missing_volumes
                    )),
                }),
                error_code: Some("missing_volume".to_string()),
                message: Some("RAR archive is missing one or more required volumes".to_string()),
                ..empty_response()
            };
        }

        let declared_size = info.unpacked_size.unwrap_or(0);
        expanded_bytes = match expanded_bytes.checked_add(declared_size) {
            Some(total) if total <= MAX_ARCHIVE_EXPANDED_BYTES => total,
            _ => {
                return failed_message(
                    "expanded_too_large",
                    &format!("RAR archive expands beyond {MAX_ARCHIVE_EXPANDED_BYTES} bytes"),
                );
            }
        };

        if let Some(parent) = destination.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return failed_response(
                "create_parent",
                "failed to create RAR parent directory",
                error,
            );
        }

        let written =
            match archive.extract_member_to_file(member.index, &options, None, &destination) {
                Ok(written) => written,
                Err(error) => {
                    let _ = fs::remove_file(&destination);
                    return rar_error_response(
                        "extract_rar",
                        "failed to extract RAR member",
                        error,
                    );
                }
            };

        if written > declared_size {
            expanded_bytes = expanded_bytes
                .saturating_sub(declared_size)
                .saturating_add(written);
            if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
                let _ = fs::remove_file(&destination);
                return failed_message(
                    "expanded_too_large",
                    &format!("RAR archive expands beyond {MAX_ARCHIVE_EXPANDED_BYTES} bytes"),
                );
            }
        }

        files.push(ArchivePluginExtractedFile {
            relative_path: relative_path.to_string_lossy().replace('\\', "/"),
            size: Some(written),
            checksum: info.crc32.map(|crc| format!("{crc:08x}")),
        });
    }

    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Ok,
        files,
        expanded_bytes: Some(expanded_bytes),
        ..empty_response()
    }
}

fn extract_zip(
    archive_path: &str,
    output_dir: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    if password.is_some_and(|password| !password.is_empty()) {
        return ArchivePluginProcessResponse {
            status: ArchivePluginStatus::PasswordRequired,
            message: Some("encrypted ZIP archives are not implemented yet".to_string()),
            ..empty_response()
        };
    }

    let archive_file = match fs::File::open(archive_path) {
        Ok(file) => file,
        Err(error) => return failed_response("open_zip", "failed to open ZIP archive", error),
    };
    let mut archive = match zip::ZipArchive::new(archive_file) {
        Ok(archive) => archive,
        Err(error) => return failed_response("read_zip", "failed to read ZIP archive", error),
    };

    let output_root = Path::new(output_dir);
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }

    let mut files = Vec::new();
    let mut expanded_bytes = 0_u64;

    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return failed_message("too_many_entries", "ZIP archive contains too many entries");
    }

    for index in 0..archive.len() {
        let mut entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(error) => return failed_response("read_entry", "failed to read ZIP entry", error),
        };

        let Some(relative_path) = entry.enclosed_name() else {
            return failed_message("unsafe_path", "ZIP archive contains an unsafe path");
        };
        let relative_path = normalize_relative_path(&relative_path);
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        if !entry.is_dir() {
            expanded_bytes = match expanded_bytes.checked_add(entry.size()) {
                Some(total) if total <= MAX_ARCHIVE_EXPANDED_BYTES => total,
                _ => {
                    return failed_message(
                        "expanded_too_large",
                        &format!("ZIP archive expands beyond {MAX_ARCHIVE_EXPANDED_BYTES} bytes"),
                    );
                }
            };
        }

        let entry_mode = entry.unix_mode().unwrap_or_default();
        if entry_mode & 0o170000 == 0o120000 {
            return failed_message("symlink_entry", "ZIP archive contains a symlink entry");
        }

        let destination = output_root.join(&relative_path);
        if entry.is_dir() {
            if let Err(error) = fs::create_dir_all(&destination) {
                return failed_response(
                    "create_directory",
                    "failed to create ZIP directory",
                    error,
                );
            }
            continue;
        }

        if let Some(parent) = destination.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return failed_response(
                "create_parent",
                "failed to create ZIP parent directory",
                error,
            );
        }

        let mut output = match fs::File::create(&destination) {
            Ok(file) => file,
            Err(error) => {
                return failed_response("create_file", "failed to create ZIP output file", error);
            }
        };
        let copy_limit = MAX_ARCHIVE_EXPANDED_BYTES.saturating_sub(
            expanded_bytes
                .saturating_sub(entry.size())
                .min(MAX_ARCHIVE_EXPANDED_BYTES),
        );
        let written = match copy_limited(&mut entry, &mut output, copy_limit) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                return failed_response("extract_file", "failed to extract ZIP entry", error);
            }
        };
        if written > entry.size() {
            expanded_bytes = expanded_bytes
                .saturating_sub(entry.size())
                .saturating_add(written);
            if expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
                let _ = fs::remove_file(&destination);
                return failed_message(
                    "expanded_too_large",
                    &format!("ZIP archive expands beyond {MAX_ARCHIVE_EXPANDED_BYTES} bytes"),
                );
            }
        }
        files.push(ArchivePluginExtractedFile {
            relative_path: relative_path.to_string_lossy().replace('\\', "/"),
            size: Some(written),
            checksum: None,
        });
    }

    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Ok,
        files,
        expanded_bytes: Some(expanded_bytes),
        ..empty_response()
    }
}

fn verify_par2_set(source_dir: &str, par2_path: Option<&str>) -> ArchivePluginProcessResponse {
    let source_dir = PathBuf::from(source_dir);
    let par2_paths = match par2_paths_for_request(&source_dir, par2_path) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };
    match verify_par2_with_placement(source_dir, par2_paths) {
        Ok(verification) => verification.response,
        Err(response) => response,
    }
}

fn repair_par2_set(source_dir: &str, par2_path: Option<&str>) -> ArchivePluginProcessResponse {
    let source_dir = PathBuf::from(source_dir);
    let par2_paths = match par2_paths_for_request(&source_dir, par2_path) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };
    repair_par2_via_host(source_dir, par2_paths)
}

struct Par2PlacementVerification {
    actual_by_canonical: HashMap<String, PathBuf>,
    response: ArchivePluginProcessResponse,
}

struct RarVolumeCandidate {
    group: String,
    index: usize,
    canonical_name: String,
    actual_path: PathBuf,
}

fn verify_par2_with_placement(
    source_dir: PathBuf,
    par2_paths: Vec<PathBuf>,
) -> Result<Par2PlacementVerification, ArchivePluginProcessResponse> {
    let set = Par2FileSet::from_paths(&par2_paths)
        .map_err(|error| failed_response("par2_load", "failed to load PAR2 set", error))?;
    let plan = scan_placement(&source_dir, &set).map_err(|error| {
        failed_response(
            "par2_placement_scan",
            "failed to scan PAR2 file placement",
            error,
        )
    })?;

    if !plan.conflicts.is_empty() {
        return Err(par2_repair_failed_response(
            "par2_placement_conflict",
            format!(
                "PAR2 placement is ambiguous for {} file(s); refusing to guess archive order",
                plan.conflicts.len()
            ),
        ));
    }

    let access = PlacementFileAccess::from_plan(source_dir.clone(), &set, &plan);
    let verification = verify_all(&set, &access);
    let response = par2_placement_response(&verification, placement_move_count(&plan));
    if !matches!(response.status, ArchivePluginStatus::Ok) {
        return Err(response);
    }

    Ok(Par2PlacementVerification {
        actual_by_canonical: par2_actual_paths_by_canonical(&source_dir, &set, &plan),
        response,
    })
}

fn placement_move_count(plan: &PlacementPlan) -> usize {
    plan.renames.len() + plan.swaps.len().saturating_mul(2)
}

fn par2_placement_response(
    verification: &VerificationResult,
    placement_move_count: usize,
) -> ArchivePluginProcessResponse {
    match &verification.repairable {
        Repairability::NotNeeded => {
            let message = if placement_move_count == 0 {
                "PAR2 set verified cleanly".to_string()
            } else {
                format!(
                    "PAR2 set verified cleanly after placement normalization of {placement_move_count} file(s)"
                )
            };
            par2_ok_response(ArchivePluginRepairState::Verified, 0, &message)
        }
        Repairability::Repairable { .. } => par2_repair_status_response(
            ArchivePluginStatus::RepairRequired,
            ArchivePluginRepairState::Failed,
            "par2_repair_required",
            "PAR2 verification found repairable damage; writable repair staging is required",
        ),
        Repairability::Insufficient { .. } => par2_repair_status_response(
            ArchivePluginStatus::RepairRequired,
            ArchivePluginRepairState::InsufficientRecoveryData,
            "par2_insufficient_recovery_data",
            "PAR2 set does not have enough recovery data",
        ),
        Repairability::ResourceLimited { .. } => par2_repair_status_response(
            ArchivePluginStatus::RepairFailed,
            ArchivePluginRepairState::Failed,
            "par2_resource_limited",
            "PAR2 verification exceeded resource limits",
        ),
    }
}

fn par2_actual_paths_by_canonical(
    source_dir: &Path,
    set: &Par2FileSet,
    plan: &PlacementPlan,
) -> HashMap<String, PathBuf> {
    let mut actual = HashMap::new();
    for description in set.files.values() {
        actual.insert(
            description.filename.clone(),
            source_child_path(source_dir, &description.filename),
        );
    }
    for (left, right) in &plan.swaps {
        actual.insert(
            left.correct_name.clone(),
            source_child_path(source_dir, &left.current_name),
        );
        actual.insert(
            right.correct_name.clone(),
            source_child_path(source_dir, &right.current_name),
        );
    }
    for entry in &plan.renames {
        actual.insert(
            entry.correct_name.clone(),
            source_child_path(source_dir, &entry.current_name),
        );
    }
    actual
}

fn source_child_path(source_dir: &Path, name: &str) -> PathBuf {
    source_dir.join(normalize_relative_path(Path::new(name)))
}

impl Par2PlacementVerification {
    fn rar_volume_paths(
        &self,
        archive_path: &str,
    ) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
        let mut candidates = Vec::new();
        for (canonical, actual_path) in &self.actual_by_canonical {
            let Some(canonical_name) = file_name_string(canonical) else {
                continue;
            };
            let Some((group, index)) = rar_volume_info(&canonical_name) else {
                continue;
            };
            candidates.push(RarVolumeCandidate {
                group,
                index,
                canonical_name,
                actual_path: actual_path.clone(),
            });
        }

        if candidates.is_empty() {
            return Err(Box::new(failed_message(
                "missing_archive",
                "PAR2 metadata does not describe any RAR archive volumes",
            )));
        }

        let group = match archive_hint_group(archive_path, &candidates) {
            Some(group) => group,
            None => single_rar_group(&candidates)?,
        };
        let mut selected = candidates
            .into_iter()
            .filter(|candidate| candidate.group == group)
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| {
            left.index
                .cmp(&right.index)
                .then_with(|| left.canonical_name.cmp(&right.canonical_name))
        });

        let Some(first) = selected.first() else {
            return Err(Box::new(failed_message(
                "missing_archive",
                "PAR2 metadata did not identify a RAR archive set",
            )));
        };
        if first.index != 0 {
            return Err(Box::new(failed_message(
                "missing_first_volume",
                "PAR2 metadata did not identify the first RAR volume",
            )));
        }

        let mut previous_index = None;
        for candidate in &selected {
            if previous_index == Some(candidate.index) {
                return Err(Box::new(failed_message(
                    "duplicate_rar_volume",
                    "PAR2 metadata maps multiple files to the same RAR volume index",
                )));
            }
            previous_index = Some(candidate.index);
        }

        Ok(selected
            .into_iter()
            .map(|candidate| candidate.actual_path)
            .collect())
    }

    fn zip_archive_path(&self, archive_path: &str) -> Result<PathBuf, Box<ArchivePluginProcessResponse>> {
        let hint_name = Path::new(archive_path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_ascii_lowercase());
        let mut candidates = self
            .actual_by_canonical
            .iter()
            .filter_map(|(canonical, actual_path)| {
                let canonical_name = file_name_string(canonical)?;
                canonical_name
                    .to_ascii_lowercase()
                    .ends_with(".zip")
                    .then_some((canonical_name, actual_path.clone()))
            })
            .collect::<Vec<_>>();

        if let Some(hint_name) = hint_name
            && let Some((_, actual_path)) = candidates.iter().find(|(canonical_name, actual_path)| {
                canonical_name.eq_ignore_ascii_case(&hint_name)
                    || actual_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case(&hint_name))
            })
        {
            return Ok(actual_path.clone());
        }

        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        match candidates.as_slice() {
            [(_, path)] => Ok(path.clone()),
            [] => Err(Box::new(failed_message(
                "missing_archive",
                "PAR2 metadata does not describe a ZIP archive",
            ))),
            _ => Err(Box::new(failed_message(
                "ambiguous_archive",
                "PAR2 metadata describes multiple ZIP archives and the requested path did not identify one",
            ))),
        }
    }
}

fn archive_hint_group(archive_path: &str, candidates: &[RarVolumeCandidate]) -> Option<String> {
    let hint_name = Path::new(archive_path)
        .file_name()
        .and_then(|name| name.to_str())?;
    candidates
        .iter()
        .find(|candidate| {
            candidate.canonical_name.eq_ignore_ascii_case(hint_name)
                || candidate
                    .actual_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(hint_name))
        })
        .map(|candidate| candidate.group.clone())
}

fn single_rar_group(
    candidates: &[RarVolumeCandidate],
) -> Result<String, Box<ArchivePluginProcessResponse>> {
    let mut groups = candidates
        .iter()
        .map(|candidate| candidate.group.clone())
        .collect::<Vec<_>>();
    groups.sort();
    groups.dedup();
    match groups.as_slice() {
        [group] => Ok(group.clone()),
        _ => Err(Box::new(failed_message(
            "ambiguous_archive",
            "PAR2 metadata describes multiple RAR archive sets and the requested path did not identify one",
        ))),
    }
}

fn file_name_string(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
}

fn rar_volume_info(file_name: &str) -> Option<(String, usize)> {
    let lower = file_name.to_ascii_lowercase();
    if let Some(stem) = lower.strip_suffix(".rar") {
        if let Some((group, part)) = stem.rsplit_once(".part")
            && let Ok(part_index) = part.parse::<usize>()
            && part_index > 0
        {
            return Some((group.to_string(), part_index - 1));
        }
        return Some((stem.to_string(), 0));
    }

    let (group, extension) = lower.rsplit_once('.')?;
    if !is_old_rar_volume_extension(extension) {
        return None;
    }
    let mut chars = extension.chars();
    let family = chars.next()?;
    let digits = chars.as_str();
    let number = digits.parse::<usize>().ok()?;
    let family_offset = (family as u8).checked_sub(b'r')? as usize;
    Some((group.to_string(), family_offset * 100 + number + 1))
}

/// Repair a damaged PAR2 set by dispatching the Reed-Solomon reconstruct to the
/// native host (RFC 123 WP2.5).
///
/// The reconstruct seam ([`execute_repair_with_solver`]) sits below
/// [`Par2Repairer`], whose native rayon reconstruct cannot run on
/// `wasm32-wasip1`, so this drives the repair from weaver-par2's public
/// primitives instead: load the set, verify it against the source directory,
/// plan the repair, then reconstruct through [`HostDispatchSolver`], which
/// marshals the problem to the host function. Reconstructed slices are written
/// in place into the source files by [`DiskFileAccess`].
fn repair_par2_via_host(
    source_dir: PathBuf,
    par2_paths: Vec<PathBuf>,
) -> ArchivePluginProcessResponse {
    // Parse packets from the .par2 files (recovery-slice payloads stay
    // file-backed and lazily read; no mmap, so this is wasip1-safe).
    let set = match Par2FileSet::from_paths(&par2_paths) {
        Ok(set) => set,
        Err(error) => return failed_response("par2_load", "failed to load PAR2 set", error),
    };

    let mut access = DiskFileAccess::new(source_dir, &set);
    let verification = verify_all(&set, &access);

    match &verification.repairable {
        Repairability::NotNeeded => par2_ok_response(
            ArchivePluginRepairState::Verified,
            0,
            "PAR2 set verified cleanly",
        ),
        Repairability::Insufficient { .. } => par2_repair_status_response(
            ArchivePluginStatus::RepairRequired,
            ArchivePluginRepairState::InsufficientRecoveryData,
            "par2_insufficient_recovery_data",
            "PAR2 set does not have enough recovery data",
        ),
        Repairability::ResourceLimited { .. } => par2_repair_status_response(
            ArchivePluginStatus::RepairFailed,
            ArchivePluginRepairState::Failed,
            "par2_resource_limited",
            "PAR2 verification exceeded resource limits",
        ),
        Repairability::Repairable { .. } => {
            let plan = match plan_repair(&set, &verification) {
                Ok(plan) => plan,
                Err(error) => {
                    return par2_repair_failed_response(
                        "par2_repair_plan",
                        format!("failed to plan PAR2 repair: {error}"),
                    );
                }
            };
            let reconstructed_bytes =
                (verification.total_missing_blocks as u64).saturating_mul(plan.slice_size);

            match execute_repair_with_solver(
                &plan,
                &set,
                &mut access,
                &RepairOptions::default(),
                &HostDispatchSolver,
            ) {
                Ok(()) => {
                    // Fail closed: re-verify the reconstructed on-disk set before
                    // reporting success, mirroring native Par2Repairer's post-repair
                    // verification. A reconstruct that silently produced wrong bytes
                    // must never gate extraction, so extraction only proceeds when
                    // the whole set now reads back clean (no missing blocks, every
                    // file Complete).
                    let post = verify_all(&set, &access);
                    if post.needs_repair() {
                        par2_repair_failed_response(
                            "par2_repair_postverify",
                            format!(
                                "PAR2 reconstruction did not verify clean: {} block(s) still damaged or missing",
                                post.total_missing_blocks
                            ),
                        )
                    } else {
                        par2_ok_response(
                            ArchivePluginRepairState::Repaired,
                            reconstructed_bytes,
                            "PAR2 set was repaired via host-thread reconstruction and re-verified",
                        )
                    }
                }
                Err(error) => par2_repair_failed_response(
                    "par2_repair_reconstruct",
                    format!("PAR2 reconstruction failed: {error}"),
                ),
            }
        }
    }
}

/// Successful verify/repair: overall `Ok` with a populated repair status so the
/// gating caller (`repair_then_extract`) proceeds to extraction.
fn par2_ok_response(
    state: ArchivePluginRepairState,
    written_bytes: u64,
    message: &str,
) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Ok,
        repair: Some(ArchivePluginRepairStatus {
            status: state,
            read_bytes: None,
            written_bytes: Some(written_bytes),
            message: Some(message.to_string()),
        }),
        ..empty_response()
    }
}

/// A non-`Ok` repair outcome (insufficient data, resource-limited) that carries
/// a repair status and an error code but is not a hard host failure.
fn par2_repair_status_response(
    status: ArchivePluginStatus,
    repair_state: ArchivePluginRepairState,
    error_code: &str,
    message: &str,
) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status,
        repair: Some(ArchivePluginRepairStatus {
            status: repair_state,
            read_bytes: None,
            written_bytes: None,
            message: Some(message.to_string()),
        }),
        error_code: Some(error_code.to_string()),
        message: Some(message.to_string()),
        ..empty_response()
    }
}

/// A hard reconstruct/plan failure. Every negative host return code lands here
/// (including `-7` deadline, which the host also surfaces as a timeout), mapped
/// to the in-band `RepairFailed` status.
fn par2_repair_failed_response(error_code: &str, message: String) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::RepairFailed,
        repair: Some(ArchivePluginRepairStatus {
            status: ArchivePluginRepairState::Failed,
            read_bytes: None,
            written_bytes: None,
            message: Some(message.clone()),
        }),
        error_code: Some(error_code.to_string()),
        message: Some(message),
        ..empty_response()
    }
}

fn par2_paths_for_request(
    source_dir: &Path,
    par2_path: Option<&str>,
) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    if let Some(par2_path) = par2_path {
        return par2_paths_for_primary_path(source_dir, &PathBuf::from(par2_path));
    }

    par2_paths_from_dir(source_dir)
}

fn par2_paths_for_primary_path(
    source_dir: &Path,
    primary_path: &Path,
) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    let parent = primary_path.parent().unwrap_or(source_dir);
    let Some(set_prefix) = par2_set_prefix(primary_path) else {
        return Ok(vec![primary_path.to_path_buf()]);
    };
    let mut par2_paths = par2_paths_from_dir(parent)?;
    par2_paths.retain(|path| par2_path_matches_set_prefix(path, &set_prefix));
    if par2_paths.iter().all(|path| path != primary_path) {
        par2_paths.push(primary_path.to_path_buf());
        par2_paths.sort();
    }
    Ok(par2_paths)
}

fn par2_paths_from_dir(
    source_dir: &Path,
) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    let entries = fs::read_dir(source_dir).map_err(|error| {
        Box::new(failed_response(
            "read_source_dir",
            "failed to read source dir",
            error,
        ))
    })?;
    let mut par2_paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            Box::new(failed_response(
                "read_source_dir",
                "failed to read source dir",
                error,
            ))
        })?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("par2"))
        {
            par2_paths.push(path);
        }
    }
    par2_paths.sort();

    if par2_paths.is_empty() {
        return Err(Box::new(failed_message(
            "missing_par2",
            "PAR2 verification requires at least one .par2 file",
        )));
    }

    Ok(par2_paths)
}

fn par2_set_prefix(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.split_once(".vol")
        .map(|(prefix, _)| prefix.to_string())
        .or_else(|| Some(stem.to_string()))
}

fn par2_path_matches_set_prefix(path: &Path, set_prefix: &str) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    stem == set_prefix || stem.starts_with(&format!("{set_prefix}.vol"))
}

fn attach_rar_volumes(
    archive: &mut RarArchive,
    source_dir: &Path,
    archive_path: &Path,
) -> Result<(), RarError> {
    let mut volume_paths = collect_rar_volume_paths(source_dir, archive_path)?;
    volume_paths.sort();

    for (offset, volume_path) in volume_paths.into_iter().enumerate() {
        let volume_file = fs::File::open(&volume_path)?;
        archive.add_volume(offset + 1, Box::new(volume_file))?;
    }

    Ok(())
}

fn collect_rar_volume_paths(
    source_dir: &Path,
    archive_path: &Path,
) -> Result<Vec<PathBuf>, RarError> {
    let archive_file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let entries = fs::read_dir(source_dir)?;
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path == archive_path || !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_likely_rar_volume(&file_name.to_ascii_lowercase(), &archive_file_name) {
            paths.push(path);
        }
    }

    Ok(paths)
}

fn is_likely_rar_volume(file_name: &str, first_archive_file_name: &str) -> bool {
    if file_name == first_archive_file_name {
        return false;
    }
    if file_name.ends_with(".rar") && file_name.contains(".part") {
        return true;
    }
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    extension.len() == 3
        && extension.starts_with('r')
        && extension[1..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            normalized.push(part);
        }
    }
    normalized
}

fn safe_archive_relative_path(path: &str) -> Result<PathBuf, Box<ArchivePluginProcessResponse>> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains an absolute path",
        )));
    }
    let relative = normalize_relative_path(path);
    if relative.as_os_str().is_empty() {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains an empty path",
        )));
    }
    Ok(relative)
}

fn copy_limited<R: Read, W: Write>(reader: &mut R, writer: &mut W, limit: u64) -> io::Result<u64> {
    let mut written = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read_len = reader.read(&mut buffer)?;
        if read_len == 0 {
            return Ok(written);
        }
        let read = read_len as u64;
        written = written.checked_add(read).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "archive entry is too large")
        })?;
        if written > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive expands beyond the configured limit",
            ));
        }
        writer.write_all(&buffer[..read_len])?;
    }
}

fn unsupported_response(message: &str) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::UnsupportedFormat,
        message: Some(message.to_string()),
        ..empty_response()
    }
}

fn failed_message(error_code: &str, message: &str) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Failed,
        error_code: Some(error_code.to_string()),
        message: Some(message.to_string()),
        ..empty_response()
    }
}

fn failed_response(
    error_code: &str,
    message: &str,
    error: impl std::fmt::Display,
) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Failed,
        error_code: Some(error_code.to_string()),
        message: Some(format!("{message}: {error}")),
        ..empty_response()
    }
}

fn rar_error_response(
    error_code: &str,
    message: &str,
    error: RarError,
) -> ArchivePluginProcessResponse {
    let status = match error {
        RarError::EncryptedArchive | RarError::EncryptedMember { .. } => {
            ArchivePluginStatus::PasswordRequired
        }
        RarError::InvalidPassword | RarError::WrongPassword { .. } => {
            ArchivePluginStatus::PasswordInvalid
        }
        RarError::UnsupportedFormat { .. } => ArchivePluginStatus::UnsupportedFormat,
        _ => ArchivePluginStatus::Failed,
    };

    ArchivePluginProcessResponse {
        status,
        error_code: Some(error_code.to_string()),
        message: Some(format!("{message}: {error}")),
        ..empty_response()
    }
}

fn empty_response() -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Failed,
        files: vec![],
        repair: None,
        expanded_bytes: None,
        copied_bytes: None,
        staged_bytes: None,
        error_code: None,
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn explicit_par2_path_loads_matching_recovery_volumes_only() {
        let dir = temp_test_dir("explicit-par2");
        touch(&dir.join("movie.par2"));
        touch(&dir.join("movie.vol00+2.par2"));
        touch(&dir.join("movie.vol02+2.par2"));
        touch(&dir.join("other.par2"));
        touch(&dir.join("other.vol00+1.par2"));

        let primary = dir.join("movie.par2").to_string_lossy().to_string();
        let paths = par2_paths_for_request(&dir, Some(&primary)).unwrap();

        assert_eq!(
            file_names(paths),
            vec![
                "movie.par2".to_string(),
                "movie.vol00+2.par2".to_string(),
                "movie.vol02+2.par2".to_string(),
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn omitted_par2_path_loads_all_par2_files_in_source_dir() {
        let dir = temp_test_dir("all-par2");
        touch(&dir.join("movie.par2"));
        touch(&dir.join("movie.vol00+2.par2"));
        touch(&dir.join("other.par2"));
        touch(&dir.join("readme.txt"));

        let paths = par2_paths_for_request(&dir, None).unwrap();

        assert_eq!(
            file_names(paths),
            vec![
                "movie.par2".to_string(),
                "movie.vol00+2.par2".to_string(),
                "other.par2".to_string(),
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "scryer-archive-plugin-{name}-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"par2").unwrap();
    }

    fn file_names(paths: Vec<PathBuf>) -> Vec<String> {
        paths
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }
}
