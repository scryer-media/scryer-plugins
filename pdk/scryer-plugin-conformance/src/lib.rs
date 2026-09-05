//! The shared half of every plugin's `tests/host_conformance.rs`.
//!
//! Each family component's contract is not "these functions behave" but "this
//! exact `.wasm` runs under Scryer's host". Every plugin therefore builds its
//! shipping `wasm32-wasip2` artifact and drives it the way the production host
//! does: the family world is linked, the shared `scryer:host/services@1.0.0`
//! import is served by a scripted stand-in for `CommandHost` speaking the same
//! postcard `PluginHostRequest`/`PluginHostResponse`, WASI Preview 2 comes from
//! the linker, and `process` carries the `PluginCommandRequest` JSON envelope.
//!
//! That machinery was copied into 36 files before this crate existed. What is
//! genuinely per-plugin — the descriptor identity, the scripted configuration,
//! the endpoint a delivery must reach — stays in the plugin, as a handful of
//! builder calls. What is not stays here, once, with the explanations of *why*
//! each check exists intact.
//!
//! # Layout
//!
//! - This module is family-agnostic: the artifact checks, the release build,
//!   the scripted host switchboard, and the [`HostResponder`] seam a plugin
//!   with extra host-call variants extends without reopening this crate.
//! - [`download_client`], [`notification`] and [`subtitle`] are behind cargo
//!   features of the same name. Each owns its own
//!   `wasmtime::component::bindgen!` over the WIT vendored under this crate's
//!   `wit/`, plus the family's default check set as a builder.
//!
//! # This crate is a dev-dependency, and only ever a dev-dependency
//!
//! Cargo unifies features across normal and dev dependencies in a single build
//! graph. Pulling this crate in behind a feature of the PDK or of
//! `notify-common` would therefore put `wasmtime` into the `cdylib`'s graph and
//! break the `wasm32-wasip2` build outright.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use scryer_plugin_sdk::host::{
    PluginConfigGetResponse, PluginHostRequest, PluginHostResponse, PluginHttpResponse,
    PluginStateGetResponse, PluginStateMutationResponse,
};
use scryer_plugin_sdk::{PluginError, PluginErrorCode, PluginResult};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[cfg(feature = "download-client")]
pub mod download_client;
#[cfg(feature = "notification")]
pub mod notification;
#[cfg(feature = "subtitle")]
pub mod subtitle;

// ---------------------------------------------------------------------------
// Artifact shape
// ---------------------------------------------------------------------------

/// No family host has a core-module backing, so a core wasm artifact is not a
/// degraded plugin but an uninstallable one. Check the component preamble
/// directly rather than inferring it from a link failure.
pub fn assert_artifact_is_a_component(wasm_path: &Path) {
    let bytes = std::fs::read(wasm_path)
        .unwrap_or_else(|error| panic!("read plugin wasm {}: {error}", wasm_path.display()));
    assert!(
        bytes.starts_with(b"\0asm\r\0\x01\0"),
        "the release artifact must be a WebAssembly component, not a core module"
    );
}

// ---------------------------------------------------------------------------
// Building the release artifact
// ---------------------------------------------------------------------------

/// Build the plugin's shipping `wasm32-wasip2` component and return the path to
/// it.
///
/// The artifact location is read back out of cargo's own
/// `compiler-artifact` messages rather than assembled from the plugin
/// directory. The assembled form — `<plugin>/target/wasm32-wasip2/…` — is wrong
/// the moment `CARGO_TARGET_DIR` is set, which is exactly what a developer does
/// to stop 36 plugins each building their own copy of wasmtime.
///
/// One test binary drives one plugin, so the process-wide cache keyed by
/// manifest directory only ever holds a single entry in practice; it is keyed
/// anyway so nothing here depends on that staying true.
pub fn build_plugin_wasm(manifest_dir: &str, wasm_name: &str) -> PathBuf {
    static BUILT: OnceLock<Mutex<HashMap<(PathBuf, String), PathBuf>>> = OnceLock::new();
    let cache = BUILT.get_or_init(|| Mutex::new(HashMap::new()));

    let plugin_root = PathBuf::from(manifest_dir);
    let key = (plugin_root.clone(), wasm_name.to_string());
    if let Some(path) = cache.lock().expect("conformance build cache").get(&key) {
        return path.clone();
    }

    let path = build_plugin_wasm_uncached(&plugin_root, wasm_name);
    cache
        .lock()
        .expect("conformance build cache")
        .insert(key, path.clone());
    path
}

fn build_plugin_wasm_uncached(plugin_root: &Path, wasm_name: &str) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(plugin_root.join("Cargo.toml"))
        .arg("--profile")
        .arg("plugin-release")
        .arg("--target")
        .arg("wasm32-wasip2")
        // Diagnostics still render to stderr in their usual form; only the
        // machine-readable artifact records come back on stdout.
        .arg("--message-format=json-render-diagnostics")
        .stderr(Stdio::inherit())
        .output()
        .expect("run cargo build for the plugin");
    assert!(
        output.status.success(),
        "plugin build failed: {}",
        output.status
    );

    if let Some(path) = artifact_from_cargo_messages(&output.stdout, wasm_name) {
        return path;
    }

    // Cargo emits a `compiler-artifact` record even for a fresh build, so this
    // is a belt-and-braces path rather than the normal one. It still honours
    // `CARGO_TARGET_DIR`, which is the whole point of not hardcoding
    // `<plugin>/target`.
    let fallback = target_dir(plugin_root)
        .join("wasm32-wasip2")
        .join("plugin-release")
        .join(wasm_name);
    assert!(
        fallback.is_file(),
        "cargo reported no {wasm_name} artifact and none is at {}",
        fallback.display()
    );
    fallback
}

/// Pick the requested `.wasm` out of cargo's JSON artifact stream.
fn artifact_from_cargo_messages(stdout: &[u8], wasm_name: &str) -> Option<PathBuf> {
    let mut newest: Option<PathBuf> = None;
    for line in stdout.split(|byte| *byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(filenames) = message["filenames"].as_array() else {
            continue;
        };
        for filename in filenames {
            let Some(filename) = filename.as_str() else {
                continue;
            };
            let path = PathBuf::from(filename);
            if path.file_name().and_then(|name| name.to_str()) == Some(wasm_name) {
                newest = Some(path);
            }
        }
    }
    newest.filter(|path| path.is_file())
}

/// Where cargo puts build output for this plugin, honouring an overridden
/// target directory.
fn target_dir(plugin_root: &Path) -> PathBuf {
    for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(value) = std::env::var_os(key)
            && !value.is_empty()
        {
            return PathBuf::from(value);
        }
    }
    plugin_root.join("target")
}

// ---------------------------------------------------------------------------
// The scripted `CommandHost`
// ---------------------------------------------------------------------------

/// What the scripted host answers a `ConfigGet` with.
#[derive(Clone, Debug, Default)]
pub enum ConfigSource {
    /// A Scryer configured with nothing at all: every read is refused in-band.
    #[default]
    Refused,
    /// The configuration Scryer would have resolved for this plugin.
    Resolved(BTreeMap<String, String>),
}

/// What the scripted host answers the encoded state door with.
#[derive(Clone, Debug, Default)]
pub enum StateSource {
    /// No plugin state is available; every operation is refused in-band.
    #[default]
    Refused,
    /// Reads answer `none` and writes report a change, storing nothing.
    Ephemeral,
    /// A real map. The host backs every invocation of one configured plugin
    /// with a single `CommandHost`, and therefore a single state map — which is
    /// what lets a session cookie outlive the instance that stored it.
    Stored(BTreeMap<String, Vec<u8>>),
}

/// One scripted upstream response.
#[derive(Clone, Debug)]
pub struct HttpReply {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpReply {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body,
        }
    }

    pub fn ok(body: &str) -> Self {
        Self::new(200, body.as_bytes().to_vec())
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_string(), value.to_string());
        self
    }
}

/// How a scripted route decides whether it answers a request.
#[derive(Clone, Debug)]
pub enum UrlMatch {
    /// Any URL. One catch-all route is the single-hop provider's whole script.
    Any,
    /// A substring of the request URL. Enough to name a hop (`/login`) without
    /// pinning the query string an assertion elsewhere already covers.
    Contains(String),
    /// The whole URL, verbatim. An unrouted request then fails loudly rather
    /// than quietly returning somebody else's body.
    Exact(String),
}

impl UrlMatch {
    fn matches(&self, url: &str) -> bool {
        match self {
            UrlMatch::Any => true,
            UrlMatch::Contains(needle) => url.contains(needle.as_str()),
            UrlMatch::Exact(expected) => url == expected,
        }
    }
}

/// One entry in the scripted route table.
#[derive(Clone, Debug)]
pub struct HttpRoute {
    pub url: UrlMatch,
    pub reply: HttpReply,
}

impl HttpRoute {
    pub fn contains(url: &str, reply: HttpReply) -> Self {
        Self {
            url: UrlMatch::Contains(url.to_string()),
            reply,
        }
    }

    pub fn exact(url: &str, reply: HttpReply) -> Self {
        Self {
            url: UrlMatch::Exact(url.to_string()),
            reply,
        }
    }

    pub fn any(reply: HttpReply) -> Self {
        Self {
            url: UrlMatch::Any,
            reply,
        }
    }
}

/// What the scripted host does with an outbound HTTP request.
#[derive(Clone, Debug, Default)]
pub enum HttpScript {
    /// The host itself refuses the request, in-band — a Scryer with no egress
    /// configured for this plugin, or one enforcing an origin policy.
    #[default]
    Refused,
    /// Requests are answered from an ordered route table. A request no route
    /// claims is refused in-band, which is how "the host refuses everything"
    /// is expressed as an empty table.
    Routed(Vec<HttpRoute>),
}

impl HttpScript {
    /// The provider accepted whatever it was sent.
    pub fn accepted() -> Self {
        HttpScript::Routed(vec![HttpRoute::any(HttpReply::ok("{}"))])
    }

    /// The provider answered, badly.
    pub fn status(status: u16, body: Vec<u8>) -> Self {
        HttpScript::Routed(vec![HttpRoute::any(HttpReply::new(status, body))])
    }
}

/// One outbound request, exactly as the plugin asked the host to make it.
///
/// Several channels carry the part that matters somewhere other than the URL —
/// a credential header, a form body, a JSON field — so the whole request is
/// recorded rather than just its URL.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: Option<String>,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The first header with this name, case-insensitively — the way a server
    /// would read it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The scripted host's configuration and its recording of what the plugin did.
#[derive(Clone, Debug, Default)]
pub struct Script {
    pub config: ConfigSource,
    /// Config keys the host should pretend are unset, whatever `config` says.
    pub unset: Vec<String>,
    pub state: StateSource,
    /// Plugin-owned state behind the *typed* `state-get`/`state-cas` pair of
    /// the 1.1 subtitle runtime. A real map, because the typed pair's whole
    /// advantage over the encoded door is that the compare-and-swap is atomic,
    /// and a gate spinning on it has to read back the value it just wrote.
    pub runtime_state: BTreeMap<String, Vec<u8>>,
    pub http: HttpScript,
    /// Whether the `http:` call-log entry carries the request method. A client
    /// whose contract distinguishes `POST /auth/login` from a `GET` of the same
    /// path needs it; a channel that only ever POSTs does not, and its
    /// assertions read better without it.
    pub log_http_method: bool,
    /// Every host call the plugin made, in order, as `kind:detail`.
    pub calls: Vec<String>,
    /// Every URL the plugin asked the host to fetch.
    pub urls: Vec<String>,
    /// Every request the plugin asked the host to make, whole.
    pub requests: Vec<RecordedRequest>,
}

impl Script {
    /// The stored state map, for a script that keeps one.
    pub fn stored_state(&self) -> BTreeMap<String, Vec<u8>> {
        match &self.state {
            StateSource::Stored(state) => state.clone(),
            _ => BTreeMap::new(),
        }
    }

    /// Whether this exact call was recorded.
    pub fn made_call(&self, call: &str) -> bool {
        self.calls.iter().any(|recorded| recorded == call)
    }

    /// The first recorded request whose URL starts with `prefix`.
    pub fn request_with_url_prefix(&self, prefix: &str) -> Option<&RecordedRequest> {
        self.requests
            .iter()
            .find(|request| request.url.starts_with(prefix))
    }

    /// The first recorded request to exactly this URL.
    pub fn request_to(&self, url: &str) -> Option<&RecordedRequest> {
        self.requests.iter().find(|request| request.url == url)
    }
}

/// The transport-level failures the world's `host-error` is reserved for.
///
/// Named here rather than reusing a family's generated `HostError`, because the
/// switchboard is shared and each family's bindgen mints its own copy of that
/// type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostErrorKind {
    /// A request that cannot be decoded.
    InvalidRequest,
    /// The host itself failed.
    Failed,
}

/// The seam a plugin with host-call variants beyond the shared five extends.
///
/// Email drives `SocketOpen`/`Write`/`Read`/`StartTls`/`Close`; the tsukihime
/// pilot reaches `ArchiveExtract`. Both supply their own responder, answer the
/// arms they own, and hand everything else to [`default_respond`] — so the
/// shared switchboard stays the shared switchboard and neither plugin has to
/// reopen this crate.
pub trait HostResponder: Send + 'static {
    fn respond(
        &mut self,
        request: PluginHostRequest,
        script: &mut Script,
    ) -> Result<PluginHostResponse, HostErrorKind>;
}

/// The switchboard every family shares: `ConfigGet`, `StateGet`, `StateSet`,
/// `StateDelete` and `Http`, answered from the [`Script`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultResponder;

impl HostResponder for DefaultResponder {
    fn respond(
        &mut self,
        request: PluginHostRequest,
        script: &mut Script,
    ) -> Result<PluginHostResponse, HostErrorKind> {
        default_respond(request, script)
    }
}

/// The shared switchboard, as a free function so a plugin's own responder can
/// delegate to it.
///
/// `host-error` is reserved for the transport: a request that cannot be
/// decoded. Everything a real host would refuse — an unconfigured capability, a
/// denied origin — is a well-formed response carrying a typed [`PluginError`],
/// and answering that way here is what makes the in-band assertions meaningful.
pub fn default_respond(
    request: PluginHostRequest,
    script: &mut Script,
) -> Result<PluginHostResponse, HostErrorKind> {
    let response =
        match request {
            PluginHostRequest::ConfigGet(request) => {
                script.calls.push(format!("config_get:{}", request.key));
                match &script.config {
                    ConfigSource::Refused => PluginHostResponse::ConfigGet(PluginResult::Err(
                        unsupported("this host is configured with nothing"),
                    )),
                    ConfigSource::Resolved(config) => {
                        let value = if script.unset.contains(&request.key) {
                            None
                        } else {
                            config.get(&request.key).cloned()
                        };
                        PluginHostResponse::ConfigGet(PluginResult::Ok(PluginConfigGetResponse {
                            value,
                        }))
                    }
                }
            }
            PluginHostRequest::StateGet(request) => {
                script.calls.push(format!("state_get:{}", request.key));
                match &script.state {
                    StateSource::Refused => PluginHostResponse::StateGet(PluginResult::Err(
                        unsupported("no plugin state is available"),
                    )),
                    StateSource::Ephemeral => {
                        PluginHostResponse::StateGet(PluginResult::Ok(PluginStateGetResponse {
                            value: None,
                        }))
                    }
                    StateSource::Stored(state) => {
                        PluginHostResponse::StateGet(PluginResult::Ok(PluginStateGetResponse {
                            value: state.get(&request.key).cloned(),
                        }))
                    }
                }
            }
            PluginHostRequest::StateSet(request) => {
                script.calls.push(format!("state_set:{}", request.key));
                match &mut script.state {
                    StateSource::Refused => PluginHostResponse::StateSet(PluginResult::Err(
                        unsupported("no plugin state is available"),
                    )),
                    StateSource::Ephemeral => PluginHostResponse::StateSet(PluginResult::Ok(
                        PluginStateMutationResponse { changed: true },
                    )),
                    StateSource::Stored(state) => {
                        let changed = state.insert(request.key, request.value).is_none();
                        PluginHostResponse::StateSet(PluginResult::Ok(
                            PluginStateMutationResponse { changed },
                        ))
                    }
                }
            }
            PluginHostRequest::StateDelete(request) => {
                script.calls.push(format!("state_delete:{}", request.key));
                match &mut script.state {
                    StateSource::Refused => PluginHostResponse::StateDelete(PluginResult::Err(
                        unsupported("no plugin state is available"),
                    )),
                    StateSource::Ephemeral => PluginHostResponse::StateDelete(PluginResult::Ok(
                        PluginStateMutationResponse { changed: true },
                    )),
                    StateSource::Stored(state) => {
                        let changed = state.remove(&request.key).is_some();
                        PluginHostResponse::StateDelete(PluginResult::Ok(
                            PluginStateMutationResponse { changed },
                        ))
                    }
                }
            }
            PluginHostRequest::Http(request) => {
                let method = request.method.clone().unwrap_or_else(|| "GET".to_string());
                if script.log_http_method {
                    script.calls.push(format!("http:{method} {}", request.url));
                } else {
                    script.calls.push(format!("http:{}", request.url));
                }
                script.urls.push(request.url.clone());
                script.requests.push(RecordedRequest {
                    method: request.method.clone(),
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                });

                let reply = match &script.http {
                    HttpScript::Refused => None,
                    HttpScript::Routed(routes) => routes
                        .iter()
                        .find(|route| route.url.matches(&request.url))
                        .map(|route| route.reply.clone()),
                };
                match reply {
                    Some(reply) => PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                        status: reply.status,
                        headers: reply.headers,
                        body: reply.body,
                    })),
                    None => PluginHostResponse::Http(PluginResult::Err(unsupported(&format!(
                        "no HTTP egress is configured for this plugin: {method} {}",
                        request.url
                    )))),
                }
            }
            other => {
                script.calls.push(format!("unscripted:{other:?}"));
                return Err(HostErrorKind::Failed);
            }
        };

    Ok(response)
}

/// The in-band "this host cannot do that" answer.
///
/// Every optional field is populated deliberately: `PluginError` carries
/// `skip_serializing_if` on `debug_message` and `retry_after_seconds`, which a
/// non-self-describing format like postcard cannot round-trip — a `None` there
/// produces bytes the guest decoder rejects outright. Until the SDK drops those
/// attributes, a host answering in-band must fill them in.
pub fn unsupported(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: message.to_string(),
        debug_message: Some(message.to_string()),
        retry_after_seconds: Some(0),
        details: None,
    }
}

// ---------------------------------------------------------------------------
// The store the component runs in
// ---------------------------------------------------------------------------

/// The store data every family's component runs against.
///
/// Generic in the responder rather than boxed, so a plugin that supplies its
/// own can still read its recorder back off `store.data().responder` after the
/// call.
pub struct Ctx<R: HostResponder = DefaultResponder> {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub script: Script,
    pub responder: R,
}

impl<R: HostResponder> Ctx<R> {
    /// A context with the family's default authority.
    ///
    /// No filesystem preopens, matching every family world's documented
    /// authority: none of them has ever been handed a preopened directory,
    /// under the reactor, the wasip1 command host, or this one. The host
    /// captures guest stderr and tails it into its own error messages;
    /// inheriting it here puts the same text in front of whoever is reading the
    /// test failure.
    pub fn new(script: Script, responder: R) -> Self {
        Self::with_wasi(
            script,
            responder,
            WasiCtxBuilder::new().inherit_stderr().build(),
        )
    }

    /// A context whose WASI authority is supplied by the caller — the sync
    /// plugin's case, where the host stages preopened roots per job.
    pub fn with_wasi(script: Script, responder: R, wasi: WasiCtx) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi,
            script,
            responder,
        }
    }

    /// The one host implementation behind every door the world offers.
    pub fn dispatch(
        &mut self,
        request: PluginHostRequest,
    ) -> Result<PluginHostResponse, HostErrorKind> {
        self.responder.respond(request, &mut self.script)
    }

    /// The encoded door: postcard in, postcard out.
    pub fn host_call_bytes(&mut self, request: Vec<u8>) -> Result<Vec<u8>, HostErrorKind> {
        let request: PluginHostRequest =
            postcard::from_bytes(&request).map_err(|_| HostErrorKind::InvalidRequest)?;
        let response = self.dispatch(request)?;
        postcard::to_allocvec(&response).map_err(|_| HostErrorKind::Failed)
    }
}

impl<R: HostResponder> WasiView for Ctx<R> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared descriptor helpers
// ---------------------------------------------------------------------------

/// A `descriptor["a"]["b"]` lookup written as a path, so a builder can carry
/// per-plugin descriptor expectations without a closure.
#[derive(Clone, Debug)]
pub struct DescriptorExpectation {
    pub path: Vec<String>,
    pub value: serde_json::Value,
}

impl DescriptorExpectation {
    pub fn new(path: &[&str], value: serde_json::Value) -> Self {
        Self {
            path: path.iter().map(|segment| segment.to_string()).collect(),
            value,
        }
    }

    pub fn assert(&self, descriptor: &serde_json::Value) {
        let mut cursor = descriptor;
        for segment in &self.path {
            // A numeric segment indexes an array — descriptors carry lists,
            // such as a channel's socket grants.
            cursor = match segment.parse::<usize>() {
                Ok(index) if cursor.is_array() => &cursor[index],
                _ => &cursor[segment.as_str()],
            };
        }
        assert_eq!(
            cursor,
            &self.value,
            "descriptor{} must be {}",
            self.path
                .iter()
                .map(|segment| format!("[{segment:?}]"))
                .collect::<String>(),
            self.value
        );
    }
}

/// Parse the bytes `describe` returned, with the plugin's own output in the
/// failure message.
pub fn descriptor_from(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or_else(|error| {
        panic!(
            "describe did not return valid JSON ({error}): {}",
            String::from_utf8_lossy(bytes)
        )
    })
}

/// `describe` must be a pure function of the artifact: the host runs it during
/// packaging against an inert services import, so it may not touch config,
/// state, HTTP, or any other host service.
pub fn assert_describe_was_pure(script: &Script) {
    assert!(
        script.calls.is_empty(),
        "describe used host services: {:?}",
        script.calls
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_directory_defaults_to_the_plugin_root() {
        // No override in this process, so the plugin's own `target/` is where
        // the fallback looks.
        if std::env::var_os("CARGO_TARGET_DIR").is_some()
            || std::env::var_os("CARGO_BUILD_TARGET_DIR").is_some()
        {
            // The suite is running under a shared target dir; the override case
            // below is the one that matters and this one cannot be observed.
            return;
        }
        assert_eq!(
            target_dir(Path::new("/plugins/deluge")),
            PathBuf::from("/plugins/deluge/target")
        );
    }

    #[test]
    fn an_overridden_target_directory_wins_over_the_plugin_root() {
        // The latent bug this crate fixes: the pre-migration harness hardcoded
        // `<plugin>/target/wasm32-wasip2/plugin-release/<name>.wasm`, which is
        // simply not where the artifact is once `CARGO_TARGET_DIR` is set.
        let resolved = match std::env::var_os("CARGO_TARGET_DIR") {
            Some(shared) => {
                assert_eq!(
                    target_dir(Path::new("/plugins/deluge")),
                    PathBuf::from(shared)
                );
                return;
            }
            None => target_dir(Path::new("/plugins/deluge")),
        };
        assert_eq!(resolved, PathBuf::from("/plugins/deluge/target"));
    }

    #[test]
    fn the_artifact_path_comes_out_of_cargos_json_stream() {
        let stdout = br#"{"reason":"compiler-artifact","filenames":["/shared/wasm32-wasip2/plugin-release/deluge_download_client.wasm"]}
{"reason":"build-finished","success":true}
"#;
        // The file does not exist, so the filter rejects it — which is the
        // behaviour that makes the fallback path reachable rather than handing
        // back a path to nothing.
        assert!(artifact_from_cargo_messages(stdout, "deluge_download_client.wasm").is_none());
        assert!(artifact_from_cargo_messages(stdout, "other.wasm").is_none());
    }

    #[test]
    fn a_real_artifact_path_is_returned_verbatim() {
        let file = std::env::temp_dir().join("scryer_conformance_probe.wasm");
        std::fs::write(&file, b"\0asm\r\0\x01\0").expect("write probe artifact");
        let stdout = format!(
            r#"{{"reason":"compiler-artifact","filenames":["{}"]}}"#,
            file.display()
        );
        assert_eq!(
            artifact_from_cargo_messages(stdout.as_bytes(), "scryer_conformance_probe.wasm"),
            Some(file.clone())
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn an_unrouted_request_is_refused_in_band() {
        let mut script = Script {
            http: HttpScript::Routed(vec![HttpRoute::exact(
                "https://example.invalid/a",
                HttpReply::ok("{}"),
            )]),
            ..Script::default()
        };
        let response = default_respond(
            PluginHostRequest::Http(scryer_plugin_sdk::host::PluginHttpRequest {
                url: "https://example.invalid/b".to_string(),
                method: None,
                headers: BTreeMap::new(),
                body: Vec::new(),
            }),
            &mut script,
        )
        .expect("an unrouted request is in-band, not a transport failure");
        assert!(matches!(
            response,
            PluginHostResponse::Http(PluginResult::Err(_))
        ));
        assert_eq!(script.urls, vec!["https://example.invalid/b".to_string()]);
    }
}
