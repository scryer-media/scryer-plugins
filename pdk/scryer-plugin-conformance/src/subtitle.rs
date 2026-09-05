//! Conformance against the real Scryer subtitle host, run on the RELEASE
//! artifact.
//!
//! The world is linked as `scryer:subtitle/subtitle-provider@1.1.0` the way
//! `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs` links
//! it, the shared `scryer:host/services@1.0.0` import is served by the scripted
//! stand-in for `CommandHost` in the crate root, WASI Preview 2 comes from the
//! linker, and `process` carries the `PluginCommandRequest` JSON envelope.
//!
//! Since `scryer:subtitle@1.1.0` the world's `process` is an `async func`, and
//! the runtime import's `http` and `sleep` are too. The driving layer below
//! absorbs that: 1.1 changed how the guest is driven, not what it must do, so
//! every assertion reads exactly as it did against 1.0.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadGetCompletedRequest, PluginSubtitleCommand,
    PluginSubtitleCommandResult,
};
use scryer_plugin_sdk::host::{
    PluginConfigGetRequest, PluginHostRequest, PluginHostResponse,
    PluginHttpRequest as SdkPluginHttpRequest,
};
use scryer_plugin_sdk::{
    PluginErrorCode, PluginResult, SubtitleGeneratorInputRef, SubtitlePluginDownloadRequest,
    SubtitlePluginGenerateRequest, SubtitlePluginValidateConfigRequest, SubtitleQueryMediaKind,
    SubtitleValidateConfigStatus,
};
use std::collections::BTreeMap;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::WasiCtx;

use crate::{
    ConfigSource, Ctx, DefaultResponder, DescriptorExpectation, HostErrorKind, HostResponder,
    HttpReply, HttpRoute, HttpScript, Script, StateSource, assert_describe_was_pure,
    build_plugin_wasm, descriptor_from,
};

mod subtitle_world {
    wasmtime::component::bindgen!({
        world: "scryer:subtitle/subtitle-provider@1.1.0",
        // Three packages, three paths — the same layout the host's bindgen
        // uses, and the same three files every provider vendors for its own
        // guest bindings.
        path: ["wit/host-v1.0.0", "wit/runtime-v1.0.0", "wit/subtitle-v1.1.0"],
        // `process` is an `async func` and so are the runtime import's `http`
        // and `sleep`, so both directions are generated async — the same pair
        // of options the host's own bindgen passes.
        imports: { default: async },
        exports: { default: async },
    });
}

pub use subtitle_world::InvocationError;
pub use subtitle_world::SubtitleProvider;
use subtitle_world::scryer::host::services::{Host as ServicesHost, HostError};
use subtitle_world::scryer::runtime::host::{
    Header as RuntimeHeader, Host as RuntimeHost, HostWithStore as RuntimeHostWithStore,
    HttpRequest as RuntimeHttpRequest, HttpResponse as RuntimeHttpResponse,
    LogLevel as RuntimeLogLevel, TransportError as RuntimeTransportError,
};

impl<R: HostResponder> ServicesHost for Ctx<R> {
    /// The shared host import, standing in for Scryer's `CommandHost`.
    ///
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`.
    async fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        self.host_call_bytes(request).map_err(|error| match error {
            HostErrorKind::InvalidRequest => HostError::InvalidRequest,
            HostErrorKind::Failed => HostError::Failed,
        })
    }
}

/// The typed, family-neutral runtime import of contract 1.1.
///
/// It answers from the same script and records into the same call log as
/// [`ServicesHost::host_call`], because the production host answers both doors
/// from a single `CommandHost` under one lock. A provider that moves a call
/// from the encoded door to the typed one therefore meets the same script and
/// the same assertions.
impl<R: HostResponder> RuntimeHost for Ctx<R> {
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
        self.script.calls.push(format!("runtime-state-get:{key}"));
        self.script.runtime_state.get(&key).cloned()
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
        self.script.calls.push(format!("runtime-state-cas:{key}"));
        if self.script.runtime_state.get(&key).cloned() != expected {
            return false;
        }
        match replacement {
            Some(value) => self.script.runtime_state.insert(key, value),
            None => self.script.runtime_state.remove(&key),
        };
        true
    }

    async fn log(&mut self, level: RuntimeLogLevel, message: String) {
        eprintln!("[guest {level:?}] {message}");
    }
}

/// The concurrent half of the runtime import: its two `async func`s.
impl<R: HostResponder> RuntimeHostWithStore<Ctx<R>> for HasSelf<Ctx<R>> {
    /// One HTTP attempt, re-encoded onto the very same scripted route table the
    /// encoded door uses, so URL and call-log assertions hold whichever door a
    /// provider reaches for.
    async fn http(
        accessor: &wasmtime::component::Accessor<Ctx<R>, Self>,
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
    async fn sleep(accessor: &wasmtime::component::Accessor<Ctx<R>, Self>, duration_ms: u64) {
        let _ = accessor;
        tokio::time::sleep(Duration::from_millis(duration_ms.min(50))).await;
    }
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
pub fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).expect("build the subtitle engine")
}

/// One current-thread runtime, shared by every helper below.
///
/// The assertions stay synchronous on purpose: the driving layer absorbs the
/// async and every expectation reads exactly as it did against 1.0.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a current-thread runtime")
    })
}

/// The process-relative origin the typed `monotonic-now-ms` counts from.
pub fn clock_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// WASI Preview 2 plus both of the world's Scryer imports.
///
/// Registering the typed runtime is not optional even for a provider that never
/// calls it: the linker has to be able to satisfy the whole world, and a
/// provider that does reach it must meet the same scripted host the encoded
/// door meets.
pub fn linker<R: HostResponder>(engine: &Engine) -> Linker<Ctx<R>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).expect("register WASI Preview 2");
    SubtitleProvider::add_to_linker::<Ctx<R>, HasSelf<Ctx<R>>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services and the typed runtime");
    linker
}

/// A store and an instantiated component, with the shared switchboard behind
/// both of the world's doors.
pub fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, SubtitleProvider) {
    instantiate_with(wasm_path, script, DefaultResponder)
}

/// The same, for a provider that answers host-call variants of its own — the
/// tsukihime pilot's `ArchiveExtract`.
pub fn instantiate_with<R: HostResponder>(
    wasm_path: &Path,
    script: Script,
    responder: R,
) -> (Store<Ctx<R>>, SubtitleProvider) {
    instantiate_in(wasm_path, Ctx::new(script, responder))
}

/// The same, for a provider whose authority is WASI preopens rather than host
/// services — the sync family, whose roots the host stages per job.
pub fn instantiate_with_wasi<R: HostResponder>(
    wasm_path: &Path,
    script: Script,
    responder: R,
    wasi: WasiCtx,
) -> (Store<Ctx<R>>, SubtitleProvider) {
    instantiate_in(wasm_path, Ctx::with_wasi(script, responder, wasi))
}

fn instantiate_in<R: HostResponder>(
    wasm_path: &Path,
    ctx: Ctx<R>,
) -> (Store<Ctx<R>>, SubtitleProvider) {
    let engine = engine();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let linker = linker::<R>(&engine);

    let mut store = Store::new(&engine, ctx);
    let plugin = runtime()
        .block_on(SubtitleProvider::instantiate_async(
            &mut store, &component, &linker,
        ))
        .expect("instantiate the subtitle component");
    (store, plugin)
}

/// `describe` is still a plain synchronous export; only the store is async.
pub fn describe<R: HostResponder>(
    store: &mut Store<Ctx<R>>,
    plugin: &SubtitleProvider,
) -> wasmtime::Result<Vec<u8>> {
    runtime().block_on(plugin.call_describe(&mut *store))
}

/// `process` is an `async func`, so its task runs on the store's concurrent
/// scheduler through `run_concurrent` — exactly how the production host drives
/// it — rather than being called straight through the store.
pub fn process<R: HostResponder>(
    store: &mut Store<Ctx<R>>,
    plugin: &SubtitleProvider,
    request: Vec<u8>,
) -> wasmtime::Result<Result<Vec<u8>, InvocationError>> {
    runtime().block_on(async move {
        store
            .run_concurrent(async move |accessor| plugin.call_process(accessor, request).await)
            .await?
    })
}

/// Send one subtitle command through `process` and decode the family result out
/// of the envelope.
pub fn call_subtitle<R: HostResponder>(
    store: &mut Store<Ctx<R>>,
    plugin: &SubtitleProvider,
    command: PluginSubtitleCommand,
) -> PluginSubtitleCommandResult {
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::Subtitle(command)))
        .expect("encode the command envelope");
    let encoded = process(store, plugin, request)
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
        PluginCommandResult::Subtitle(result) => result,
        other => panic!("process answered another family: {other:?}"),
    }
}

/// The generate request the family probes an absent generator with.
pub fn generate_request() -> SubtitlePluginGenerateRequest {
    SubtitlePluginGenerateRequest {
        media_kind: SubtitleQueryMediaKind::Episode,
        facet: None,
        input: SubtitleGeneratorInputRef {
            path: PathBuf::from("/scryer/input/Show.mkv"),
            mime_type: "video/x-matroska".to_string(),
            duration_seconds: 1_400,
            size_bytes: 1_024,
            checksum: "blake3:0".to_string(),
        },
        languages: vec!["eng".to_string()],
    }
}

// ---------------------------------------------------------------------------
// The family's default check set
// ---------------------------------------------------------------------------

/// One of the shared checks, for a provider that opts out and asserts something
/// stronger locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    ArtifactIsAComponent,
    WorldConformance,
    Describe,
    ValidateConfig,
    Download,
    RefusedHostCapability,
    GenerateIsUnsupported,
    AnotherFamilyIsAnInvocationError,
}

/// How an expected URL is matched.
#[derive(Clone, Debug)]
enum UrlShape {
    None,
    Exact(String),
    Prefix(String),
}

/// The subtitle conformance suite, configured per provider.
pub struct SubtitleConformance {
    manifest_dir: String,
    wasm_name: String,
    descriptor_id: String,
    provider_type: String,
    mode: Option<String>,
    descriptor: Vec<DescriptorExpectation>,
    config: BTreeMap<String, String>,

    validate_http: Vec<HttpRoute>,
    validate_config_keys: Vec<String>,
    validate_url: UrlShape,
    validate_url_contains: Vec<String>,
    validate_calls: Vec<String>,
    validate_call_prefixes: Vec<String>,
    validate_forbids_http: bool,

    download_http: Vec<HttpRoute>,
    download_reference: String,
    download_bytes: Option<Vec<u8>>,
    download_format: Option<String>,
    download_filename: Option<String>,
    download_content_type: Option<String>,
    download_calls: Vec<String>,

    requires_services_import: bool,
    refused_code: Option<PluginErrorCode>,
    refused_not_code: Option<PluginErrorCode>,
    refused_message_contains: Option<String>,

    skipped: Vec<Check>,
}

impl SubtitleConformance {
    /// `manifest_dir` must be `env!("CARGO_MANIFEST_DIR")` expanded in the
    /// provider's own test binary: inside this library the macro would resolve
    /// to this library's directory instead.
    pub fn new(manifest_dir: &str, provider_type: &str) -> Self {
        Self {
            manifest_dir: manifest_dir.to_string(),
            wasm_name: format!("{}_subtitle_provider.wasm", provider_type.replace('-', "_")),
            descriptor_id: provider_type.to_string(),
            provider_type: provider_type.to_string(),
            mode: Some("catalog".to_string()),
            descriptor: Vec::new(),
            config: BTreeMap::new(),

            validate_http: Vec::new(),
            validate_config_keys: Vec::new(),
            validate_url: UrlShape::None,
            validate_url_contains: Vec::new(),
            validate_calls: Vec::new(),
            validate_call_prefixes: Vec::new(),
            validate_forbids_http: false,

            download_http: Vec::new(),
            download_reference: String::new(),
            download_bytes: None,
            download_format: None,
            download_filename: None,
            download_content_type: None,
            download_calls: Vec::new(),

            requires_services_import: true,
            refused_code: None,
            refused_not_code: None,
            refused_message_contains: None,

            skipped: Vec::new(),
        }
    }

    /// The release artifact's file name.
    pub fn wasm(mut self, wasm_name: &str) -> Self {
        self.wasm_name = wasm_name.to_string();
        self
    }

    /// The descriptor's `id`, when it differs from the provider type — the
    /// families that share a plugin directory with an indexer suffix theirs.
    pub fn descriptor_id(mut self, id: &str) -> Self {
        self.descriptor_id = id.to_string();
        self
    }

    /// The mode this provider advertises. `catalog` by default.
    pub fn mode(mut self, mode: &str) -> Self {
        self.mode = Some(mode.to_string());
        self
    }

    /// An extra descriptor field this provider pins.
    pub fn expects_descriptor(mut self, path: &[&str], value: serde_json::Value) -> Self {
        self.descriptor
            .push(DescriptorExpectation::new(path, value));
        self
    }

    /// One setting Scryer would have resolved for this provider.
    pub fn config(mut self, key: &str, value: &str) -> Self {
        self.config.insert(key.to_string(), value.to_string());
        self
    }

    /// One scripted upstream response for the `validate_config` probe,
    /// selected by a substring of the request URL. A provider with a single hop
    /// passes `""`, which matches everything.
    pub fn validate_route(mut self, url_contains: &str, status: u16, body: Vec<u8>) -> Self {
        self.validate_http.push(HttpRoute::contains(
            url_contains,
            HttpReply::new(status, body),
        ));
        self
    }

    /// A configuration key `validate_config` must read through host services.
    pub fn validate_reads_config(mut self, key: &str) -> Self {
        self.validate_config_keys.push(key.to_string());
        self
    }

    /// The whole URL of the probe, verbatim.
    pub fn validate_url(mut self, url: &str) -> Self {
        self.validate_url = UrlShape::Exact(url.to_string());
        self
    }

    /// The advertised base the probe must be built on, with the query string
    /// left to the assertions below.
    pub fn validate_url_prefix(mut self, prefix: &str) -> Self {
        self.validate_url = UrlShape::Prefix(prefix.to_string());
        self
    }

    /// Something the probe URL must carry — the key read from config, or the
    /// fixed title a provider probes with.
    pub fn validate_url_contains(mut self, text: &str) -> Self {
        self.validate_url_contains.push(text.to_string());
        self
    }

    /// Another call `validate_config` must make, in full.
    pub fn validate_expects_call(mut self, call: &str) -> Self {
        self.validate_calls.push(call.to_string());
        self
    }

    /// `validate_config` must also make a call whose recorded form starts with
    /// this prefix — the shape for "it read *some* key from host state"
    /// without pinning the key a provider is free to rename.
    pub fn validate_expects_call_prefix(mut self, prefix: &str) -> Self {
        self.validate_call_prefixes.push(prefix.to_string());
        self
    }

    /// This provider validates locally. A validation that quietly started
    /// calling upstream would be a new egress from a code path Scryer runs on
    /// every settings save.
    pub fn validate_makes_no_http(mut self) -> Self {
        self.validate_forbids_http = true;
        self
    }

    /// One scripted upstream response for the `download`, selected by a
    /// substring of the request URL.
    pub fn download_route(mut self, url_contains: &str, status: u16, body: Vec<u8>) -> Self {
        self.download_http.push(HttpRoute::contains(
            url_contains,
            HttpReply::new(status, body),
        ));
        self
    }

    /// The reference `search` embeds in `provider_file_id`, as the provider
    /// builds it from one upstream row.
    pub fn download_reference(mut self, reference: &str) -> Self {
        self.download_reference = reference.to_string();
        self
    }

    /// The bytes that must reach Scryer, untouched.
    pub fn download_expects_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.download_bytes = Some(bytes);
        self
    }

    pub fn download_expects_format(mut self, format: &str) -> Self {
        self.download_format = Some(format.to_string());
        self
    }

    pub fn download_expects_filename(mut self, filename: &str) -> Self {
        self.download_filename = Some(filename.to_string());
        self
    }

    pub fn download_expects_content_type(mut self, content_type: &str) -> Self {
        self.download_content_type = Some(content_type.to_string());
        self
    }

    /// A call the download must make, in full — one per upstream hop.
    pub fn download_expects_call(mut self, call: &str) -> Self {
        self.download_calls.push(call.to_string());
        self
    }

    /// The typed error code a refused host capability must surface as.
    pub fn refused_expects_code(mut self, code: PluginErrorCode) -> Self {
        self.refused_code = Some(code);
        self
    }

    /// This artifact imports *fewer* interfaces than its siblings: it makes no
    /// host calls at all, so LLVM eliminates the unused transport pointer and
    /// `scryer:host/services@1.0.0` does not appear in its world. That is
    /// benign — a linker may offer more than a component takes — and
    /// `SubtitleProviderPre::new` is still exactly the host's own
    /// instantiation path.
    pub fn without_services_import(mut self) -> Self {
        self.requires_services_import = false;
        self
    }

    /// The refusal must not be reported as a missing capability: a provider
    /// whose only upstream hop was refused has the capability, it just could
    /// not use it.
    pub fn refused_is_not_code(mut self, code: PluginErrorCode) -> Self {
        self.refused_not_code = Some(code);
        self
    }

    /// Text the refusal's own message must keep, for a provider that reports
    /// the transport failure it saw rather than a code.
    pub fn refused_expects_message_contains(mut self, text: &str) -> Self {
        self.refused_message_contains = Some(text.to_string());
        self
    }

    /// Drop one shared check, because the provider asserts something stronger
    /// in its own file.
    pub fn without(mut self, check: Check) -> Self {
        self.skipped.push(check);
        self
    }

    /// The built release artifact.
    pub fn wasm_path(&self) -> PathBuf {
        build_plugin_wasm(&self.manifest_dir, &self.wasm_name)
    }

    /// The configuration Scryer would have resolved for this provider, with a
    /// host that refuses every request — the "no HTTP response scripted" case
    /// the in-band assertion needs.
    pub fn script(&self) -> Script {
        Script {
            config: ConfigSource::Resolved(self.config.clone()),
            state: StateSource::Ephemeral,
            http: HttpScript::Refused,
            ..Script::default()
        }
    }

    /// The same, with a route table behind the host's HTTP service.
    pub fn script_with_routes(&self, routes: Vec<HttpRoute>) -> Script {
        Script {
            http: HttpScript::Routed(routes),
            ..self.script()
        }
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
            self.assert_describe_returns_a_subtitle_descriptor();
        }
        if self.runs(Check::ValidateConfig) {
            self.assert_validate_config_reaches_the_host_services();
        }
        if self.runs(Check::Download) {
            self.assert_download_streams_the_file_through_host_http();
        }
        if self.runs(Check::RefusedHostCapability) {
            self.assert_a_refused_host_capability_stays_in_band();
        }
        if self.runs(Check::GenerateIsUnsupported) {
            self.assert_generate_is_unsupported_in_band();
        }
        if self.runs(Check::AnotherFamilyIsAnInvocationError) {
            self.assert_another_family_is_an_invocation_error();
        }
    }

    /// The subtitle host has no core-module backing, so a core wasm artifact is
    /// not a degraded plugin but an uninstallable one.
    pub fn assert_artifact_is_a_component(&self) {
        crate::assert_artifact_is_a_component(&self.wasm_path());
    }

    /// The exact check the host performs on install: the artifact compiles,
    /// every import it emits is satisfiable from WASI Preview 2 plus the
    /// world's `scryer:host/services` and `scryer:runtime/host` interfaces, and
    /// its exports match `scryer:subtitle/subtitle-provider@1.1.0`.
    ///
    /// This is also the regression guard for the *import set*. The PDK compiles
    /// one crate against two capability contracts, and a family component that
    /// keeps a live `scryer:indexer/host` import links fine here and then fails
    /// to instantiate under the real host. A subtitle artifact may name the
    /// encoded services door and the typed runtime host, and nothing else
    /// outside WASI.
    pub fn assert_world_conformance(&self) {
        let engine = engine();
        let component =
            Component::from_file(&engine, self.wasm_path()).expect("compile subtitle component");
        let linker = linker::<DefaultResponder>(&engine);
        linker
            .instantiate_pre(&component)
            .and_then(subtitle_world::SubtitleProviderPre::new)
            .expect("the artifact must satisfy scryer:subtitle/subtitle-provider@1.1.0");

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
        // The typed runtime import is absent unless reachable code calls one of
        // its functions — the linker drops an import nothing loads — so only
        // the encoded door is required to be present.
        if self.requires_services_import {
            assert!(
                non_wasi
                    .iter()
                    .any(|name| name == "scryer:host/services@1.0.0"),
                "every family component reaches Scryer through the encoded \
                 services door: {non_wasi:?}"
            );
        }
    }

    /// `describe` is a world export now, not a bare exported symbol: the host
    /// calls it directly and parses the returned bytes as a `PluginDescriptor`.
    pub fn assert_describe_returns_a_subtitle_descriptor(&self) {
        let descriptor = self.describe();

        assert_eq!(descriptor["id"], self.descriptor_id);
        // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
        // fields sit alongside it rather than under a nested key.
        assert_eq!(descriptor["provider"]["kind"], "subtitle");
        assert_eq!(descriptor["provider"]["provider_type"], self.provider_type);
        if let Some(mode) = &self.mode {
            assert_eq!(
                descriptor["provider"]["capabilities"]["mode"],
                mode.as_str(),
                "{} advertises itself as a {mode} provider",
                self.provider_type
            );
        }
        for expectation in &self.descriptor {
            expectation.assert(&descriptor);
        }
    }

    /// `describe` parsed, having proved it was pure.
    ///
    /// The host runs `describe` during packaging against an inert services
    /// import, so it may not touch config, state, or HTTP. That matters most
    /// for the descriptors that name a host binding for an API key: reading one
    /// at describe time would be a packaging-time credential fetch.
    pub fn describe(&self) -> serde_json::Value {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
        let bytes = describe(&mut store, &plugin).expect("call describe");
        assert_describe_was_pure(&store.data().script);
        descriptor_from(&bytes)
    }

    /// The provider's credentials and its upstream probe both travel over the
    /// one `host-call` import, and the probe goes to the base URL the
    /// descriptor advertises carrying what was read from config.
    pub fn assert_validate_config_reaches_the_host_services(&self) {
        let script = self.script_with_routes(self.validate_http.clone());
        let (mut store, plugin) = instantiate(&self.wasm_path(), script);

        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
        );
        let PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(response)) = result else {
            panic!("validate_config did not return a typed ok result: {result:?}");
        };
        assert!(
            matches!(response.status, SubtitleValidateConfigStatus::Valid),
            "validate_config should accept a healthy upstream: {response:?}"
        );

        let calls = &store.data().script.calls;
        for key in &self.validate_config_keys {
            assert!(
                calls
                    .iter()
                    .any(|call| call == &format!("config_get:{key}")),
                "the provider must read '{key}' through host services: {calls:?}"
            );
        }
        if self.validate_forbids_http {
            assert!(
                !calls.iter().any(|call| call.starts_with("http:")),
                "validate_config is local; it must not reach upstream: {calls:?}"
            );
        }
        match &self.validate_url {
            UrlShape::None => {}
            shape => {
                let http = calls
                    .iter()
                    .find(|call| call.starts_with("http:"))
                    .unwrap_or_else(|| panic!("validate_config made no HTTP call: {calls:?}"));
                match shape {
                    UrlShape::Exact(url) => assert_eq!(
                        http,
                        &format!("http:{url}"),
                        "the advertised API base must be used verbatim"
                    ),
                    UrlShape::Prefix(prefix) => assert!(
                        http.starts_with(&format!("http:{prefix}")),
                        "the advertised API base must be used verbatim: {http}"
                    ),
                    UrlShape::None => unreachable!(),
                }
                for text in &self.validate_url_contains {
                    assert!(
                        http.contains(text.as_str()),
                        "the probe must carry {text}: {http}"
                    );
                }
            }
        }
        for expected in &self.validate_calls {
            assert!(
                calls.iter().any(|call| call == expected),
                "validate_config must also make {expected}: {calls:?}"
            );
        }
        for prefix in &self.validate_call_prefixes {
            assert!(
                calls.iter().any(|call| call.starts_with(prefix.as_str())),
                "validate_config must also make a {prefix}* call: {calls:?}"
            );
        }
    }

    /// The bytes reach Scryer over host services, untouched, with no archive
    /// service involved — every catalog provider but the tsukihime pilot hands
    /// its container straight through.
    pub fn assert_download_streams_the_file_through_host_http(&self) {
        let script = self.script_with_routes(self.download_http.clone());
        let (mut store, plugin) = instantiate(&self.wasm_path(), script);

        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
                provider_file_id: self.download_reference.clone(),
            }),
        );
        let PluginSubtitleCommandResult::Download(PluginResult::Ok(response)) = result else {
            panic!("download did not return a typed ok result: {result:?}");
        };

        if let Some(expected) = &self.download_bytes {
            let content = base64::engine::general_purpose::STANDARD
                .decode(&response.content_base64)
                .expect("download content is base64");
            assert_eq!(
                &content, expected,
                "the file must reach Scryer byte-for-byte"
            );
        }
        if let Some(format) = &self.download_format {
            assert_eq!(&response.format, format);
        }
        if let Some(filename) = &self.download_filename {
            assert_eq!(response.filename.as_deref(), Some(filename.as_str()));
        }
        if let Some(content_type) = &self.download_content_type {
            assert_eq!(
                response.content_type.as_deref(),
                Some(content_type.as_str())
            );
        }

        let calls = &store.data().script.calls;
        for expected in &self.download_calls {
            assert!(
                calls.iter().any(|call| call == expected),
                "missing hop {expected}: {calls:?}"
            );
        }
        assert!(
            !calls.iter().any(|call| call.starts_with("archive_extract")),
            "this provider does not open archives: {calls:?}"
        );
    }

    /// Capability availability is in-band. A host that refuses a request
    /// answers through the response, never through `host-error`, and the
    /// provider must surface that as a typed plugin error rather than a
    /// world-level invocation failure.
    ///
    /// This is also the assertion that proves the migration carries
    /// `FailureKind` to the host: an unreachable upstream arrives as
    /// `UpstreamUnavailable`, not as a bare message.
    pub fn assert_a_refused_host_capability_stays_in_band(&self) {
        // An empty route table makes the scripted host answer
        // `PluginResult::Err` to every request, which is what a Scryer refusing
        // an egress policy sends.
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());

        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
                provider_file_id: self.download_reference.clone(),
            }),
        );
        let PluginSubtitleCommandResult::Download(PluginResult::Err(error)) = result else {
            panic!("a refused host capability must be a typed plugin error: {result:?}");
        };
        if let Some(code) = self.refused_code {
            assert_eq!(error.code, code);
        }
        if let Some(code) = self.refused_not_code {
            assert_ne!(
                error.code, code,
                "a refused egress is not a missing capability: {error:?}"
            );
        }
        if let Some(text) = &self.refused_message_contains {
            assert!(
                error.public_message.contains(text.as_str()),
                "the provider should report the transport failure it saw: {error:?}"
            );
        }
    }

    /// A catalog provider has no generator. The host reads that from the
    /// descriptor and never routes a generate here, so the arm exists to answer
    /// rather than to trap.
    pub fn assert_generate_is_unsupported_in_band(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
        let result = call_subtitle(
            &mut store,
            &plugin,
            PluginSubtitleCommand::Generate(generate_request()),
        );
        let PluginSubtitleCommandResult::Generate(PluginResult::Err(error)) = result else {
            panic!("generate must report an in-band error: {result:?}");
        };
        assert_eq!(error.code, PluginErrorCode::Unsupported);
    }

    /// The one thing that *is* a world-level `invocation-error`: an envelope
    /// this plugin cannot answer at all.
    pub fn assert_another_family_is_an_invocation_error(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
        let request =
            serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::DownloadClient(
                PluginDownloadClientCommand::GetCompleted(PluginDownloadGetCompletedRequest {
                    client_item_id: "opaque".to_string(),
                }),
            )))
            .expect("encode a download-client envelope");

        let outcome = process(&mut store, &plugin, request).expect("process call itself succeeds");
        assert!(
            outcome.is_err(),
            "a download-client command must not produce a subtitle response"
        );
    }
}
