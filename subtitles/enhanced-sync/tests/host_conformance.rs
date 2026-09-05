//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The suite itself lives in `scryer-plugin-conformance`, which builds the
//! shipping `wasm32-wasip2` component and drives it the way
//! `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs` does.
//!
//! ## What is different about this plugin
//!
//! conformance: bespoke — every other family component reaches Scryer through
//! `scryer:host/services`. This one reaches the *filesystem*: alignment reads a
//! media file and a subtitle, and writes a rewritten subtitle back. Its
//! authority is therefore WASI preopens, not host services — the fixed roots
//! the host stages per job (`crates/scryer-plugins/src/subtitle_sync_adapter.rs`)
//! — and this suite reproduces that staging rather than scripting host calls.
//! Nothing in the shared catalog check set applies: it validates no config,
//! downloads no file and generates nothing.
//!
//! [`aligns_a_real_desynced_subtitle_inside_the_sandbox`] is consequently a
//! genuine end-to-end proof and not a dispatch round-trip: a real AAC fixture
//! and a real 2.2s-early SRT go in through the preopens, the FFmpeg-derived
//! decode, the libfvad VAD and the rustfft correlation all run *inside the
//! component*, and the rewritten subtitle is read back off the host
//! filesystem.

use std::path::{Path, PathBuf};

use scryer_plugin_conformance::Script;
use scryer_plugin_conformance::subtitle::{
    Check, SubtitleConformance, call_subtitle, instantiate, instantiate_with_wasi,
};
use scryer_plugin_sdk::PluginErrorCode;
use scryer_plugin_sdk::command::{PluginSubtitleCommand, PluginSubtitleCommandResult};
use scryer_plugin_sdk::{
    AudioStreamSelector, PluginResult, SubtitlePluginSearchRequest,
    SubtitlePluginValidateConfigRequest, SubtitleSyncCommandAlignRequest,
    SubtitleSyncCommandInputFile, SubtitleSyncCommandOutputTarget, SubtitleSyncCommandSubtitleFile,
    SubtitleSyncPluginOperation, SubtitleSyncPluginProcessRequest, SubtitleSyncPluginResponse,
    SubtitleSyncProbeRequest,
};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

/// The guest roots the host stages for an align job. Kept as constants so a
/// drift between this suite and `subtitle_sync_adapter.rs` is visible.
const GUEST_INPUT_ROOT: &str = "/input";
const GUEST_SUBTITLE_ROOT: &str = "/subtitle";
const GUEST_OUTPUT_ROOT: &str = "/output";
const GUEST_SCRATCH_ROOT: &str = "/scratch";

/// The fixture subtitle is 2.2s early, so a correct align pushes it later by
/// roughly that much. The tolerance is the one the in-crate parity suite uses
/// for this fixture family.
const FIXTURE_EARLY_MS: i64 = 2200;
const FIXTURE_TOLERANCE_MS: i64 = 450;

#[test]
fn enhanced_sync_release_wasm_conforms_to_the_subtitle_host_contract() {
    let suite = suite();

    suite.assert_artifact_is_a_component();
    suite.assert_world_conformance();
    suite.assert_describe_returns_a_subtitle_descriptor();
    assert_probe_round_trips_the_sync_envelope(&suite);
    assert_catalog_operations_are_unsupported_in_band(&suite);
    suite.assert_another_family_is_an_invocation_error();
}

fn suite() -> SubtitleConformance {
    SubtitleConformance::new(env!("CARGO_MANIFEST_DIR"), "enhanced-subtitle-sync")
        .wasm("enhanced_subtitle_sync_plugin.wasm")
        .descriptor_id("enhanced-subtitle-sync")
        // This is what separates a sync plugin from a catalog provider sharing
        // the same world, and it is what routes align jobs here.
        .mode("sync")
        .expects_descriptor(
            &["provider", "capabilities", "sync", "command_model"],
            serde_json::json!(true),
        )
        .without_services_import()
        // None of the catalog check set applies to a sync-only provider: the
        // four catalog operations are refused, below, rather than exercised.
        .without(Check::ValidateConfig)
        .without(Check::Download)
        .without(Check::RefusedHostCapability)
        .without(Check::GenerateIsUnsupported)
}

/// `Probe` is the cheapest sync operation — it touches no filesystem — so it
/// isolates the part this plugin's family envelope actually changed: that a
/// `PluginSubtitleCommand::Sync` reaches the plugin's existing
/// `SubtitleSyncPluginProcessRequest` handler through the family envelope and
/// comes back as a well-formed `PluginSubtitleCommandResult::Sync`.
fn assert_probe_round_trips_the_sync_envelope(suite: &SubtitleConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), Script::default());
    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Sync(SubtitleSyncPluginProcessRequest {
            operation: SubtitleSyncPluginOperation::Probe {
                request: SubtitleSyncProbeRequest {
                    codec: None,
                    codec_label: Some("ac3".to_string()),
                    packet_base64: None,
                },
            },
        }),
    );

    let PluginSubtitleCommandResult::Sync(result) = result else {
        panic!("a sync command must come back as a sync result");
    };
    let process = match result {
        PluginResult::Ok(process) => process,
        PluginResult::Err(error) => panic!("probe refused in-band: {error:?}"),
    };
    let SubtitleSyncPluginResponse::Probe { response } = process.response else {
        panic!("a probe must come back as a probe response");
    };
    assert!(
        response.supported,
        "ac3 is one of this plugin's advertised codecs, got {response:?}"
    );
    assert!(
        !response.backend.is_empty(),
        "a probe response must name its backend"
    );
    assert!(
        store.data().script.calls.is_empty(),
        "a sync operation must not call host services, saw {:?}",
        store.data().script.calls
    );
}

/// This provider is `mode: Sync`. The four catalog operations now reach every
/// subtitle plugin because they share one envelope, so each must be refused
/// with a typed `Unsupported` rather than a trap — the host keeps a diagnosis
/// it can show an operator instead of a generic invocation error.
fn assert_catalog_operations_are_unsupported_in_band(suite: &SubtitleConformance) {
    let (mut store, plugin) = instantiate(&suite.wasm_path(), Script::default());

    let validate = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
    );
    let PluginSubtitleCommandResult::ValidateConfig(PluginResult::Err(error)) = validate else {
        panic!("a sync-only provider must refuse validate-config in-band");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);

    // `media_kind` and `title` are the only required fields; the rest carry
    // serde defaults, so the minimal wire form is also the least brittle way
    // to build one.
    let search: SubtitlePluginSearchRequest =
        serde_json::from_value(serde_json::json!({ "media_kind": "movie", "title": "Fixture" }))
            .expect("build a minimal search request");
    let search = call_subtitle(&mut store, &plugin, PluginSubtitleCommand::Search(search));
    let PluginSubtitleCommandResult::Search(PluginResult::Err(error)) = search else {
        panic!("a sync-only provider must refuse search in-band");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);

    assert!(
        store.data().script.calls.is_empty(),
        "refusals must not reach host services, saw {:?}",
        store.data().script.calls
    );
}

/// The real thing: a desynced subtitle and a real media fixture go into the
/// sandbox through the preopens the host stages, and a corrected subtitle
/// comes back out on the host filesystem.
///
/// This is a separate `#[test]` from the contract sweep above because it is
/// the expensive one — it decodes an entire audio track inside wasmtime — and
/// because a failure here means something quite different: the ABI is fine and
/// the *DSP* is wrong.
#[test]
fn aligns_a_real_desynced_subtitle_inside_the_sandbox() {
    let suite = suite();
    let wasm_path = suite.wasm_path();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-data");

    // Stage the job exactly as `subtitle_sync_adapter.rs` does: media parent
    // read-only, a subtitle dir read-only, output and scratch writable.
    let subtitle_dir = tempfile::tempdir().expect("subtitle dir");
    let output_dir = tempfile::tempdir().expect("output dir");
    let scratch_dir = tempfile::tempdir().expect("scratch dir");

    let original = std::fs::read(fixtures.join("subtitles/srt/early_2200.srt"))
        .expect("read the desynced fixture subtitle");
    std::fs::write(subtitle_dir.path().join("early_2200.srt"), &original)
        .expect("stage the subtitle");

    // The host captures guest stderr and tails it into its own error messages;
    // inheriting it here puts the same text in front of whoever is reading the
    // test failure.
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    for (host_path, guest_path, perms) in [
        (fixtures.join("media"), GUEST_INPUT_ROOT, FsPerms::ReadOnly),
        (
            subtitle_dir.path().to_path_buf(),
            GUEST_SUBTITLE_ROOT,
            FsPerms::ReadOnly,
        ),
        (
            output_dir.path().to_path_buf(),
            GUEST_OUTPUT_ROOT,
            FsPerms::ReadWrite,
        ),
        (
            scratch_dir.path().to_path_buf(),
            GUEST_SCRATCH_ROOT,
            FsPerms::ReadWrite,
        ),
    ] {
        builder
            .preopened_dir(&host_path, guest_path, perms)
            .unwrap_or_else(|error| {
                panic!("preopen {} as {guest_path}: {error}", host_path.display())
            });
    }

    let (mut store, plugin) = instantiate_with_wasi(
        &wasm_path,
        Script::default(),
        scryer_plugin_conformance::DefaultResponder,
        builder.build(),
    );
    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Sync(SubtitleSyncPluginProcessRequest {
            operation: SubtitleSyncPluginOperation::Align {
                request: Box::new(SubtitleSyncCommandAlignRequest {
                    input: SubtitleSyncCommandInputFile {
                        path: Path::new(GUEST_INPUT_ROOT).join("test-data-aac.mp4"),
                    },
                    subtitle: SubtitleSyncCommandSubtitleFile {
                        path: Path::new(GUEST_SUBTITLE_ROOT).join("early_2200.srt"),
                        format: "srt".to_string(),
                        file_name: Some("early_2200.srt".to_string()),
                        encoding_hint: None,
                    },
                    reference_subtitle: None,
                    output: SubtitleSyncCommandOutputTarget {
                        path: Path::new(GUEST_OUTPUT_ROOT).join("rewritten.srt"),
                        format: "srt".to_string(),
                    },
                    scratch_dir: PathBuf::from(GUEST_SCRATCH_ROOT),
                    media_metadata: None,
                    subtitle_spans: Vec::new(),
                    max_offset_seconds: 60,
                    sync_options: None,
                    selector: Some(AudioStreamSelector::Default),
                    expected_codec: None,
                }),
            },
        }),
    );

    let PluginSubtitleCommandResult::Sync(result) = result else {
        panic!("an align command must come back as a sync result");
    };
    let process = match result {
        PluginResult::Ok(process) => process,
        PluginResult::Err(error) => panic!("align refused in-band: {error:?}"),
    };
    let SubtitleSyncPluginResponse::Align { response } = process.response else {
        panic!("an align must come back as an align response");
    };

    assert!(
        response.applied,
        "the fixture is {FIXTURE_EARLY_MS}ms out of sync and must be corrected: {response:?}"
    );
    // A real correlation, not a no-op: the recovered offset has to be the
    // fixture's own desync within the tolerance the in-crate parity suite uses.
    assert!(
        (response.offset_ms - FIXTURE_EARLY_MS).abs() <= FIXTURE_TOLERANCE_MS,
        "recovered offset {}ms is not within {FIXTURE_TOLERANCE_MS}ms of the \
         fixture's {FIXTURE_EARLY_MS}ms desync",
        response.offset_ms
    );

    // The rewritten subtitle must be a real file in the writable preopen, at
    // the path the guest reported, because that is what the host reads back.
    let rewritten = response
        .rewritten_subtitle
        .as_ref()
        .expect("an applied align must name its rewritten subtitle");
    assert_eq!(
        rewritten.path,
        Path::new(GUEST_OUTPUT_ROOT).join("rewritten.srt")
    );
    assert_eq!(rewritten.format, "srt");

    let produced = std::fs::read(output_dir.path().join("rewritten.srt"))
        .expect("the guest must have written the rewritten subtitle into /output");
    assert!(
        !produced.is_empty(),
        "the rewritten subtitle must not be empty"
    );
    assert_ne!(
        produced, original,
        "the rewritten subtitle must differ from the desynced input"
    );

    // Alignment is filesystem work, not host-service work.
    assert!(
        store.data().script.calls.is_empty(),
        "align must not call host services, saw {:?}",
        store.data().script.calls
    );
}
