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
//! # Why this one is not a webhook test with the nouns changed
//!
//! Email is the notification family's socket case. Every other channel makes
//! one HTTP call; this one opens a TCP stream and drives a stateful SMTP
//! conversation across `SocketOpen`, `SocketWrite`, `SocketRead`,
//! `SocketStartTls` and `SocketClose` — five host-call variants no other
//! first-party component exercises. So the stand-in below is not a canned
//! response: it is a small SMTP responder that answers what was actually
//! written to it, which is the only way to prove the *sequence* survived the
//! move from five host functions to one `host-call` import.
//!
//! A mismatch here means the artifact would fail in production, which is the
//! only failure mode this file is trying to catch.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_sdk::command::{
    PluginActionRequest, PluginCommand, PluginCommandRequest, PluginCommandResponse,
    PluginCommandResult, PluginDownloadClientCommand, PluginDownloadGetCompletedRequest,
    PluginNotificationCommand, PluginNotificationCommandResult,
};
use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginStateGetResponse,
    PluginStateMutationResponse,
};
use scryer_plugin_sdk::{
    NotificationEventType, PluginError, PluginErrorCode, PluginNotificationApp,
    PluginNotificationExternalIds, PluginNotificationRequest, PluginNotificationTitle,
    PluginResult, SocketCloseResponse, SocketOpenResponse, SocketReadResponse,
    SocketStartTlsResponse, SocketWriteResponse,
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

const SMTP_HOST: &str = "smtp.email-notification.invalid";
const SMTP_PORT: u16 = 587;
const FROM_ADDRESS: &str = "scryer@example.com";
const TO_ADDRESS: &str = "ops@example.com";
const SOCKET_HANDLE: u32 = 7;

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn email_release_wasm_conforms_to_the_notification_host_contract() {
    let wasm_path = email_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_socket_scoped_notification_descriptor(&wasm_path);
    assert_send_reaches_config_and_drives_smtp_over_host_sockets(&wasm_path);
    assert_starttls_upgrade_is_a_host_call(&wasm_path);
    assert_missing_socket_service_stays_in_band(&wasm_path);
    assert_permission_denied_is_in_band_and_not_unsupported(&wasm_path);
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
    let bytes = std::fs::read(wasm_path).expect("read email plugin wasm");
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
/// This is also the regression guard for the *import set*, and it matters more
/// for this plugin than for any other in the family. `scryer_plugin_sdk::net`
/// still carries the old socket host-function externs, and the SDK is a
/// dependency here — so a single call left routed through it would compile
/// perfectly, build a valid-looking `.wasm`, and then fail to instantiate under
/// this host with an unresolvable legacy host-namespace import.
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
///
/// The socket grant is part of that document and is what the host resolves
/// `${smtp_host}` against, so an assertion on it here is an assertion on the
/// authority this component is allowed to ask for at all.
fn assert_describe_returns_a_socket_scoped_notification_descriptor(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let bytes = plugin.call_describe(&mut store).expect("call describe");
    let descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });

    assert_eq!(descriptor["id"], "email");
    // `ProviderDescriptor` is internally tagged on `kind`, so the notification
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "notification");
    assert_eq!(descriptor["provider"]["provider_type"], "email");

    let permission = &descriptor["socket_permissions"][0];
    assert_eq!(
        permission["host_pattern"], "${smtp_host}",
        "the socket grant must stay bound to this channel's own configuration"
    );
    assert_eq!(permission["ports"], serde_json::json!([25, 465, 587]));
    assert_eq!(
        permission["tls_modes"],
        serde_json::json!(["plain", "starttls", "tls"])
    );

    // `describe` must be a pure function of the artifact: the host runs it
    // during packaging against an inert services import, so it may not touch
    // config, state, HTTP, or sockets.
    assert!(
        store.data().script.calls.is_empty(),
        "describe used host services: {:?}",
        store.data().script.calls
    );
}

// ---------------------------------------------------------------------------
// process
// ---------------------------------------------------------------------------

/// The whole delivery — configuration and every byte of the SMTP conversation —
/// travels over the one `host-call` import.
fn assert_send_reaches_config_and_drives_smtp_over_host_sockets(wasm_path: &Path) {
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
    assert_eq!(response.provider_status.as_deref(), Some("smtp_accepted"));
    assert_eq!(response.target_results.len(), 1);
    assert_eq!(response.target_results[0].target, TO_ADDRESS);

    let calls = &store.data().script.calls;
    for key in ["smtp_host", "smtp_port", "security", "from_address"] {
        assert!(
            calls
                .iter()
                .any(|call| call == &format!("config_get:{key}")),
            "the channel must read {key} through host services: {calls:?}"
        );
    }
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("socket_open:{SMTP_HOST}:{SMTP_PORT}:starttls")),
        "the connection must be opened through host services at the configured \
         host, port and TLS mode: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call == "socket_close"),
        "the transport must close its handle: {calls:?}"
    );

    // The conversation itself, not merely that *a* socket was opened.
    let transcript = store.data().script.transcript.join("");
    for expected in [
        "EHLO scryer.local",
        "STARTTLS",
        &format!("MAIL FROM:<{FROM_ADDRESS}>"),
        &format!("RCPT TO:<{TO_ADDRESS}>"),
        "DATA",
        "Subject: Test Notification",
        "QUIT",
    ] {
        assert!(
            transcript.contains(expected),
            "the SMTP conversation is missing {expected:?}: {transcript}"
        );
    }
}

/// STARTTLS is the one step that cannot be expressed as reads and writes: the
/// guest asks the *host* to upgrade the stream in place, because the guest has
/// no TLS stack and — deliberately — no `wasi:sockets` to bring one to.
fn assert_starttls_upgrade_is_a_host_call(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());
    let _ = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("socket_starttls:{SMTP_HOST}")),
        "the STARTTLS upgrade must cross host services, naming the host it is \
         verifying the certificate against: {calls:?}"
    );
    let starttls = calls
        .iter()
        .position(|call| call.starts_with("socket_starttls:"))
        .expect("starttls call");
    let open = calls
        .iter()
        .position(|call| call.starts_with("socket_open:"))
        .expect("open call");
    assert!(
        open < starttls,
        "the stream must be opened in the clear and upgraded afterwards: {calls:?}"
    );
    assert!(
        store.data().script.tls_upgraded,
        "the plugin must actually complete the upgrade before sending mail"
    );
}

/// Capability availability is in-band. A Scryer with no socket service — every
/// other family's host, and a notification host built for describe — answers
/// `Unsupported` through the response, never through `host-error`, and the
/// channel must surface that as a typed plugin error rather than a world-level
/// invocation failure.
fn assert_missing_socket_service_stays_in_band(wasm_path: &Path) {
    let script = Script {
        socket: SocketScript::Unsupported,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Err(error)) = result else {
        panic!("a missing socket service must be a typed plugin error: {result:?}");
    };
    assert_eq!(error.code, PluginErrorCode::Unsupported);
}

/// The distinction the world's doc comment insists on, pinned from the guest
/// side: a denial by *descriptor permission* is not the same answer as an
/// absent service. The host reports it `Permanent` — the grant is what it is
/// and retrying cannot change it — and this plugin must pass that through
/// rather than re-deriving a code of its own.
fn assert_permission_denied_is_in_band_and_not_unsupported(wasm_path: &Path) {
    let script = Script {
        socket: SocketScript::PermissionDenied,
        ..Script::default()
    };
    let (mut store, plugin) = instantiate(wasm_path, script);

    let result = call_notification(
        &mut store,
        &plugin,
        PluginNotificationCommand::Send(test_request()),
    );
    let PluginNotificationCommandResult::Send(PluginResult::Err(error)) = result else {
        panic!("a denied socket must be a typed plugin error: {result:?}");
    };
    assert_eq!(
        error.code,
        PluginErrorCode::Permanent,
        "a permission denial must not be reported as an absent capability"
    );
    assert!(
        error.public_message.contains("not permitted"),
        "the host's own message must survive: {error:?}"
    );
}

/// Email has no interactive action. The host reads that from the descriptor and
/// never routes one here, so the arm exists to answer rather than to trap.
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
// A scripted `CommandHost` with a working SMTP server behind its socket table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SocketScript {
    /// A host with the socket service configured and this channel permitted.
    #[default]
    Connected,
    /// A host with no socket service at all.
    Unsupported,
    /// A host whose socket service refused *this* descriptor's grant.
    PermissionDenied,
}

#[derive(Clone, Debug, Default)]
struct Script {
    socket: SocketScript,
    /// Bytes the server has queued for the guest to read.
    outbound: VecDeque<u8>,
    /// Everything the guest wrote, for transcript assertions.
    transcript: Vec<String>,
    /// Whether the server is inside a `DATA` block.
    in_data: bool,
    tls_upgraded: bool,
    calls: Vec<String>,
}

impl Script {
    fn greet(&mut self) {
        self.reply("220 smtp.test.invalid ESMTP ready\r\n");
    }

    fn reply(&mut self, line: &str) {
        self.outbound.extend(line.as_bytes());
    }

    /// A minimal but real SMTP server: it answers what was actually written,
    /// which is what makes the ordering assertions meaningful. A canned reply
    /// queue would pass even if the guest sent the conversation backwards.
    fn handle_written(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data).to_string();
        self.transcript.push(text.clone());

        for line in text.split("\r\n") {
            if self.in_data {
                if line == "." {
                    self.in_data = false;
                    self.reply("250 2.0.0 Ok: queued as TESTMSG\r\n");
                }
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") {
                self.reply(
                    "250-smtp.test.invalid\r\n250-STARTTLS\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
                );
            } else if upper.starts_with("HELO") {
                self.reply("250 smtp.test.invalid\r\n");
            } else if upper.starts_with("STARTTLS") {
                self.reply("220 2.0.0 Ready to start TLS\r\n");
            } else if upper.starts_with("AUTH") {
                self.reply("235 2.7.0 Authentication successful\r\n");
            } else if upper.starts_with("DATA") {
                self.in_data = true;
                self.reply("354 End data with <CR><LF>.<CR><LF>\r\n");
            } else if upper.starts_with("QUIT") {
                self.reply("221 2.0.0 Bye\r\n");
            } else {
                // MAIL FROM, RCPT TO, RSET, NOOP.
                self.reply("250 2.1.0 Ok\r\n");
            }
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
    /// capability, a denied socket grant — is a well-formed response carrying a
    /// typed `PluginError`, which is what the two failure scripts exercise.
    fn host_call(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostError> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostError::InvalidRequest)?;

        let response = match request {
            PluginHostRequest::ConfigGet(request) => {
                self.script
                    .calls
                    .push(format!("config_get:{}", request.key));
                let value = match request.key.as_str() {
                    "smtp_host" => Some(SMTP_HOST.to_string()),
                    "smtp_port" => Some(SMTP_PORT.to_string()),
                    "security" => Some("starttls".to_string()),
                    "from_address" => Some(FROM_ADDRESS.to_string()),
                    "to_addresses" => Some(TO_ADDRESS.to_string()),
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
            PluginHostRequest::SocketOpen(request) => {
                self.script.calls.push(format!(
                    "socket_open:{}:{}:{}",
                    request.host,
                    request.port,
                    match request.tls_mode {
                        scryer_plugin_sdk::SocketTlsMode::Plain => "plain",
                        scryer_plugin_sdk::SocketTlsMode::Starttls => "starttls",
                        scryer_plugin_sdk::SocketTlsMode::Tls => "tls",
                    }
                ));
                match self.script.socket {
                    SocketScript::Connected => {
                        self.script.greet();
                        PluginHostResponse::SocketOpen(PluginResult::Ok(SocketOpenResponse {
                            handle: SOCKET_HANDLE,
                        }))
                    }
                    SocketScript::Unsupported => PluginHostResponse::SocketOpen(PluginResult::Err(
                        unsupported("this host has no socket service"),
                    )),
                    SocketScript::PermissionDenied => {
                        PluginHostResponse::SocketOpen(PluginResult::Err(permission_denied()))
                    }
                }
            }
            PluginHostRequest::SocketWrite(request) => {
                self.script.calls.push("socket_write".to_string());
                let data = BASE64
                    .decode(request.data_base64)
                    .expect("the guest must base64 what it writes");
                self.script.handle_written(&data);
                PluginHostResponse::SocketWrite(PluginResult::Ok(SocketWriteResponse {
                    bytes_written: data.len(),
                }))
            }
            PluginHostRequest::SocketRead(request) => {
                self.script.calls.push("socket_read".to_string());
                let take = request.max_bytes.min(self.script.outbound.len());
                let data: Vec<u8> = self.script.outbound.drain(..take).collect();
                let eof = data.is_empty();
                PluginHostResponse::SocketRead(PluginResult::Ok(SocketReadResponse {
                    data_base64: BASE64.encode(&data),
                    eof,
                }))
            }
            PluginHostRequest::SocketStartTls(request) => {
                self.script
                    .calls
                    .push(format!("socket_starttls:{}", request.host));
                self.script.tls_upgraded = true;
                PluginHostResponse::SocketStartTls(PluginResult::Ok(SocketStartTlsResponse {
                    handle: SOCKET_HANDLE,
                }))
            }
            PluginHostRequest::SocketClose(_) => {
                self.script.calls.push("socket_close".to_string());
                PluginHostResponse::SocketClose(PluginResult::Ok(SocketCloseResponse {
                    closed: true,
                }))
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

/// What Scryer's socket layer answers when the resolved `socket_permissions`
/// do not cover the requested host, port or TLS mode — `Permanent`, not
/// `Unsupported`, and carrying the socket layer's own message.
fn permission_denied() -> PluginError {
    let message = format!("socket to {SMTP_HOST}:{SMTP_PORT} is not permitted by this plugin");
    PluginError {
        code: PluginErrorCode::Permanent,
        public_message: message.clone(),
        debug_message: Some(message),
        retry_after_seconds: Some(0),
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn test_request() -> PluginNotificationRequest {
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
            path: None,
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

fn email_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the email plugin");
            assert!(status.success(), "email plugin build failed: {status}");

            plugin_root.join("target/wasm32-wasip2/plugin-release/email_notification.wasm")
        })
        .clone()
}
