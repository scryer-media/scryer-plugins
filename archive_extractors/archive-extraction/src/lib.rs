use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ArchiveExtractorCapabilities, ArchiveExtractorDescriptor, ArchivePluginExtractedFile,
    ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessRequest,
    ArchivePluginProcessResponse, ArchivePluginRepairFormat, ArchivePluginRepairState,
    ArchivePluginRepairStatus, ArchivePluginStatus, PluginDescriptor, PluginError, PluginErrorCode,
    PluginResult, ProviderDescriptor, SDK_VERSION,
};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use weaver_par2::{Par2RepairOutcome, Par2RepairStatus, Par2Repairer, Par2RepairerOptions};
use weaver_unrar::{ExtractOptions, RarArchive, RarError};

const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&build_descriptor())?)
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

#[plugin_fn]
pub fn scryer_archive_process(input: String) -> FnResult<String> {
    let request = match serde_json::from_str::<ArchivePluginProcessRequest>(&input) {
        Ok(request) => request,
        Err(error) => {
            return plugin_error(PluginErrorCode::Permanent, "invalid archive request", error);
        }
    };

    let response = match request.operation {
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
    };

    Ok(serde_json::to_string(&PluginResult::Ok(response))?)
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
    let repair_response = repair_par2_set(source_dir, par2_path);
    if !matches!(repair_response.status, ArchivePluginStatus::Ok) {
        return repair_response;
    }

    let Some(archive_path) = archive_path else {
        return failed_message(
            "missing_archive",
            "repair_then_extract requires an archive path after PAR2 verification",
        );
    };

    let mut response = extract_archive(archive_path, output_dir, format, password);
    response.repair = repair_response.repair;
    response
}

fn extract_rar(
    archive_path: &str,
    output_dir: &str,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let archive_path = Path::new(archive_path);
    let archive_file = match fs::File::open(archive_path) {
        Ok(file) => file,
        Err(error) => return failed_response("open_rar", "failed to open RAR archive", error),
    };

    let mut archive = match password.filter(|password| !password.is_empty()) {
        Some(password) => match RarArchive::open_with_password(archive_file, password) {
            Ok(archive) => archive,
            Err(error) => {
                return rar_error_response("open_rar", "failed to read RAR archive", error);
            }
        },
        None => match RarArchive::open(archive_file) {
            Ok(archive) => archive,
            Err(error) => {
                return rar_error_response("open_rar", "failed to read RAR archive", error);
            }
        },
    };

    if let Some(password) = password.filter(|password| !password.is_empty()) {
        archive.set_password(password.to_string());
    }

    let source_dir = archive_path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = attach_rar_volumes(&mut archive, source_dir, archive_path) {
        return rar_error_response("read_rar_volume", "failed to read RAR volume", error);
    }

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
    run_par2_set(source_dir, par2_path, false)
}

fn repair_par2_set(source_dir: &str, par2_path: Option<&str>) -> ArchivePluginProcessResponse {
    run_par2_set(source_dir, par2_path, true)
}

fn run_par2_set(
    source_dir: &str,
    par2_path: Option<&str>,
    repair: bool,
) -> ArchivePluginProcessResponse {
    let source_dir = PathBuf::from(source_dir);
    let par2_paths = match par2_paths_for_request(&source_dir, par2_path) {
        Ok(paths) => paths,
        Err(response) => return *response,
    };

    let mut options = Par2RepairerOptions::new(source_dir, par2_paths);
    options.repair = repair;

    let outcome = match Par2Repairer::new(options).verify_or_repair() {
        Ok(outcome) => outcome,
        Err(error) => {
            let operation = if repair {
                "PAR2 repair"
            } else {
                "PAR2 verification"
            };
            let error_code = if repair { "par2_repair" } else { "par2_verify" };
            return failed_response(error_code, &format!("{operation} failed"), error);
        }
    };

    par2_response(outcome)
}

fn par2_response(outcome: Par2RepairOutcome) -> ArchivePluginProcessResponse {
    let repair = Some(par2_repair_status(&outcome));
    match outcome.status {
        Par2RepairStatus::Verified | Par2RepairStatus::Repaired => ArchivePluginProcessResponse {
            status: ArchivePluginStatus::Ok,
            repair,
            ..empty_response()
        },
        Par2RepairStatus::RepairPossible => ArchivePluginProcessResponse {
            status: ArchivePluginStatus::RepairRequired,
            repair,
            error_code: Some("par2_repair_required".to_string()),
            message: Some(
                "PAR2 verification found repairable damage; writable repair staging is required"
                    .to_string(),
            ),
            ..empty_response()
        },
        Par2RepairStatus::Insufficient => ArchivePluginProcessResponse {
            status: ArchivePluginStatus::RepairRequired,
            repair,
            error_code: Some("par2_insufficient_recovery_data".to_string()),
            message: Some("PAR2 set does not have enough recovery data".to_string()),
            ..empty_response()
        },
        Par2RepairStatus::ResourceLimited => ArchivePluginProcessResponse {
            status: ArchivePluginStatus::RepairFailed,
            repair,
            error_code: Some("par2_resource_limited".to_string()),
            message: Some("PAR2 verification exceeded resource limits".to_string()),
            ..empty_response()
        },
    }
}

fn par2_paths_for_request(
    source_dir: &Path,
    par2_path: Option<&str>,
) -> Result<Vec<PathBuf>, Box<ArchivePluginProcessResponse>> {
    if let Some(par2_path) = par2_path {
        return Ok(vec![PathBuf::from(par2_path)]);
    }

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

fn par2_repair_status(outcome: &Par2RepairOutcome) -> ArchivePluginRepairStatus {
    let status = match outcome.status {
        Par2RepairStatus::Verified => ArchivePluginRepairState::Verified,
        Par2RepairStatus::Repaired => ArchivePluginRepairState::Repaired,
        Par2RepairStatus::RepairPossible => ArchivePluginRepairState::Failed,
        Par2RepairStatus::Insufficient => ArchivePluginRepairState::InsufficientRecoveryData,
        Par2RepairStatus::ResourceLimited => ArchivePluginRepairState::Failed,
    };
    let message = match outcome.status {
        Par2RepairStatus::Verified => Some("PAR2 set verified cleanly".to_string()),
        Par2RepairStatus::Repaired => Some("PAR2 set was repaired".to_string()),
        Par2RepairStatus::RepairPossible => {
            Some("PAR2 repair is possible but requires writable staging".to_string())
        }
        Par2RepairStatus::Insufficient => {
            Some("PAR2 set has insufficient recovery data".to_string())
        }
        Par2RepairStatus::ResourceLimited => {
            Some("PAR2 verify/repair exceeded resource limits".to_string())
        }
    };

    ArchivePluginRepairStatus {
        status,
        read_bytes: Some(outcome.bytes_copied),
        written_bytes: Some(outcome.bytes_reconstructed),
        message,
    }
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

fn plugin_error(
    code: PluginErrorCode,
    public_message: &str,
    error: impl std::fmt::Display,
) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::<
        ArchivePluginProcessResponse,
    >::Err(PluginError {
        code,
        public_message: public_message.to_string(),
        debug_message: Some(error.to_string()),
        retry_after_seconds: None,
    }))?)
}
