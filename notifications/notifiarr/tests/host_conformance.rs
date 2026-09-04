//! Conformance against the real Scryer host, run on the RELEASE artifact.
//!
//! This suite exists because the plugin's contract is not "these functions
//! behave" but "this exact `.wasm` runs under Scryer's notification host". It
//! therefore builds the shipping `wasm32-wasip2` component and drives it the
//! way `crates/scryer-plugins/src/wasmtime_host/notification_component_host.rs`
//! does: the world is linked as `scryer:notification/notification@1.0.0`, the
//! shared `scryer:host/services@1.0.0` import is served by a scripted stand-in
//! for `CommandHost` speaking the same postcard
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
    PluginActionRequest, PluginCommand, PluginCommandRequest, PluginCommandResponse,
    PluginCommandResult, PluginDownloadClientCommand, PluginDownloadGetCompletedRequest,
    PluginNotificationCommand, PluginNotificationCommandResult,
};
use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginHttpResponse,
    PluginStateGetResponse, PluginStateMutationResponse,
};
use scryer_plugin_sdk::{
    NotificationEventType, PluginError, PluginErrorCode, PluginNotificationApp,
    PluginNotificationExternalIds, PluginNotificationFile, PluginNotificationMediaFile,
    PluginNotificationRequest, PluginNotificationTitle, PluginResult,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod notification_world {
    wasmtime::component::bindgen!({
        world: "scryer:notification/notification@1.0.0",
        // Two packages, two paths — the same layout the host's bindgen uses,
        // and the same two files this crate vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    });
}

use notification_world::Notification;
use notification_world::scryer::host::services::{Host as ServicesHost, HostError};

// ---------------------------------------------------------------------------
// What differs per channel
// ---------------------------------------------------------------------------

const PLUGIN_ID: &str = "notifiarr";
const PROVIDER_TYPE: &str = "notifiarr";
const WASM_NAME: &str = "notifiarr_notification.wasm";

/// Notifiarr's own 36-character key shape (`APIKeyLength`,
/// `Notifiarr/notifiarr:pkg/website/website_routes.go:24`).
const SCRIPTED_API_KEY: &str = "00000000-1111-2222-3333-444444444444";

/// The configuration Scryer would have resolved for this channel.
///
/// `channel_id` is the Discord channel Notifiarr's passthrough integration
/// requires (`discord.ids.channel`, Required).
fn scripted_config(key: &str) -> Option<String> {
    match key {
        "api_key" => Some(SCRIPTED_API_KEY.to_string()),
        "channel_id" => Some("910000000000000001".to_string()),
        _ => None,
    }
}

/// The upstream endpoint a `send` must reach, built from that configuration.
///
/// A prefix rather than a whole URL: several channels append query parameters
/// carrying the notification text, which is the payload's business and not this
/// assertion's. What is pinned is that the endpoint comes from the resolved
/// configuration and is used verbatim — here including the API key, which the
/// passthrough integration takes as a path segment.
const EXPECTED_URL_PREFIX: &str = "https://notifiarr.com/api/v1/notification/passthrough/";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn notifiarr_release_wasm_conforms_to_the_notification_host_contract() {
    let wasm_path = plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_notification_descriptor(&wasm_path);
    assert_send_reaches_the_configured_endpoint_over_host_http(&wasm_path);
    assert_an_upstream_failure_is_a_reported_delivery_failure(&wasm_path);
    assert_a_refused_http_capability_stays_in_band(&wasm_path);
    assert_a_missing_required_setting_is_a_typed_error(&wasm_path);
    assert_action_is_unsupported_in_band(&wasm_path);
    assert_another_family_is_an_invocation_error(&wasm_path);
}

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// The notification host has no core-module backing, so a core wasm artifact is
/// not a degraded plugin but an uninstallable one. Check the component preamble
/// directly rather than inferring it from a link failure.
fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path).expect("read the plugin wasm");
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

/// The exact check the host performs on install: the artifact compiles, every
/// import it emits is satisfiable from WASI Preview 2 plus the world's
/// `services` interface, and its exports match
/// `scryer:notification/notification@1.0.0`.
///
/// This is also the regression guard for the *import set*. The PDK links one
/// crate against two different component contracts, and the published
/// `scryer-plugin-sdk` still declares host-function externs behind its `net`
/// and process modules — so a component that accidentally keeps a live
/// `scryer:indexer/host` import, or one of the legacy host-namespace imports
/// that SDK can still emit, compiles perfectly and then fails to instantiate
/// under this host.
fn assert_world_conformance(wasm_path: &Path) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile notification component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    Notification::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");
    linker
        .instantiate_pre(&component)
        .and_then(notification_world::NotificationPre::new)
        .expect("the artifact must satisfy scryer:notification/notification@1.0.0");
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

/// `describe` is a world export now, not a bare exported symbol: the host calls
/// it directly and parses the returned bytes as a `PluginDescriptor`.
fn assert_describe_returns_a_notification_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], PLUGIN_ID);
    // `ProviderDescriptor` is internally tagged on `kind`, so the notification
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "notification");
    assert_eq!(descriptor["provider"]["provider_type"], PROVIDER_TYPE);
    assert_eq!(
        descriptor["provider"]["capabilities"]["requires_host_filesystem"], false,
        "notification channels receive no filesystem preopens on any operation"
    );
    assert_eq!(
        descriptor["provider"]["capabilities"]["requires_host_process"], false,
        "this channel delivers over HTTP and must not ask for process authority"
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

/// The channel's configuration and its upstream request both travel over the
/// one `host-call` import, and the endpoint is built from that configuration
/// rather than from anything ambient.
fn assert_send_reaches_the_configured_endpoint_over_host_http(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("send did not return a typed ok result: {result:?}");
    };
    assert!(response.success, "delivery failed: {response:?}");

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call.starts_with("config_get:")),
        "the channel must read its settings through host services: {calls:?}"
    );
    assert!(
        store
            .data()
            .script
            .urls
            .iter()
            .any(|url| url.starts_with(EXPECTED_URL_PREFIX)),
        "the configured endpoint must be used verbatim; got {:?}",
        store.data().script.urls
    );
}

/// An upstream rejection is not a plugin failure: the channel reports an
/// unsuccessful delivery with the provider's own status, and the operator sees
/// what the provider said. That behaviour predates the migration and must
/// survive it.
fn assert_an_upstream_failure_is_a_reported_delivery_failure(wasm_path: &Path) {
    let script = Script {
        http: HttpScript::Status(500, b"upstream exploded".to_vec()),
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("an upstream failure must stay in-band: {result:?}");
    };
    assert!(!response.success, "a 500 cannot be a successful delivery");
}

/// Capability availability is in-band. A Scryer whose HTTP egress refuses this
/// channel answers through the response, never through `host-error`, and the
/// channel must surface that as a reported failure rather than a world-level
/// invocation failure or a trap.
fn assert_a_refused_http_capability_stays_in_band(wasm_path: &Path) {
    let script = Script {
        http: HttpScript::Refused,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    match result {
        PluginNotificationCommandResult::Send(PluginResult::Ok(response)) => {
            assert!(
                !response.success,
                "a refused egress cannot be a successful delivery: {response:?}"
            );
        }
        PluginNotificationCommandResult::Send(PluginResult::Err(_)) => {
            // Also acceptable: a typed plugin error is still in-band.
        }
        other => panic!("send answered another operation: {other:?}"),
    }
}

/// A missing required setting used to be a `FnResult` hard fault: the host saw
/// a string and a generic ABI failure, indistinguishable from a crashed plugin.
/// It is now a typed `PluginResult::Err` the operator can act on, and — the
/// part that matters under a component — the instance survives it.
fn assert_a_missing_required_setting_is_a_typed_error(wasm_path: &Path) {
    let script = Script {
        unset: vec!["api_key".to_string()],
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Err(error)) = result else {
        panic!("a missing required setting must be a typed plugin error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::InvalidConfig);
    assert!(
        error.public_message.contains("api_key"),
        "the operator has to be told which setting: {error:?}"
    );
}

/// This channel has no interactive action. The host reads that from the
/// descriptor and never routes one here, so the arm exists to answer rather
/// than to trap — a trap under a component costs the whole instance.
fn assert_action_is_unsupported_in_band(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Action(PluginActionRequest {
            action: "test".to_string(),
            payload: serde_json::Value::Null,
        }),
    );
    let PluginNotificationCommandResult::Action(PluginResult::Err(error)) = result else {
        panic!("action must report an in-band error: {result:?}");
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
        "a download-client command must not produce a notification response"
    );
}

// ---------------------------------------------------------------------------
// Driving the component
// ---------------------------------------------------------------------------

fn call_notification(
    store: &mut Store<Ctx>,
    plugin: &Notification,
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    let request = serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::Notification(
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
        PluginCommandResult::Notification(result) => result,
        other => panic!("process answered another family: {other:?}"),
    }
}

fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, Notification) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile notification component");
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    Notification::add_to_linker::<Ctx, HasSelf<Ctx>>(&mut linker, |ctx| ctx)
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
    let plugin = Notification::instantiate(&mut store, &component, &linker)
        .expect("instantiate the notification component");
    (store, plugin)
}

// ---------------------------------------------------------------------------
// A scripted `CommandHost`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
enum HttpScript {
    /// The provider accepted the notification.
    #[default]
    Accepted,
    /// The provider answered, badly.
    Status(u16, Vec<u8>),
    /// The host itself refused the request, in-band.
    Refused,
}

#[derive(Clone, Debug, Default)]
struct Script {
    http: HttpScript,
    /// Config keys the host should pretend are unset, whatever
    /// `scripted_config` says.
    unset: Vec<String>,
    /// Every URL the channel asked the host to fetch.
    urls: Vec<String>,
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
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`, which is what the `Refused` script exercises.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.script
                    .calls
                    .push(format!("config_get:{}", request.key));
                let value = if self.script.unset.contains(&request.key) {
                    None
                } else {
                    scripted_config(&request.key)
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
                self.script.urls.push(request.url.clone());
                match self.script.http.clone() {
                    HttpScript::Accepted => {
                        PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                            status: 200,
                            headers: BTreeMap::new(),
                            body: b"{}".to_vec(),
                        }))
                    }
                    HttpScript::Status(status, body) => {
                        PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                            status,
                            headers: BTreeMap::new(),
                            body,
                        }))
                    }
                    HttpScript::Refused => PluginHostResponse::Http(PluginResult::Err(
                        unsupported("this host refuses egress for this plugin"),
                    )),
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
/// Every optional field is populated deliberately: the published SDK's
/// `PluginError` still carries `skip_serializing_if` on `debug_message` and
/// `retry_after_seconds`, which a non-self-describing format like postcard
/// cannot round-trip — a `None` there produces bytes the guest decoder rejects
/// outright. Until that lands, a host answering in-band must fill them in.
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

/// A staged media path, for the channels whose delivery is a library refresh
/// rather than a message.
const MEDIA_PATH: &str = "/media/TV/Example Show/S01E01.mkv";

fn test_request() -> PluginNotificationRequest {
    PluginNotificationRequest {
        schema_version: 1,
        event_type: NotificationEventType::Test,
        file: Some(PluginNotificationFile {
            primary_path: Some(MEDIA_PATH.to_string()),
            media_updates: Vec::new(),
        }),
        media_files: vec![PluginNotificationMediaFile {
            path: MEDIA_PATH.to_string(),
            ..PluginNotificationMediaFile::default()
        }],
        event_id: Some("evt-1".to_string()),
        occurred_at: Some("2026-04-29T12:00:00Z".to_string()),
        correlation_id: None,
        actor: None,
        severity: None,
        is_test: true,
        summary_title: "Test Notification".to_string(),
        summary_message: "This is a test.".to_string(),
        app: PluginNotificationApp {
            name: "Scryer".to_string(),
            version: "test".to_string(),
        },
        title: Some(PluginNotificationTitle {
            id: None,
            name: "Example Show".to_string(),
            facet: "tv".to_string(),
            year: Some(2026),
            slug: None,
            path: Some("/media/TV/Example Show".to_string()),
            overview: None,
            sort_title: None,
            background_url: None,
            poster_url: None,
            tags: Vec::new(),
            aliases: Vec::new(),
            original_language: None,
            original_country: None,
            external_ids: PluginNotificationExternalIds::default(),
        }),
        episode: None,
        episodes: Vec::new(),
        release: None,
        download: None,
        import: None,
        health: None,
        application_update: None,
        manual_interaction: None,
        media_request: None,
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
                .join("target/wasm32-wasip2/plugin-release")
                .join(WASM_NAME)
        })
        .clone()
}
