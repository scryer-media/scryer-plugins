//! Scryer's archive-extraction plugin, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:archive/archive-extractor@1.1.0`: two exports
//! carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`, `process`
//! exchanges an `ArchivePluginProcessRequest` for an
//! `ArchivePluginProcessResponse`), plus one imported `crypto` interface for
//! AES-CBC, CRC-32, and the catalog `crc` function (used here for CRC-64/XZ). WASI Preview 2 arrives from the linker, which is how the
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
//! [`unrar_rs::hooks`] that this crate wires to the world's
//! `crypto` import; sevenz-turbo's AES decrypt and lzma-turbo's CRC-32 (which
//! sevenz-turbo checksums through too) go through the same import. Extraction behaviour itself — formats, limits, path safety,
//! partial-output cleanup — is unchanged.
//!
//! ## PAR2 is internal
//!
//! PAR2 is deliberately absent from the plugin contract. Recovery sets are
//! handled data-driven inside [`par2`]: when the source directory has one it is
//! verified, placed, and repaired before extraction starts.

use lzma_turbo::hooks::{HostHashHooks, HostSha256Handle, install_host_hash_hooks};
use lzma_turbo::xz::{XzError, XzErrorKind, XzReader};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ArchiveExtractorCapabilities, ArchiveExtractorDescriptor, ArchivePluginExtractedFile,
    ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessRequest,
    ArchivePluginProcessResponse, ArchivePluginStatus, PluginDescriptor, ProviderDescriptor,
    SDK_VERSION,
};
use sevenz_turbo::hooks as sevenz_hooks;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use unrar_rs::hooks::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};
use unrar_rs::{RarArchive, RarError};

mod par2;
mod split_volumes;

wit_bindgen::generate!({
    world: "archive-extractor",
    path: "wit",
});

use crate::scryer::archive::crypto as host_crypto;

pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 20_000;
pub(crate) const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024 * 1024;
const MAX_XZ_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_XZ_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
/// XZ decoder memory ceiling.
///
/// This is not a size budget — it is the dictionary the *producer* chose,
/// recorded in each block header. `xz -9` (and `-9e`) select a 64 MiB
/// dictionary, so a ceiling at or just under 64 MiB rejects every
/// maximum-preset stream regardless of how small the payload is: a 1.2 KiB
/// subtitle fails the same way a gigabyte would. 192 MiB clears the largest
/// preset xz(1) can emit with room for hand-tuned dictionaries, and stays far
/// below `DEFAULT_ARCHIVE_MEMORY_CAP_BYTES` so an abusive header still fails as
/// a diagnosable plugin error instead of a host OOM trap.
const MAX_XZ_DECODER_MEMORY_BYTES: u64 = 192 * 1024 * 1024;

/// This crate's implementation of `scryer:archive/archive-extractor@1.1.0`.
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

/// Point unrar-rs's bulk AES-CBC and CRC-32 delegation, sevenz-turbo's bulk
/// AES-CBC decrypt, and lzma-turbo's bulk CRC-32 and CRC-64/XZ at the world's
/// `crypto` import.
///
/// lzma-turbo's SHA-256 hooks are never called: `crypto-host` is off, and the
/// xz SHA-256 check stays on the in-guest RustCrypto backend.
///
/// All three crates are transport-agnostic — they hold plain `fn` pointers and know
/// nothing about WIT — so this adapter is the whole seam between them and the
/// component ABI. It runs at the top of every `process` because the host
/// instantiates the component once per invocation.
///
/// A length rejection from the host is a contract violation rather than a
/// recoverable condition: this plugin only ever passes 16/32-byte keys, 16-byte
/// IVs, and block-aligned buffers. The error is handed back to the calling
/// crate, which panics naming the offending status.
fn install_crypto_hooks() {
    fn rar_aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
        host_crypto::aes_cbc_decrypt(key, iv, data).map_err(|error| match error {
            host_crypto::AesError::BadKeyLength => HostAesError::BadKeyLength,
            host_crypto::AesError::BadBlockLength => HostAesError::BadBlockLength,
            host_crypto::AesError::BadIvLength => HostAesError::BadIvLength,
        })
    }

    fn sevenz_aes_cbc_decrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>, sevenz_hooks::HostAesError> {
        host_crypto::aes_cbc_decrypt(key, iv, data).map_err(|error| match error {
            host_crypto::AesError::BadKeyLength => sevenz_hooks::HostAesError::BadKeyLength,
            host_crypto::AesError::BadBlockLength => sevenz_hooks::HostAesError::BadBlockLength,
            host_crypto::AesError::BadIvLength => sevenz_hooks::HostAesError::BadIvLength,
        })
    }

    fn crc32(seed: u32, data: &[u8]) -> u32 {
        host_crypto::crc32(seed, data)
    }

    /// lzma-turbo's hook resumes in the finalized domain with seed 0 starting
    /// a stream. The host's `crc` resumes from `some(previous)` the same way,
    /// and CRC-64/XZ's initial value equals its final XOR, so `some(0)` starts
    /// a stream there too.
    fn crc64_xz(seed: u64, data: &[u8]) -> u64 {
        match host_crypto::crc(host_crypto::CrcAlgorithm::Crc64Xz, Some(seed), data) {
            Ok(checksum) => checksum,
            // A 64-bit algorithm has no out-of-range seed.
            Err(host_crypto::CrcError::SeedOutOfRange) => {
                unreachable!("host rejected a CRC-64/XZ seed as out of range")
            }
        }
    }

    install_host_crypto_hooks(HostCryptoHooks {
        aes_cbc_decrypt: rar_aes_cbc_decrypt,
        crc32,
    });
    sevenz_hooks::install_host_crypto_hooks(sevenz_hooks::HostCryptoHooks {
        aes_cbc_decrypt: sevenz_aes_cbc_decrypt,
    });

    fn sha256_not_delegated() -> ! {
        unreachable!("lzma-turbo `crypto-host` is off; SHA-256 is computed in the guest")
    }
    install_host_hash_hooks(HostHashHooks::new(
        crc32,
        crc64_xz,
        || sha256_not_delegated(),
        |_: HostSha256Handle| sha256_not_delegated(),
        |_: HostSha256Handle, _: &[u8]| sha256_not_delegated(),
        |_: HostSha256Handle| sha256_not_delegated(),
        |_: HostSha256Handle| sha256_not_delegated(),
    ));
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
        ArchivePluginFormat::SevenZip | ArchivePluginFormat::Zip => {
            extract_seekable_archive(archive_path, output_dir, format, password)
        }
        ArchivePluginFormat::Xz => extract_xz(archive_path, output_dir, password),
    }
}

/// 7z and ZIP read through one seekable stream, which is what lets a
/// byte-split set (`name.7z.001`, `name.zip.001`, `name.001`) extract as the
/// single archive it was cut from.
fn extract_seekable_archive(
    archive_path: &Path,
    output_dir: &Path,
    format: ArchivePluginFormat,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let (source, format) = match split_volumes::open_archive(archive_path, format) {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    match format {
        ArchivePluginFormat::Zip => extract_zip(source, output_dir, password),
        _ => extract_sevenz(source, output_dir, password),
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
    let mut decoder = XzReader::new(input).with_memory_limit(decoder_memory_limit);
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
            let code = xz_failure_code(&error);
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

/// Classify a decode failure so the operator sees which ceiling stopped it.
///
/// The memory limit is the one that does not scale with the payload: it
/// reports the dictionary the *producer* chose, so a kilobyte and a gigabyte
/// fail identically. Folding it into the generic `extract_xz` code made that
/// indistinguishable from a corrupt stream.
fn xz_failure_code(error: &io::Error) -> &'static str {
    let memlimit = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<XzError>())
        .is_some_and(|inner| matches!(inner.kind, XzErrorKind::MemoryLimit { .. }));
    if memlimit {
        return "decoder_memory_too_large";
    }
    if error.kind() == io::ErrorKind::InvalidData && error.to_string().contains("configured limit")
    {
        return "expanded_too_large";
    }
    "extract_xz"
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
            let wrong_key = is_wrong_key_symptom(&error) && rar_headers_are_encrypted(archive_path);
            let mut response = rar_error_response("open_rar", "failed to read RAR archive", error);
            if wrong_key {
                response.status = ArchivePluginStatus::PasswordInvalid;
            }
            Box::new(response)
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

/// Whether the archive's headers are encrypted, asked by opening it without
/// a password: that refuses a header-encrypted archive up front, where a
/// wrong password only surfaces as headers that decrypt to garbage.
fn rar_headers_are_encrypted(archive_path: &Path) -> bool {
    fs::File::open(archive_path)
        .is_ok_and(|file| matches!(RarArchive::open(file), Err(RarError::EncryptedArchive)))
}

/// The failures a wrong key produces on encrypted RAR data.
///
/// RAR5 checks the password against a stored check value and says so
/// directly. RAR4 stores none: a wrong key decrypts to garbage that fails a
/// header CRC, a data CRC, or the decompressor's own sanity checks. Only
/// meaningful once it is known that a password was supplied and that what
/// failed was encrypted — on plain data these are ordinary corruption.
fn is_wrong_key_symptom(error: &RarError) -> bool {
    matches!(
        error,
        RarError::HeaderCrcMismatch { .. }
            | RarError::DataCrcMismatch { .. }
            | RarError::Blake2Mismatch { .. }
            | RarError::CorruptArchive { .. }
            | RarError::TruncatedHeader { .. }
            | RarError::TruncatedData { .. }
            | RarError::InvalidVint { .. }
            | RarError::InvalidHuffmanTable
            | RarError::UnsupportedFilter { .. }
            | RarError::SolidStatePoisoned { .. }
    )
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
    let password = password
        .filter(|password| !password.is_empty())
        .map(str::to_string);

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

        let written = match archive.by_index(member.index).and_then(|entry| {
            let entry = match &password {
                Some(password) => entry.with_password(password.clone()),
                None => entry,
            };
            entry.unpack_to(&destination)
        }) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                let wrong_key =
                    password.is_some() && info.is_encrypted && is_wrong_key_symptom(&error);
                let mut response =
                    rar_error_response("extract_rar", "failed to extract RAR member", error);
                if wrong_key {
                    response.status = ArchivePluginStatus::PasswordInvalid;
                }
                return response;
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

fn extract_zip<R: Read + Seek>(
    source: R,
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    // Encryption is per entry: a password is only ever applied to an entry
    // flagged as encrypted (ZipCrypto or WinZip AES), and the zip crate reads
    // every other entry as plaintext whatever was supplied.
    let password = password.filter(|password| !password.is_empty());
    let mut archive = match zip::ZipArchive::new(source) {
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
        let entry = match password {
            Some(password) => archive.by_index_decrypt(index, password.as_bytes()),
            None => archive.by_index(index),
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(zip::result::ZipError::UnsupportedArchive(message))
                if message == zip::result::ZipError::PASSWORD_REQUIRED =>
            {
                return ArchivePluginProcessResponse {
                    status: ArchivePluginStatus::PasswordRequired,
                    error_code: Some("read_entry".to_string()),
                    message: Some("ZIP archive contains an encrypted entry".to_string()),
                    ..empty_response()
                };
            }
            // The password check stored with the entry (WinZip AES's
            // verifier, or ZipCrypto's check byte) rejected the key.
            Err(zip::result::ZipError::InvalidPassword) => {
                return ArchivePluginProcessResponse {
                    status: ArchivePluginStatus::PasswordInvalid,
                    error_code: Some("read_entry".to_string()),
                    message: Some("wrong password for an encrypted ZIP entry".to_string()),
                    ..empty_response()
                };
            }
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
        let encrypted = entry.encrypted();
        let mut entry = ReadOutcome::new(entry);
        let written = match copy_limited(&mut entry, &mut output, copy_limit) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                let mut response =
                    failed_response("extract_file", "failed to extract ZIP entry", error);
                // ZipCrypto's check is one byte, so one wrong key in 256 gets
                // past it and only fails the CRC (or inflating the garbage);
                // WinZip AES fails its authentication code the same way. A
                // failure while reading an encrypted entry the caller supplied
                // a key for is that key being wrong.
                if password.is_some() && encrypted && entry.failed {
                    response.status = ArchivePluginStatus::PasswordInvalid;
                }
                return response;
            }
        };
        let entry = entry.into_inner();
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

fn extract_sevenz<R: Read + Seek>(
    source: R,
    output_dir: &Path,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    let password_value = match password.filter(|password| !password.is_empty()) {
        Some(password) => sevenz_turbo::Password::from(password),
        None => sevenz_turbo::Password::empty(),
    };
    let mut archive = match sevenz_turbo::ArchiveReader::new(source, password_value) {
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

    let encrypted = archive.archive().blocks.iter().any(|block| {
        block
            .coders
            .iter()
            .any(|coder| coder.encoder_method_id() == sevenz_turbo::EncoderMethod::ID_AES256_SHA256)
    });
    let mut files = Vec::new();
    let mut actual_expanded_bytes = 0_u64;
    let mut output_paths = HashSet::new();
    // Where a failure came from: this callback's own checks and writes, the
    // entry's decoded bytes, or the decoder between entries.
    let mut in_callback = false;
    let mut entry_read_failed = false;
    let extraction = archive.for_each_entries(|entry, entry_reader| {
        in_callback = true;
        let relative_path = safe_archive_relative_path(entry.name())
            .map_err(|response| sevenz_error_from_message(response.message.as_deref()))?;
        let destination = output_root.join(&relative_path);
        if entry.is_directory() {
            fs::create_dir_all(&destination)?;
            in_callback = false;
            return Ok(true);
        }
        record_output_file_path(&mut output_paths, &relative_path)
            .map_err(|response| sevenz_error_from_message(response.message.as_deref()))?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&destination)?;
        let copy_limit = MAX_ARCHIVE_EXPANDED_BYTES.saturating_sub(actual_expanded_bytes);
        let mut entry_reader = ReadOutcome::new(entry_reader);
        let written = match copy_limited(&mut entry_reader, &mut output, copy_limit) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&destination);
                entry_read_failed = entry_reader.failed;
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
        in_callback = false;
        Ok(true)
    });

    if let Err(error) = extraction {
        // 7z's AES has no password check value either. A wrong key decrypts
        // to garbage that fails to decode or fails its CRC, and only that
        // failure says anything: when the key was supplied for an encrypted
        // archive, it is the key that is wrong.
        let decode_failed = entry_read_failed || !in_callback;
        let wrong_key = password.is_some_and(|password| !password.is_empty())
            && encrypted
            && decode_failed
            && is_sevenz_wrong_key_symptom(&error);
        let mut response = sevenz_error_response("extract_7z", error, password);
        if wrong_key {
            response.status = ArchivePluginStatus::PasswordInvalid;
        }
        return response;
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
    for (index, volume_path) in collect_rar_volume_paths(source_dir, archive_path)? {
        let volume_file = fs::File::open(&volume_path)?;
        archive.add_volume(index, Box::new(volume_file))?;
    }

    Ok(())
}

/// The later volumes of the primary archive's own set, each with its volume
/// index relative to the primary (which is volume 0).
///
/// A directory can hold more than one RAR set, so a sibling only belongs when
/// it has the primary's set name (ignoring ASCII case) under the same naming
/// scheme: `name.partN.rar`, or the legacy `name.rar`, `name.r00`…`name.r99`,
/// `name.s00`…, which is what RAR continues with once `.r99` is used up. The
/// index comes from the name rather than the listing order, so a missing
/// volume leaves a gap the extractor reports instead of shifting every later
/// volume down by one.
fn collect_rar_volume_paths(
    source_dir: &Path,
    archive_path: &Path,
) -> Result<Vec<(usize, PathBuf)>, RarError> {
    let archive_file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let named_volume = rar_volume_name(&archive_file_name);
    let primary_is_a_volume_name = named_volume.is_some();
    let primary = named_volume.unwrap_or_else(|| {
        // An archive under a name that is not a RAR volume name at all still
        // anchors a legacy set by its stem: `name.bin` pairs with `name.r00`.
        let stem = archive_file_name
            .rsplit_once('.')
            .map_or(archive_file_name.as_str(), |(stem, _)| stem);
        RarVolumeName {
            modern: false,
            set_name: stem.to_string(),
            index: 0,
        }
    });

    let mut volumes = std::collections::BTreeMap::new();
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == archive_path || !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(volume) = rar_volume_name(&file_name.to_ascii_lowercase()) else {
            continue;
        };
        if volume.modern != primary.modern || volume.set_name != primary.set_name {
            continue;
        }
        if volume.index == primary.index && !primary_is_a_volume_name {
            continue;
        }
        if volume.index == primary.index || volumes.contains_key(&volume.index) {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "more than one file is RAR volume {} of this set",
                    volume.index + 1
                ),
            )));
        }
        // A volume before the one the host named cannot be attached after it.
        if let Some(index) = volume.index.checked_sub(primary.index) {
            volumes.insert(index, path);
        }
    }

    Ok(volumes.into_iter().collect())
}

struct RarVolumeName {
    /// `name.partN.rar`, as opposed to the legacy `name.rar` / `name.rNN`.
    modern: bool,
    set_name: String,
    index: usize,
}

/// Parse a lowercased file name as a RAR volume name, through the same
/// scheme the PAR2 target resolution uses.
fn rar_volume_name(file_name: &str) -> Option<RarVolumeName> {
    let (set_name, index) = par2::rar_volume_info(file_name)?;
    let modern = file_name
        .strip_suffix(".rar")
        .is_some_and(|stem| stem.len() > set_name.len());
    Some(RarVolumeName {
        modern,
        set_name,
        index,
    })
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

/// A reader that remembers whether it failed, so a failed copy can tell the
/// source's own decode or integrity failure from the copy's size limit or a
/// failed write.
struct ReadOutcome<R> {
    inner: R,
    failed: bool,
}

impl<R> ReadOutcome<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            failed: false,
        }
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for ReadOutcome<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(buf);
        self.failed |= result.is_err();
        result
    }
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
    error: sevenz_turbo::Error,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    use sevenz_turbo::{BlockErrorKind, Error as SevenzError};

    let unsupported_method = matches!(
        error,
        SevenzError::UnsupportedCompressionMethod(_)
            | SevenzError::Unsupported(_)
            | SevenzError::ExternalUnsupported
            | SevenzError::BlockDecode {
                kind: BlockErrorKind::UnsupportedMethod,
                ..
            }
    );
    let password_error = matches!(
        error,
        SevenzError::PasswordRequired
            | SevenzError::MaybeBadPassword(_)
            | SevenzError::BlockDecode {
                kind: BlockErrorKind::Password,
                ..
            }
    );
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let (status, code, public_message) = if unsupported_method
        || lower.contains("unsupported")
        || lower.contains("zstd")
        || lower.contains("method")
    {
        (
                ArchivePluginStatus::Failed,
                "unsupported_7z_method",
                "This 7z archive uses a compression method the Archive Extraction plugin does not support yet.".to_string(),
            )
    } else if password_error || lower.contains("password") || lower.contains("encrypted") {
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

/// Decode and integrity failures, as opposed to a method this build lacks.
fn is_sevenz_wrong_key_symptom(error: &sevenz_turbo::Error) -> bool {
    use sevenz_turbo::{BlockErrorKind, Error as SevenzError};

    match error {
        SevenzError::BlockDecode { kind, .. } => !matches!(kind, BlockErrorKind::UnsupportedMethod),
        SevenzError::ChecksumVerificationFailed
        | SevenzError::MaybeBadPassword(_)
        | SevenzError::Io(..)
        | SevenzError::Other(_) => true,
        _ => false,
    }
}

fn sevenz_error_from_message(message: Option<&str>) -> sevenz_turbo::Error {
    sevenz_turbo::Error::Other(Cow::Owned(
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
    use lzma_turbo::{LzmaEncProps, XzWriter};

    /// `xz -6` (the default preset) and `xz -9` dictionaries. Set explicitly
    /// because the SDK encoder's own level table maps levels to different
    /// dictionaries than xz(1)'s presets do.
    const XZ_DEFAULT_PRESET_DICT: u32 = 8 * 1024 * 1024;
    const XZ_MAX_PRESET_DICT: u32 = 64 * 1024 * 1024;

    fn write_xz_fixture_with(path: &Path, content: &[u8], props: &LzmaEncProps) {
        let file = fs::File::create(path).expect("create XZ fixture");
        let mut encoder = XzWriter::new(file, props).expect("configure XZ fixture encoder");
        encoder.write_all(content).expect("compress XZ fixture");
        encoder.finish().expect("finish XZ fixture");
    }

    fn write_xz_fixture(path: &Path, content: &[u8]) {
        write_xz_fixture_with(
            path,
            content,
            &LzmaEncProps::new()
                .with_level(6)
                .with_dict_size(XZ_DEFAULT_PRESET_DICT),
        );
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

    /// `xz -9` is the canonical "compress it as hard as you can" invocation and
    /// selects a 64 MiB dictionary. The ceiling used to sit at exactly 64 MiB,
    /// so every maximum-preset stream was rejected no matter how small — a
    /// 1.2 KiB subtitle included.
    ///
    /// The fixture carries a genuine 64 MiB dictionary property. lzma-turbo's
    /// writer also records the block's uncompressed size, which caps what the
    /// decoder charges; a single-threaded `xz -9` omits it, and then the whole
    /// 64 MiB is charged against [`MAX_XZ_DECODER_MEMORY_BYTES`]. A 64 MiB
    /// dictionary costs hundreds of MiB in the encoder's match finder even for
    /// a few bytes of input; that transient allocation is the price of a real
    /// preset-9 stream rather than an asserted number.
    #[test]
    fn xz_stream_at_the_largest_preset_extracts_under_shipped_limits() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitle.srt.xz");
        let content = b"1\n00:00:01,000 --> 00:00:02,000\nmaximum preset\n";
        write_xz_fixture_with(
            &input_path,
            content,
            &LzmaEncProps::new()
                .with_level(9)
                .with_dict_size(XZ_MAX_PRESET_DICT),
        );

        let response = extract_xz(&input_path, output.path(), None);

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "preset-9 XZ must decode: {:?}",
            response.message
        );
        assert_eq!(
            fs::read(output.path().join("subtitle.srt")).unwrap(),
            content
        );
    }

    #[test]
    fn xz_stream_over_the_decoder_memory_ceiling_reports_its_own_code() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let input_path = source.path().join("subtitle.srt.xz");
        // A block's dictionary is only charged up to its declared uncompressed
        // size, so the payload has to outgrow the ceiling as well: 2 MiB under
        // an 8 MiB (preset 6) dictionary needs 2 MiB, which a 1 MiB ceiling
        // refuses at the block header, before any payload decodes.
        write_xz_fixture(&input_path, &b"subtitle\n".repeat(2 * 1024 * 1024 / 9 + 1));

        let response = extract_xz_with_limits(
            &input_path,
            output.path(),
            None,
            MAX_XZ_COMPRESSED_BYTES,
            MAX_XZ_EXPANDED_BYTES,
            1024 * 1024,
        );

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(
            response.error_code.as_deref(),
            Some("decoder_memory_too_large"),
            "a dictionary the ceiling cannot admit must not read as a corrupt stream: {:?}",
            response.message
        );
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

    fn touch_all(dir: &Path, names: &[&str]) {
        for name in names {
            fs::write(dir.join(name), b"").expect("write volume stub");
        }
    }

    fn collected(dir: &Path, primary: &str) -> Vec<(usize, String)> {
        collect_rar_volume_paths(dir, &dir.join(primary))
            .expect("collect RAR volumes")
            .into_iter()
            .map(|(index, path)| {
                (
                    index,
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn rar_volumes_of_another_set_in_the_same_directory_are_not_attached() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &[
                "Example.Show.S01E01.part1.rar",
                "Example.Show.S01E01.part2.rar",
                "EXAMPLE.SHOW.S01E01.PART3.RAR",
                "Example.Show.S01E02.part1.rar",
                "Example.Show.S01E02.part2.rar",
                "Example.Show.S01E01.rar",
                "Example.Show.S01E01.r00",
                "Example.Show.S01E02.r00",
                "Example.Show.S01E01.nfo",
            ],
        );

        assert_eq!(
            collected(dir.path(), "Example.Show.S01E01.part1.rar"),
            [
                (1, "Example.Show.S01E01.part2.rar".to_string()),
                (2, "EXAMPLE.SHOW.S01E01.PART3.RAR".to_string()),
            ]
        );
        assert_eq!(
            collected(dir.path(), "Example.Show.S01E01.rar"),
            [(1, "Example.Show.S01E01.r00".to_string())]
        );
    }

    #[test]
    fn rar_part_volumes_are_ordered_by_number_not_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let names = (1..=11)
            .map(|part| format!("Example.Show.S01E01.part{part}.rar"))
            .collect::<Vec<_>>();
        touch_all(
            dir.path(),
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let volumes = collected(dir.path(), "Example.Show.S01E01.part1.rar");

        assert_eq!(
            volumes,
            (2..=11)
                .map(|part| (part - 1, format!("Example.Show.S01E01.part{part}.rar")))
                .collect::<Vec<_>>()
        );
    }

    /// Old-style naming runs `.rar`, `.r00`…`.r99`, then carries on at `.s00`.
    #[test]
    fn legacy_rar_volumes_continue_past_r99_into_s00() {
        let dir = tempfile::tempdir().unwrap();
        let mut names = vec!["Example.Show.S01E01.rar".to_string()];
        names.extend((0..100).map(|number| format!("Example.Show.S01E01.r{number:02}")));
        names.extend((0..3).map(|number| format!("Example.Show.S01E01.s{number:02}")));
        touch_all(
            dir.path(),
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let volumes = collected(dir.path(), "Example.Show.S01E01.rar");

        assert_eq!(volumes.len(), 103);
        assert_eq!(volumes[0], (1, "Example.Show.S01E01.r00".to_string()));
        assert_eq!(volumes[99], (100, "Example.Show.S01E01.r99".to_string()));
        assert_eq!(volumes[100], (101, "Example.Show.S01E01.s00".to_string()));
        assert_eq!(volumes[102], (103, "Example.Show.S01E01.s02".to_string()));
    }

    #[test]
    fn a_missing_rar_volume_leaves_a_gap_instead_of_renumbering() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &[
                "Example.Show.S01E01.rar",
                "Example.Show.S01E01.r00",
                "Example.Show.S01E01.r02",
            ],
        );

        assert_eq!(
            collected(dir.path(), "Example.Show.S01E01.rar"),
            [
                (1, "Example.Show.S01E01.r00".to_string()),
                (3, "Example.Show.S01E01.r02".to_string()),
            ]
        );
    }

    #[test]
    fn two_files_claiming_one_rar_volume_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        touch_all(
            dir.path(),
            &[
                "Example.Show.S01E01.part1.rar",
                "Example.Show.S01E01.part2.rar",
                "Example.Show.S01E01.part02.rar",
            ],
        );

        assert!(
            collect_rar_volume_paths(
                dir.path(),
                &dir.path().join("Example.Show.S01E01.part1.rar")
            )
            .is_err()
        );
    }

    /// The real multi-volume fixture, next to a second copy of itself under
    /// another set name: extracting one set must not attach the other's
    /// volumes, which carry the same volume numbers.
    #[test]
    fn a_multivolume_rar_extracts_beside_another_set() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/par2");
        for part in 1..=6 {
            let fixture = fixtures.join(format!("fixture_rar5_lz_plain.part{part}.rar"));
            fs::copy(
                &fixture,
                source
                    .path()
                    .join(format!("Example.Show.S01E01.part{part}.rar")),
            )
            .unwrap();
            fs::copy(
                &fixture,
                source
                    .path()
                    .join(format!("Example.Show.S01E02.part{part}.rar")),
            )
            .unwrap();
        }

        let response = extract_rar(
            &source.path().join("Example.Show.S01E01.part1.rar"),
            output.path(),
            None,
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(response.expanded_bytes, Some(1_109_271));
    }

    /// A password the host passes along for a download is not a claim that
    /// the archive is encrypted: plain entries extract regardless.
    #[test]
    fn a_plain_zip_extracts_when_a_password_is_supplied() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let archive = source.path().join("Example.Show.S01E01.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file(
            "Example.Show.S01E01.srt",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"1\n00:00:01,000 --> 00:00:02,000\nplain\n")
            .unwrap();
        zip.finish().unwrap();

        let response = extract_archive(
            archive.to_str().unwrap(),
            output.path().to_str().unwrap(),
            ArchivePluginFormat::Zip,
            Some("Example-Password"),
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(
            fs::read(output.path().join("Example.Show.S01E01.srt")).unwrap(),
            b"1\n00:00:01,000 --> 00:00:02,000\nplain\n"
        );
    }

    fn rar_fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rar")
            .join(name)
    }

    /// RAR4 stores no password check value, so a wrong key only shows up as
    /// headers or data that fail their CRC. With a password supplied and
    /// encryption present, that is a wrong password, not a damaged archive.
    #[test]
    fn rar4_wrong_password_is_password_invalid() {
        for (fixture, password) in [
            // Data-only encryption, stored.
            ("rar4_enc_store.rar", "testpass123"),
            // Data-only encryption, compressed: the garbage fails to unpack.
            ("rar4_enc_lz.rar", "testpass123"),
            // Header encryption: the headers themselves fail to decrypt.
            ("rar4_hp_store.rar", "secretpass"),
        ] {
            let source = tempfile::tempdir().unwrap();
            fs::copy(rar_fixture(fixture), source.path().join(fixture)).unwrap();
            let archive = source.path().join(fixture);

            let wrong_output = tempfile::tempdir().unwrap();
            let wrong = extract_rar(&archive, wrong_output.path(), Some("not-the-password"));
            assert_eq!(
                wrong.status,
                ArchivePluginStatus::PasswordInvalid,
                "{fixture}: {:?}",
                wrong.message
            );
            assert!(wrong.files.is_empty(), "{fixture}");
            assert_eq!(
                fs::read_dir(wrong_output.path()).unwrap().count(),
                0,
                "{fixture}: a wrong password must not leave output behind"
            );

            let missing = extract_rar(&archive, tempfile::tempdir().unwrap().path(), None);
            assert_eq!(
                missing.status,
                ArchivePluginStatus::PasswordRequired,
                "{fixture}: {:?}",
                missing.message
            );

            let right_output = tempfile::tempdir().unwrap();
            let right = extract_rar(&archive, right_output.path(), Some(password));
            assert_eq!(
                right.status,
                ArchivePluginStatus::Ok,
                "{fixture}: {:?}",
                right.message
            );
        }
    }

    /// As with ZIP, a password handed along for a download does not make a
    /// plain 7z encrypted: it is ignored and the archive extracts.
    #[test]
    fn a_plain_7z_extracts_when_a_password_is_supplied() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let archive = source.path().join("Example.Show.S01E01.7z");
        let mut writer = sevenz_turbo::ArchiveWriter::create(&archive).unwrap();
        writer
            .push_archive_entry(
                sevenz_turbo::ArchiveEntry::new_file("Example.Show.S01E01.srt"),
                Some(b"1\n00:00:01,000 --> 00:00:02,000\nplain\n".as_slice()),
            )
            .unwrap();
        writer.finish().unwrap();

        let response = extract_archive(
            archive.to_str().unwrap(),
            output.path().to_str().unwrap(),
            ArchivePluginFormat::SevenZip,
            Some("Example-Password"),
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(
            fs::read(output.path().join("Example.Show.S01E01.srt")).unwrap(),
            b"1\n00:00:01,000 --> 00:00:02,000\nplain\n"
        );
    }

    #[test]
    fn sevenz_unsupported_method_maps_to_structured_error() {
        let response = sevenz_error_response(
            "extract_7z",
            sevenz_turbo::Error::Other(Cow::Borrowed("unsupported compression method zstd")),
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

/// Byte-split 7z and ZIP sets, end to end through [`extract_archive`].
#[cfg(test)]
mod split_volume_tests {
    use super::*;
    use std::io::Cursor;

    fn payload(len: usize) -> Vec<u8> {
        let mut state = 0x5EED_u64 | 0x9E37_79B9_7F4A_7C15;
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

    fn sevenz_bytes(entry: &str, contents: &[u8]) -> Vec<u8> {
        let mut archive =
            sevenz_turbo::ArchiveWriter::new(Cursor::new(Vec::new())).expect("create 7z fixture");
        archive
            .push_archive_entry(sevenz_turbo::ArchiveEntry::new_file(entry), Some(contents))
            .expect("write 7z fixture entry");
        archive.finish().expect("finish 7z fixture").into_inner()
    }

    fn zip_bytes(entry: &str, contents: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(entry, zip::write::SimpleFileOptions::default())
            .expect("start ZIP entry");
        zip.write_all(contents).expect("write ZIP payload");
        zip.finish().expect("finish ZIP fixture").into_inner()
    }

    /// Cut `bytes` into `parts` roughly equal files named `{set_name}.001`…
    fn split_into(dir: &Path, set_name: &str, bytes: &[u8], parts: usize) -> Vec<PathBuf> {
        let chunk = bytes.len().div_ceil(parts);
        bytes
            .chunks(chunk)
            .enumerate()
            .map(|(index, part)| {
                let path = dir.join(format!("{set_name}.{:03}", index + 1));
                fs::write(&path, part).expect("write split part");
                path
            })
            .collect()
    }

    fn extract(
        source: &Path,
        output: &Path,
        archive: &str,
        format: ArchivePluginFormat,
    ) -> ArchivePluginProcessResponse {
        extract_archive(
            source.join(archive).to_str().unwrap(),
            output.to_str().unwrap(),
            format,
            None,
        )
    }

    #[test]
    fn a_split_7z_extracts_as_one_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let contents = payload(96 * 1024);
        split_into(
            source.path(),
            "Example.Show.S01E01.7z",
            &sevenz_bytes("Example.Show.S01E01.mkv", &contents),
            3,
        );

        let response = extract(
            source.path(),
            output.path(),
            "Example.Show.S01E01.7z.001",
            ArchivePluginFormat::SevenZip,
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(
            fs::read(output.path().join("Example.Show.S01E01.mkv")).unwrap(),
            contents
        );
    }

    #[test]
    fn a_split_zip_extracts_as_one_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let contents = payload(96 * 1024);
        split_into(
            source.path(),
            "Example.Show.S01E01.zip",
            &zip_bytes("Example.Show.S01E01.mkv", &contents),
            4,
        );

        let response = extract(
            source.path(),
            output.path(),
            "Example.Show.S01E01.zip.001",
            ArchivePluginFormat::Zip,
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(
            fs::read(output.path().join("Example.Show.S01E01.mkv")).unwrap(),
            contents
        );
    }

    /// A bare `name.001` says nothing about its contents, so the joined
    /// stream's signature wins over whichever format the host guessed.
    #[test]
    fn a_bare_numbered_set_extracts_by_its_signature() {
        for (bytes, host_format) in [
            (
                sevenz_bytes("Example.Show.S01E01.mkv", &payload(40_000)),
                ArchivePluginFormat::Zip,
            ),
            (
                zip_bytes("Example.Show.S01E01.mkv", &payload(40_000)),
                ArchivePluginFormat::SevenZip,
            ),
        ] {
            let source = tempfile::tempdir().unwrap();
            let output = tempfile::tempdir().unwrap();
            split_into(source.path(), "Example.Show.S01E01", &bytes, 2);

            let response = extract(
                source.path(),
                output.path(),
                "Example.Show.S01E01.001",
                host_format,
            );

            assert_eq!(
                response.status,
                ArchivePluginStatus::Ok,
                "{:?}",
                response.message
            );
            assert_eq!(
                fs::read(output.path().join("Example.Show.S01E01.mkv")).unwrap(),
                payload(40_000)
            );
        }
    }

    #[test]
    fn a_split_set_with_a_missing_middle_volume_names_it() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let parts = split_into(
            source.path(),
            "Example.Show.S01E01.7z",
            &sevenz_bytes("Example.Show.S01E01.mkv", &payload(96 * 1024)),
            3,
        );
        fs::remove_file(&parts[1]).unwrap();

        let response = extract(
            source.path(),
            output.path(),
            "Example.Show.S01E01.7z.001",
            ArchivePluginFormat::SevenZip,
        );

        assert_eq!(response.status, ArchivePluginStatus::Failed);
        assert_eq!(response.error_code.as_deref(), Some("missing_volume"));
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("'Example.Show.S01E01.7z.002'")),
            "{:?}",
            response.message
        );
        assert!(!output.path().join("Example.Show.S01E01.mkv").exists());
    }

    /// A recovery set over the parts of a split archive protects an archive,
    /// not plain media: the damaged part is repaired in the scratch copy and
    /// the joined parts are extracted from there.
    #[test]
    fn a_damaged_split_set_is_repaired_then_joined() {
        use par2_rs::{BlockSizing, Par2Creator, Par2CreatorOptions, RecoveryAmount};
        use std::io::{Seek, SeekFrom};

        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let contents = payload(96 * 1024);
        let parts = split_into(
            source.path(),
            "Example.Show.S01E01.7z",
            &sevenz_bytes("Example.Show.S01E01.mkv", &contents),
            3,
        );
        let mut options = Par2CreatorOptions::with_output(
            source.path().join("recovery"),
            Some(source.path().to_path_buf()),
            parts.clone(),
        );
        options.block_sizing = BlockSizing::Bytes(4_096);
        options.recovery_amount = RecoveryAmount::Count(4);
        let creator = Par2Creator::new(options);
        let plan = creator.plan().expect("plan PAR2 creation");
        creator.create(&plan).expect("create PAR2 recovery set");

        let mut damaged = fs::OpenOptions::new().write(true).open(&parts[1]).unwrap();
        damaged.seek(SeekFrom::Start(4_096)).unwrap();
        damaged.write_all(&[0xA5; 4_096]).unwrap();
        drop(damaged);

        let response = extract(
            source.path(),
            output.path(),
            "Example.Show.S01E01.7z.001",
            ArchivePluginFormat::SevenZip,
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{:?}",
            response.message
        );
        assert_eq!(
            response
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["Example.Show.S01E01.mkv"]
        );
        assert_eq!(
            fs::read(output.path().join("Example.Show.S01E01.mkv")).unwrap(),
            contents
        );
    }
}

/// Encrypted ZIP and 7z entries, end to end through [`extract_archive`].
#[cfg(test)]
mod encryption_tests {
    use super::*;
    use sevenz_turbo::EncoderConfiguration;
    use sevenz_turbo::encoder_options::{
        AesEncoderOptions, Bzip2Options, DeflateOptions, Lzma2Options, LzmaOptions, PpmdOptions,
    };
    use std::io::Cursor;
    use zip::AesMode;
    use zip::unstable::write::FileOptionsExt;

    const PASSWORD: &str = "example-pass-42";
    const NON_ASCII_PASSWORD: &str = "pässwörd-例";
    const ENTRY: &str = "Example.Show.S01E01.mkv";

    /// Half text, half noise: every codec gets something to compress and
    /// something it cannot.
    fn payload() -> Vec<u8> {
        let mut out = b"Example.Show.S01E01 synthetic payload line\n".repeat(700);
        let mut state = 0x5EED_u64 | 0x9E37_79B9_7F4A_7C15;
        while out.len() < 60_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        out
    }

    #[derive(Clone, Copy, Debug)]
    enum ZipCipher {
        Aes(AesMode),
        ZipCrypto,
    }

    const ZIP_CIPHERS: [ZipCipher; 4] = [
        ZipCipher::Aes(AesMode::Aes128),
        ZipCipher::Aes(AesMode::Aes192),
        ZipCipher::Aes(AesMode::Aes256),
        ZipCipher::ZipCrypto,
    ];

    /// `(entry name, contents, cipher)`; `None` stores the entry in the clear.
    fn zip_bytes(entries: &[(&str, &[u8], Option<ZipCipher>)], password: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents, cipher) in entries {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let options = match cipher {
                None => options,
                Some(ZipCipher::Aes(mode)) => options.with_aes_encryption(*mode, password),
                Some(ZipCipher::ZipCrypto) => {
                    options.with_deprecated_encryption(password.as_bytes())
                }
            };
            zip.start_file(*name, options).expect("start ZIP entry");
            zip.write_all(contents).expect("write ZIP payload");
        }
        zip.finish().expect("finish ZIP fixture").into_inner()
    }

    fn sevenz_bytes(codec: EncoderConfiguration, password: &str, encrypt_header: bool) -> Vec<u8> {
        let mut archive =
            sevenz_turbo::ArchiveWriter::new(Cursor::new(Vec::new())).expect("create 7z fixture");
        archive.set_content_methods(vec![
            AesEncoderOptions::new(sevenz_turbo::Password::new(password)).into(),
            codec,
        ]);
        archive.set_encrypt_header(encrypt_header);
        archive
            .push_archive_entry(
                sevenz_turbo::ArchiveEntry::new_file(ENTRY),
                Some(payload().as_slice()),
            )
            .expect("write 7z fixture entry");
        archive.finish().expect("finish 7z fixture").into_inner()
    }

    fn extract(
        bytes: &[u8],
        name: &str,
        format: ArchivePluginFormat,
        password: Option<&str>,
    ) -> (ArchivePluginProcessResponse, tempfile::TempDir) {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        fs::write(source.path().join(name), bytes).unwrap();
        let response = extract_archive(
            source.path().join(name).to_str().unwrap(),
            output.path().to_str().unwrap(),
            format,
            password,
        );
        (response, output)
    }

    fn assert_extracted(
        response: &ArchivePluginProcessResponse,
        output: &Path,
        expected: &[(&str, &[u8])],
        label: &str,
    ) {
        assert_eq!(
            response.status,
            ArchivePluginStatus::Ok,
            "{label}: {:?}",
            response.message
        );
        assert_eq!(response.files.len(), expected.len(), "{label}");
        for (name, contents) in expected {
            assert_eq!(fs::read(output.join(name)).unwrap(), *contents, "{label}");
        }
    }

    /// Nothing is reported and nothing is left behind, and the reply never
    /// echoes the key it was given.
    fn assert_refused(
        response: &ArchivePluginProcessResponse,
        output: &Path,
        status: ArchivePluginStatus,
        password: Option<&str>,
        label: &str,
    ) {
        assert_eq!(response.status, status, "{label}: {:?}", response.message);
        assert!(response.files.is_empty(), "{label}: {:?}", response.files);
        assert!(
            !output.join(ENTRY).exists(),
            "{label}: partial output left behind"
        );
        if let (Some(password), Some(message)) = (password, response.message.as_deref()) {
            assert!(
                !message.contains(password),
                "{label}: message echoes the key"
            );
        }
    }

    #[test]
    fn encrypted_zip_entries_extract_only_with_the_right_password() {
        let contents = payload();
        for cipher in ZIP_CIPHERS {
            let label = format!("{cipher:?}");
            let bytes = zip_bytes(&[(ENTRY, &contents, Some(cipher))], PASSWORD);

            let (right, output) = extract(
                &bytes,
                "sample.zip",
                ArchivePluginFormat::Zip,
                Some(PASSWORD),
            );
            assert_extracted(&right, output.path(), &[(ENTRY, &contents)], &label);

            let (wrong, output) = extract(
                &bytes,
                "sample.zip",
                ArchivePluginFormat::Zip,
                Some("not-the-password"),
            );
            assert_refused(
                &wrong,
                output.path(),
                ArchivePluginStatus::PasswordInvalid,
                Some("not-the-password"),
                &label,
            );

            let (missing, output) = extract(&bytes, "sample.zip", ArchivePluginFormat::Zip, None);
            assert_refused(
                &missing,
                output.path(),
                ArchivePluginStatus::PasswordRequired,
                None,
                &label,
            );
        }
    }

    /// ZipCrypto checks a key against a single byte, so one wrong key in 256
    /// gets past it; the entry then fails its CRC, and that is reported as the
    /// wrong key it is rather than as a damaged archive.
    #[test]
    fn a_wrong_zipcrypto_key_that_passes_the_check_byte_is_password_invalid() {
        let contents = payload();
        let bytes = zip_bytes(&[(ENTRY, &contents, Some(ZipCipher::ZipCrypto))], PASSWORD);
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let lucky = (0..100_000)
            .map(|attempt| format!("wrong-{attempt}"))
            .find(|candidate| archive.by_index_decrypt(0, candidate.as_bytes()).is_ok())
            .expect("some wrong key passes a one-byte check");

        let (response, output) =
            extract(&bytes, "sample.zip", ArchivePluginFormat::Zip, Some(&lucky));
        assert_refused(
            &response,
            output.path(),
            ArchivePluginStatus::PasswordInvalid,
            Some(&lucky),
            "ZipCrypto check-byte collision",
        );
    }

    #[test]
    fn a_zip_mixing_plain_and_encrypted_entries_extracts_both() {
        let plain = b"Example.Show.S01E01 plain notes\n".as_slice();
        let secret = payload();
        let bytes = zip_bytes(
            &[
                ("Example.Show.S01E01.nfo", plain, None),
                (ENTRY, &secret, Some(ZipCipher::Aes(AesMode::Aes256))),
            ],
            PASSWORD,
        );

        let (right, output) = extract(
            &bytes,
            "sample.zip",
            ArchivePluginFormat::Zip,
            Some(PASSWORD),
        );
        assert_extracted(
            &right,
            output.path(),
            &[("Example.Show.S01E01.nfo", plain), (ENTRY, &secret)],
            "mixed ZIP",
        );

        let (missing, output) = extract(&bytes, "sample.zip", ArchivePluginFormat::Zip, None);
        assert_refused(
            &missing,
            output.path(),
            ArchivePluginStatus::PasswordRequired,
            None,
            "mixed ZIP without a password",
        );
    }

    /// ZIP keys are the password's UTF-8 bytes, as 7-Zip and Info-ZIP on a
    /// UTF-8 system write them.
    #[test]
    fn a_non_ascii_zip_password_extracts() {
        let contents = payload();
        for cipher in [ZipCipher::Aes(AesMode::Aes256), ZipCipher::ZipCrypto] {
            let bytes = zip_bytes(&[(ENTRY, &contents, Some(cipher))], NON_ASCII_PASSWORD);
            let (response, output) = extract(
                &bytes,
                "sample.zip",
                ArchivePluginFormat::Zip,
                Some(NON_ASCII_PASSWORD),
            );
            assert_extracted(
                &response,
                output.path(),
                &[(ENTRY, &contents)],
                &format!("{cipher:?}"),
            );
        }
    }

    /// Both 7z layouts: the header encrypted along with the data, and the
    /// data alone behind a plain header that lists the entries.
    #[test]
    fn encrypted_7z_extracts_only_with_the_right_password() {
        let contents = payload();
        for encrypt_header in [true, false] {
            let label = format!("encrypted header: {encrypt_header}");
            let bytes = sevenz_bytes(Lzma2Options::default().into(), PASSWORD, encrypt_header);

            let (right, output) = extract(
                &bytes,
                "sample.7z",
                ArchivePluginFormat::SevenZip,
                Some(PASSWORD),
            );
            assert_extracted(&right, output.path(), &[(ENTRY, &contents)], &label);

            let (wrong, output) = extract(
                &bytes,
                "sample.7z",
                ArchivePluginFormat::SevenZip,
                Some("not-the-password"),
            );
            assert_refused(
                &wrong,
                output.path(),
                ArchivePluginStatus::PasswordInvalid,
                Some("not-the-password"),
                &label,
            );

            let (missing, output) =
                extract(&bytes, "sample.7z", ArchivePluginFormat::SevenZip, None);
            assert_refused(
                &missing,
                output.path(),
                ArchivePluginStatus::PasswordRequired,
                None,
                &label,
            );
        }
    }

    /// A wrong key fails differently behind each codec — a stored entry only
    /// fails its CRC, the others usually fail to decode first — and each is
    /// still the wrong key.
    #[test]
    fn every_7z_codec_decrypts_and_refuses_a_wrong_key() {
        let contents = payload();
        let codecs: [(&str, EncoderConfiguration); 6] = [
            ("LZMA2", Lzma2Options::default().into()),
            ("LZMA", LzmaOptions::default().into()),
            ("BZip2", Bzip2Options::default().into()),
            ("Deflate", DeflateOptions::default().into()),
            ("PPMd", PpmdOptions::default().into()),
            (
                "stored",
                EncoderConfiguration::new(sevenz_turbo::EncoderMethod::COPY),
            ),
        ];
        for (label, codec) in codecs {
            let bytes = sevenz_bytes(codec, PASSWORD, false);

            let (right, output) = extract(
                &bytes,
                "sample.7z",
                ArchivePluginFormat::SevenZip,
                Some(PASSWORD),
            );
            assert_extracted(&right, output.path(), &[(ENTRY, &contents)], label);

            let (wrong, output) = extract(
                &bytes,
                "sample.7z",
                ArchivePluginFormat::SevenZip,
                Some("not-the-password"),
            );
            assert_refused(
                &wrong,
                output.path(),
                ArchivePluginStatus::PasswordInvalid,
                Some("not-the-password"),
                label,
            );
        }
    }

    /// 7z keys are the password's UTF-16LE code units.
    #[test]
    fn a_non_ascii_7z_password_extracts() {
        let contents = payload();
        for encrypt_header in [true, false] {
            let bytes = sevenz_bytes(
                Lzma2Options::default().into(),
                NON_ASCII_PASSWORD,
                encrypt_header,
            );
            let (response, output) = extract(
                &bytes,
                "sample.7z",
                ArchivePluginFormat::SevenZip,
                Some(NON_ASCII_PASSWORD),
            );
            assert_extracted(
                &response,
                output.path(),
                &[(ENTRY, &contents)],
                &format!("encrypted header: {encrypt_header}"),
            );
        }
    }
}
