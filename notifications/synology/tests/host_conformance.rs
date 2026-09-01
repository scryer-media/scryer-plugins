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
//! # Why this one scripts a process, not an HTTP call
//!
//! Synology is the notification family's process case, and the only first-party
//! component of any family that executes a host binary. WASI Preview 2 has no
//! process capability at all, so unlike sockets there was never an ambient
//! alternative to weigh: the only route is `PluginHostRequest::ProcessExec` over
//! the shared import. The scripted host below therefore records the exact
//! command line — a wrong argv here is a wrong `synoindex` invocation on
//! somebody's NAS — and models the two refusals a real host gives: no process
//! service configured, and an allowlist that does not cover the command.
//!
//! A mismatch here means the artifact would fail in production, which is the
//! only failure mode this file is trying to catch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use scryer_plugin_sdk::command::{
    PluginActionRequest, PluginCommand, PluginCommandRequest, PluginCommandResponse,
    PluginCommandResult, PluginDownloadClientCommand, PluginDownloadGetCompletedRequest,
    PluginNotificationCommand, PluginNotificationCommandResult,
};
use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginProcessExecResponse,
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

const SYNOINDEX: &str = "/usr/syno/bin/synoindex";
const IMPORTED_PATH: &str = "/volume1/media/TV/Example Show/S01E01.mkv";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn synology_release_wasm_conforms_to_the_notification_host_contract() {
    let wasm_path = synology_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_declares_the_host_process_capability(&wasm_path);
    assert_send_runs_synoindex_through_host_process_exec(&wasm_path);
    assert_the_config_gate_is_honoured_before_any_process_runs(&wasm_path);
    assert_missing_process_service_stays_in_band(&wasm_path);
    assert_permission_denied_is_reported_not_trapped(&wasm_path);
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
    let bytes = std::fs::read(wasm_path).expect("read synology plugin wasm");
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
/// This is also the regression guard for the *import set*. `notify-common`'s
/// `process_exec` used to be a `#[host_fn] extern "ExtismHost"` declaration;
/// re-introducing anything of that shape here — or reaching
/// `scryer_plugin_pdk::component::*`, which names the indexer world — compiles
/// perfectly and then fails to instantiate under this host.
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

/// `describe` is a world export now, not an Extism entry point: the host calls
/// it directly and parses the returned bytes as a `PluginDescriptor`.
///
/// `requires_host_process` is the field the loader reads to decide whether this
/// channel gets a populated process allowlist at all, so asserting it here is
/// asserting the authority the component is allowed to ask for.
fn assert_describe_declares_the_host_process_capability(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "synology");
    // `ProviderDescriptor` is internally tagged on `kind`, so the notification
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "notification");
    assert_eq!(descriptor["provider"]["provider_type"], "synology");
    assert_eq!(
        descriptor["provider"]["capabilities"]["requires_host_process"], true,
        "the loader gates the process allowlist on this flag"
    );
    assert_eq!(
        descriptor["provider"]["capabilities"]["requires_host_filesystem"], false,
        "notification channels receive no filesystem preopens on any operation"
    );

    // `describe` must be a pure function of the artifact: the host runs it
    // during packaging against an inert services import, so it may not touch
    // config, state, or the process service.
    assert!(
        store.data().script.calls.is_empty(),
        "describe used host services: {:?}",
        store.data().script.calls
    );
}

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// The delivery is a host process, and it crosses the same one `host-call`
/// import as configuration does.
fn assert_send_runs_synoindex_through_host_process_exec(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(download_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("send did not return a typed ok result: {result:?}");
    };
    assert!(response.success, "delivery failed: {response:?}");

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call == "config_get:update_library"),
        "the channel must read its gate through host services: {calls:?}"
    );
    assert_eq!(
        store.data().script.commands,
        vec![vec![
            SYNOINDEX.to_string(),
            "-a".to_string(),
            IMPORTED_PATH.to_string(),
        ]],
        "an imported file must be added to the DSM index by exactly this argv"
    );
}

/// The gate is checked *before* any process request is made, not after — a
/// disabled channel must not reach the host's allowlist at all.
fn assert_the_config_gate_is_honoured_before_any_process_runs(wasm_path: &Path) {
    let script = Script {
        update_library: false,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(download_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("a disabled channel must still answer successfully: {result:?}");
    };
    assert!(response.success);
    assert!(
        store.data().script.commands.is_empty(),
        "a disabled channel executed something: {:?}",
        store.data().script.commands
    );
}

/// Capability availability is in-band. A Scryer with no process service — every
/// other family's host, and a notification host built for describe — answers
/// `Unsupported` through the response, never through `host-error`.
///
/// Synology reports a failed index update as an unsuccessful *notification
/// response* rather than a plugin error, which is the behaviour it has always
/// had: the notification itself was delivered as far as this channel is
/// concerned, and the operator sees why the index did not update.
fn assert_missing_process_service_stays_in_band(wasm_path: &Path) {
    let script = Script {
        process: ProcessScript::Unsupported,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(download_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("a missing process service must stay in-band: {result:?}");
    };
    assert!(!response.success, "the index update cannot have succeeded");
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("synoindex failed")),
        "the host's refusal must reach the operator: {response:?}"
    );
}

/// The other refusal, and the one a *community* channel would always get: the
/// process service exists but this plugin's allowlist does not cover the
/// command. It is a typed `PluginError` from the host, and it must arrive as a
/// reported failure rather than a guest trap.
fn assert_permission_denied_is_reported_not_trapped(wasm_path: &Path) {
    let script = Script {
        process: ProcessScript::PermissionDenied,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);
    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(download_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
        panic!("a denied process must stay in-band: {result:?}");
    };
    assert!(!response.success);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.contains("not on the process allowlist")),
        "the host's own message must survive: {response:?}"
    );
}

/// Synology has no interactive action. The host reads that from the descriptor
/// and never routes one here, so the arm exists to answer rather than to trap.
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProcessScript {
    /// A host with the process service configured and this command allowlisted.
    #[default]
    Allowed,
    /// A host with no process service at all.
    Unsupported,
    /// A host whose allowlist does not cover the command — what every
    /// community channel declaring `requires_host_process` gets.
    PermissionDenied,
}

#[derive(Clone, Debug)]
struct Script {
    process: ProcessScript,
    update_library: bool,
    /// Every argv the guest asked the host to run, command first.
    commands: Vec<Vec<String>>,
    calls: Vec<String>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            process: ProcessScript::default(),
            update_library: true,
            commands: Vec::new(),
            calls: Vec::new(),
        }
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
    /// capability, a command off the allowlist — is a well-formed response
    /// carrying a typed `PluginError`, which is what the two failure scripts
    /// exercise.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.script
                    .calls
                    .push(format!("config_get:{}", request.key));
                let value = match request.key.as_str() {
                    "update_library" => Some(self.script.update_library.to_string()),
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
            PluginHostRequest::ProcessExec(request) => {
                self.script.calls.push("process_exec".to_string());
                let mut argv = vec![request.command.clone()];
                argv.extend(request.args.clone());
                self.script.commands.push(argv);
                match self.script.process {
                    ProcessScript::Allowed => PluginHostResponse::ProcessExec(PluginResult::Ok(
                        PluginProcessExecResponse {
                            exit_code: 0,
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        },
                    )),
                    ProcessScript::Unsupported => PluginHostResponse::ProcessExec(
                        PluginResult::Err(unsupported("this host has no process service")),
                    ),
                    ProcessScript::PermissionDenied => PluginHostResponse::ProcessExec(
                        PluginResult::Err(permission_denied(&request.command)),
                    ),
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

fn permission_denied(command: &str) -> PluginError {
    let message = format!("{command} is not on the process allowlist for this plugin");
    PluginError {
        code: PluginErrorCode::AuthFailed,
        public_message: message.clone(),
        debug_message: Some(message),
        retry_after_seconds: Some(0),
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A completed download, which is the event that makes synoindex add a file.
fn download_request() -> PluginNotificationRequest {
    PluginNotificationRequest {
        event_type: NotificationEventType::Download,
        is_test: false,
        file: Some(PluginNotificationFile {
            primary_path: Some(IMPORTED_PATH.to_string()),
            media_updates: Vec::new(),
        }),
        media_files: vec![PluginNotificationMediaFile {
            path: IMPORTED_PATH.to_string(),
            ..PluginNotificationMediaFile::default()
        }],
        ..base_request()
    }
}

fn base_request() -> PluginNotificationRequest {
    PluginNotificationRequest {
        schema_version: 1,
        event_type: NotificationEventType::Test,
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
            path: Some("/volume1/media/TV/Example Show".to_string()),
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
        file: None,
        media_files: Vec::new(),
        application_update: None,
        manual_interaction: None,
        media_request: None,
    }
}

fn synology_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the synology plugin");
            assert!(status.success(), "synology plugin build failed: {status}");

            plugin_root.join("target/wasm32-wasip2/plugin-release/synology_notification.wasm")
        })
        .clone()
}
