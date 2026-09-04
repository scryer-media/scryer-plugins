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
use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginHttpResponse,
    PluginStateGetResponse, PluginStateMutationResponse,
};
use scryer_plugin_sdk::{
    PluginError, PluginErrorCode, PluginResult, SubtitleGeneratorInputRef,
    SubtitlePluginDownloadRequest, SubtitlePluginGenerateRequest,
    SubtitlePluginValidateConfigRequest, SubtitleQueryMediaKind, SubtitleValidateConfigStatus,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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

/// Jimaku's API base is a compiled-in constant rather than a config field, so
/// the URL assertions pin the advertised base rather than a scripted one.
const EXPECTED_VALIDATION_URL: &str = "https://jimaku.cc/api/entries/search?query=naruto";
const SUBTITLE_DOWNLOAD_URL: &str = "https://jimaku.cc/api/entries/42/files/Show.S01E01.eng.srt";
const TEST_API_KEY: &str = "test-api-key";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn jimaku_release_wasm_conforms_to_the_subtitle_host_contract() {
    let wasm_path = jimaku_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_catalog_subtitle_descriptor(&wasm_path);
    assert_validate_config_reaches_the_host_services(&wasm_path);
    assert_download_streams_the_file_through_host_http(&wasm_path);
    assert_a_refused_host_capability_stays_in_band(&wasm_path);
    assert_generate_is_unsupported_in_band(&wasm_path);
    assert_another_family_is_an_invocation_error(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The subtitle host has no core-module backing, so a core wasm artifact is
/// not a degraded plugin but an uninstallable one. Check the component
/// preamble directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path).expect("read jimaku plugin wasm");
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
    // The typed runtime import is absent unless reachable code calls one of
    // its functions — the linker drops an import nothing loads — so only the
    // encoded door is required to be present.
    assert!(
        non_wasi
            .iter()
            .any(|name| name == "scryer:host/services@1.0.0"),
        "every family component reaches Scryer through the encoded services door: {non_wasi:?}"
    );
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

/// `describe` is a world export now, not a bare exported symbol: the host calls
/// it directly and parses the returned bytes as a `PluginDescriptor`.
fn assert_describe_returns_a_catalog_subtitle_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = describe(&mut store, &plugin).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "jimaku");
    // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "subtitle");
    assert_eq!(descriptor["provider"]["provider_type"], "jimaku");
    assert_eq!(
        descriptor["provider"]["capabilities"]["mode"], "catalog",
        "jimaku advertises itself as a catalog provider"
    );

    // `describe` must be a pure function of the artifact: the host runs it
    // during packaging against an inert services import, so it may not touch
    // config, state, or HTTP.
    assert!(
        store.data().script.calls.is_empty(),
        "describe used host services: {:?}",
        store.data().script.calls
    );
}

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// The provider's API key and its upstream probe both travel over the one
/// `host-call` import, and the probe goes to the base URL the descriptor
/// advertises rather than anywhere else.
fn assert_validate_config_reaches_the_host_services(wasm_path: &Path) {
    let script = Script {
        http: Some(HttpScript {
            status: 200,
            body: b"[]".to_vec(),
        }),
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);

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
    assert!(
        calls.iter().any(|call| call == "config_get:api_key"),
        "the provider must read its API key through host services: {calls:?}"
    );
    let http = calls
        .iter()
        .find(|call| call.starts_with("http:"))
        .unwrap_or_else(|| panic!("validate_config made no HTTP call: {calls:?}"));
    assert_eq!(
        http,
        &format!("http:{EXPECTED_VALIDATION_URL}"),
        "the advertised API base must be used verbatim"
    );
}

/// Unlike the tsukihime pilot, this provider never opens a container: Jimaku
/// serves plain subtitle files and archives alike, and both are handed to
/// Scryer as-is. The assertion pins that — one host HTTP call, bytes through
/// untouched, no archive service involved.
fn assert_download_streams_the_file_through_host_http(wasm_path: &Path) {
    let script = Script {
        http: Some(HttpScript {
            status: 200,
            body: subtitle_fixture(),
        }),
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Ok(response)) = result else {
        panic!("download did not return a typed ok result: {result:?}");
    };

    use base64::Engine as _;
    let content = base64::engine::general_purpose::STANDARD
        .decode(response.content_base64)
        .expect("download content is base64");
    assert_eq!(
        content,
        subtitle_fixture(),
        "the subtitle file must reach Scryer byte-for-byte"
    );
    assert_eq!(response.format, "srt");
    assert_eq!(response.filename.as_deref(), Some("Show.S01E01.eng.srt"));

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{SUBTITLE_DOWNLOAD_URL}")),
        "the file must be fetched over host services: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("archive_extract")),
        "this provider does not open archives: {calls:?}"
    );
}

/// Capability availability is in-band. A host that refuses a request answers
/// through the response, never through `host-error`, and the provider must
/// surface that as a typed plugin error rather than a world-level invocation
/// failure.
fn assert_a_refused_host_capability_stays_in_band(wasm_path: &Path) {
    // `http: None` makes the scripted host answer `PluginResult::Err`, which
    // is exactly what a Scryer refusing an egress policy would send.
    let (mut store, plugin) = instantiate(wasm_path, Script::default());

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: download_reference(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Err(error)) = result else {
        panic!("a refused host capability must be a typed plugin error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
}

/// A catalog provider has no generator. The host reads that from the
/// descriptor and never routes a generate here, so the arm exists to answer
/// rather than to trap.
fn assert_generate_is_unsupported_in_band(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Generate(SubtitlePluginGenerateRequest {
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
        }),
    );
    let PluginSubtitleCommandResult::Generate(PluginResult::Err(error)) = result else {
        panic!("generate must report an in-band error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

/// The one thing that *is* a world-level `invocation-error`: an envelope this
/// plugin cannot answer at all.
fn assert_another_family_is_an_invocation_error(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::DownloadClient(
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

// ---------------------------------------------------------------------------
// Driving the component
// ---------------------------------------------------------------------------

fn call_subtitle(
    store: &mut Store<Ctx>,
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

fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, SubtitleProvider) {
    let engine = engine();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let linker = linker(&engine);

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            state: BTreeMap::new(),
            // The host captures guest stderr and tails it into its own error
            // messages; inheriting it here puts the same text in front of
            // whoever is reading the test failure.
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            script,
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

#[derive(Clone, Debug)]
struct HttpScript {
    status: u16,
    body: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct Script {
    http: Option<HttpScript>,
    calls: Vec<String>,
}

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
    script: Script,
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
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied origin — is a well-formed response carrying a
    /// typed `PluginError`.
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
        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.script
                    .calls
                    .push(format!("config_get:{}", request.key));
                let value = match request.key.as_str() {
                    "api_key" => Some(TEST_API_KEY.to_string()),
                    _ => None,
                };
                PluginHostResponse::ConfigGet(PluginResult::Ok(PluginConfigGetResponse { value }))
            }
            PluginHostRequest::StateGet(request) => {
                self.script.calls.push(format!("state_get:{}", request.key));
                PluginHostResponse::StateGet(PluginResult::Ok(PluginStateGetResponse {
                    value: None,
                }))
            }
            PluginHostRequest::StateSet(request) => {
                self.script.calls.push(format!("state_set:{}", request.key));
                PluginHostResponse::StateSet(PluginResult::Ok(PluginStateMutationResponse {
                    changed: true,
                }))
            }
            PluginHostRequest::StateDelete(request) => {
                self.script
                    .calls
                    .push(format!("state_delete:{}", request.key));
                PluginHostResponse::StateDelete(PluginResult::Ok(PluginStateMutationResponse {
                    changed: true,
                }))
            }
            PluginHostRequest::Http(request) => {
                self.script.calls.push(format!("http:{}", request.url));
                match self.script.http.clone() {
                    Some(script) => {
                        PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                            status: script.status,
                            headers: BTreeMap::new(),
                            body: script.body,
                        }))
                    }
                    None => PluginHostResponse::Http(PluginResult::Err(unsupported(
                        "no HTTP response scripted",
                    ))),
                }
            }
            other => {
                self.script.calls.push(format!("unscripted:{other:?}"));
                return Err(HostError::Failed);
            }
        };

        Ok(response)
    }
}

/// The in-band "this host cannot do that" answer.
///
/// Every optional field is populated deliberately: `PluginError` carries
/// `skip_serializing_if` on `debug_message` and `retry_after_seconds`, which a
/// non-self-describing format like postcard cannot round-trip — a `None` there
/// produces bytes the guest decoder rejects outright. Until the SDK drops
/// those attributes, a host answering in-band must fill them in.
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

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one Jimaku entry file.
fn download_reference() -> String {
    serde_json::json!({
        "url": SUBTITLE_DOWNLOAD_URL,
        "filename": "Show.S01E01.eng.srt",
        "language": "eng",
        "episode": 1,
    })
    .to_string()
}

/// A plain subtitle body comfortably over the provider's 500-byte floor for
/// non-archive files.
fn subtitle_fixture() -> Vec<u8> {
    let mut body = String::new();
    for index in 1..=20 {
        body.push_str(&format!(
            "{index}\n00:00:{index:02},000 --> 00:00:{index:02},900\nLine {index}\n\n"
        ));
    }
    body.into_bytes()
}

fn jimaku_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the jimaku plugin");
            assert!(status.success(), "jimaku plugin build failed: {status}");

            plugin_root.join("target/wasm32-wasip2/plugin-release/jimaku_subtitle_provider.wasm")
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
        self.script.calls.push(format!("runtime-state-get:{key}"));
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
        self.script.calls.push(format!("runtime-state-cas:{key}"));
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
