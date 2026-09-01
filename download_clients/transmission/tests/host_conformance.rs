//! Conformance against the real Scryer host, run on the RELEASE artifact.
//!
//! This suite exists because the plugin's contract is not "these functions
//! behave" but "this exact `.wasm` runs under Scryer's download-client host".
//! It therefore builds the shipping `wasm32-wasip2` component and drives it the
//! way the host does: the world is linked as
//! `scryer:download-client/download-client@1.0.0`, the shared
//! `scryer:host/services@1.0.0` import is served by a scripted stand-in for
//! `CommandHost` speaking the same postcard
//! `PluginHostRequest`/`PluginHostResponse`, WASI Preview 2 comes from the
//! linker, and `process` carries the `PluginCommandRequest` JSON envelope.
//!
//! The migration was transport-only, so the operation bodies stay pinned by the
//! unit tests in `src/lib.rs`, which run unchanged. What this file adds is the
//! half a unit test cannot reach: that the artifact instantiates under the real
//! world with the right import set, that `describe` is pure, that `process`
//! reaches host services through the one import, and that a refused service
//! stays in-band instead of becoming a world-level `invocation-error`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use scryer_plugin_sdk::PluginResult;
use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadClientCommandResult, PluginSubtitleCommand,
};
use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};
use scryer_plugin_sdk::{PluginError, PluginErrorCode, SubtitlePluginValidateConfigRequest};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod download_client_world {
    wasmtime::component::bindgen!({
        world: "scryer:download-client/download-client@1.0.0",
        // Two packages, two paths — the same layout the host's bindgen uses,
        // and the same two files this crate vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/download-client-v1.0.0"],
    });
}

use download_client_world::DownloadClient;
use download_client_world::scryer::host::services::{Host as ServicesHost, HostError};

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn transmission_release_wasm_conforms_to_the_download_client_host_contract() {
    let wasm_path = plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_download_client_descriptor(&wasm_path);
    assert_process_reaches_host_services_over_the_one_import(&wasm_path);
    assert_a_refused_host_stays_in_band(&wasm_path);
    assert_another_family_is_an_invocation_error(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The download-client host has no core-module backing, so a core wasm artifact
/// is not a degraded plugin but an uninstallable one. Check the component
/// preamble directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path).expect("read plugin wasm");
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

/// The exact check the host performs on install: the artifact compiles, every
/// import it emits is satisfiable from WASI Preview 2 plus the world's
/// `services` interface, and its exports match
/// `scryer:download-client/download-client@1.0.0`.
///
/// This is also the regression guard for the *import set*. The PDK links one
/// crate against two different component contracts, and a family component that
/// accidentally keeps a live `scryer:indexer/host` import compiles perfectly and
/// then fails to instantiate under this host.
fn assert_world_conformance(wasm_path: &Path) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile download-client component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    DownloadClient::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");
    linker
        .instantiate_pre(&component)
        .and_then(download_client_world::DownloadClientPre::new)
        .expect("the artifact must satisfy scryer:download-client/download-client@1.0.0");
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

/// `describe` is a world export now, not a `main` writing to stdout: the host
/// calls it directly and parses the returned bytes as a `PluginDescriptor`.
fn assert_describe_returns_a_download_client_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path);
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "transmission");
    // `ProviderDescriptor` is internally tagged on `kind`, so the client fields
    // sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "download_client");
    assert_eq!(descriptor["provider"]["provider_type"], "transmission");

    // `describe` must be a pure function of the artifact: the host runs it
    // during packaging against an inert services import, so it may not touch
    // config, state, or HTTP.
    assert!(
        store.data().calls.is_empty(),
        "describe used host services: {:?}",
        store.data().calls
    );
}

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// The transport this migration installs is an injected `fn` pointer, bound by
/// the entry macro at the top of *both* exports because a component instance
/// does not survive a call. This is the assertion that it is actually bound on
/// the `process` path: the plugin's very first act is to read its configuration,
/// and that has to cross `scryer:host/services@1.0.0`.
fn assert_process_reaches_host_services_over_the_one_import(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path);
    let _ = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);

    let calls = &store.data().calls;
    assert!(
        calls.iter().any(|call| call.starts_with("config_get:")),
        "process must reach host services through the one import: {calls:?}"
    );
}

/// Capability availability is in-band. A host that refuses a service answers
/// through the response, never through `host-error`, and the plugin must turn
/// that into a well-formed command response — an `Ok` or a typed `PluginError`,
/// but never a world-level `invocation-error` and never a trap. Otherwise the
/// host loses the plugin's own diagnosis, and an unconfigured Scryer looks like
/// a broken artifact.
fn assert_a_refused_host_stays_in_band(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path);
    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    assert!(
        matches!(result, PluginDownloadClientCommandResult::ListQueue(_)),
        "a refused host must still produce a typed list_queue result: {result:?}"
    );
}

/// The one thing that *is* a world-level `invocation-error`: an envelope this
/// plugin cannot answer at all.
fn assert_another_family_is_an_invocation_error(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path);
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::Subtitle(
        PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
    )))
    .expect("encode a subtitle envelope");

    let outcome = plugin
        .call_process(&mut store, &request)
        .expect("process call itself succeeds");
    assert!(
        outcome.is_err(),
        "a subtitle command must not produce a download-client response"
    );
}

// ---------------------------------------------------------------------------
// Driving the component
// ---------------------------------------------------------------------------

fn call_download_client(
    store: &mut Store<Ctx>,
    plugin: &DownloadClient,
    command: PluginDownloadClientCommand,
) -> PluginDownloadClientCommandResult {
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::DownloadClient(
        command,
    )))
    .expect("encode the command envelope");
    let encoded = plugin
        .call_process(store, &request)
        .expect("process call itself succeeds")
        .expect("process returned an invocation-error");
    let response: PluginCommandResponse =
        serde_json::from_slice(&encoded).unwrap_or_else(|error| {
            panic!(
                "process did not return a command response ({error}): {}",
                String::from_utf8_lossy(&encoded)
            )
        });
    match response.response {
        PluginCommandResult::DownloadClient(result) => result,
        other => panic!("process answered another family: {other:?}"),
    }
}

fn instantiate(wasm_path: &Path) -> (Store<Ctx>, DownloadClient) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile download-client component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    DownloadClient::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            // No filesystem preopens, matching the world's documented
            // authority: this family has never been handed a preopened
            // directory, under the reactor, the wasip1 command host, or this
            // one.
            //
            // The host captures guest stderr and tails it into its own error
            // messages; inheriting it here puts the same text in front of
            // whoever is reading the test failure.
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            calls: Vec::new(),
        },
    );
    let plugin = DownloadClient::instantiate(&mut store, &component, &linker)
        .expect("instantiate the download-client component");
    (store, plugin)
}

// ---------------------------------------------------------------------------
// A scripted `CommandHost` that refuses everything, in-band
// ---------------------------------------------------------------------------

struct Ctx {
    table: ResourceTable,
    wasi: WasiCtx,
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
    /// The shared host import, standing in for a Scryer that has been
    /// configured with nothing at all.
    ///
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`, and answering that way here is what makes the in-band
    /// assertion meaningful.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.calls.push(format!("config_get:{}", request.key));
                PluginHostResponse::ConfigGet(PluginResult::Err(unsupported(
                    "this host is configured with nothing",
                )))
            }
            PluginHostRequest::StateGet(request) => {
                self.calls.push(format!("state_get:{}", request.key));
                PluginHostResponse::StateGet(PluginResult::Err(unsupported(
                    "no plugin state is available",
                )))
            }
            PluginHostRequest::StateSet(request) => {
                self.calls.push(format!("state_set:{}", request.key));
                PluginHostResponse::StateSet(PluginResult::Err(unsupported(
                    "no plugin state is available",
                )))
            }
            PluginHostRequest::StateDelete(request) => {
                self.calls.push(format!("state_delete:{}", request.key));
                PluginHostResponse::StateDelete(PluginResult::Err(unsupported(
                    "no plugin state is available",
                )))
            }
            PluginHostRequest::Http(request) => {
                self.calls.push(format!("http:{}", request.url));
                PluginHostResponse::Http(PluginResult::Err(unsupported(
                    "no HTTP egress is configured for this plugin",
                )))
            }
            other => {
                self.calls.push(format!("unscripted:{other:?}"));
                return Err(HostError::Failed);
            }
        };

        postcard::to_allocvec(&response).map_err(|_| HostError::Failed)
    }
}

/// The in-band "this host cannot do that" answer.
///
/// Every optional field is populated deliberately: `PluginError` carries
/// `skip_serializing_if` on `debug_message` and `retry_after_seconds`, which a
/// non-self-describing format like postcard cannot round-trip — a `None` there
/// produces bytes the guest decoder rejects outright. Until the SDK drops those
/// attributes, a host answering in-band must fill them in.
fn unsupported(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: message.to_string(),
        debug_message: Some(message.to_string()),
        retry_after_seconds: Some(0),
        details: None,
    }
}

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
                .expect("run cargo build for the plugin");
            assert!(status.success(), "plugin build failed: {status}");

            plugin_root
                .join("target/wasm32-wasip2/plugin-release/transmission_download_client.wasm")
        })
        .clone()
}
