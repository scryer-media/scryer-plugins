use std::fs;
use std::io::Write;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use scryer_plugin_sdk::{
    ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessRequest,
    ArchivePluginProcessResponse, ArchivePluginStatus,
};
use wasmtime::{Caller, Engine, Extern, ExternType, Linker, Module, Store, ValType};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

const GUEST_SOURCE_ROOT: &str = "/scryer/source";
const GUEST_OUTPUT_ROOT: &str = "/scryer/output";
const RAR_PASSWORD: &str = "testpass123";

static AES_CALLS: AtomicUsize = AtomicUsize::new(0);
static CRC_CALLS: AtomicUsize = AtomicUsize::new(0);
static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn archive_extraction_release_wasm_conforms_to_host_contract() {
    let wasm_path = archive_plugin_wasm();

    assert_describe_emits_descriptor(&wasm_path);
    assert_frozen_abi(&wasm_path);
    assert_plain_rar4_extracts(&wasm_path);
    assert_rar5_multivolume_extracts(&wasm_path);
    assert_encrypted_rars_use_raw_host_calls(&wasm_path);
    assert_sevenz_extracts(&wasm_path);
    assert_sevenz_rejects_unsafe_paths(&wasm_path);
    assert_sevenz_rejects_duplicate_paths(&wasm_path);
    assert_zip_extracts(&wasm_path);
    assert_zip_path_escape_is_rejected(&wasm_path);
    assert_inspect_is_unsupported(&wasm_path);
    assert_request_path_escape_is_rejected(&wasm_path);
}

/// The command binary has no Extism `scryer_describe` export; instead it emits
/// its `PluginDescriptor` as JSON to stdout when run with the `describe`
/// argument. This is the descriptor path a catalog/packaging step drives via
/// wasmtime for a wasip1 command artifact.
fn assert_describe_emits_descriptor(wasm_path: &Path) {
    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm_path).expect("load archive plugin wasm");
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx: &mut WasiP1Ctx| ctx)
        .expect("add WASI preview1 linker functions");
    register_crypto_host(&mut linker);
    let stdout = MemoryOutputPipe::new(1024 * 1024);
    let wasi = WasiCtxBuilder::new()
        .args(&["archive-extraction", "describe"])
        .stdout(stdout.clone())
        .inherit_stderr()
        .build_p1();
    let mut store = Store::new(&engine, wasi);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate archive plugin");
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .expect("archive plugin should export _start");
    if let Err(error) = start.call(&mut store, ()) {
        if let Some(exit) = error.downcast_ref::<I32Exit>() {
            assert_eq!(exit.0, 0, "describe exited with {}", exit.0);
        } else {
            panic!("describe trapped: {error:?}");
        }
    }
    drop(store);

    let bytes = stdout.contents();
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not emit valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    assert_eq!(
        descriptor.get("id").and_then(|id| id.as_str()),
        Some("archive-extraction"),
        "unexpected descriptor id: {descriptor}"
    );
    let formats = descriptor
        .pointer("/provider/capabilities/formats")
        .and_then(|formats| formats.as_array())
        .unwrap_or_else(|| panic!("descriptor did not include archive formats: {descriptor}"));
    for format in ["rar", "zip", "7z"] {
        assert!(
            formats.iter().any(|value| value.as_str() == Some(format)),
            "descriptor did not advertise {format}: {descriptor}"
        );
    }
}

fn assert_encrypted_rars_use_raw_host_calls(wasm_path: &Path) {
    let source = stage_files(&[
        fixture_path("rar/rar4_enc_store.rar"),
        fixture_path("rar/rar5_enc_lz.rar"),
    ]);
    let before = host_call_counts();
    assert_encrypted_rar4_password_states(wasm_path, source.path());
    assert_encrypted_rar5_password_states(wasm_path, source.path());
    let after = host_call_counts();

    assert!(
        after.aes > before.aes,
        "encrypted RAR fixtures did not call host_aes_cbc_decrypt"
    );
    assert!(
        after.crc > before.crc,
        "encrypted RAR fixtures did not call host_crc32"
    );
}

fn assert_plain_rar4_extracts(wasm_path: &Path) {
    let source = stage_files(&[fixture_path("plain-rar4/rar4_multifile_lz.rar")]);
    let output = tempfile::tempdir().expect("create plain RAR4 output dir");
    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "rar4_multifile_lz.rar",
        ArchivePluginFormat::Rar,
        None,
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "RAR4: {:?}",
        response.message
    );
    assert_eq!(response.files.len(), 3, "RAR4 should extract every member");
    assert_response_files_are_byte_correct(&response, output.path(), "RAR4");
}

fn assert_rar5_multivolume_extracts(wasm_path: &Path) {
    let source = stage_files(&par2_fixture_files());
    let output = tempfile::tempdir().expect("create RAR5 output dir");
    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "fixture_rar5_lz_plain.part1.rar",
        ArchivePluginFormat::Rar,
        None,
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "RAR5: {:?}",
        response.message
    );
    assert_eq!(
        response.files.len(),
        1,
        "RAR5 should produce one assembled member"
    );
    assert_eq!(response.expanded_bytes, Some(1_109_271));
    assert_response_files_are_byte_correct(&response, output.path(), "RAR5");
}

fn assert_encrypted_rar4_password_states(wasm_path: &Path, source: &Path) {
    let archive = "rar4_enc_store.rar";
    let missing_output = tempfile::tempdir().expect("create no-password RAR4 output dir");
    let missing = extract_archive(
        wasm_path,
        source,
        missing_output.path(),
        archive,
        ArchivePluginFormat::Rar,
        None,
    );
    assert_eq!(missing.status, ArchivePluginStatus::PasswordRequired);

    let wrong_output = tempfile::tempdir().expect("create wrong-password RAR4 output dir");
    let wrong = extract_archive(
        wasm_path,
        source,
        wrong_output.path(),
        archive,
        ArchivePluginFormat::Rar,
        Some("not-the-password"),
    );
    assert_eq!(wrong.status, ArchivePluginStatus::Failed);

    let output = tempfile::tempdir().expect("create RAR4 output dir");
    let correct = extract_archive(
        wasm_path,
        source,
        output.path(),
        archive,
        ArchivePluginFormat::Rar,
        Some(RAR_PASSWORD),
    );
    assert_eq!(
        correct.status,
        ArchivePluginStatus::Ok,
        "RAR4: {:?}",
        correct.message
    );
    assert_response_contains_file_bytes(
        &correct,
        output.path(),
        &fs::read(fixture_path("rar/small.txt")).expect("read RAR4 plaintext"),
        "encrypted RAR4",
    );
}

fn assert_encrypted_rar5_password_states(wasm_path: &Path, source: &Path) {
    let archive = "rar5_enc_lz.rar";
    let missing_output = tempfile::tempdir().expect("create no-password RAR5 output dir");
    let missing = extract_archive(
        wasm_path,
        source,
        missing_output.path(),
        archive,
        ArchivePluginFormat::Rar,
        None,
    );
    assert_eq!(missing.status, ArchivePluginStatus::PasswordRequired);

    let wrong_output = tempfile::tempdir().expect("create wrong-password RAR5 output dir");
    let wrong = extract_archive(
        wasm_path,
        source,
        wrong_output.path(),
        archive,
        ArchivePluginFormat::Rar,
        Some("not-the-password"),
    );
    assert_eq!(wrong.status, ArchivePluginStatus::PasswordInvalid);

    let output = tempfile::tempdir().expect("create RAR5 output dir");
    let correct = extract_archive(
        wasm_path,
        source,
        output.path(),
        archive,
        ArchivePluginFormat::Rar,
        Some(RAR_PASSWORD),
    );
    assert_eq!(
        correct.status,
        ArchivePluginStatus::Ok,
        "RAR5: {:?}",
        correct.message
    );
    assert_response_contains_file_bytes(
        &correct,
        output.path(),
        &fs::read(fixture_path("rar/compressible.txt")).expect("read RAR5 plaintext"),
        "encrypted RAR5",
    );
}

fn extract_archive(
    wasm_path: &Path,
    source: &Path,
    output: &Path,
    archive_name: &str,
    format: ArchivePluginFormat,
    password: Option<&str>,
) -> ArchivePluginProcessResponse {
    call_archive_plugin(
        wasm_path,
        source,
        output,
        ArchivePluginOperation::ExtractArchive {
            archive_path: format!("{GUEST_SOURCE_ROOT}/{archive_name}"),
            output_dir: GUEST_OUTPUT_ROOT.to_string(),
            format,
            password: password.map(str::to_string),
        },
    )
}

fn assert_zip_extracts(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create ZIP source dir");
    let output = tempfile::tempdir().expect("create ZIP output dir");
    create_zip_fixture(
        &source.path().join("sample.zip"),
        "nested/hello.txt",
        b"hello from zip\n",
    );

    let response = call_archive_plugin(
        wasm_path,
        source.path(),
        output.path(),
        ArchivePluginOperation::ExtractArchive {
            archive_path: format!("{GUEST_SOURCE_ROOT}/sample.zip"),
            output_dir: GUEST_OUTPUT_ROOT.to_string(),
            format: ArchivePluginFormat::Zip,
            password: None,
        },
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "ZIP extract failed: {:?}",
        response.message
    );
    assert_response_contains_file_bytes(&response, output.path(), b"hello from zip\n", "ZIP");
}

fn assert_zip_path_escape_is_rejected(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create unsafe ZIP source dir");
    let output = tempfile::tempdir().expect("create unsafe ZIP output dir");
    create_zip_fixture(&source.path().join("evil.zip"), "../escape.txt", b"pwned");

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "evil.zip",
        ArchivePluginFormat::Zip,
        None,
    );

    assert_eq!(response.status, ArchivePluginStatus::Failed);
    assert_eq!(response.error_code.as_deref(), Some("unsafe_path"));
    let output_parent = output.path().parent().expect("temp output has a parent");
    assert!(!output_parent.join("escape.txt").exists());
    assert!(!output.path().join("escape.txt").exists());
}

fn assert_inspect_is_unsupported(wasm_path: &Path) {
    let source = stage_files(&[fixture_path("plain-rar4/rar4_store.rar")]);
    let output = tempfile::tempdir().expect("create inspect output dir");
    let response = call_archive_plugin(
        wasm_path,
        source.path(),
        output.path(),
        ArchivePluginOperation::Inspect {
            source_dir: GUEST_SOURCE_ROOT.to_string(),
            archive_path: None,
        },
    );
    assert_eq!(response.status, ArchivePluginStatus::UnsupportedFormat);
}

fn assert_request_path_escape_is_rejected(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create escape source dir");
    let output = tempfile::tempdir().expect("create escape output dir");
    let response = call_archive_plugin(
        wasm_path,
        source.path(),
        output.path(),
        ArchivePluginOperation::ExtractArchive {
            archive_path: format!("{GUEST_SOURCE_ROOT}/../outside.zip"),
            output_dir: GUEST_OUTPUT_ROOT.to_string(),
            format: ArchivePluginFormat::Zip,
            password: None,
        },
    );
    assert_eq!(response.status, ArchivePluginStatus::Failed);
}

fn assert_frozen_abi(wasm_path: &Path) {
    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm_path).expect("compile archive plugin wasm");

    let mut host_imports = Vec::new();
    for import in module.imports() {
        match import.module() {
            "extism:host/user" => host_imports.push((import.name().to_string(), import.ty())),
            "wasi_snapshot_preview1" => {}
            other => panic!("unexpected import module {other} for {}", import.name()),
        }
    }
    host_imports.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        host_imports
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["host_aes_cbc_decrypt", "host_crc32"],
    );
    for (name, arity) in [("host_aes_cbc_decrypt", 5), ("host_crc32", 3)] {
        let ty = &host_imports
            .iter()
            .find(|(candidate, _)| candidate == name)
            .expect("required host import")
            .1;
        let ExternType::Func(function) = ty else {
            panic!("{name} must be a function import");
        };
        assert_eq!(function.params().count(), arity, "{name} parameter count");
        assert!(function.params().all(|value| matches!(value, ValType::I64)));
        assert_eq!(function.results().count(), 1, "{name} result count");
        assert!(
            function
                .results()
                .all(|value| matches!(value, ValType::I64))
        );
    }

    assert!(
        module
            .exports()
            .any(|export| export.name() == "_start" && matches!(export.ty(), ExternType::Func(_))),
        "archive command must export _start"
    );
    assert!(
        module
            .exports()
            .any(|export| export.name() == "memory" && matches!(export.ty(), ExternType::Memory(_))),
        "archive command must export memory"
    );
}

fn assert_sevenz_extracts(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create 7z source dir");
    let output = tempfile::tempdir().expect("create 7z output dir");
    create_sevenz_fixture(
        &source.path().join("sample.7z"),
        "nested/hello.txt",
        b"hello from 7z\n",
    );

    let response = call_archive_plugin(
        wasm_path,
        source.path(),
        output.path(),
        ArchivePluginOperation::ExtractArchive {
            archive_path: format!("{GUEST_SOURCE_ROOT}/sample.7z"),
            output_dir: GUEST_OUTPUT_ROOT.to_string(),
            format: ArchivePluginFormat::SevenZip,
            password: None,
        },
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "7z extract failed: {:?}",
        response.message
    );
    assert_response_contains_file_bytes(&response, output.path(), b"hello from 7z\n", "7z");
}

fn assert_sevenz_rejects_unsafe_paths(wasm_path: &Path) {
    for (archive_name, entry_name) in [
        ("traversal.7z", "../escape.txt"),
        ("backslash.7z", r"nested\escape.txt"),
    ] {
        let source = tempfile::tempdir().expect("create unsafe 7z source dir");
        let output = tempfile::tempdir().expect("create unsafe 7z output dir");
        create_sevenz_fixture(
            &source.path().join(archive_name),
            entry_name,
            b"unsafe 7z\n",
        );

        let response = call_archive_plugin(
            wasm_path,
            source.path(),
            output.path(),
            ArchivePluginOperation::ExtractArchive {
                archive_path: format!("{GUEST_SOURCE_ROOT}/{archive_name}"),
                output_dir: GUEST_OUTPUT_ROOT.to_string(),
                format: ArchivePluginFormat::SevenZip,
                password: None,
            },
        );

        assert_eq!(
            response.status,
            ArchivePluginStatus::Failed,
            "unsafe 7z path was not rejected: {:?}",
            response.message
        );
        assert_eq!(response.error_code.as_deref(), Some("unsafe_path"));
    }
}

fn assert_sevenz_rejects_duplicate_paths(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create duplicate 7z source dir");
    let output = tempfile::tempdir().expect("create duplicate 7z output dir");
    create_sevenz_fixture_with_entries(
        &source.path().join("duplicate.7z"),
        &[
            ("nested/duplicate.txt", b"first".as_slice()),
            ("nested/duplicate.txt", b"second".as_slice()),
        ],
    );

    let response = call_archive_plugin(
        wasm_path,
        source.path(),
        output.path(),
        ArchivePluginOperation::ExtractArchive {
            archive_path: format!("{GUEST_SOURCE_ROOT}/duplicate.7z"),
            output_dir: GUEST_OUTPUT_ROOT.to_string(),
            format: ArchivePluginFormat::SevenZip,
            password: None,
        },
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Failed,
        "duplicate 7z output path was not rejected: {:?}",
        response.message
    );
    assert_eq!(
        response.error_code.as_deref(),
        Some("duplicate_output_path")
    );
}

/// Drive the archive plugin as a `wasm32-wasip1` command (RFC 123 §7.2.5): the
/// request JSON is fed on stdin, the response JSON is captured from stdout, the
/// two frozen §5 crypto functions are registered under `extism:host/user`, and
/// the fixed guest roots are preopened. This mirrors how the Scryer host invokes
/// the command binary — no Extism.
fn call_archive_plugin(
    wasm_path: &Path,
    source_dir: &Path,
    output_dir: &Path,
    operation: ArchivePluginOperation,
) -> ArchivePluginProcessResponse {
    call_archive_plugin_with_source_perms(
        wasm_path,
        source_dir,
        output_dir,
        operation,
        DirPerms::READ,
        FilePerms::READ,
    )
}

fn call_archive_plugin_with_source_perms(
    wasm_path: &Path,
    source_dir: &Path,
    output_dir: &Path,
    operation: ArchivePluginOperation,
    source_dir_perms: DirPerms,
    source_file_perms: FilePerms,
) -> ArchivePluginProcessResponse {
    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm_path).expect("load archive plugin wasm");
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx: &mut WasiP1Ctx| ctx)
        .expect("add WASI preview1 linker functions");
    register_crypto_host(&mut linker);

    let request = ArchivePluginProcessRequest { operation };
    let input = serde_json::to_vec(&request).expect("serialize archive request");
    let stdout = MemoryOutputPipe::new(8 * 1024 * 1024);
    let wasi = WasiCtxBuilder::new()
        .args(&["archive-extraction"])
        .stdin(MemoryInputPipe::new(input))
        .stdout(stdout.clone())
        .inherit_stderr()
        .env("TMPDIR", GUEST_OUTPUT_ROOT)
        .preopened_dir(
            source_dir,
            GUEST_SOURCE_ROOT,
            source_dir_perms,
            source_file_perms,
        )
        .expect("preopen archive source")
        .preopened_dir(
            output_dir,
            GUEST_OUTPUT_ROOT,
            DirPerms::READ | DirPerms::MUTATE,
            FilePerms::READ | FilePerms::WRITE,
        )
        .expect("preopen archive output")
        .build_p1();
    let mut store = Store::new(&engine, wasi);
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate archive plugin");
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .expect("archive plugin should export _start");
    match start.call(&mut store, ()) {
        Ok(()) => {}
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<I32Exit>() {
                assert_eq!(exit.0, 0, "archive plugin exited with {}", exit.0);
            } else {
                panic!("archive plugin trapped: {error:?}");
            }
        }
    }
    drop(store);

    let bytes = stdout.contents();
    serde_json::from_slice::<ArchivePluginProcessResponse>(&bytes).unwrap_or_else(|error| {
        panic!(
            "decode archive plugin response ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn archive_plugin_wasm() -> PathBuf {
    PLUGIN_WASM
        .get_or_init(|| {
            let repo_root = repo_root();
            let plugin_root = repo_root.join("archive_extractors/archive-extraction");
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .current_dir(&repo_root)
                .arg("build")
                .arg("--manifest-path")
                .arg(plugin_root.join("Cargo.toml"))
                .arg("--profile")
                .arg("plugin-release")
                .arg("--target")
                .arg("wasm32-wasip1")
                .status()
                .expect("run cargo build for archive plugin");
            assert!(status.success(), "archive plugin build failed: {status}");

            plugin_root.join(
                "target/wasm32-wasip1/plugin-release/archive_extraction_archive_extractor.wasm",
            )
        })
        .clone()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("archive plugin must live below the repository root")
        .to_path_buf()
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_path(relative: &str) -> PathBuf {
    fixture_root().join(relative)
}

fn stage_files(files: &[PathBuf]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create archive source dir");
    for file in files {
        let name = file.file_name().expect("fixture has file name");
        fs::copy(file, dir.path().join(name))
            .unwrap_or_else(|error| panic!("copy {}: {error}", file.display()));
    }
    dir
}

fn par2_fixture_files() -> Vec<PathBuf> {
    let dir = fixture_path("par2");
    let mut files = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn create_zip_fixture(path: &Path, entry_name: &str, payload: &[u8]) {
    let file = fs::File::create(path).expect("create zip fixture");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(entry_name, options)
        .expect("start zip file entry");
    zip.write_all(payload).expect("write zip payload");
    zip.finish().expect("finish zip fixture");
}

fn create_sevenz_fixture(path: &Path, entry_name: &str, payload: &[u8]) {
    create_sevenz_fixture_with_entries(path, &[(entry_name, payload)]);
}

fn create_sevenz_fixture_with_entries(path: &Path, entries: &[(&str, &[u8])]) {
    let temp = tempfile::tempdir().expect("create 7z fixture input dir");
    let mut archive = sevenz_rust2::ArchiveWriter::create(path).expect("create 7z fixture");
    for (index, (entry_name, payload)) in entries.iter().enumerate() {
        let source_path = temp.path().join(format!("payload-{index}.txt"));
        fs::write(&source_path, payload).expect("write 7z fixture payload");
        archive
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::from_path(&source_path, (*entry_name).to_string()),
                Some(fs::File::open(&source_path).expect("open 7z fixture payload")),
            )
            .expect("write 7z fixture entry");
    }
    archive.finish().expect("finish 7z fixture");
}

fn assert_response_contains_file_bytes(
    response: &ArchivePluginProcessResponse,
    output_dir: &Path,
    expected: &[u8],
    label: &str,
) {
    for file in &response.files {
        let path = output_dir.join(&file.relative_path);
        if fs::read(&path).is_ok_and(|actual| actual == expected) {
            return;
        }
    }
    panic!(
        "{label} response did not contain expected output bytes; files={:?}",
        response.files
    );
}

fn assert_response_files_are_byte_correct(
    response: &ArchivePluginProcessResponse,
    output_dir: &Path,
    label: &str,
) {
    assert!(
        !response.files.is_empty(),
        "{label} must return extracted files"
    );
    for file in &response.files {
        let path = output_dir.join(&file.relative_path);
        let bytes =
            fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        if let Some(size) = file.size {
            assert_eq!(
                bytes.len() as u64,
                size,
                "{label} size for {}",
                file.relative_path
            );
        }
        if let Some(checksum) = &file.checksum {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&bytes);
            assert_eq!(
                format!("{:08x}", hasher.finalize()),
                *checksum,
                "{label} CRC for {}",
                file.relative_path
            );
        }
    }
}

#[derive(Clone, Copy)]
struct HostCallCounts {
    aes: usize,
    crc: usize,
}

fn host_call_counts() -> HostCallCounts {
    HostCallCounts {
        aes: AES_CALLS.load(Ordering::SeqCst),
        crc: CRC_CALLS.load(Ordering::SeqCst),
    }
}

/// Register the §5 crypto host functions on `linker`.
///
/// Serves both the current `host_*` ABI and the pre-rename `scryer_*` aliases.
/// The latter remain a compatibility path for already-published archive plugin
/// artifacts while current builds use weaver-unrar's `host-abi-extism` feature.
fn register_crypto_host(linker: &mut Linker<WasiP1Ctx>) {
    linker
        .func_wrap(
            "extism:host/user",
            "host_aes_cbc_decrypt",
            host_aes_cbc_decrypt,
        )
        .expect("define host_aes_cbc_decrypt");
    linker
        .func_wrap("extism:host/user", "host_crc32", host_crc32)
        .expect("define host_crc32");
    linker
        .func_wrap(
            "extism:host/user",
            "scryer_aes_cbc_decrypt",
            host_aes_cbc_decrypt,
        )
        .expect("define scryer_aes_cbc_decrypt");
    linker
        .func_wrap("extism:host/user", "scryer_crc32", host_crc32)
        .expect("define scryer_crc32");
}

fn host_aes_cbc_decrypt(
    mut caller: Caller<'_, WasiP1Ctx>,
    key_ptr: i64,
    key_len: i64,
    iv_ptr: i64,
    buf_ptr: i64,
    buf_len: i64,
) -> i64 {
    AES_CALLS.fetch_add(1, Ordering::SeqCst);

    if key_len != 16 && key_len != 32 {
        return -1;
    }
    if buf_len < 0 || buf_len % 16 != 0 {
        return -2;
    }

    let Some(memory) = caller.get_export("memory").and_then(|export| match export {
        Extern::Memory(memory) => Some(memory),
        _ => None,
    }) else {
        return -3;
    };

    let Some((key_ptr, key_len)) = wasm_range(key_ptr, key_len) else {
        return -3;
    };
    let Some((iv_ptr, iv_len)) = wasm_range(iv_ptr, 16) else {
        return -3;
    };
    let Some((buf_ptr, buf_len)) = wasm_range(buf_ptr, buf_len) else {
        return -3;
    };

    let mut key = vec![0_u8; key_len];
    if memory.read(&caller, key_ptr, &mut key).is_err() {
        return -3;
    }
    let mut iv = vec![0_u8; iv_len];
    if memory.read(&caller, iv_ptr, &mut iv).is_err() {
        return -3;
    }
    if buf_len == 0 {
        return 0;
    }
    let mut buf = vec![0_u8; buf_len];
    if memory.read(&caller, buf_ptr, &mut buf).is_err() {
        return -3;
    }

    reference_cbc_decrypt(&key, &iv, &mut buf);
    if memory.write(&mut caller, buf_ptr, &buf).is_err() {
        return -3;
    }

    0
}

fn host_crc32(mut caller: Caller<'_, WasiP1Ctx>, seed: i64, buf_ptr: i64, buf_len: i64) -> i64 {
    CRC_CALLS.fetch_add(1, Ordering::SeqCst);

    if buf_len < 0 {
        return -1;
    }
    let Some(memory) = caller.get_export("memory").and_then(|export| match export {
        Extern::Memory(memory) => Some(memory),
        _ => None,
    }) else {
        return -1;
    };
    let Some((buf_ptr, buf_len)) = wasm_range(buf_ptr, buf_len) else {
        return -1;
    };

    let mut buf = vec![0_u8; buf_len];
    if memory.read(&caller, buf_ptr, &mut buf).is_err() {
        return -1;
    }
    let mut hasher = crc32fast::Hasher::new_with_initial(seed as u64 as u32);
    hasher.update(&buf);
    hasher.finalize() as u64 as i64
}

fn wasm_range(ptr: i64, len: i64) -> Option<(usize, usize)> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let ptr = usize::try_from(ptr as u64).ok()?;
    let len = usize::try_from(len as u64).ok()?;
    ptr.checked_add(len)?;
    Some((ptr, len))
}

fn reference_cbc_decrypt(key: &[u8], iv: &[u8], data: &mut [u8]) {
    let mut aes_key = MaybeUninit::<aws_lc_sys::AES_KEY>::uninit();
    let bits = (key.len() * 8) as u32;
    let set_key_result =
        unsafe { aws_lc_sys::AES_set_decrypt_key(key.as_ptr(), bits, aes_key.as_mut_ptr()) };
    assert_eq!(set_key_result, 0, "AWS-LC rejected AES key length");
    let aes_key = unsafe { aes_key.assume_init() };
    let mut iv = iv.to_vec();
    unsafe {
        aws_lc_sys::AES_cbc_encrypt(
            data.as_ptr(),
            data.as_mut_ptr(),
            data.len(),
            &aes_key,
            iv.as_mut_ptr(),
            aws_lc_sys::AES_DECRYPT,
        );
    }
}
