//! Conformance against the real Scryer host, run on the RELEASE artifact.
//!
//! This suite exists because the plugin's contract is not "these functions
//! behave" but "this exact `.wasm` runs under Scryer's subtitle host". It
//! therefore builds the shipping `wasm32-wasip2` component and drives it the
//! way `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs`
//! does: the world is linked as `scryer:subtitle/subtitle-provider@1.1.0`,
//! the shared `scryer:host/services@1.0.0` import is served by a scripted
//! stand-in for `CommandHost` speaking the same postcard
//! `PluginHostRequest`/`PluginHostResponse`, WASI Preview 2 comes from the
//! linker, and `process` carries the `PluginCommandRequest` JSON envelope.
//!
//! A mismatch here means the artifact would fail in production, which is the
//! only failure mode this file is trying to catch.
//!
//! ## What is different about this plugin
//!
//! Every other family component reaches Scryer through `scryer:host/services`.
//! This one reaches the *filesystem*: alignment reads a media file and a
//! subtitle, and writes a rewritten subtitle back. Its authority is therefore
//! WASI preopens, not host services — five fixed roots the host stages per job
//! (`crates/scryer-plugins/src/subtitle_sync_adapter.rs`) — and this suite
//! reproduces that staging rather than scripting host calls.
//!
//! [`aligns_a_real_desynced_subtitle_inside_the_sandbox`] is consequently a
//! genuine end-to-end proof and not a dispatch round-trip: a real AAC fixture
//! and a real 2.2s-early SRT go in through the preopens, the FFmpeg-derived
//! decode, the libfvad VAD and the rustfft correlation all run *inside the
//! component*, and the rewritten subtitle is read back off the host
//! filesystem.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadGetCompletedRequest, PluginSubtitleCommand,
    PluginSubtitleCommandResult,
};
use scryer_plugin_sdk::host::{PluginConfigGetRequest, PluginHttpRequest as SdkPluginHttpRequest};
use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};
use scryer_plugin_sdk::{
    AudioStreamSelector, PluginError, PluginErrorCode, PluginResult, SubtitlePluginSearchRequest,
    SubtitlePluginValidateConfigRequest, SubtitleSyncCommandAlignRequest,
    SubtitleSyncCommandInputFile, SubtitleSyncCommandOutputTarget, SubtitleSyncCommandSubtitleFile,
    SubtitleSyncPluginOperation, SubtitleSyncPluginProcessRequest, SubtitleSyncPluginResponse,
    SubtitleSyncProbeRequest,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod subtitle_world {
    wasmtime::component::bindgen!({
        world: "scryer:subtitle/subtitle-provider@1.1.0",
        // Three packages, three paths — the same layout the host's bindgen uses,
        // and the same two files this crate vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/subtitle-v1.1.0"],
        // `process` is an `async func` and so are the runtime import's
        // `http` and `sleep`, so both directions are generated async —
        // the same pair of options the host's own bindgen passes.
        imports: { default: async },
        exports: { default: async },
    });
}

use subtitle_world::InvocationError;
use subtitle_world::SubtitleProvider;
use subtitle_world::scryer::host::services::{Host as ServicesHost, HostError};
use subtitle_world::scryer::runtime::host::{
    Header as RuntimeHeader, Host as RuntimeHost, HostWithStore as RuntimeHostWithStore,
    HttpRequest as RuntimeHttpRequest, HttpResponse as RuntimeHttpResponse,
    LogLevel as RuntimeLogLevel, TransportError as RuntimeTransportError,
};

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

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn enhanced_sync_release_wasm_conforms_to_the_subtitle_host_contract() {
    let wasm_path = plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_sync_subtitle_descriptor(&wasm_path);
    assert_probe_round_trips_the_sync_envelope(&wasm_path);
    assert_catalog_operations_are_unsupported_in_band(&wasm_path);
    assert_another_family_is_an_invocation_error(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The subtitle host has no core-module backing, so a core wasm artifact is
/// not a degraded plugin but an uninstallable one. Check the component
/// preamble directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path).expect("read enhanced-sync plugin wasm");
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

/// The exact check the host performs on install: the artifact compiles, every
/// import it emits is satisfiable from WASI Preview 2 plus the world's
/// `scryer:host/services` and `scryer:runtime/host` interfaces, and its
/// exports match `scryer:subtitle/subtitle-provider@1.1.0`.
///
/// This is also the regression guard for the *import set*. The PDK links one
/// crate against two different component contracts, and a family component
/// that accidentally keeps a live `scryer:indexer/host` import compiles
/// perfectly and then fails to instantiate under this host.
///
/// Note this artifact imports *fewer* interfaces than its siblings: it makes
/// no host calls at all, so LLVM eliminates the unused transport pointer and
/// `scryer:host/services@1.0.0` does not appear in its world. That is benign —
/// a linker may offer more than a component takes — and this assertion is what
/// proves it, because `SubtitleProviderPre::new` is exactly the host's own
/// instantiation path.
fn assert_world_conformance(wasm_path: &Path) {
    let engine = engine();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let linker = linker(&engine);
    linker
        .instantiate_pre(&component)
        .and_then(subtitle_world::SubtitleProviderPre::new)
        .expect("the artifact must satisfy scryer:subtitle/subtitle-provider@1.1.0");

    // The import *set*, not merely its satisfiability. The PDK compiles one
    // crate against two capability contracts, and a family component that
    // keeps a live `scryer:indexer/host` import links fine here and then fails
    // to instantiate under the real host. A subtitle artifact may name the
    // encoded services door and the typed runtime host, and nothing else
    // outside WASI.
    let non_wasi: Vec<String> = component
        .component_type()
        .imports(&engine)
        .map(|(name, _)| name.to_string())
        .filter(|name| !name.starts_with("wasi:"))
        .collect();
    for name in &non_wasi {
        assert!(
            matches!(
                name.as_str(),
                "scryer:host/services@1.0.0" | "scryer:runtime/host@1.0.0"
            ),
            "the artifact imports {name}, which no subtitle host serves: {non_wasi:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

/// `describe` is a world export now, not a Preview 1 command: the host calls
/// it directly and parses the returned bytes as a `PluginDescriptor`.
///
/// The descriptor is also what routes align jobs here — the loader looks up a
/// sync client by `provider_type`, and `PluginRuntimeBacking::for_artifact`
/// classifies on the `subtitle` provider kind — so those three fields are the
/// contract, not decoration.
fn assert_describe_returns_a_sync_subtitle_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Vec::new());
    let bytes = describe(&mut store, &plugin).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "enhanced-subtitle-sync");
    // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "subtitle");
    assert_eq!(
        descriptor["provider"]["provider_type"],
        "enhanced-subtitle-sync"
    );
    // This is what separates a sync plugin from a catalog provider sharing the
    // same world.
    assert_eq!(descriptor["provider"]["capabilities"]["mode"], "sync");
    assert!(
        descriptor["provider"]["capabilities"]["sync"]["command_model"]
            .as_bool()
            .unwrap_or(false),
        "the sync capability must advertise the command model"
    );

    // Describing must not need Scryer: the host calls `describe` during
    // install, before any services exist.
    assert!(
        store.data().calls.is_empty(),
        "describe must not call host services, saw {:?}",
        store.data().calls
    );
}

// ---------------------------------------------------------------------------
// The sync operation
// ---------------------------------------------------------------------------

/// `Probe` is the cheapest sync operation — it touches no filesystem — so it
/// isolates the part this migration actually changed: that a
/// `PluginSubtitleCommand::Sync` reaches the plugin's existing
/// `SubtitleSyncPluginProcessRequest` handler through the family envelope and
/// comes back as a well-formed `PluginSubtitleCommandResult::Sync`.
fn assert_probe_round_trips_the_sync_envelope(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Vec::new());
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
        store.data().calls.is_empty(),
        "a sync operation must not call host services, saw {:?}",
        store.data().calls
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
    let wasm_path = plugin_wasm();
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

    let preopens = vec![
        Preopen::read_only(fixtures.join("media"), GUEST_INPUT_ROOT),
        Preopen::read_only(subtitle_dir.path().to_path_buf(), GUEST_SUBTITLE_ROOT),
        Preopen::writable(output_dir.path().to_path_buf(), GUEST_OUTPUT_ROOT),
        Preopen::writable(scratch_dir.path().to_path_buf(), GUEST_SCRATCH_ROOT),
    ];

    let (mut store, plugin) = instantiate(&wasm_path, preopens);
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
        store.data().calls.is_empty(),
        "align must not call host services, saw {:?}",
        store.data().calls
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// This provider is `mode: Sync`. The four catalog operations now reach every
/// subtitle plugin because they share one envelope, so each must be refused
/// with a typed `Unsupported` rather than a trap — the host keeps a diagnosis
/// it can show an operator instead of a generic invocation error.
fn assert_catalog_operations_are_unsupported_in_band(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Vec::new());

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
        store.data().calls.is_empty(),
        "refusals must not reach host services, saw {:?}",
        store.data().calls
    );
}

/// A wrong-family envelope is not a typed refusal but a world-level
/// `invocation-error`: the guest cannot produce a `PluginSubtitleCommandResult`
/// for a download-client request at all, and the host must see that as a
/// protocol failure rather than a plugin opinion.
fn assert_another_family_is_an_invocation_error(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Vec::new());
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::DownloadClient(
        PluginDownloadClientCommand::GetCompleted(PluginDownloadGetCompletedRequest {
            client_item_id: "opaque".to_string(),
        }),
    )))
    .expect("encode a download-client envelope");

    let outcome =
        process(&mut store, &plugin, request).expect("the process export itself must not trap");
    assert!(
        outcome.is_err(),
        "a download-client envelope must be a world invocation-error, got a response"
    );
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Send one subtitle command through `process` and decode the family result.
fn call_subtitle(
    store: &mut Store<Ctx>,
    plugin: &SubtitleProvider,
    command: PluginSubtitleCommand,
) -> PluginSubtitleCommandResult {
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::Subtitle(command)))
        .expect("encode the command envelope");
    let bytes = process(store, plugin, request)
        .expect("call process")
        .unwrap_or_else(|error| panic!("process reported a world invocation-error: {error:?}"));
    let response: PluginCommandResponse =
        serde_json::from_slice(&bytes).expect("decode the response envelope");
    match response.response {
        PluginCommandResult::Subtitle(result) => result,
        other => panic!("expected a subtitle result, got {other:?}"),
    }
}

struct Preopen {
    host_path: PathBuf,
    guest_path: &'static str,
    writable: bool,
}

impl Preopen {
    fn read_only(host_path: PathBuf, guest_path: &'static str) -> Self {
        Self {
            host_path,
            guest_path,
            writable: false,
        }
    }

    fn writable(host_path: PathBuf, guest_path: &'static str) -> Self {
        Self {
            host_path,
            guest_path,
            writable: true,
        }
    }
}

fn instantiate(wasm_path: &Path, preopens: Vec<Preopen>) -> (Store<Ctx>, SubtitleProvider) {
    let engine = engine();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let linker = linker(&engine);

    // The host captures guest stderr and tails it into its own error messages;
    // inheriting it here puts the same text in front of whoever is reading the
    // test failure.
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    for preopen in &preopens {
        let perms = if preopen.writable {
            FsPerms::ReadWrite
        } else {
            FsPerms::ReadOnly
        };
        builder
            .preopened_dir(&preopen.host_path, preopen.guest_path, perms)
            .unwrap_or_else(|error| {
                panic!(
                    "preopen {} as {}: {error}",
                    preopen.host_path.display(),
                    preopen.guest_path
                )
            });
    }

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            state: BTreeMap::new(),
            wasi: builder.build(),
            calls: Vec::new(),
        },
    );
    let plugin = runtime()
        .block_on(SubtitleProvider::instantiate_async(
            &mut store, &component, &linker,
        ))
        .expect("instantiate the subtitle component");
    (store, plugin)
}

// ---------------------------------------------------------------------------
// Driving a 1.1 artifact
// ---------------------------------------------------------------------------

/// The engine the subtitle host builds, minus the parts no subtitle guest
/// reaches.
///
/// `wasm_component_model_async` is the load-bearing line. Since
/// `scryer:subtitle@1.1.0` the world's `process` is an `async func`, and an
/// engine without component-model async rejects the artifact outright rather
/// than quietly degrading to a blocking call.
fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).expect("build the subtitle engine")
}

/// One current-thread runtime, shared by every helper below.
///
/// The assertions in this file stay synchronous on purpose: 1.1 changed how
/// the guest is driven, not what it must do, so the driving layer absorbs the
/// async and every expectation reads exactly as it did against 1.0.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a current-thread runtime")
    })
}

/// The process-relative origin the typed `monotonic-now-ms` counts from.
fn clock_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// WASI Preview 2 plus both of the world's Scryer imports.
///
/// Registering the typed runtime is not optional even for a provider that
/// never calls it: the linker has to be able to satisfy the whole world, and
/// a provider that does reach it must meet the same scripted host the encoded
/// door meets.
fn linker(engine: &Engine) -> Linker<Ctx> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).expect("register WASI Preview 2");
    SubtitleProvider::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services and the typed runtime");
    linker
}

/// `describe` is still a plain synchronous export; only the store is async.
fn describe(store: &mut Store<Ctx>, plugin: &SubtitleProvider) -> wasmtime::Result<Vec<u8>> {
    runtime().block_on(plugin.call_describe(&mut *store))
}

/// `process` is an `async func`, so its task runs on the store's concurrent
/// scheduler through `run_concurrent` — exactly how the production host drives
/// it — rather than being called straight through the store.
fn process(
    store: &mut Store<Ctx>,
    plugin: &SubtitleProvider,
    request: Vec<u8>,
) -> wasmtime::Result<Result<Vec<u8>, InvocationError>> {
    runtime().block_on(async move {
        store
            .run_concurrent(async move |accessor| plugin.call_process(accessor, request).await)
            .await?
    })
}

// ---------------------------------------------------------------------------
// A scripted `CommandHost`
// ---------------------------------------------------------------------------

struct Ctx {
    table: ResourceTable,
    wasi: WasiCtx,
    /// Plugin-owned state behind the typed `state-get`/`state-cas` pair.
    ///
    /// The encoded door's `StateGet` stays scripted; this is a real map
    /// because the typed pair's whole advantage over it is that the
    /// compare-and-swap is atomic, and a gate spinning on it has to read back
    /// the value it just wrote.
    state: BTreeMap<String, Vec<u8>>,
    /// Every host call the guest makes. This plugin should make none — its
    /// authority is the preopens, not Scryer — so the assertions read this as
    /// "stayed inside its sandbox" rather than as a transcript.
    calls: Vec<String>,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl ServicesHost for Ctx {
    /// The shared host import, standing in for Scryer's `CommandHost`.
    ///
    /// This plugin's production spec is `CommandHost::disabled()`, which
    /// answers every request in-band with `Unsupported`; that is reproduced
    /// here rather than a richer script, so a plugin that started reaching for
    /// a capability would get the same typed refusal it gets in production
    /// instead of a convenient fixture.
    ///
    /// `host-error` stays reserved for the transport: a request that cannot be
    /// decoded.
    async fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;
        let response = self.dispatch(request)?;
        postcard::to_allocvec(&response).map_err(|_| HostError::Failed)
    }
}

impl Ctx {
    /// The one host implementation behind both of the world's doors.
    ///
    /// 1.1 gives a provider two ways to reach the same capabilities — the
    /// encoded `host-call` and the typed `scryer:runtime/host` — and the
    /// production host answers both from a single `CommandHost` under one
    /// lock. This stand-in does the same, so moving a call from one door to
    /// the other cannot change what a provider observes.
    fn dispatch(&mut self, request: PluginHostRequest) -> Result<PluginHostResponse, HostError> {
        self.calls.push(format!("{request:?}"));

        let response = match request {
            PluginHostRequest::ConfigGet(_) => PluginHostResponse::ConfigGet(PluginResult::Err(
                unsupported("this plugin is offered no host services"),
            )),
            other => {
                self.calls.push(format!("unscripted:{other:?}"));
                return Err(HostError::Failed);
            }
        };

        Ok(response)
    }
}

/// The in-band "this host cannot do that" answer.
///
/// Every optional field is populated deliberately: the published SDK carries
/// `skip_serializing_if` on `debug_message` and `retry_after_seconds`, which a
/// non-self-describing format like postcard cannot round-trip — a `None` there
/// produces bytes the guest decoder rejects outright. The in-repo SDK this
/// crate patches to has that fixed, but filling them in keeps the stand-in
/// honest against either.
fn unsupported(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: message.to_string(),
        debug_message: Some(message.to_string()),
        retry_after_seconds: Some(0),
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the enhanced-sync plugin");
            assert!(
                status.success(),
                "enhanced-sync plugin build failed: {status}"
            );

            plugin_root
                .join("target/wasm32-wasip2/plugin-release/enhanced_subtitle_sync_plugin.wasm")
        })
        .clone()
}

/// The typed, family-neutral runtime import of contract 1.1.
///
/// It answers from the same fixture and records into the same call log as
/// [`ServicesHost::host_call`], because the production host answers both doors
/// from a single `CommandHost`. A provider that moves a call from the encoded
/// door to the typed one therefore meets the same script and the same
/// assertions.
impl RuntimeHost for Ctx {
    async fn monotonic_now_ms(&mut self) -> u64 {
        clock_origin()
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// A generous but finite invocation budget, so a guest pacing gate takes
    /// its wait rather than deferring, and a runaway one still terminates.
    async fn operation_deadline_monotonic_ms(&mut self) -> u64 {
        self.monotonic_now_ms().await.saturating_add(30_000)
    }

    async fn wall_now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    async fn config_get(&mut self, key: String) -> Option<String> {
        match self.dispatch(PluginHostRequest::ConfigGet(PluginConfigGetRequest { key })) {
            Ok(PluginHostResponse::ConfigGet(PluginResult::Ok(response))) => response.value,
            _ => None,
        }
    }

    /// Subtitle instances carry no provider profile, so shared engine code
    /// linked into a subtitle component has to treat `none` as "use your
    /// defaults". Answering `none` here is what proves it does.
    async fn provider_profile(&mut self) -> Option<Vec<u8>> {
        None
    }

    async fn state_get(&mut self, key: String) -> Option<Vec<u8>> {
        self.calls.push(format!("runtime-state-get:{key}"));
        self.state.get(&key).cloned()
    }

    /// A real compare-and-swap, because that atomicity is the whole reason the
    /// typed pair exists alongside the encoded one, and a guest pacing gate
    /// spins on it.
    async fn state_cas(
        &mut self,
        key: String,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
    ) -> bool {
        self.calls.push(format!("runtime-state-cas:{key}"));
        if self.state.get(&key).cloned() != expected {
            return false;
        }
        match replacement {
            Some(value) => self.state.insert(key, value),
            None => self.state.remove(&key),
        };
        true
    }

    async fn log(&mut self, level: RuntimeLogLevel, message: String) {
        eprintln!("[guest {level:?}] {message}");
    }
}

/// The concurrent half of the runtime import: its two `async func`s.
impl RuntimeHostWithStore<Ctx> for HasSelf<Ctx> {
    /// One HTTP attempt, re-encoded onto the very same scripted route table
    /// the encoded door uses, so URL and call-log assertions hold whichever
    /// door a provider reaches for.
    async fn http(
        accessor: &wasmtime::component::Accessor<Ctx, Self>,
        request: RuntimeHttpRequest,
    ) -> Result<RuntimeHttpResponse, RuntimeTransportError> {
        let encoded = PluginHostRequest::Http(SdkPluginHttpRequest {
            url: request.url,
            method: Some(request.method),
            headers: request
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: request.body,
        });

        match accessor.with(|mut access| access.get().dispatch(encoded)) {
            Ok(PluginHostResponse::Http(PluginResult::Ok(response))) => Ok(RuntimeHttpResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|(name, value)| RuntimeHeader { name, value })
                    .collect(),
                body: response.body,
            }),
            // The scripted host refuses in band, which is what a Scryer
            // enforcing an egress policy does. The typed door has no in-band
            // channel, so that refusal has to land on one of the seven cases.
            Ok(_) => Err(RuntimeTransportError::ForbiddenOrigin),
            Err(_) => Err(RuntimeTransportError::Transport),
        }
    }

    /// Real time, capped so a guest cannot park the suite.
    async fn sleep(accessor: &wasmtime::component::Accessor<Ctx, Self>, duration_ms: u64) {
        let _ = accessor;
        tokio::time::sleep(Duration::from_millis(duration_ms.min(50))).await;
    }
}
