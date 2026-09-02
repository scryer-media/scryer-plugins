//! Scryer's archive-extraction plugin, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:archive/archive-extractor@1.0.0`: two exports
//! carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`, `process`
//! exchanges an `ArchivePluginProcessRequest` for an
//! `ArchivePluginProcessResponse`), plus one imported `crypto` interface for
//! AES-CBC and CRC-32. WASI Preview 2 arrives from the linker, which is how the
//! guest sees its read-only source preopen, its writable output preopen, and
//! its private `TMPDIR` scratch dir.
//!
//! ## Why a component, and what it changed
//!
//! The previous artifact was a `wasm32-wasip1` command binary that reached the
//! host through raw guest pointers (`host_aes_cbc_decrypt` / `host_crc32` in an
//! legacy host namespace) and framed its request/response over stdio. A component
//! has no exported linear memory for a host to slice, and no stdio protocol, so
//! both halves move onto the canonical ABI: payloads cross as `list<u8>`, and
//! the crypto delegation inside unrar-rs is re-pointed at
//! [`unrar_rs::component_abi`] hooks that this crate wires to the world's
//! `crypto` import. Extraction behaviour itself — formats, limits, path safety,
//! partial-output cleanup — is unchanged.
//!
//! ## PAR2 is internal
//!
//! PAR2 is deliberately absent from the plugin contract. Recovery sets are
//! handled data-driven inside [`par2`]: when the source directory has one it is
//! verified, placed, and repaired before extraction starts.

use liblzma::read::XzDecoder;
use liblzma::stream::{CONCATENATED, Stream};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ArchiveExtractorCapabilities, ArchiveExtractorDescriptor, ArchivePluginExtractedFile,
    ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessRequest,
    ArchivePluginProcessResponse, ArchivePluginStatus, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION,
};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use unrar_rs::component_abi::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};
use unrar_rs::{ExtractOptions, RarArchive, RarError};

mod par2;

wit_bindgen::generate!({
    world: "archive-extractor",
    path: "wit",
});

use crate::scryer::archive::crypto as host_crypto;

pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 20_000;
pub(crate) const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;
const MAX_XZ_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_XZ_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_XZ_DECODER_MEMORY_BYTES: u64 = 64 * 1024 * 1024;

/// This crate's implementation of `scryer:archive/archive-extractor@1.0.0`.
struct ArchiveExtractorComponent;

impl Guest for ArchiveExtractorComponent {
    /// The catalog/packaging descriptor, as UTF-8 JSON.
    ///
    /// `describe` returns a bare `list<u8>`, so a serialization failure has no
    /// channel of its own; an empty document is emitted instead and the host
    /// reports it as invalid descriptor JSON. The descriptor is a fixed literal,
    /// so that path is unreachable in practice.
    fn describe() -> Vec<u8> {
        serde_json::to_vec(&build_descriptor()).unwrap_or_default()
    }

    /// One request, one response.
    ///
    /// `invocation-error` is reserved for payloads that cannot be parsed or
    /// produced at all. Every operational outcome — a wrong password, a damaged
    /// archive, an unrepairable PAR2 set — is an ordinary
    /// `ArchivePluginProcessResponse` with a non-`ok` status, so the host keeps
    /// this plugin's own diagnosis instead of a generic ABI failure.
    fn process(request: Vec<u8>) -> Result<Vec<u8>, InvocationError> {
        install_crypto_hooks();
        let request = serde_json::from_slice::<ArchivePluginProcessRequest>(&request)
            .map_err(|_| InvocationError::InvalidResponse)?;
        let response = handle_request(request);
        serde_json::to_vec(&response).map_err(|_| InvocationError::Failed)
    }
}

export!(ArchiveExtractorComponent);

/// Point unrar-rs's bulk AES-CBC and CRC-32 delegation at the world's `crypto`
/// import.
///
/// unrar-rs is transport-agnostic — it holds two `fn` pointers and knows nothing
/// about WIT — so this adapter is the whole seam between that crate and the
/// component ABI. It runs at the top of every `process` because the host
/// instantiates the component once per invocation.
///
/// A length rejection from the host is a contract violation rather than a
/// recoverable condition: this plugin only ever passes 16/32-byte keys, 16-byte
/// IVs, and block-aligned buffers. The error is handed back to unrar-rs, which
/// panics naming the offending status.
fn install_crypto_hooks() {
    fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
        host_crypto::aes_cbc_decrypt(key, iv, data).map_err(|error| match error {
            host_crypto::AesError::BadKeyLength => HostAesError::BadKeyLength,
            host_crypto::AesError::BadBlockLength => HostAesError::BadBlockLength,
            host_crypto::AesError::BadIvLength => HostAesError::BadIvLength,
        })
    }

    fn crc32(seed: u32, data: &[u8]) -> u32 {
        host_crypto::crc32(seed, data)
    }

    install_host_crypto_hooks(HostCryptoHooks {
        aes_cbc_decrypt,
        crc32,
    });
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
                formats: vec![
                    ArchivePluginFormat::Rar,
                    ArchivePluginFormat::Zip,
                    ArchivePluginFormat::SevenZip,
                    ArchivePluginFormat::Xz,
                ],
            },
        }),
    }
}

/// Map one request onto the per-operation logic.
///
/// Operational outcomes are reported in-band via [`ArchivePluginStatus`]; only
/// a payload that cannot be parsed or produced becomes an `invocation-error`.
fn handle_request(request: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse {
    match request.operation {
        ArchivePluginOperation::Inspect { source_dir, .. } => {
            // A directory carrying a recovery set can be described without
            // extracting anything, which is the only thing `Inspect` currently
            // has to say. Without one, inspection is still unimplemented.
            par2::inspect(Path::new(&source_dir)).unwrap_or_else(|| {
                unsupported_response("archive inspection is not implemented yet")
            })
        }
        ArchivePluginOperation::ExtractArchive {
            archive_path,
            output_dir,
            format,
            password,
        } => extract_archive(&archive_path, &output_dir, format, password.as_deref()),
    }
}

/// Extract one archive, repairing its PAR2 recovery set first when there is one.
///
/// PAR2 handling can end the request three ways: no recovery set (extract the
/// requested archive as-is), a prepared input set (extract the archive PAR2
/// identified, out of wherever the corrected bytes were materialized), or a
/// complete response — either an unrepairable failure or the plain-file
/// emission case, where the recovery set protects media rather than an archive
/// and the repaired files themselves are the deliverable.
fn extract_archive(
    archive_path: &str,
    output_dir: &str,
    format: ArchivePluginFormat,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let archive_path = Path::new(archive_path);
    let output_root = Path::new(output_dir);
    let source_dir = archive_path.parent().unwrap_or_else(|| Path::new("."));

    match par2::prepare_for_extraction(source_dir, archive_path, format, output_root) {
        par2::Par2Plan::NoRecoverySet => {
            extract_prepared_archive(archive_path, output_root, format, password)
        }
        par2::Par2Plan::Prepared(inputs) => {
            let response =
                extract_prepared_archive(&inputs.archive_path, output_root, format, password);
            // The staged copy duplicates inputs the host still owns; drop it
            // whether or not extraction succeeded.
            inputs.cleanup();
            response
        }
        par2::Par2Plan::Complete(response) => *response,
    }
}

fn extract_prepared_archive(
    archive_path: &Path,
    output_dir: &Path,
    format: ArchivePluginFormat,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    match format {
        ArchivePluginFormat::Rar => extract_rar(archive_path, output_dir, password),
        ArchivePluginFormat::SevenZip => extract_sevenz(archive_path, output_dir, password),
        ArchivePluginFormat::Zip => extract_zip(archive_path, output_dir, password),
        ArchivePluginFormat::Xz => extract_xz(archive_path, output_dir, password),
    }
}

pub fn extract_xz(
    archive_path: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    extract_xz_with_limits(
        archive_path,
        output_dir,
        password,
        MAX_XZ_COMPRESSED_BYTES,
        MAX_XZ_EXPANDED_BYTES,
        MAX_XZ_DECODER_MEMORY_BYTES,
    )
}

fn extract_xz_with_limits(
    archive_path: &Path,
    output_dir: &Path,
    password: Option<&str>,
    compressed_limit: u64,
    expanded_limit: u64,
    decoder_memory_limit: u64,
) -> ArchivePluginProcessResponse {
    if password.is_some_and(|password| !password.is_empty()) {
        return unsupported_response("XZ streams do not support passwords");
    }

    let metadata = match fs::metadata(archive_path) {
        Ok(metadata) => metadata,
        Err(error) => return failed_response("open_xz", "failed to open XZ stream", error),
    };
    if metadata.len() > compressed_limit {
        return failed_message(
            "compressed_too_large",
            &format!("XZ stream is larger than {compressed_limit} bytes"),
        );
    }

    let relative_path = match xz_output_relative_path(archive_path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let output_root = output_dir;
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }
    let destination = output_root.join(&relative_path);
    let input = match fs::File::open(archive_path) {
        Ok(file) => file,
        Err(error) => return failed_response("open_xz", "failed to open XZ stream", error),
    };
    let stream = match Stream::new_stream_decoder(decoder_memory_limit, CONCATENATED) {
        Ok(stream) => stream,
        Err(error) => {
            return failed_response("initialize_xz", "failed to initialize XZ decoder", error);
        }
    };
    let mut decoder = XzDecoder::new_stream(input, stream);
    let mut output = match fs::File::create(&destination) {
        Ok(file) => file,
        Err(error) => {
            return failed_response("create_file", "failed to create XZ output file", error);
        }
    };
    let written = match copy_limited(&mut decoder, &mut output, expanded_limit) {
        Ok(written) => written,
        Err(error) => {
            let _ = fs::remove_file(&destination);
            let code = if error.kind() == io::ErrorKind::InvalidData
                && error.to_string().contains("configured limit")
            {
                "expanded_too_large"
            } else {
                "extract_xz"
            };
            return failed_response(code, "failed to decompress XZ stream", error);
        }
    };

    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Ok,
        files: vec![ArchivePluginExtractedFile {
            relative_path: relative_path.to_string_lossy().replace('\\', "/"),
            size: Some(written),
            checksum: None,
        }],
        expanded_bytes: Some(written),
        ..empty_response()
    }
}

fn xz_output_relative_path(
    archive_path: &Path,
) -> Result<PathBuf, Box<ArchivePluginProcessResponse>> {
    let filename = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Box::new(failed_message(
                "invalid_xz_name",
                "XZ stream has no valid filename",
            ))
        })?;
    let lower = filename.to_ascii_lowercase();
    let output_name = if lower.ends_with(".txz") {
        format!("{}.tar", &filename[..filename.len() - 4])
    } else if lower.ends_with(".xz") {
        filename[..filename.len() - 3].to_string()
    } else {
        return Err(Box::new(failed_message(
            "invalid_xz_name",
            "XZ stream filename must end in .xz or .txz",
        )));
    };
    safe_archive_relative_path(&output_name)
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
    archive_path: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
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
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let output_root = output_dir;
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }

    let mut files = Vec::new();
    let mut expanded_bytes = 0_u64;
    let mut output_paths = HashSet::new();
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
        if !info.is_directory
            && let Err(response) = record_output_file_path(&mut output_paths, &relative_path)
        {
            return *response;
        }

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
    archive_path: &Path,
    output_dir: &Path,
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

    let output_root = output_dir;
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }

    let mut files = Vec::new();
    let mut expanded_bytes = 0_u64;
    let mut output_paths = HashSet::new();

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
            if let Err(response) = record_output_file_path(&mut output_paths, &relative_path) {
                return *response;
            }
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

fn extract_sevenz(
    archive_path: &Path,
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let archive_file = match fs::File::open(archive_path) {
        Ok(file) => file,
        Err(error) => return failed_response("open_7z", "failed to open 7z archive", error),
    };
    let password_value = match password.filter(|password| !password.is_empty()) {
        Some(password) => sevenz_rust2::Password::from(password),
        None => sevenz_rust2::Password::empty(),
    };
    let mut archive = match sevenz_rust2::ArchiveReader::new(archive_file, password_value) {
        Ok(archive) => archive,
        Err(error) => return sevenz_error_response("read_7z", error, password),
    };

    let output_root = output_dir;
    if let Err(error) = fs::create_dir_all(output_root) {
        return failed_response(
            "create_output",
            "failed to create archive output directory",
            error,
        );
    }

    if archive.archive().files.len() > MAX_ARCHIVE_ENTRIES {
        return failed_message("too_many_entries", "7z archive contains too many entries");
    }

    let mut declared_expanded_bytes = 0_u64;
    let mut declared_output_paths = HashSet::new();
    for entry in &archive.archive().files {
        let relative_path = match safe_archive_relative_path(entry.name()) {
            Ok(path) => path,
            Err(response) => return *response,
        };
        if entry.is_directory() {
            continue;
        }
        if let Err(response) = record_output_file_path(&mut declared_output_paths, &relative_path) {
            return *response;
        }
        declared_expanded_bytes = match declared_expanded_bytes.checked_add(entry.size()) {
            Some(total) if total <= MAX_ARCHIVE_EXPANDED_BYTES => total,
            _ => {
                return failed_message(
                    "expanded_too_large",
                    &format!("7z archive expands beyond {MAX_ARCHIVE_EXPANDED_BYTES} bytes"),
                );
            }
        };
    }

    let mut files = Vec::new();
    let mut actual_expanded_bytes = 0_u64;
    let mut output_paths = HashSet::new();
    let extraction = archive.for_each_entries(|entry, entry_reader| {
        let relative_path = safe_archive_relative_path(entry.name())
            .map_err(|response| sevenz_error_from_message(response.message.as_deref()))?;
        let destination = output_root.join(&relative_path);
        if entry.is_directory() {
            fs::create_dir_all(&destination)?;
            return Ok(true);
        }
        record_output_file_path(&mut output_paths, &relative_path)
            .map_err(|response| sevenz_error_from_message(response.message.as_deref()))?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&destination)?;
        let copy_limit = MAX_ARCHIVE_EXPANDED_BYTES.saturating_sub(actual_expanded_bytes);
        let written = match copy_limited(entry_reader, &mut output, copy_limit) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                return Err(error.into());
            }
        };
        actual_expanded_bytes = actual_expanded_bytes
            .checked_add(written)
            .ok_or_else(|| sevenz_error_from_message(Some("archive entry is too large")))?;
        if actual_expanded_bytes > MAX_ARCHIVE_EXPANDED_BYTES {
            let _ = fs::remove_file(&destination);
            return Err(sevenz_error_from_message(Some(
                "archive expands beyond the configured limit",
            )));
        }
        files.push(ArchivePluginExtractedFile {
            relative_path: relative_path.to_string_lossy().replace('\\', "/"),
            size: Some(written),
            checksum: None,
        });
        Ok(true)
    });

    if let Err(error) = extraction {
        return sevenz_error_response("extract_7z", error, password);
    }

    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Ok,
        files,
        expanded_bytes: Some(actual_expanded_bytes),
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
    if path.trim().is_empty() {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains an empty path",
        )));
    }
    if path.contains('\\') {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains a backslash path separator",
        )));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains an absolute path",
        )));
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Box::new(failed_message(
                    "unsafe_path",
                    "archive contains an unsafe path component",
                )));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(Box::new(failed_message(
            "unsafe_path",
            "archive contains an empty path",
        )));
    }
    Ok(relative)
}

fn record_output_file_path(
    output_paths: &mut HashSet<PathBuf>,
    relative_path: &Path,
) -> Result<(), Box<ArchivePluginProcessResponse>> {
    if !output_paths.insert(relative_path.to_path_buf()) {
        return Err(Box::new(failed_message(
            "duplicate_output_path",
            "archive contains multiple file entries for the same output path",
        )));
    }
    Ok(())
}

pub(crate) fn copy_limited<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    limit: u64,
) -> io::Result<u64> {
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

pub(crate) fn failed_message(error_code: &str, message: &str) -> ArchivePluginProcessResponse {
    ArchivePluginProcessResponse {
        status: ArchivePluginStatus::Failed,
        error_code: Some(error_code.to_string()),
        message: Some(message.to_string()),
        ..empty_response()
    }
}

pub(crate) fn failed_response(
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

fn sevenz_error_response(
    error_code: &str,
    error: sevenz_rust2::Error,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let (status, code, public_message) = if lower.contains("unsupported")
        || lower.contains("zstd")
        || lower.contains("method")
    {
        (
                ArchivePluginStatus::Failed,
                "unsupported_7z_method",
                "This 7z archive uses a compression method the Archive Extraction plugin does not support yet.".to_string(),
            )
    } else if lower.contains("password") || lower.contains("encrypted") {
        let status = if password.is_some_and(|password| !password.is_empty()) {
            ArchivePluginStatus::PasswordInvalid
        } else {
            ArchivePluginStatus::PasswordRequired
        };
        (
            status,
            error_code,
            format!("7z archive password error: {message}"),
        )
    } else {
        (
            ArchivePluginStatus::Failed,
            error_code,
            format!("failed to extract 7z archive: {message}"),
        )
    };

    ArchivePluginProcessResponse {
        status,
        error_code: Some(code.to_string()),
        message: Some(public_message),
        ..empty_response()
    }
}

fn sevenz_error_from_message(message: Option<&str>) -> sevenz_rust2::Error {
    sevenz_rust2::Error::Other(Cow::Owned(
        message
            .filter(|message| !message.is_empty())
            .unwrap_or("7z extraction failed")
            .to_string(),
    ))
}

pub(crate) fn empty_response() -> ArchivePluginProcessResponse {
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

#[cfg(test)]
mod tests {
    use super::*;
    use liblzma::write::XzEncoder;

    fn write_xz_fixture(path: &Path, content: &[u8]) {
        let file = fs::File::create(path).expect("create XZ fixture");
        let mut encoder = XzEncoder::new(file, 6);
        encoder.write_all(content).expect("compress XZ fixture");
        encoder.finish().expect("finish XZ fixture");
    }

    #[test]
    fn xz_stream_extracts_to_suffix_stripped_file() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitle.ass.xz");
        let content = b"[Script Info]\nTitle: XZ fixture\n";
        write_xz_fixture(&input_path, content);

        let response = extract_xz(&input_path, output.path(), None);

        assert_eq!(response.status, ArchivePluginStatus::Ok);
        assert_eq!(response.expanded_bytes, Some(content.len() as u64));
        assert_eq!(response.files.len(), 1);
        assert_eq!(response.files[0].relative_path, "subtitle.ass");
        assert_eq!(
            fs::read(output.path().join("subtitle.ass")).unwrap(),
            content
        );
    }

    #[test]
    fn txz_stream_extracts_to_tar_file() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitles.txz");
        write_xz_fixture(&input_path, b"tar fixture");

        let response = extract_xz(&input_path, output.path(), None);

        assert_eq!(response.status, ArchivePluginStatus::Ok);
        assert_eq!(response.files[0].relative_path, "subtitles.tar");
    }

    #[test]
    fn xz_stream_enforces_expanded_limit_and_removes_partial_output() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitle.srt.xz");
        write_xz_fixture(&input_path, b"0123456789");

        let response = extract_xz_with_limits(
            &input_path,
            output.path(),
            None,
            1024,
            5,
            MAX_XZ_DECODER_MEMORY_BYTES,
        );

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(response.error_code.as_deref(), Some("expanded_too_large"));
        assert!(!output.path().join("subtitle.srt").exists());
    }

    #[test]
    fn xz_stream_rejects_input_over_compressed_limit() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitle.srt.xz");
        write_xz_fixture(&input_path, b"subtitle");

        let response = extract_xz_with_limits(
            &input_path,
            output.path(),
            None,
            1,
            MAX_XZ_EXPANDED_BYTES,
            MAX_XZ_DECODER_MEMORY_BYTES,
        );

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(response.error_code.as_deref(), Some("compressed_too_large"));
        assert!(!output.path().join("subtitle.srt").exists());
    }

    #[test]
    fn sevenz_unsupported_method_maps_to_structured_error() {
        let response = sevenz_error_response(
            "extract_7z",
            sevenz_rust2::Error::Other(Cow::Borrowed("unsupported compression method zstd")),
            None,
        );

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(
            response.error_code.as_deref(),
            Some("unsupported_7z_method")
        );
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("does not support yet"))
        );
    }
}

/// PAR2 behaviour, end to end through [`extract_archive`].
///
/// Every fixture here is generated in-process by par2-rs's own creator, so the
/// recovery data always matches the payload it protects and there is nothing
/// checked in to drift. The payload is deterministic pseudo-random bytes, which
/// keeps it incompressible: a ZIP of it is close to its own size, so the slice
/// map is dense enough that damaging a byte really does cost a slice.
#[cfg(test)]
mod par2_tests {
    use super::*;
    use par2_rs::{BlockSizing, Par2Creator, Par2CreatorOptions, RecoveryAmount};
    use std::io::{Seek, SeekFrom};

    /// Source slice size for every fixture set. Small enough that a modest
    /// payload still has tens of slices, so "damage N slices" is precise.
    const SLICE_BYTES: u64 = 4_096;
    const PAYLOAD_BYTES: usize = 128 * 1024;

    /// Deterministic xorshift64* bytes — reproducible, incompressible, and no
    /// new dependency.
    fn payload(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 0x9E37_79B9_7F4A_7C15;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn write_zip(path: &Path, entry: &str, contents: &[u8]) {
        let file = fs::File::create(path).expect("create ZIP fixture");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(entry, zip::write::SimpleFileOptions::default())
            .expect("start ZIP entry");
        zip.write_all(contents).expect("write ZIP payload");
        zip.finish().expect("finish ZIP fixture");
    }

    /// Build a recovery set over `inputs` with an explicit recovery-block count,
    /// which is what makes "repairable" and "unrepairable" reproducible rather
    /// than a function of par2-rs's default percentage.
    fn create_recovery_set(dir: &Path, inputs: &[PathBuf], recovery_blocks: u32) {
        let mut options = Par2CreatorOptions::with_output(
            dir.join("recovery"),
            Some(dir.to_path_buf()),
            inputs.to_vec(),
        );
        options.block_sizing = BlockSizing::Bytes(SLICE_BYTES);
        options.recovery_amount = RecoveryAmount::Count(recovery_blocks);
        let creator = Par2Creator::new(options);
        let plan = creator.plan().expect("plan PAR2 creation");
        creator.create(&plan).expect("create PAR2 recovery set");
    }

    /// Overwrite one slice-sized run, `slice_index` slices in. Writing a
    /// constant that the pseudo-random payload cannot contain guarantees the
    /// slice checksum actually moves.
    fn damage_slice(path: &Path, slice_index: u64) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open fixture for damage");
        file.seek(SeekFrom::Start(slice_index * SLICE_BYTES))
            .expect("seek to damaged slice");
        file.write_all(&vec![0xA5_u8; SLICE_BYTES as usize])
            .expect("write damage");
    }

    struct Fixture {
        source: tempfile::TempDir,
        output: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                source: tempfile::tempdir().expect("create PAR2 source dir"),
                output: tempfile::tempdir().expect("create PAR2 output dir"),
            }
        }

        fn extract(
            &self,
            archive: &str,
            format: ArchivePluginFormat,
        ) -> ArchivePluginProcessResponse {
            extract_archive(
                self.source.path().join(archive).to_str().unwrap(),
                self.output.path().to_str().unwrap(),
                format,
                None,
            )
        }
    }

    /// The core promise: a damaged archive covered by enough recovery data is
    /// repaired first, and the extraction that follows produces the ORIGINAL
    /// bytes — not merely "no error".
    #[test]
    fn a_damaged_archive_is_repaired_before_extraction() {
        let fixture = Fixture::new();
        let contents = payload(0x9001, PAYLOAD_BYTES);
        let archive = fixture.source.path().join("payload.zip");
        write_zip(&archive, "media/episode.bin", &contents);
        create_recovery_set(fixture.source.path(), std::slice::from_ref(&archive), 8);

        damage_slice(&archive, 5);
        damage_slice(&archive, 9);

        let response = fixture.extract("payload.zip", ArchivePluginFormat::Zip);

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "damaged-but-repairable set must extract: {:?}",
            response.message
        );
        assert_eq!(
            fs::read(fixture.output.path().join("media/episode.bin")).unwrap(),
            contents,
            "repair must reconstruct the original bytes"
        );
    }

    /// An obfuscated download: the archive is on disk under a meaningless name.
    /// Placement matches it by content hash and the staged copy carries the
    /// canonical name, so extraction can open it at all.
    #[test]
    fn a_misnamed_archive_is_placed_by_content_before_extraction() {
        let fixture = Fixture::new();
        let contents = payload(0x9002, PAYLOAD_BYTES);
        let archive = fixture.source.path().join("payload.zip");
        write_zip(&archive, "media/episode.bin", &contents);
        create_recovery_set(fixture.source.path(), std::slice::from_ref(&archive), 4);

        let obfuscated = fixture.source.path().join("a3f19c2e.bin");
        fs::rename(&archive, &obfuscated).expect("obfuscate the archive name");

        let response = fixture.extract("a3f19c2e.bin", ArchivePluginFormat::Zip);

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "a misnamed archive must be placed and extracted: {:?}",
            response.message
        );
        assert_eq!(
            fs::read(fixture.output.path().join("media/episode.bin")).unwrap(),
            contents
        );
    }

    /// Damage beyond the recovery data is terminal, and says so. Silently
    /// extracting a corrupt archive would be the worse failure.
    #[test]
    fn an_unrepairable_set_fails_with_a_clear_message() {
        let fixture = Fixture::new();
        let contents = payload(0x9003, PAYLOAD_BYTES);
        let archive = fixture.source.path().join("payload.zip");
        write_zip(&archive, "media/episode.bin", &contents);
        create_recovery_set(fixture.source.path(), std::slice::from_ref(&archive), 2);

        for slice in [3, 6, 9, 12, 15, 18] {
            damage_slice(&archive, slice);
        }

        let response = fixture.extract("payload.zip", ArchivePluginFormat::Zip);

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(
            response.error_code.as_deref(),
            Some("par2_insufficient_recovery"),
            "{:?}",
            response.message
        );
        assert!(
            !fixture.output.path().join("media/episode.bin").exists(),
            "an unrepairable set must not leave partial output"
        );
    }

    /// No recovery set means the PAR2 path is not merely skipped but invisible:
    /// the extraction is byte-for-byte the one this plugin did before PAR2
    /// existed, including its output layout.
    #[test]
    fn an_archive_without_a_recovery_set_extracts_unchanged() {
        let fixture = Fixture::new();
        let contents = payload(0x9004, 4_096);
        write_zip(
            &fixture.source.path().join("payload.zip"),
            "media/episode.bin",
            &contents,
        );

        let response = fixture.extract("payload.zip", ArchivePluginFormat::Zip);

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(response.files.len(), 1);
        assert_eq!(response.files[0].relative_path, "media/episode.bin");
        assert_eq!(
            fs::read(fixture.output.path().join("media/episode.bin")).unwrap(),
            contents
        );
    }

    /// A recovery set that protects plain media rather than an archive: there is
    /// nothing to extract, so the repaired files themselves are the deliverable
    /// and land in the output directory for the host's import pass.
    #[test]
    fn a_recovery_set_over_plain_files_emits_the_repaired_files() {
        let fixture = Fixture::new();
        let contents = payload(0x9005, PAYLOAD_BYTES);
        let media = fixture.source.path().join("episode.mkv");
        fs::write(&media, &contents).expect("write plain fixture");
        create_recovery_set(fixture.source.path(), std::slice::from_ref(&media), 8);

        damage_slice(&media, 4);

        let response = fixture.extract("episode.mkv", ArchivePluginFormat::Rar);

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "a plain-file recovery set must repair and emit: {:?}",
            response.message
        );
        assert_eq!(
            response
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["episode.mkv"]
        );
        assert_eq!(
            fs::read(fixture.output.path().join("episode.mkv")).unwrap(),
            contents,
            "the emitted plain file must be the repaired original"
        );
        assert_eq!(
            fs::read(&media).unwrap().len(),
            contents.len(),
            "the read-only source must not be rewritten in place"
        );
        assert_ne!(
            fs::read(&media).unwrap(),
            contents,
            "the damaged source copy stays damaged; only the output is repaired"
        );
    }

    /// `Inspect` reports a recovery set without materializing anything — it has
    /// no writable output preopen — and still answers "unimplemented" when the
    /// directory carries no set at all.
    #[test]
    fn inspect_reports_a_recovery_set_and_nothing_else() {
        let fixture = Fixture::new();
        let contents = payload(0x9006, PAYLOAD_BYTES);
        let media = fixture.source.path().join("episode.mkv");
        fs::write(&media, &contents).expect("write plain fixture");

        let bare = handle_request(ArchivePluginProcessRequest {
            operation: ArchivePluginOperation::Inspect {
                source_dir: fixture.source.path().to_string_lossy().into_owned(),
                archive_path: None,
            },
        });
        assert_eq!(bare.status, ArchivePluginStatus::UnsupportedFormat);

        create_recovery_set(fixture.source.path(), &[media], 4);
        let described = handle_request(ArchivePluginProcessRequest {
            operation: ArchivePluginOperation::Inspect {
                source_dir: fixture.source.path().to_string_lossy().into_owned(),
                archive_path: None,
            },
        });
        assert_eq!(described.status, ArchivePluginStatus::Ok);
        assert_eq!(
            described
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["episode.mkv"]
        );
    }
}
