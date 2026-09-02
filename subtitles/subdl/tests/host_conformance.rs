//! Conformance against the real Scryer host, run on the RELEASE artifact.
//!
//! This suite exists because the plugin's contract is not "these functions
//! behave" but "this exact `.wasm` runs under Scryer's subtitle host". It
//! therefore builds the shipping `wasm32-wasip2` component and drives it the
//! way `crates/scryer-plugins/src/wasmtime_host/subtitle_component_host.rs`
//! does: the world is linked as `scryer:subtitle/subtitle-provider@1.0.0`,
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

use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadGetCompletedRequest, PluginSubtitleCommand,
    PluginSubtitleCommandResult,
};
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
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod subtitle_world {
    wasmtime::component::bindgen!({
        world: "scryer:subtitle/subtitle-provider@1.0.0",
        // Two packages, two paths — the same layout the host's bindgen uses,
        // and the same two files this crate vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/subtitle-v1.0.0"],
    });
}

use subtitle_world::SubtitleProvider;
use subtitle_world::scryer::host::services::{Host as ServicesHost, HostError};

/// Subdl's API base is a compiled-in constant rather than a config field, so
/// the URL assertions pin the advertised base rather than a scripted one.
const API_BASE: &str = "https://api.subdl.com/api/v1";
const SUBTITLE_DOWNLOAD_URL: &str = "https://dl.subdl.com/subtitle/Show.S01E01.eng.zip";
const TEST_API_KEY: &str = "test-api-key";
const ZIP_BYTES: &[u8] = b"PK\x03\x04 pretend this is a subtitle archive";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn subdl_release_wasm_conforms_to_the_subtitle_host_contract() {
    let wasm_path = subdl_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_catalog_subtitle_descriptor(&wasm_path);
    assert_validate_config_reaches_the_host_services(&wasm_path);
    assert_download_streams_the_container_through_host_http(&wasm_path);
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
    let bytes = std::fs::read(wasm_path).expect("read subdl plugin wasm");
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

/// The exact check the host performs on install: the artifact compiles, every
/// import it emits is satisfiable from WASI Preview 2 plus the world's
/// `services` interface, and its exports match
/// `scryer:subtitle/subtitle-provider@1.0.0`.
///
/// This is also the regression guard for the *import set*. The PDK links one
/// crate against two different component contracts, and a family component
/// that accidentally keeps a live `scryer:indexer/host` import compiles
/// perfectly and then fails to instantiate under this host.
fn assert_world_conformance(wasm_path: &Path) {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    SubtitleProvider::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");
    linker
        .instantiate_pre(&component)
        .and_then(subtitle_world::SubtitleProviderPre::new)
        .expect("the artifact must satisfy scryer:subtitle/subtitle-provider@1.0.0");
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

/// `describe` is a world export now, not a bare exported symbol: the host calls
/// it directly and parses the returned bytes as a `PluginDescriptor`.
fn assert_describe_returns_a_catalog_subtitle_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "subdl");
    // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "subtitle");
    assert_eq!(descriptor["provider"]["provider_type"], "subdl");
    assert_eq!(
        descriptor["provider"]["capabilities"]["mode"], "catalog",
        "subdl advertises itself as a catalog provider"
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
/// advertises carrying the key that was read from config.
fn assert_validate_config_reaches_the_host_services(wasm_path: &Path) {
    let script = Script {
        http: Some(HttpScript {
            status: 200,
            body: br#"{"status":true,"results":[],"subtitles":[]}"#.to_vec(),
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
    assert!(
        http.starts_with(&format!("http:{API_BASE}/subtitles?")),
        "the advertised API base must be used verbatim: {http}"
    );
    assert!(
        http.contains(&format!("api_key={TEST_API_KEY}")),
        "the key read from config must reach the request: {http}"
    );
    assert!(
        http.contains("film_name=Inception"),
        "validate_config probes with its fixed title: {http}"
    );
}

/// Unlike the tsukihime pilot, this provider never opens a container: Subdl
/// serves zipped subtitles and they are handed to Scryer as-is. The assertion
/// pins that — one host HTTP call, bytes through untouched, no archive
/// service involved.
fn assert_download_streams_the_container_through_host_http(wasm_path: &Path) {
    let script = Script {
        http: Some(HttpScript {
            status: 200,
            body: ZIP_BYTES.to_vec(),
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
        content, ZIP_BYTES,
        "the container must reach Scryer byte-for-byte"
    );
    assert_eq!(response.format, "zip");
    assert_eq!(response.filename.as_deref(), Some("Show.S01E01.eng.zip"));
    assert_eq!(response.content_type.as_deref(), Some("application/zip"));

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{SUBTITLE_DOWNLOAD_URL}")),
        "the container must be fetched over host services: {calls:?}"
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
///
/// This is also the assertion that proves the migration finally carries
/// [`FailureKind`] to the host: an unreachable upstream arrives as
/// `UpstreamUnavailable`, not as a bare message.
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

    let outcome = plugin
        .call_process(&mut store, &request)
        .expect("process call itself succeeds");
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
        PluginCommandResult::Subtitle(result) => result,
        other => panic!("process answered another family: {other:?}"),
    }
}

fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, SubtitleProvider) {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm_path).expect("compile subtitle component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    SubtitleProvider::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");

    let mut store = Store::new(
        &engine,
        Ctx {
            table: ResourceTable::new(),
            // The host captures guest stderr and tails it into its own error
            // messages; inheriting it here puts the same text in front of
            // whoever is reading the test failure.
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            script,
        },
    );
    let plugin = SubtitleProvider::instantiate(&mut store, &component, &linker)
        .expect("instantiate the subtitle component");
    (store, plugin)
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
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

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

        postcard::to_allocvec(&response).map_err(|_| HostError::Failed)
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
/// it from one Subdl subtitle row.
fn download_reference() -> String {
    serde_json::json!({
        "download_url": SUBTITLE_DOWNLOAD_URL,
        "filename": "Show.S01E01.eng.zip",
        "content_type": "application/zip",
        "page_url": "https://subdl.com/subtitle/1",
    })
    .to_string()
}

fn subdl_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the subdl plugin");
            assert!(status.success(), "subdl plugin build failed: {status}");

            plugin_root.join("target/wasm32-wasip2/plugin-release/subdl_subtitle_provider.wasm")
        })
        .clone()
}
