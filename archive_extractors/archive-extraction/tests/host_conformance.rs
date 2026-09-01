//! Conformance against the real Scryer host, run on the RELEASE artifact.
//!
//! This suite exists because the plugin's contract is not "these functions
//! behave" but "this exact `.wasm` runs under Scryer's archive host". It
//! therefore builds the shipping `wasm32-wasip2` component and drives it the
//! way `crates/scryer-plugins/src/wasmtime_host/archive_component_host.rs`
//! does: the world is linked as `scryer:archive/archive-extractor@1.0.0`, the
//! `crypto` interface is served by the same AES-CBC and CRC-32 cores the host
//! uses, WASI Preview 2 comes from the linker, and the sandbox is exactly the
//! host's — a read-only source preopen, a writable output preopen, and a
//! private `TMPDIR` scratch dir.
//!
//! A mismatch here means the artifact would fail in production, which is the
//! only failure mode this file is trying to catch.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use liblzma::write::XzEncoder;
use par2_rs::{BlockSizing, Par2Creator, Par2CreatorOptions, RecoveryAmount};
use scryer_plugin_sdk::{
    ArchivePluginFormat, ArchivePluginOperation, ArchivePluginProcessRequest,
    ArchivePluginProcessResponse, ArchivePluginStatus,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod archive_world {
    wasmtime::component::bindgen!({
        world: "scryer:archive/archive-extractor@1.0.0",
        path: "wit",
    });
}

use archive_world::ArchiveExtractor;
use archive_world::scryer::archive::crypto::{AesError, Host as CryptoHost};

/// The host's fixed guest paths (`crates/scryer-plugins/src/archive_adapter.rs`)
/// and its scratch mount (`wasmtime_host/sandbox.rs`).
const GUEST_SOURCE_ROOT: &str = "/scryer/source";
const GUEST_OUTPUT_ROOT: &str = "/scryer/output";
const GUEST_SCRATCH_ROOT: &str = "/tmp";
const RAR_PASSWORD: &str = "testpass123";

static AES_CALLS: AtomicUsize = AtomicUsize::new(0);
static CRC_CALLS: AtomicUsize = AtomicUsize::new(0);
static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn archive_extraction_release_wasm_conforms_to_host_contract() {
    let wasm_path = archive_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_descriptor(&wasm_path);
    assert_plain_rar4_extracts(&wasm_path);
    assert_rar5_multivolume_extracts(&wasm_path);
    assert_encrypted_rars_use_the_crypto_import(&wasm_path);
    assert_sevenz_extracts(&wasm_path);
    assert_sevenz_rejects_unsafe_paths(&wasm_path);
    assert_sevenz_rejects_duplicate_paths(&wasm_path);
    assert_xz_extracts(&wasm_path);
    assert_zip_extracts(&wasm_path);
    assert_zip_path_escape_is_rejected(&wasm_path);
    assert_par2_repairs_a_damaged_archive_before_extracting(&wasm_path);
    assert_par2_emits_repaired_plain_files(&wasm_path);
    assert_par2_unrepairable_damage_fails(&wasm_path);
    assert_inspect_without_a_recovery_set_is_unsupported(&wasm_path);
    assert_request_path_escape_is_rejected(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The host removed the core-module archive backing outright, so a core wasm
/// artifact is not a degraded plugin but an uninstallable one. Check the
/// component preamble directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = fs::read(wasm_path).expect("read archive plugin wasm");
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

/// The exact check `validate_archive_component` performs on install: the
/// artifact compiles, every import the guest emits is satisfiable from WASI
/// Preview 2 plus the world's `crypto` interface, and its exports match
/// `scryer:archive/archive-extractor@1.0.0`.
fn assert_world_conformance(wasm_path: &Path) {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm_path).expect("compile archive component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    ArchiveExtractor::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the archive crypto host");
    linker
        .instantiate_pre(&component)
        .and_then(archive_world::ArchiveExtractorPre::new)
        .expect("the artifact must satisfy scryer:archive/archive-extractor@1.0.0");
}

/// `describe` is a world export now, not an argv-driven stdout dump: the host
/// calls it directly and parses the returned bytes as a `PluginDescriptor`.
fn assert_describe_returns_descriptor(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create describe source dir");
    let output = tempfile::tempdir().expect("create describe output dir");
    let (mut store, plugin) = instantiate(wasm_path, source.path(), output.path());
    let bytes = plugin.call_describe(&mut store).expect("call describe");

    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
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
    for format in ["rar", "zip", "7z", "xz"] {
        assert!(
            formats.iter().any(|value| value.as_str() == Some(format)),
            "descriptor did not advertise {format}: {descriptor}"
        );
    }
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

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

/// The multi-volume RAR5 fixture ships beside its real recovery set, so this
/// also covers the healthy PAR2 path: verification passes, nothing is staged,
/// and the archive to open is resolved out of the PAR2 metadata.
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

/// Encrypted RAR is the only reason the world imports `crypto` at all, so this
/// asserts the imports were genuinely reached — a guest that quietly fell back
/// to in-wasm AES would still decrypt correctly and would still be wrong.
fn assert_encrypted_rars_use_the_crypto_import(wasm_path: &Path) {
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
        "encrypted RAR fixtures did not call the crypto aes-cbc-decrypt import"
    );
    assert!(
        after.crc > before.crc,
        "encrypted RAR fixtures did not call the crypto crc32 import"
    );
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

fn assert_zip_extracts(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create ZIP source dir");
    let output = tempfile::tempdir().expect("create ZIP output dir");
    create_zip_fixture(
        &source.path().join("sample.zip"),
        "nested/hello.txt",
        b"hello from zip\n",
    );

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "sample.zip",
        ArchivePluginFormat::Zip,
        None,
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

fn assert_sevenz_extracts(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create 7z source dir");
    let output = tempfile::tempdir().expect("create 7z output dir");
    create_sevenz_fixture(
        &source.path().join("sample.7z"),
        "nested/hello.txt",
        b"hello from 7z\n",
    );

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "sample.7z",
        ArchivePluginFormat::SevenZip,
        None,
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

        let response = extract_archive(
            wasm_path,
            source.path(),
            output.path(),
            archive_name,
            ArchivePluginFormat::SevenZip,
            None,
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

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "duplicate.7z",
        ArchivePluginFormat::SevenZip,
        None,
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

fn assert_xz_extracts(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create XZ source dir");
    let output = tempfile::tempdir().expect("create XZ output dir");
    let path = source.path().join("episode.ass.xz");
    let file = fs::File::create(&path).expect("create XZ fixture");
    let mut encoder = XzEncoder::new(file, 6);
    encoder
        .write_all(b"[Script Info]\nTitle: XZ fixture\n")
        .expect("write XZ fixture");
    encoder.finish().expect("finish XZ fixture");

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "episode.ass.xz",
        ArchivePluginFormat::Xz,
        None,
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "XZ extract failed: {:?}",
        response.message
    );
    assert_response_contains_file_bytes(
        &response,
        output.path(),
        b"[Script Info]\nTitle: XZ fixture\n",
        "XZ",
    );
}

// ---------------------------------------------------------------------------
// PAR2, through the real sandbox
// ---------------------------------------------------------------------------

/// The whole point of doing PAR2 in the guest: the source preopen is READ-ONLY,
/// so the repaired volume can only be materialized in the `TMPDIR` scratch, and
/// the extraction has to read it from there. A plugin that tried to repair in
/// place would fail here and nowhere else.
fn assert_par2_repairs_a_damaged_archive_before_extracting(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create PAR2 repair source dir");
    let output = tempfile::tempdir().expect("create PAR2 repair output dir");
    let contents = deterministic_bytes(0xC0DE_0001, 256 * 1024);
    let archive = source.path().join("payload.zip");
    create_zip_fixture_with_contents(&archive, "media/episode.bin", &contents);
    create_recovery_set(source.path(), std::slice::from_ref(&archive), 8);
    damage_slices(&archive, &[5, 11]);

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "payload.zip",
        ArchivePluginFormat::Zip,
        None,
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "damaged-but-repairable archive did not extract: {:?}",
        response.message
    );
    assert_eq!(
        fs::read(output.path().join("media/episode.bin")).expect("read repaired member"),
        contents,
        "PAR2 repair must reconstruct the original bytes"
    );
    assert!(
        fs::read(&archive).expect("read source archive") != contents,
        "the read-only source must not have been repaired in place"
    );
}

/// A recovery set over plain media: there is no archive to open, so the
/// repaired files are written straight into the writable output preopen for the
/// host's import pass.
fn assert_par2_emits_repaired_plain_files(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create PAR2 plain source dir");
    let output = tempfile::tempdir().expect("create PAR2 plain output dir");
    let contents = deterministic_bytes(0xC0DE_0002, 256 * 1024);
    let media = source.path().join("episode.mkv");
    fs::write(&media, &contents).expect("write plain fixture");
    create_recovery_set(source.path(), std::slice::from_ref(&media), 8);
    damage_slices(&media, &[7]);

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "episode.mkv",
        ArchivePluginFormat::Rar,
        None,
    );

    assert_eq!(
        response.status,
        ArchivePluginStatus::Ok,
        "plain-file recovery set did not emit: {:?}",
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
        fs::read(output.path().join("episode.mkv")).expect("read emitted file"),
        contents
    );
}

fn assert_par2_unrepairable_damage_fails(wasm_path: &Path) {
    let source = tempfile::tempdir().expect("create PAR2 failure source dir");
    let output = tempfile::tempdir().expect("create PAR2 failure output dir");
    let contents = deterministic_bytes(0xC0DE_0003, 256 * 1024);
    let archive = source.path().join("payload.zip");
    create_zip_fixture_with_contents(&archive, "media/episode.bin", &contents);
    create_recovery_set(source.path(), std::slice::from_ref(&archive), 2);
    damage_slices(&archive, &[3, 6, 9, 12, 15, 18]);

    let response = extract_archive(
        wasm_path,
        source.path(),
        output.path(),
        "payload.zip",
        ArchivePluginFormat::Zip,
        None,
    );

    assert_eq!(response.status, ArchivePluginStatus::Failed);
    assert_eq!(
        response.error_code.as_deref(),
        Some("par2_insufficient_recovery"),
        "{:?}",
        response.message
    );
    assert!(
        !output.path().join("media/episode.bin").exists(),
        "an unrepairable set must not leave partial output"
    );
}

fn assert_inspect_without_a_recovery_set_is_unsupported(wasm_path: &Path) {
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

// ---------------------------------------------------------------------------
// The host, reproduced
// ---------------------------------------------------------------------------

/// Store data for one invocation, mirroring `ArchiveComponentCtx`.
struct Ctx {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl CryptoHost for Ctx {
    /// The host's AES core (AWS-LC), with the frozen validation order: key
    /// length, then block alignment, then IV length.
    fn aes_cbc_decrypt(
        &mut self,
        key: Vec<u8>,
        iv: Vec<u8>,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, AesError> {
        AES_CALLS.fetch_add(1, Ordering::SeqCst);

        if key.len() != 16 && key.len() != 32 {
            return Err(AesError::BadKeyLength);
        }
        if !data.len().is_multiple_of(16) {
            return Err(AesError::BadBlockLength);
        }
        if iv.len() != 16 {
            return Err(AesError::BadIvLength);
        }

        let mut buffer = data;
        if !buffer.is_empty() {
            reference_cbc_decrypt(&key, &iv, &mut buffer);
        }
        Ok(buffer)
    }

    fn crc32(&mut self, seed: u32, data: Vec<u8>) -> u32 {
        CRC_CALLS.fetch_add(1, Ordering::SeqCst);
        let mut hasher = crc32fast::Hasher::new_with_initial(seed);
        hasher.update(&data);
        hasher.finalize()
    }
}

/// Instantiate the component under the host's sandbox: read-only source,
/// writable output, private `TMPDIR` scratch, captured stdio, no env beyond
/// `TMPDIR`, no network.
fn instantiate(
    wasm_path: &Path,
    source_dir: &Path,
    output_dir: &Path,
) -> (Store<Ctx>, ArchiveExtractor) {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm_path).expect("compile archive component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    ArchiveExtractor::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the archive crypto host");

    // Leaked deliberately: the scratch dir must outlive the store, and this
    // process is a single test binary.
    let scratch = Box::leak(Box::new(
        tempfile::tempdir().expect("create archive scratch dir"),
    ));
    let mut builder = WasiCtxBuilder::new();
    builder
        // The host captures guest stderr and tails it into its own error
        // messages; inheriting it here puts the same text in front of whoever
        // is reading the test failure.
        .inherit_stderr()
        .env("TMPDIR", GUEST_SCRATCH_ROOT)
        .preopened_dir(
            source_dir,
            GUEST_SOURCE_ROOT,
            DirPerms::READ,
            FilePerms::READ,
        )
        .expect("preopen archive source")
        .preopened_dir(
            output_dir,
            GUEST_OUTPUT_ROOT,
            DirPerms::READ | DirPerms::MUTATE,
            FilePerms::READ | FilePerms::WRITE,
        )
        .expect("preopen archive output")
        .preopened_dir(
            scratch.path(),
            GUEST_SCRATCH_ROOT,
            DirPerms::READ | DirPerms::MUTATE,
            FilePerms::READ | FilePerms::WRITE,
        )
        .expect("preopen archive scratch");

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            wasi: builder.build(),
        },
    );
    let plugin = ArchiveExtractor::instantiate(&mut store, &component, &linker)
        .expect("instantiate archive component");
    (store, plugin)
}

/// One request/response exchange, instance-per-request exactly as the host does.
fn call_archive_plugin(
    wasm_path: &Path,
    source_dir: &Path,
    output_dir: &Path,
    operation: ArchivePluginOperation,
) -> ArchivePluginProcessResponse {
    let (mut store, plugin) = instantiate(wasm_path, source_dir, output_dir);
    let request = serde_json::to_vec(&ArchivePluginProcessRequest { operation })
        .expect("serialize archive request");
    let bytes = plugin
        .call_process(&mut store, &request)
        .expect("archive component trapped")
        .unwrap_or_else(|error| panic!("archive component reported {error:?}"));
    serde_json::from_slice::<ArchivePluginProcessResponse>(&bytes).unwrap_or_else(|error| {
        panic!(
            "decode archive plugin response ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
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

/// Build the RELEASE component. Nothing in this file is allowed to test a debug
/// artifact: the shipped plugin is what has to conform.
fn archive_plugin_wasm() -> PathBuf {
    PLUGIN_WASM
        .get_or_init(|| {
            let plugin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(cargo)
                .arg("build")
                .arg("--manifest-path")
                .arg(plugin_root.join("Cargo.toml"))
                .arg("--profile")
                .arg("plugin-release")
                .arg("--target")
                .arg("wasm32-wasip2")
                .status()
                .expect("run cargo build for archive plugin");
            assert!(status.success(), "archive plugin build failed: {status}");

            plugin_root.join(
                "target/wasm32-wasip2/plugin-release/archive_extraction_archive_extractor.wasm",
            )
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

/// Source slice size for generated recovery sets, matching the plugin's own
/// PAR2 unit tests so "damage slice N" means the same thing in both suites.
const PAR2_SLICE_BYTES: u64 = 4_096;

/// Deterministic xorshift64* bytes: reproducible and incompressible, so a ZIP
/// of them stays close to its own size and the slice map is dense.
fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// Generate a real recovery set with par2-rs, with an explicit recovery-block
/// count so "repairable" and "unrepairable" are reproducible rather than a
/// function of the default percentage.
fn create_recovery_set(dir: &Path, inputs: &[PathBuf], recovery_blocks: u32) {
    let mut options = Par2CreatorOptions::with_output(
        dir.join("recovery"),
        Some(dir.to_path_buf()),
        inputs.to_vec(),
    );
    options.block_sizing = BlockSizing::Bytes(PAR2_SLICE_BYTES);
    options.recovery_amount = RecoveryAmount::Count(recovery_blocks);
    let creator = Par2Creator::new(options);
    let plan = creator.plan().expect("plan PAR2 creation");
    creator.create(&plan).expect("create PAR2 recovery set");
}

fn damage_slices(path: &Path, slices: &[u64]) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open fixture for damage");
    for slice in slices {
        file.seek(SeekFrom::Start(slice * PAR2_SLICE_BYTES))
            .expect("seek to damaged slice");
        file.write_all(&vec![0xA5_u8; PAR2_SLICE_BYTES as usize])
            .expect("write damage");
    }
}

fn create_zip_fixture(path: &Path, entry_name: &str, payload: &[u8]) {
    create_zip_fixture_with_contents(path, entry_name, payload);
}

fn create_zip_fixture_with_contents(path: &Path, entry_name: &str, payload: &[u8]) {
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

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

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

/// The host's AES-CBC core, so a divergence means the plugin mangled the
/// buffers rather than that the reference is wrong.
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
