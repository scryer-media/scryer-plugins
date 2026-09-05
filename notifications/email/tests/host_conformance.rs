//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
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
//! The family-shared half of the suite — the artifact and world checks, the
//! descriptor identity, the action and wrong-family arms, and the
//! `ConfigGet`/`StateGet`/`StateSet`/`StateDelete` switchboard — comes from
//! `scryer-plugin-conformance`. Only the socket half lives here.

// conformance: bespoke

use std::collections::{BTreeMap, VecDeque};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scryer_plugin_conformance::notification::{
    Check, NotificationConformance, call_notification, instantiate_with,
};
use scryer_plugin_conformance::{
    ConfigSource, HostErrorKind, HostResponder, Script, StateSource, default_respond, unsupported,
};
use scryer_plugin_sdk::command::{PluginNotificationCommand, PluginNotificationCommandResult};
use scryer_plugin_sdk::host::{PluginHostRequest, PluginHostResponse};
use scryer_plugin_sdk::{
    NotificationEventType, PluginError, PluginErrorCode, PluginNotificationApp,
    PluginNotificationExternalIds, PluginNotificationRequest, PluginNotificationTitle,
    PluginResult, SocketCloseResponse, SocketOpenResponse, SocketReadResponse,
    SocketStartTlsResponse, SocketWriteResponse,
};

const SMTP_HOST: &str = "smtp.email-notification.invalid";
const SMTP_PORT: u16 = 587;
const FROM_ADDRESS: &str = "scryer@example.com";
const TO_ADDRESS: &str = "ops@example.com";
const SOCKET_HANDLE: u32 = 7;

#[test]
fn email_release_wasm_conforms_to_the_notification_host_contract() {
    let conformance = conformance();

    conformance.assert_artifact_is_a_component();
    conformance.assert_world_conformance();
    conformance.assert_describe_returns_a_notification_descriptor();
    assert_send_reaches_config_and_drives_smtp_over_host_sockets(&conformance);
    assert_starttls_upgrade_is_a_host_call(&conformance);
    assert_missing_socket_service_stays_in_band(&conformance);
    assert_permission_denied_is_in_band_and_not_unsupported(&conformance);
    conformance.assert_action_is_unsupported_in_band();
    conformance.assert_another_family_is_an_invocation_error();
}

/// The shared suite, minus every check that assumes an HTTP channel.
///
/// The descriptor defaults go too: they say this channel holds no authority
/// beyond HTTP, and email's whole point is that it holds a socket grant. The
/// grant is asserted in their place — it is part of the packaging document the
/// host resolves `${smtp_host}` against, so an assertion on it is an assertion
/// on the authority this component is allowed to ask for at all.
fn conformance() -> NotificationConformance {
    NotificationConformance::new(env!("CARGO_MANIFEST_DIR"), "email")
        .wasm("email_notification.wasm")
        .config("smtp_host", SMTP_HOST)
        .config("smtp_port", &SMTP_PORT.to_string())
        .config("security", "starttls")
        .config("from_address", FROM_ADDRESS)
        .config("to_addresses", TO_ADDRESS)
        .without_descriptor_defaults()
        .expects_descriptor(
            &["socket_permissions", "0", "host_pattern"],
            serde_json::json!("${smtp_host}"),
        )
        .expects_descriptor(
            &["socket_permissions", "0", "ports"],
            serde_json::json!([25, 465, 587]),
        )
        .expects_descriptor(
            &["socket_permissions", "0", "tls_modes"],
            serde_json::json!(["plain", "starttls", "tls"]),
        )
        .without(Check::SendReachesTheConfiguredEndpoint)
        .without(Check::UpstreamFailureIsReported)
        .without(Check::RefusedHttpStaysInBand)
        .without(Check::MissingRequiredSetting)
}

/// The configuration the shared switchboard answers from, plus the socket
/// behaviour this channel's own responder adds.
fn script() -> Script {
    Script {
        config: ConfigSource::Resolved(BTreeMap::from([
            ("smtp_host".to_string(), SMTP_HOST.to_string()),
            ("smtp_port".to_string(), SMTP_PORT.to_string()),
            ("security".to_string(), "starttls".to_string()),
            ("from_address".to_string(), FROM_ADDRESS.to_string()),
            ("to_addresses".to_string(), TO_ADDRESS.to_string()),
        ])),
        state: StateSource::Ephemeral,
        ..Script::default()
    }
}

/// The whole delivery — configuration and every byte of the SMTP conversation —
/// travels over the one `host-call` import.
fn assert_send_reaches_config_and_drives_smtp_over_host_sockets(
    conformance: &NotificationConformance,
) {
    let (mut store, plugin) =
        instantiate_with(&conformance.wasm_path(), script(), SmtpResponder::default());
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
    let transcript = store.data().responder.transcript.join("");
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
fn assert_starttls_upgrade_is_a_host_call(conformance: &NotificationConformance) {
    let (mut store, plugin) =
        instantiate_with(&conformance.wasm_path(), script(), SmtpResponder::default());
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
        store.data().responder.tls_upgraded,
        "the plugin must actually complete the upgrade before sending mail"
    );
}

/// Capability availability is in-band. A Scryer with no socket service — every
/// other family's host, and a notification host built for describe — answers
/// `Unsupported` through the response, never through `host-error`, and the
/// channel must surface that as a typed plugin error rather than a world-level
/// invocation failure.
fn assert_missing_socket_service_stays_in_band(conformance: &NotificationConformance) {
    let responder = SmtpResponder {
        socket: SocketScript::Unsupported,
        ..SmtpResponder::default()
    };
    let (mut store, plugin) = instantiate_with(&conformance.wasm_path(), script(), responder);

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
fn assert_permission_denied_is_in_band_and_not_unsupported(conformance: &NotificationConformance) {
    let responder = SmtpResponder {
        socket: SocketScript::PermissionDenied,
        ..SmtpResponder::default()
    };
    let (mut store, plugin) = instantiate_with(&conformance.wasm_path(), script(), responder);

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

// ---------------------------------------------------------------------------
// A working SMTP server behind the shared switchboard's socket arms
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
struct SmtpResponder {
    socket: SocketScript,
    /// Bytes the server has queued for the guest to read.
    outbound: VecDeque<u8>,
    /// Everything the guest wrote, for transcript assertions.
    transcript: Vec<String>,
    /// Whether the server is inside a `DATA` block.
    in_data: bool,
    tls_upgraded: bool,
}

impl SmtpResponder {
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

impl HostResponder for SmtpResponder {
    /// The five socket variants, with everything else handed back to the
    /// family-shared switchboard.
    ///
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied socket grant — is a well-formed response carrying a
    /// typed `PluginError`, which is what the two failure scripts exercise.
    fn respond(
        &mut self,
        request: PluginHostRequest,
        script: &mut Script,
    ) -> Result<PluginHostResponse, HostErrorKind> {
        let response = match request {
            PluginHostRequest::SocketOpen(request) => {
                script.calls.push(format!(
                    "socket_open:{}:{}:{}",
                    request.host,
                    request.port,
                    match request.tls_mode {
                        scryer_plugin_sdk::SocketTlsMode::Plain => "plain",
                        scryer_plugin_sdk::SocketTlsMode::Starttls => "starttls",
                        scryer_plugin_sdk::SocketTlsMode::Tls => "tls",
                    }
                ));
                match self.socket {
                    SocketScript::Connected => {
                        self.greet();
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
                script.calls.push("socket_write".to_string());
                let data = BASE64
                    .decode(request.data_base64)
                    .expect("the guest must base64 what it writes");
                self.handle_written(&data);
                PluginHostResponse::SocketWrite(PluginResult::Ok(SocketWriteResponse {
                    bytes_written: data.len(),
                }))
            }
            PluginHostRequest::SocketRead(request) => {
                script.calls.push("socket_read".to_string());
                let take = request.max_bytes.min(self.outbound.len());
                let data: Vec<u8> = self.outbound.drain(..take).collect();
                let eof = data.is_empty();
                PluginHostResponse::SocketRead(PluginResult::Ok(SocketReadResponse {
                    data_base64: BASE64.encode(&data),
                    eof,
                }))
            }
            PluginHostRequest::SocketStartTls(request) => {
                script
                    .calls
                    .push(format!("socket_starttls:{}", request.host));
                self.tls_upgraded = true;
                PluginHostResponse::SocketStartTls(PluginResult::Ok(SocketStartTlsResponse {
                    handle: SOCKET_HANDLE,
                }))
            }
            PluginHostRequest::SocketClose(_) => {
                script.calls.push("socket_close".to_string());
                PluginHostResponse::SocketClose(PluginResult::Ok(SocketCloseResponse {
                    closed: true,
                }))
            }
            other => return default_respond(other, script),
        };

        Ok(response)
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

/// This channel's own notification fixture rather than the family's: email
/// renders whatever media the request carries into the message body, and the
/// transcript assertions above are written against a request with none.
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
