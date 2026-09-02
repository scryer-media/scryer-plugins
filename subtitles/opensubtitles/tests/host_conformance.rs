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
//!
//! ## Why the script routes by URL
//!
//! Unlike the single-hop providers, one OpenSubtitles operation is several
//! upstream requests: log in, ask for a download link, then fetch the link.
//! A one-response script cannot express that, so the stand-in matches a
//! request URL against an ordered route table. An empty table is the
//! "host refuses everything" case, which is what the in-band assertion needs.

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

/// OpenSubtitles' API base is a compiled-in constant (the login response may
/// renegotiate it), so the URL assertions pin the advertised base.
const API_BASE: &str = "https://api.opensubtitles.com/api/v1";
const CONTENT_URL: &str = "https://dl.opensubtitles.invalid/sub.srt";
const TEST_API_KEY: &str = "test-api-key";
const TEST_USERNAME: &str = "test-user";
const TEST_PASSWORD: &str = "test-password";
const SUBTITLE_TEXT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn opensubtitles_release_wasm_conforms_to_the_subtitle_host_contract() {
    let wasm_path = opensubtitles_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_catalog_subtitle_descriptor(&wasm_path);
    assert_validate_config_reaches_the_host_services(&wasm_path);
    assert_download_walks_every_hop_over_host_http(&wasm_path);
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
    let bytes = std::fs::read(wasm_path).expect("read opensubtitles plugin wasm");
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

    assert_eq!(descriptor["id"], "opensubtitles");
    // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "subtitle");
    assert_eq!(descriptor["provider"]["provider_type"], "opensubtitles");
    assert_eq!(
        descriptor["provider"]["capabilities"]["mode"], "catalog",
        "opensubtitles advertises itself as a catalog provider"
    );

    // `describe` must be a pure function of the artifact: the host runs it
    // during packaging against an inert services import, so it may not touch
    // config, state, or HTTP. That matters more here than elsewhere — this
    // descriptor names a host binding for its API key, and reading one at
    // describe time would be a packaging-time credential fetch.
    assert!(
        store.data().script.calls.is_empty(),
        "describe used host services: {:?}",
        store.data().script.calls
    );
}

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// All three credentials and the login round trip travel over the one
/// `host-call` import, and the login goes to the advertised API base.
fn assert_validate_config_reaches_the_host_services(wasm_path: &Path) {
    let script = Script {
        http: vec![
            route("/login", 200, br#"{"token":"test-token"}"#.to_vec()),
            route("/infos/user", 200, br#"{"data":{}}"#.to_vec()),
        ],
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
        "validate_config should accept a healthy session: {response:?}"
    );

    let calls = &store.data().script.calls;
    for key in ["api_key", "username", "password"] {
        assert!(
            calls
                .iter()
                .any(|call| call == &format!("config_get:{key}")),
            "the provider must read '{key}' through host services: {calls:?}"
        );
    }
    let http = calls
        .iter()
        .find(|call| call.starts_with("http:"))
        .unwrap_or_else(|| panic!("validate_config made no HTTP call: {calls:?}"));
    assert_eq!(
        http,
        &format!("http:{API_BASE}/login"),
        "the advertised API base must be used verbatim"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{API_BASE}/infos/user")),
        "validate_config must confirm the session it just opened: {calls:?}"
    );
}

/// Download is three upstream hops — login, download link, content — and every
/// one of them crosses the single host-services import. Nothing here opens a
/// container: OpenSubtitles hands back a plain `srt` document.
fn assert_download_walks_every_hop_over_host_http(wasm_path: &Path) {
    let script = Script {
        http: vec![
            route("/login", 200, br#"{"token":"test-token"}"#.to_vec()),
            route(
                "/download",
                200,
                format!(r#"{{"link":"{CONTENT_URL}"}}"#).into_bytes(),
            ),
            route("dl.opensubtitles.invalid", 200, SUBTITLE_TEXT.to_vec()),
        ],
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: "123456".to_string(),
        }),
    );
    let PluginSubtitleCommandResult::Download(PluginResult::Ok(response)) = result else {
        panic!("download did not return a typed ok result: {result:?}");
    };

    use base64::Engine as _;
    let content = base64::engine::general_purpose::STANDARD
        .decode(response.content_base64)
        .expect("download content is base64");
    assert_eq!(content, SUBTITLE_TEXT);
    assert_eq!(response.format, "srt");
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );

    let calls = &store.data().script.calls;
    for expected in [
        format!("http:{API_BASE}/login"),
        format!("http:{API_BASE}/download"),
        format!("http:{CONTENT_URL}"),
    ] {
        assert!(
            calls.iter().any(|call| call == &expected),
            "missing hop {expected}: {calls:?}"
        );
    }
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
    // An empty route table makes the scripted host answer `PluginResult::Err`
    // to every request, which is what a Scryer refusing an egress policy
    // sends. The login hop is the one that trips.
    let (mut store, plugin) = instantiate(wasm_path, Script::default());

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Download(SubtitlePluginDownloadRequest {
            provider_file_id: "123456".to_string(),
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

/// One scripted upstream response, selected by a substring of the request URL.
#[derive(Clone, Debug)]
struct HttpRoute {
    url_contains: &'static str,
    status: u16,
    body: Vec<u8>,
}

fn route(url_contains: &'static str, status: u16, body: Vec<u8>) -> HttpRoute {
    HttpRoute {
        url_contains,
        status,
        body,
    }
}

#[derive(Clone, Debug, Default)]
struct Script {
    http: Vec<HttpRoute>,
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
                    "username" => Some(TEST_USERNAME.to_string()),
                    "password" => Some(TEST_PASSWORD.to_string()),
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
                match self
                    .script
                    .http
                    .iter()
                    .find(|route| request.url.contains(route.url_contains))
                {
                    Some(route) => PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                        status: route.status,
                        headers: BTreeMap::new(),
                        body: route.body.clone(),
                    })),
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

fn opensubtitles_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the opensubtitles plugin");
            assert!(
                status.success(),
                "opensubtitles plugin build failed: {status}"
            );

            plugin_root
                .join("target/wasm32-wasip2/plugin-release/opensubtitles_subtitle_provider.wasm")
        })
        .clone()
}
