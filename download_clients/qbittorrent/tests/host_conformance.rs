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
//! The migration was transport-only — every URL, header, and status rule in
//! `src/lib.rs` is unchanged, and the 69 unit tests that pin them still run
//! natively. What this file adds is the half a unit test cannot reach: that the
//! artifact instantiates under the real world, that `describe` is pure, that
//! typed failures stay in-band, and — the one thing this family leans on that
//! catalog providers do not — that the qBittorrent SID cookie written by one
//! invocation is read back by the next, through `StateSet`/`StateGet` over the
//! one host import.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadClientCommandResult, PluginSubtitleCommand,
};
use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginHttpResponse,
    PluginStateGetResponse, PluginStateMutationResponse,
};
use scryer_plugin_sdk::{
    DownloadControlAction, PluginDownloadClientControlRequest, PluginError, PluginErrorCode,
    PluginResult, SubtitlePluginValidateConfigRequest,
};
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

/// The plugin normalises `base_url` into `<base>/api/v2`, so every scripted URL
/// below is the exact string the migrated artifact must still produce.
const BASE_URL: &str = "http://qbittorrent.invalid:8080";
const API_ROOT: &str = "http://qbittorrent.invalid:8080/api/v2";
/// `var::get::<String>` decodes JSON, so the stored cookie is a JSON string.
const COOKIE_STATE_KEY: &str = "qbittorrent.sid";
const SESSION_COOKIE: &str = "SID=conformance-session";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn qbittorrent_release_wasm_conforms_to_the_download_client_host_contract() {
    let wasm_path = qbittorrent_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_download_client_descriptor(&wasm_path);
    assert_list_queue_logs_in_and_uses_the_configured_base_url(&wasm_path);
    assert_the_session_cookie_outlives_the_instance_that_stored_it(&wasm_path);
    assert_test_connection_clears_the_session_through_state_delete(&wasm_path);
    assert_a_refused_host_service_stays_in_band(&wasm_path);
    assert_an_unsupported_control_action_is_in_band(&wasm_path);
    assert_another_family_is_an_invocation_error(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The download-client host has no core-module backing, so a core wasm artifact
/// is not a degraded plugin but an uninstallable one. Check the component
/// preamble directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path).expect("read qbittorrent plugin wasm");
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
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "qbittorrent");
    // `ProviderDescriptor` is internally tagged on `kind`, so the client fields
    // sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "download_client");
    assert_eq!(descriptor["provider"]["provider_type"], "qbittorrent");
    assert_eq!(
        descriptor["provider"]["capabilities"]["mark_imported_non_destructive"], true,
        "qBittorrent keeps its non-destructive import handoff across the migration"
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

/// The client's configuration, its login, and its listing request all travel
/// over the one `host-call` import, and the URLs are the pre-migration ones.
fn assert_list_queue_logs_in_and_uses_the_configured_base_url(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, authenticated_script());

    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    let PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(items)) = result else {
        panic!("list_queue did not return a typed ok result: {result:?}");
    };
    assert!(items.is_empty(), "the scripted client has no torrents");

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call == "config_get:base_url"),
        "the client must read its base URL through host services: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:POST {API_ROOT}/auth/login")),
        "an empty session must log in first: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call
            == &format!("http:GET {API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all")),
        "the configured base URL and query must be used verbatim: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("state_set:{COOKIE_STATE_KEY}")),
        "the SID cookie must be written to plugin state: {calls:?}"
    );
}

/// The one service this family leans on that a catalog provider does not.
///
/// A component instance does not survive a `process` call, so a client that
/// authenticates once and reuses its session must keep the cookie in plugin
/// state. The host backs every invocation of a configured client with one
/// `CommandHost`, and therefore one state map — modelled here by carrying the
/// first invocation's map into a second, freshly instantiated component.
///
/// The proof is negative as well as positive: the second invocation must send
/// `Cookie: SID=…` *and* must not call `/auth/login` again.
fn assert_the_session_cookie_outlives_the_instance_that_stored_it(wasm_path: &Path) {
    let (mut first_store, first_plugin) = instantiate(wasm_path, authenticated_script());
    let first = call_download_client(
        &mut first_store,
        &first_plugin,
        PluginDownloadClientCommand::ListQueue,
    );
    assert!(
        matches!(
            first,
            PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(_))
        ),
        "the first invocation must succeed before its session can be reused: {first:?}"
    );

    let carried_state = first_store.data().script.state.clone();
    assert_eq!(
        carried_state
            .get(COOKIE_STATE_KEY)
            .map(|value| String::from_utf8_lossy(value).to_string())
            .as_deref(),
        Some(format!("\"{SESSION_COOKIE}\"").as_str()),
        "the login cookie must be the value handed to StateSet"
    );

    // A brand-new instance, the same state map — exactly what the host does on
    // the next poll of the same configured client.
    let mut script = authenticated_script();
    script.state = carried_state;
    let (mut second_store, second_plugin) = instantiate(wasm_path, script);
    let second = call_download_client(
        &mut second_store,
        &second_plugin,
        PluginDownloadClientCommand::ListQueue,
    );
    assert!(
        matches!(
            second,
            PluginDownloadClientCommandResult::ListQueue(PluginResult::Ok(_))
        ),
        "the reused session must still list the queue: {second:?}"
    );

    let calls = &second_store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("state_get:{COOKIE_STATE_KEY}")),
        "the second invocation must read the cookie back out of state: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| call == &format!("http:POST {API_ROOT}/auth/login")),
        "a stored session must not be re-authenticated: {calls:?}"
    );
    assert!(
        store_sent_cookie(&second_store.data().script),
        "the reused session must travel as a Cookie header: {:?}",
        second_store.data().script.cookies
    );
}

/// `test_connection` deliberately discards the session before probing, so it is
/// the operation that pins `StateDelete` crossing the same import.
fn assert_test_connection_clears_the_session_through_state_delete(wasm_path: &Path) {
    let mut script = authenticated_script();
    script.state.insert(
        COOKIE_STATE_KEY.to_string(),
        format!("\"{SESSION_COOKIE}\"").into_bytes(),
    );
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::TestConnection,
    );
    let PluginDownloadClientCommandResult::TestConnection(PluginResult::Ok(version)) = result
    else {
        panic!("test_connection did not return a typed ok result: {result:?}");
    };
    assert_eq!(version, "v5.2.0");

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("state_delete:{COOKIE_STATE_KEY}")),
        "test_connection must clear the stored session: {calls:?}"
    );
    // Deleting the session is not cosmetic: the probe that follows must
    // re-authenticate rather than ride the cookie it was handed, which is the
    // whole point of `test_connection` proving credentials rather than reach.
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:POST {API_ROOT}/auth/login")),
        "test_connection must re-authenticate after clearing the session: {calls:?}"
    );
    let delete_index = calls
        .iter()
        .position(|call| call == &format!("state_delete:{COOKIE_STATE_KEY}"))
        .expect("state_delete was asserted above");
    let login_index = calls
        .iter()
        .position(|call| call == &format!("http:POST {API_ROOT}/auth/login"))
        .expect("login was asserted above");
    assert!(
        delete_index < login_index,
        "the session must be cleared before the probe, not after: {calls:?}"
    );
}

/// Capability availability is in-band. A host that refuses a service answers
/// through the response, never through `host-error`, and the client must
/// surface that as a typed plugin error rather than a world-level invocation
/// failure — otherwise the host loses the plugin's own diagnosis.
fn assert_a_refused_host_service_stays_in_band(wasm_path: &Path) {
    let mut script = authenticated_script();
    script.http_refused = true;
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_download_client(&mut store, &plugin, PluginDownloadClientCommand::ListQueue);
    let PluginDownloadClientCommandResult::ListQueue(PluginResult::Err(error)) = result else {
        panic!("a refused HTTP service must be a typed plugin error: {result:?}");
    };
    assert!(
        !error.public_message.is_empty(),
        "the client keeps its own diagnosis on the way out"
    );
}

/// qBittorrent has no force-start, and the pre-migration plugin answered that
/// in-band. The component must not turn it into an `invocation-error`.
fn assert_an_unsupported_control_action_is_in_band(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, authenticated_script());
    let result = call_download_client(
        &mut store,
        &plugin,
        PluginDownloadClientCommand::Control(PluginDownloadClientControlRequest {
            client_item_id: "0000000000000000000000000000000000000000".to_string(),
            action: DownloadControlAction::ForceStart,
            remove_data: false,
            is_history: false,
        }),
    );
    let PluginDownloadClientCommandResult::Control(PluginResult::Err(error)) = result else {
        panic!("force_start must report an in-band error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

/// The one thing that *is* a world-level `invocation-error`: an envelope this
/// plugin cannot answer at all.
fn assert_another_family_is_an_invocation_error(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
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

fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, DownloadClient) {
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
            // The host captures guest stderr and tails it into its own error
            // messages; inheriting it here puts the same text in front of
            // whoever is reading the test failure.
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            script,
        },
    );
    let plugin = DownloadClient::instantiate(&mut store, &component, &linker)
        .expect("instantiate the download-client component");
    (store, plugin)
}

fn store_sent_cookie(script: &Script) -> bool {
    script.cookies.iter().any(|cookie| cookie == SESSION_COOKIE)
}

// ---------------------------------------------------------------------------
// A scripted `CommandHost`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Script {
    config: BTreeMap<String, String>,
    /// One map per `CommandHost`, shared by every invocation of a configured
    /// client — which is what lets a session cookie outlive its instance.
    state: BTreeMap<String, Vec<u8>>,
    /// Exact-match URL routes; anything unrouted is a scripting bug and fails
    /// loudly rather than quietly returning an empty body.
    http: BTreeMap<String, HttpReply>,
    /// What a host with no egress configured for this plugin answers.
    http_refused: bool,
    calls: Vec<String>,
    cookies: Vec<String>,
}

#[derive(Clone, Debug)]
struct HttpReply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpReply {
    fn ok(body: &str) -> Self {
        Self {
            status: 200,
            headers: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }
}

/// A qBittorrent 5.2 stand-in: credentials configured, a 204 login carrying the
/// session in `Set-Cookie` and no body, and an empty torrent list.
fn authenticated_script() -> Script {
    let mut config = BTreeMap::new();
    config.insert("base_url".to_string(), BASE_URL.to_string());
    config.insert("username".to_string(), "scryer".to_string());
    config.insert("password".to_string(), "secret".to_string());

    let mut login_headers = BTreeMap::new();
    login_headers.insert(
        "Set-Cookie".to_string(),
        format!("{SESSION_COOKIE}; HttpOnly; path=/"),
    );

    let mut http = BTreeMap::new();
    http.insert(
        format!("{API_ROOT}/auth/login"),
        HttpReply {
            // qBittorrent 5.2 answers a successful login with 204 and no body;
            // the client's `login_response_is_success` accepts that, and this
            // script would catch a regression that started demanding "Ok.".
            status: 204,
            headers: login_headers,
            body: Vec::new(),
        },
    );
    http.insert(
        format!("{API_ROOT}/torrents/info?sort=added_on&reverse=true&filter=all"),
        HttpReply::ok("[]"),
    );
    http.insert(format!("{API_ROOT}/app/version"), HttpReply::ok("v5.2.0"));

    Script {
        config,
        http,
        ..Script::default()
    }
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
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`, which is what `http_refused` exercises.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.script
                    .calls
                    .push(format!("config_get:{}", request.key));
                PluginHostResponse::ConfigGet(PluginResult::Ok(PluginConfigGetResponse {
                    value: self.script.config.get(&request.key).cloned(),
                }))
            }
            PluginHostRequest::StateGet(request) => {
                self.script.calls.push(format!("state_get:{}", request.key));
                PluginHostResponse::StateGet(PluginResult::Ok(PluginStateGetResponse {
                    value: self.script.state.get(&request.key).cloned(),
                }))
            }
            PluginHostRequest::StateSet(request) => {
                self.script.calls.push(format!("state_set:{}", request.key));
                let changed = self
                    .script
                    .state
                    .insert(request.key, request.value)
                    .is_none();
                PluginHostResponse::StateSet(PluginResult::Ok(PluginStateMutationResponse {
                    changed,
                }))
            }
            PluginHostRequest::StateDelete(request) => {
                self.script
                    .calls
                    .push(format!("state_delete:{}", request.key));
                let changed = self.script.state.remove(&request.key).is_some();
                PluginHostResponse::StateDelete(PluginResult::Ok(PluginStateMutationResponse {
                    changed,
                }))
            }
            PluginHostRequest::Http(request) => {
                let method = request.method.clone().unwrap_or_else(|| "GET".to_string());
                self.script
                    .calls
                    .push(format!("http:{method} {}", request.url));
                if let Some(cookie) = request
                    .headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.clone())
                {
                    self.script.cookies.push(cookie);
                }
                if self.script.http_refused {
                    PluginHostResponse::Http(PluginResult::Err(unsupported(
                        "no HTTP egress is configured for this plugin",
                    )))
                } else {
                    match self.script.http.get(&request.url).cloned() {
                        Some(reply) => {
                            PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                                status: reply.status,
                                headers: reply.headers,
                                body: reply.body,
                            }))
                        }
                        None => PluginHostResponse::Http(PluginResult::Err(unsupported(&format!(
                            "unscripted URL: {method} {}",
                            request.url
                        )))),
                    }
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn qbittorrent_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the qbittorrent plugin");
            assert!(
                status.success(),
                "qbittorrent plugin build failed: {status}"
            );

            plugin_root.join("target/wasm32-wasip2/plugin-release/qbittorrent_download_client.wasm")
        })
        .clone()
}
