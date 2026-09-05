//! Conformance against the real Scryer download-client host, run on the
//! RELEASE artifact.
//!
//! The world is linked as `scryer:download-client/download-client@1.0.0`, the
//! shared `scryer:host/services@1.0.0` import is served by the scripted
//! stand-in for `CommandHost` in the crate root, WASI Preview 2 comes from the
//! linker, and `process` carries the `PluginCommandRequest` JSON envelope.
//!
//! The migration to the component ABI was transport-only, so the operation
//! bodies stay pinned by each plugin's own unit tests, which run unchanged.
//! What this suite adds is the half a unit test cannot reach: that the artifact
//! instantiates under the real world with the right import set, that `describe`
//! is pure, that `process` reaches host services through the one import, and
//! that a refused service stays in-band instead of becoming a world-level
//! `invocation-error`.

use std::path::{Path, PathBuf};

use scryer_plugin_sdk::SubtitlePluginValidateConfigRequest;
use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadClientCommandResult, PluginSubtitleCommand,
};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

use crate::{
    Ctx, DefaultResponder, DescriptorExpectation, HostErrorKind, HostResponder, Script,
    assert_describe_was_pure, build_plugin_wasm, descriptor_from,
};

mod download_client_world {
    wasmtime::component::bindgen!({
        world: "scryer:download-client/download-client@1.0.0",
        // Two packages, two paths — the same layout the host's bindgen uses,
        // and the same two files every plugin vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/download-client-v1.0.0"],
    });
}

pub use download_client_world::DownloadClient;
use download_client_world::scryer::host::services::{Host as ServicesHost, HostError};

impl<R: HostResponder> ServicesHost for Ctx<R> {
    /// The shared host import, standing in for Scryer's `CommandHost`.
    ///
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`, and answering that way here is what makes the in-band
    /// assertion meaningful.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        self.host_call_bytes(request).map_err(|error| match error {
            HostErrorKind::InvalidRequest => HostError::InvalidRequest,
            HostErrorKind::Failed => HostError::Failed,
        })
    }
}

// ---------------------------------------------------------------------------
// Driving the component
// ---------------------------------------------------------------------------

/// A store and an instantiated component, with the shared switchboard behind
/// the world's one import.
pub fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, DownloadClient) {
    instantiate_with(wasm_path, script, DefaultResponder)
}

/// The same, for a client that answers host-call variants of its own.
pub fn instantiate_with<R: HostResponder>(
    wasm_path: &Path,
    script: Script,
    responder: R,
) -> (Store<Ctx<R>>, DownloadClient) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile download-client component");
    let linker = linker::<R>(&engine);

    let mut store = Store::new(&engine, Ctx::new(script, responder));
    let plugin = DownloadClient::instantiate(&mut store, &component, &linker)
        .expect("instantiate the download-client component");
    (store, plugin)
}

fn linker<R: HostResponder>(engine: &Engine) -> Linker<Ctx<R>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    DownloadClient::add_to_linker::<Ctx<R>, HasSelf<Ctx<R>>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");
    linker
}

/// Send one download-client command through `process` and decode the family
/// result out of the envelope.
pub fn call_download_client<R: HostResponder>(
    store: &mut Store<Ctx<R>>,
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

// ---------------------------------------------------------------------------
// The family's default check set
// ---------------------------------------------------------------------------

/// One of the shared checks, for a client that opts out and asserts something
/// stronger locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    ArtifactIsAComponent,
    WorldConformance,
    Describe,
    ProcessReachesHostServices,
    RefusedHostStaysInBand,
    AnotherFamilyIsAnInvocationError,
}

/// The download-client conformance suite, configured per plugin.
pub struct DownloadClientConformance {
    manifest_dir: String,
    plugin_id: String,
    provider_type: String,
    wasm_name: String,
    descriptor: Vec<DescriptorExpectation>,
    skipped: Vec<Check>,
}

impl DownloadClientConformance {
    /// `manifest_dir` must be `env!("CARGO_MANIFEST_DIR")` expanded in the
    /// plugin's own test binary: inside this library the macro would resolve to
    /// this library's directory instead.
    pub fn new(manifest_dir: &str, plugin_id: &str) -> Self {
        Self {
            manifest_dir: manifest_dir.to_string(),
            plugin_id: plugin_id.to_string(),
            provider_type: plugin_id.to_string(),
            wasm_name: format!("{}_download_client.wasm", plugin_id.replace('-', "_")),
            descriptor: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// The release artifact's file name.
    pub fn wasm(mut self, wasm_name: &str) -> Self {
        self.wasm_name = wasm_name.to_string();
        self
    }

    /// The descriptor's `provider_type`, when it differs from the plugin id.
    pub fn provider_type(mut self, provider_type: &str) -> Self {
        self.provider_type = provider_type.to_string();
        self
    }

    /// An extra descriptor field this client pins.
    pub fn expects_descriptor(mut self, path: &[&str], value: serde_json::Value) -> Self {
        self.descriptor
            .push(DescriptorExpectation::new(path, value));
        self
    }

    /// Drop one shared check, because the plugin asserts something stronger in
    /// its own file.
    pub fn without(mut self, check: Check) -> Self {
        self.skipped.push(check);
        self
    }

    /// The built release artifact.
    pub fn wasm_path(&self) -> PathBuf {
        build_plugin_wasm(&self.manifest_dir, &self.wasm_name)
    }

    /// A Scryer configured with nothing at all — the family's baseline script.
    pub fn refusing_script(&self) -> Script {
        Script::default()
    }

    fn runs(&self, check: Check) -> bool {
        !self.skipped.contains(&check)
    }

    /// The family's default check set, in order.
    pub fn run(&self) {
        if self.runs(Check::ArtifactIsAComponent) {
            self.assert_artifact_is_a_component();
        }
        if self.runs(Check::WorldConformance) {
            self.assert_world_conformance();
        }
        if self.runs(Check::Describe) {
            self.assert_describe_returns_a_download_client_descriptor();
        }
        if self.runs(Check::ProcessReachesHostServices) {
            self.assert_process_reaches_host_services_over_the_one_import();
        }
        if self.runs(Check::RefusedHostStaysInBand) {
            self.assert_a_refused_host_stays_in_band();
        }
        if self.runs(Check::AnotherFamilyIsAnInvocationError) {
            self.assert_another_family_is_an_invocation_error();
        }
    }

    /// The download-client host has no core-module backing, so a core wasm
    /// artifact is not a degraded plugin but an uninstallable one.
    pub fn assert_artifact_is_a_component(&self) {
        crate::assert_artifact_is_a_component(&self.wasm_path());
    }

    /// The exact check the host performs on install: the artifact compiles,
    /// every import it emits is satisfiable from WASI Preview 2 plus the
    /// world's `services` interface, and its exports match
    /// `scryer:download-client/download-client@1.0.0`.
    ///
    /// This is also the regression guard for the *import set*. The PDK links
    /// one crate against two different component contracts, and a family
    /// component that accidentally keeps a live `scryer:indexer/host` import
    /// compiles perfectly and then fails to instantiate under this host.
    pub fn assert_world_conformance(&self) {
        let engine = Engine::default();
        let component = Component::from_file(&engine, self.wasm_path())
            .expect("compile download-client component");
        let linker = linker::<DefaultResponder>(&engine);
        linker
            .instantiate_pre(&component)
            .and_then(download_client_world::DownloadClientPre::new)
            .expect("the artifact must satisfy scryer:download-client/download-client@1.0.0");
    }

    /// `describe` is a world export now, not a `main` writing to stdout: the
    /// host calls it directly and parses the returned bytes as a
    /// `PluginDescriptor`.
    pub fn assert_describe_returns_a_download_client_descriptor(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.refusing_script());
        let bytes = plugin.call_describe(&mut store).expect("call describe");
        let descriptor = descriptor_from(&bytes);

        assert_eq!(descriptor["id"], self.plugin_id);
        // `ProviderDescriptor` is internally tagged on `kind`, so the client
        // fields sit alongside it rather than under a nested key.
        assert_eq!(descriptor["provider"]["kind"], "download_client");
        assert_eq!(descriptor["provider"]["provider_type"], self.provider_type);
        for expectation in &self.descriptor {
            expectation.assert(&descriptor);
        }

        assert_describe_was_pure(&store.data().script);
    }

    /// The transport the component migration installs is an injected `fn`
    /// pointer, bound by the entry macro at the top of *both* exports because a
    /// component instance does not survive a call. This is the assertion that
    /// it is actually bound on the `process` path: the plugin's very first act
    /// is to read its configuration, and that has to cross
    /// `scryer:host/services@1.0.0`.
    pub fn assert_process_reaches_host_services_over_the_one_import(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.refusing_script());
        let _ = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);

        let calls = &store.data().script.calls;
        assert!(
            calls.iter().any(|call| call.starts_with("config_get:")),
            "process must reach host services through the one import: {calls:?}"
        );
    }

    /// Capability availability is in-band. A host that refuses a service
    /// answers through the response, never through `host-error`, and the plugin
    /// must turn that into a well-formed command response — an `Ok` or a typed
    /// `PluginError`, but never a world-level `invocation-error` and never a
    /// trap. Otherwise the host loses the plugin's own diagnosis, and an
    /// unconfigured Scryer looks like a broken artifact.
    pub fn assert_a_refused_host_stays_in_band(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.refusing_script());
        let result =
            call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
        assert!(
            matches!(result, PluginDownloadClientCommandResult::ListQueue(_)),
            "a refused host must still produce a typed list_queue result: {result:?}"
        );
    }

    /// The one thing that *is* a world-level `invocation-error`: an envelope
    /// this plugin cannot answer at all.
    pub fn assert_another_family_is_an_invocation_error(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.refusing_script());
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
}
