//! Conformance against the real Scryer notification host, run on the RELEASE
//! artifact.
//!
//! The world is linked as `scryer:notification/notification@1.0.0` the way
//! `crates/scryer-plugins/src/wasmtime_host/notification_component_host.rs`
//! links it, the shared `scryer:host/services@1.0.0` import is served by the
//! scripted stand-in for `CommandHost` in the crate root, WASI Preview 2 comes
//! from the linker, and `process` carries the `PluginCommandRequest` JSON
//! envelope.
//!
//! A mismatch here means the artifact would fail in production, which is the
//! only failure mode this suite is trying to catch.

use std::path::{Path, PathBuf};

use scryer_plugin_sdk::command::{
    PluginActionRequest, PluginCommand, PluginCommandRequest, PluginCommandResponse,
    PluginCommandResult, PluginDownloadClientCommand, PluginDownloadGetCompletedRequest,
    PluginNotificationCommand, PluginNotificationCommandResult,
};
use scryer_plugin_sdk::{
    NotificationEventType, PluginErrorCode, PluginNotificationApp, PluginNotificationExternalIds,
    PluginNotificationFile, PluginNotificationMediaFile, PluginNotificationRequest,
    PluginNotificationTitle, PluginResult,
};
use std::collections::BTreeMap;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

use crate::{
    ConfigSource, Ctx, DefaultResponder, DescriptorExpectation, HostErrorKind, HostResponder,
    HttpScript, Script, StateSource, assert_describe_was_pure, build_plugin_wasm, descriptor_from,
};

mod notification_world {
    wasmtime::component::bindgen!({
        world: "scryer:notification/notification@1.0.0",
        // Two packages, two paths — the same layout the host's bindgen uses,
        // and the same two files every channel vendors for its own guest
        // bindings.
        path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    });
}

pub use notification_world::Notification;
use notification_world::scryer::host::services::{Host as ServicesHost, HostError};

impl<R: HostResponder> ServicesHost for Ctx<R> {
    /// The shared host import, standing in for Scryer's `CommandHost`.
    ///
    /// `host-error` is reserved for the transport: a request that cannot be
    /// decoded. Everything a real host would refuse — an unconfigured
    /// capability, a denied origin — is a well-formed response carrying a typed
    /// `PluginError`, which is what the refused scripts exercise.
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
pub fn instantiate(wasm_path: &Path, script: Script) -> (Store<Ctx>, Notification) {
    instantiate_with(wasm_path, script, DefaultResponder)
}

/// The same, for a channel that answers host-call variants of its own — email's
/// `SocketOpen`/`Write`/`Read`/`StartTls`/`Close`.
pub fn instantiate_with<R: HostResponder>(
    wasm_path: &Path,
    script: Script,
    responder: R,
) -> (Store<Ctx<R>>, Notification) {
    let engine = Engine::default();
    let component =
        Component::from_file(&engine, wasm_path).expect("compile notification component");
    let linker = linker::<R>(&engine);

    let mut store = Store::new(&engine, Ctx::new(script, responder));
    let plugin = Notification::instantiate(&mut store, &component, &linker)
        .expect("instantiate the notification component");
    (store, plugin)
}

fn linker<R: HostResponder>(engine: &Engine) -> Linker<Ctx<R>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("register WASI Preview 2");
    Notification::add_to_linker::<Ctx<R>, HasSelf<Ctx<R>>>(&mut linker, |ctx| ctx)
        .expect("register the shared host services");
    linker
}

/// Send one notification command through `process` and decode the family result
/// out of the envelope.
pub fn call_notification<R: HostResponder>(
    store: &mut Store<Ctx<R>>,
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A staged media path, for the channels whose delivery is a library refresh
/// rather than a message.
pub const MEDIA_PATH: &str = "/media/TV/Example Show/S01E01.mkv";

/// The notification Scryer hands a channel.
pub fn test_request(event_type: NotificationEventType) -> PluginNotificationRequest {
    PluginNotificationRequest {
        schema_version: 1,
        event_type,
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

// ---------------------------------------------------------------------------
// The family's default check set
// ---------------------------------------------------------------------------

/// How the endpoint a `send` reaches is pinned.
#[derive(Clone, Debug)]
pub enum UrlExpectation {
    /// A prefix rather than a whole URL: several channels append query
    /// parameters carrying the notification text, which is the payload's
    /// business and not this assertion's. What is pinned is that the endpoint
    /// comes from the resolved configuration and is used verbatim.
    Prefix(String),
    /// The whole URL. A channel that publishes to a server root and carries
    /// everything in the body has nothing to append, and an exact match is the
    /// stronger statement.
    Exact(String),
    /// The channel's delivery is not an HTTP call at all, so the send check is
    /// the plugin's own.
    None,
}

/// One of the shared checks, for a channel that opts out and asserts something
/// stronger locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    ArtifactIsAComponent,
    WorldConformance,
    Describe,
    SendReachesTheConfiguredEndpoint,
    UpstreamFailureIsReported,
    RefusedHttpStaysInBand,
    MissingRequiredSetting,
    ActionIsUnsupported,
    AnotherFamilyIsAnInvocationError,
}

/// What a `run()` observed, for a channel that pins more of the request than
/// its URL.
pub struct Outcome {
    /// The scripted host's recording of the successful `send`.
    pub send: Script,
}

/// The notification conformance suite, configured per channel.
pub struct NotificationConformance {
    manifest_dir: String,
    plugin_id: String,
    provider_type: String,
    wasm_name: String,
    config: BTreeMap<String, String>,
    url: UrlExpectation,
    required_setting: Option<String>,
    required_setting_mentions: Vec<String>,
    event_type: NotificationEventType,
    descriptor: Vec<DescriptorExpectation>,
    descriptor_defaults: bool,
    skipped: Vec<Check>,
}

impl NotificationConformance {
    /// `manifest_dir` must be `env!("CARGO_MANIFEST_DIR")` expanded in the
    /// channel's own test binary: inside this library the macro would resolve
    /// to this library's directory instead.
    pub fn new(manifest_dir: &str, plugin_id: &str) -> Self {
        Self {
            manifest_dir: manifest_dir.to_string(),
            plugin_id: plugin_id.to_string(),
            provider_type: plugin_id.to_string(),
            wasm_name: format!("{}_notification.wasm", plugin_id.replace('-', "_")),
            config: BTreeMap::new(),
            url: UrlExpectation::None,
            required_setting: None,
            required_setting_mentions: Vec::new(),
            event_type: NotificationEventType::Test,
            descriptor: Vec::new(),
            descriptor_defaults: true,
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

    /// One setting Scryer would have resolved for this channel.
    pub fn config(mut self, key: &str, value: &str) -> Self {
        self.config.insert(key.to_string(), value.to_string());
        self
    }

    /// The upstream endpoint a `send` must reach, built from that
    /// configuration.
    pub fn expects_url_prefix(mut self, prefix: &str) -> Self {
        self.url = UrlExpectation::Prefix(prefix.to_string());
        self
    }

    /// The whole endpoint, for a channel that appends nothing.
    pub fn expects_url(mut self, url: &str) -> Self {
        self.url = UrlExpectation::Exact(url.to_string());
        self
    }

    /// The setting whose absence must be a typed error, and which the operator
    /// has to be told about by name.
    pub fn required_setting(mut self, key: &str) -> Self {
        self.required_setting = Some(key.to_string());
        if self.required_setting_mentions.is_empty() {
            self.required_setting_mentions.push(key.to_string());
        }
        self
    }

    /// Another string the missing-setting message must carry — a legacy
    /// fallback name the operator may still be using.
    pub fn required_setting_mentions(mut self, text: &str) -> Self {
        self.required_setting_mentions.push(text.to_string());
        self
    }

    /// The event a `send` carries, for a channel whose delivery is a library
    /// refresh and which therefore has nothing to do with a `Test`.
    pub fn event_type(mut self, event_type: NotificationEventType) -> Self {
        self.event_type = event_type;
        self
    }

    /// An extra descriptor field this channel pins.
    pub fn expects_descriptor(mut self, path: &[&str], value: serde_json::Value) -> Self {
        self.descriptor
            .push(DescriptorExpectation::new(path, value));
        self
    }

    /// Drop the family's default descriptor expectations, for a channel whose
    /// authority is not "HTTP and nothing else".
    pub fn without_descriptor_defaults(mut self) -> Self {
        self.descriptor_defaults = false;
        self
    }

    /// Drop one shared check, because the channel asserts something stronger in
    /// its own file — or because the check does not apply to it.
    pub fn without(mut self, check: Check) -> Self {
        self.skipped.push(check);
        self
    }

    /// The built release artifact.
    pub fn wasm_path(&self) -> PathBuf {
        build_plugin_wasm(&self.manifest_dir, &self.wasm_name)
    }

    /// The configuration Scryer would have resolved for this channel, with a
    /// host that accepts whatever it is sent.
    pub fn script(&self) -> Script {
        Script {
            config: ConfigSource::Resolved(self.config.clone()),
            state: StateSource::Ephemeral,
            http: HttpScript::accepted(),
            ..Script::default()
        }
    }

    /// The same, with one or more settings the host pretends are unset.
    pub fn script_without(&self, keys: &[&str]) -> Script {
        Script {
            unset: keys.iter().map(|key| key.to_string()).collect(),
            ..self.script()
        }
    }

    fn runs(&self, check: Check) -> bool {
        !self.skipped.contains(&check)
    }

    /// The family's default check set, in order.
    pub fn run(&self) -> Outcome {
        if self.runs(Check::ArtifactIsAComponent) {
            self.assert_artifact_is_a_component();
        }
        if self.runs(Check::WorldConformance) {
            self.assert_world_conformance();
        }
        if self.runs(Check::Describe) {
            self.assert_describe_returns_a_notification_descriptor();
        }
        let send = if self.runs(Check::SendReachesTheConfiguredEndpoint) {
            self.assert_send_reaches_the_configured_endpoint_over_host_http()
        } else {
            Script::default()
        };
        if self.runs(Check::UpstreamFailureIsReported) {
            self.assert_an_upstream_failure_is_a_reported_delivery_failure();
        }
        if self.runs(Check::RefusedHttpStaysInBand) {
            self.assert_a_refused_http_capability_stays_in_band();
        }
        if self.runs(Check::MissingRequiredSetting) && self.required_setting.is_some() {
            self.assert_a_missing_required_setting_is_a_typed_error();
        }
        if self.runs(Check::ActionIsUnsupported) {
            self.assert_action_is_unsupported_in_band();
        }
        if self.runs(Check::AnotherFamilyIsAnInvocationError) {
            self.assert_another_family_is_an_invocation_error();
        }
        Outcome { send }
    }

    /// The notification host has no core-module backing, so a core wasm
    /// artifact is not a degraded plugin but an uninstallable one.
    pub fn assert_artifact_is_a_component(&self) {
        crate::assert_artifact_is_a_component(&self.wasm_path());
    }

    /// The exact check the host performs on install: the artifact compiles,
    /// every import it emits is satisfiable from WASI Preview 2 plus the
    /// world's `services` interface, and its exports match
    /// `scryer:notification/notification@1.0.0`.
    ///
    /// This is also the regression guard for the *import set*. The PDK links
    /// one crate against two different component contracts, and the published
    /// `scryer-plugin-sdk` still declares host-function externs behind its
    /// `net` and process modules — so a component that accidentally keeps a
    /// live `scryer:indexer/host` import, or one of the legacy host-namespace
    /// imports that SDK can still emit, compiles perfectly and then fails to
    /// instantiate under this host.
    pub fn assert_world_conformance(&self) {
        let engine = Engine::default();
        let component = Component::from_file(&engine, self.wasm_path())
            .expect("compile notification component");
        let linker = linker::<DefaultResponder>(&engine);
        linker
            .instantiate_pre(&component)
            .and_then(notification_world::NotificationPre::new)
            .expect("the artifact must satisfy scryer:notification/notification@1.0.0");
    }

    /// `describe` is a world export now, not a bare exported symbol: the host
    /// calls it directly and parses the returned bytes as a `PluginDescriptor`.
    pub fn assert_describe_returns_a_notification_descriptor(&self) {
        let descriptor = self.describe();

        assert_eq!(descriptor["id"], self.plugin_id);
        // `ProviderDescriptor` is internally tagged on `kind`, so the
        // notification fields sit alongside it rather than under a nested key.
        assert_eq!(descriptor["provider"]["kind"], "notification");
        assert_eq!(descriptor["provider"]["provider_type"], self.provider_type);
        if self.descriptor_defaults {
            assert_eq!(
                descriptor["provider"]["capabilities"]["requires_host_filesystem"], false,
                "notification channels receive no filesystem preopens on any operation"
            );
            assert_eq!(
                descriptor["provider"]["capabilities"]["requires_host_process"], false,
                "this channel delivers over HTTP and must not ask for process authority"
            );
        }
        for expectation in &self.descriptor {
            expectation.assert(&descriptor);
        }
    }

    /// `describe` parsed, having proved it was pure.
    ///
    /// Purity is asserted here rather than in the caller because the host runs
    /// `describe` during packaging against an inert services import: it may not
    /// touch config, state, or HTTP, whatever else a channel goes on to pin.
    pub fn describe(&self) -> serde_json::Value {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
        let bytes = plugin.call_describe(&mut store).expect("call describe");
        assert_describe_was_pure(&store.data().script);
        descriptor_from(&bytes)
    }

    /// The channel's configuration and its upstream request both travel over
    /// the one `host-call` import, and the endpoint is built from that
    /// configuration rather than from anything ambient.
    ///
    /// Returns the scripted host's recording, so a channel that carries the
    /// part that matters in a header or a body can go on to pin it.
    pub fn assert_send_reaches_the_configured_endpoint_over_host_http(&self) -> Script {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
        let result = call_notification(
            &mut store,
            &plugin,
            PluginNotificationCommand::Send(test_request(self.event_type)),
        );
        let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
            panic!("send did not return a typed ok result: {result:?}");
        };
        assert!(response.success, "delivery failed: {response:?}");

        let script = &store.data().script;
        assert!(
            script
                .calls
                .iter()
                .any(|call| call.starts_with("config_get:")),
            "the channel must read its settings through host services: {:?}",
            script.calls
        );
        match &self.url {
            UrlExpectation::Prefix(prefix) => assert!(
                script.urls.iter().any(|url| url.starts_with(prefix)),
                "the configured endpoint must be used verbatim; got {:?}",
                script.urls
            ),
            UrlExpectation::Exact(expected) => assert!(
                script.urls.iter().any(|url| url == expected),
                "the configured endpoint must be used verbatim; got {:?}",
                script.urls
            ),
            UrlExpectation::None => {}
        }
        script.clone()
    }

    /// An upstream rejection is not a plugin failure: the channel reports an
    /// unsuccessful delivery with the provider's own status, and the operator
    /// sees what the provider said. That behaviour predates the migration and
    /// must survive it.
    pub fn assert_an_upstream_failure_is_a_reported_delivery_failure(&self) {
        let script = Script {
            http: HttpScript::status(500, b"upstream exploded".to_vec()),
            ..self.script()
        };
        let (mut store, plugin) = instantiate(&self.wasm_path(), script);
        let result = call_notification(
            &mut store,
            &plugin,
            PluginNotificationCommand::Send(test_request(self.event_type)),
        );
        let PluginNotificationCommandResult::Send(PluginResult::Ok(response)) = result else {
            panic!("an upstream failure must stay in-band: {result:?}");
        };
        assert!(!response.success, "a 500 cannot be a successful delivery");
    }

    /// Capability availability is in-band. A Scryer whose HTTP egress refuses
    /// this channel answers through the response, never through `host-error`,
    /// and the channel must surface that as a reported failure rather than a
    /// world-level invocation failure or a trap.
    pub fn assert_a_refused_http_capability_stays_in_band(&self) {
        let script = Script {
            http: HttpScript::Refused,
            ..self.script()
        };
        let (mut store, plugin) = instantiate(&self.wasm_path(), script);
        let result = call_notification(
            &mut store,
            &plugin,
            PluginNotificationCommand::Send(test_request(self.event_type)),
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

    /// A missing required setting used to be a `FnResult` hard fault: the host
    /// saw a string and a generic ABI failure, indistinguishable from a crashed
    /// plugin. It is now a typed `PluginResult::Err` the operator can act on,
    /// and — the part that matters under a component — the instance survives
    /// it.
    pub fn assert_a_missing_required_setting_is_a_typed_error(&self) {
        let key = self
            .required_setting
            .as_deref()
            .expect("a channel asserting this check names the setting");
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script_without(&[key]));
        let result = call_notification(
            &mut store,
            &plugin,
            PluginNotificationCommand::Send(test_request(self.event_type)),
        );
        let PluginNotificationCommandResult::Send(PluginResult::Err(error)) = result else {
            panic!("a missing required setting must be a typed plugin error: {result:?}");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        for mention in &self.required_setting_mentions {
            assert!(
                error.public_message.contains(mention.as_str()),
                "the operator has to be told which setting, including any legacy \
                 fallback ({mention}): {error:?}"
            );
        }
    }

    /// This channel has no interactive action. The host reads that from the
    /// descriptor and never routes one here, so the arm exists to answer rather
    /// than to trap — a trap under a component costs the whole instance.
    pub fn assert_action_is_unsupported_in_band(&self) {
        let (mut store, plugin) = instantiate(&self.wasm_path(), self.script());
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

        let outcome = plugin
            .call_process(&mut store, &request)
            .expect("process call itself succeeds");
        assert!(
            outcome.is_err(),
            "a download-client command must not produce a notification response"
        );
    }
}
