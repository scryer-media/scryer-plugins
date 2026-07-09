use scryer_plugin_pdk::run_archive_plugin;
use scryer_plugin_pdk::{
    ArchivePluginExtractedFile, ArchivePluginFormat, ArchivePluginOperation,
    ArchivePluginProcessRequest, ArchivePluginProcessResponse, ArchivePluginStatus,
};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ArchiveExtractorCapabilities, ArchiveExtractorDescriptor, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION,
};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use weaver_unrar::{ExtractOptions, RarArchive, RarError};

const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;

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
                status: ArchivePluginStatus::Failed,
                error_code: Some("missing_volume".to_string()),
                message: Some(format!(
                    "RAR member '{}' is missing volume(s): {:?}",
                    info.name, member.missing_volumes
                )),
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
        expanded_bytes: None,
        copied_bytes: None,
        staged_bytes: None,
        error_code: None,
        message: None,
    }
}
