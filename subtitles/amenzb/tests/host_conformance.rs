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
//! # What is specific to ameNZB
//!
//! This is the only subtitle provider that does not own its search: it
//! delegates to the shared newznab engine (`newznab-common-legacy`), which is
//! also linked into Preview 1 indexer guests. That shared crate is exactly
//! where a stray world import would come from, and it would compile and build
//! perfectly before failing to instantiate here. So [`assert_world_conformance`]
//! is load-bearing for this plugin in a way it is not for the self-contained
//! providers, and [`assert_search_drives_the_shared_newznab_engine`] is the
//! assertion that proves the shared engine actually *works* over
//! `scryer:host/services` rather than merely linking.
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
    SubtitlePluginDownloadRequest, SubtitlePluginGenerateRequest, SubtitlePluginSearchRequest,
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

const BASE_URL: &str = "https://amenzb.test";
const API_ENDPOINT: &str = "https://amenzb.test/api";
const RELEASE_ID: &str = "172993653";
const SUBTITLE_ID: &str = "10857";
const TEST_API_KEY: &str = "test-api-key";
const SUBTITLE_BYTES: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n";

static PLUGIN_WASM: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn amenzb_release_wasm_conforms_to_the_subtitle_host_contract() {
    let wasm_path = amenzb_plugin_wasm();

    assert_artifact_is_a_component(&wasm_path);
    assert_world_conformance(&wasm_path);
    assert_describe_returns_a_catalog_subtitle_descriptor(&wasm_path);
    assert_validate_config_reaches_the_host_services(&wasm_path);
    assert_search_drives_the_shared_newznab_engine(&wasm_path);
    assert_download_streams_the_subtitle_through_host_http(&wasm_path);
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
    let bytes = std::fs::read(wasm_path).expect("read amenzb plugin wasm");
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
/// This is the *import set* regression guard, and for ameNZB it guards two
/// hazards rather than one. The PDK links one crate against two component
/// contracts, so a family component that keeps a live `scryer:indexer/host`
/// import builds cleanly and then fails to instantiate. On top of that, the
/// shared newznab engine used to reach the host through PDK 0.5.10, whose
/// `host.rs` still declared the deleted `scryer:host/v1` core-module extern.
/// Either would show up here and nowhere earlier in the build.
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

/// `describe` is a world export now, not an Extism entry point: the host calls
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

    assert_eq!(descriptor["id"], "amenzb-subtitles");
    // `ProviderDescriptor` is internally tagged on `kind`, so the subtitle
    // fields sit alongside it rather than under a nested key.
    assert_eq!(descriptor["provider"]["kind"], "subtitle");
    assert_eq!(descriptor["provider"]["provider_type"], "amenzb");
    assert_eq!(
        descriptor["provider"]["capabilities"]["mode"], "catalog",
        "ameNZB advertises itself as a catalog provider"
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

/// Every configuration value travels over the one `host-call` import.
///
/// ameNZB validates its configuration locally — there is no upstream probe —
/// so the assertion pins both halves: the key is read through host services,
/// and no HTTP is attempted while doing it. A validation that quietly started
/// calling upstream would be a new egress from a code path Scryer runs on
/// every settings save.
fn assert_validate_config_reaches_the_host_services(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::default());

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
        "validate_config should accept a configured provider: {response:?}"
    );

    let calls = &store.data().script.calls;
    assert!(
        calls.iter().any(|call| call == "config_get:api_key"),
        "the provider must read its API key through host services: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call == "config_get:base_url"),
        "the provider must read its base URL through host services: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("http:")),
        "validate_config is local; it must not reach upstream: {calls:?}"
    );
}

/// The assertion this whole work package exists for.
///
/// ameNZB's search is `newznab_common::execute_raw_search` — shared code that
/// also serves Preview 1 indexer guests — followed by one detail-page fetch per
/// release. Both hops must travel over `scryer:host/services`, at the
/// configured base URL, carrying the key read from config. If the shared engine
/// were still bound to another world this component would not have
/// instantiated; if it were bound to no transport at all it would instantiate
/// and then reach nothing, which is what this checks.
fn assert_search_drives_the_shared_newznab_engine(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::routed());

    let result = call_subtitle(
        &mut store,
        &plugin,
        PluginSubtitleCommand::Search(search_request()),
    );
    let PluginSubtitleCommandResult::Search(PluginResult::Ok(response)) = result else {
        panic!("search did not return a typed ok result: {result:?}");
    };

    let candidate = response
        .results
        .first()
        .unwrap_or_else(|| panic!("search returned no candidates: {response:?}"));
    assert_eq!(candidate.language, "eng");

    let calls = &store.data().script.calls;
    let api_call = calls
        .iter()
        .find(|call| call.starts_with(&format!("http:{API_ENDPOINT}?")))
        .unwrap_or_else(|| {
            panic!("the shared newznab engine made no API call over host services: {calls:?}")
        });
    assert!(
        api_call.contains(&format!("apikey={TEST_API_KEY}")),
        "the key read from config must reach the newznab request: {api_call}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{BASE_URL}/release/{RELEASE_ID}")),
        "the release detail page must be fetched over host services: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("archive_extract")),
        "this provider does not open archives: {calls:?}"
    );
}

/// ameNZB serves plain subtitle files, so the bytes are handed to Scryer
/// exactly as they arrive. The assertion pins that — one host HTTP call, bytes
/// through untouched, no archive service involved.
fn assert_download_streams_the_subtitle_through_host_http(wasm_path: &Path) {
    let (mut store, plugin) = instantiate(wasm_path, Script::routed());

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
        content, SUBTITLE_BYTES,
        "the subtitle must reach Scryer byte-for-byte"
    );

    let calls = &store.data().script.calls;
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("http:{}", subtitle_url())),
        "the subtitle must be fetched over host services: {calls:?}"
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
    // An empty route table makes the scripted host answer `PluginResult::Err`
    // to every HTTP request, which is exactly what a Scryer refusing an egress
    // policy would send.
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
    assert!(
        !matches!(error.code, PluginErrorCode::Unsupported),
        "a refused egress is not a missing capability: {error:?}"
    );
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
            // messages, and since the PDK routes a family component's
            // diagnostics there, inheriting it puts the plugin's own log lines
            // in front of whoever is reading the test failure.
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

/// One ameNZB operation is several upstream requests — a newznab API search,
/// then a detail page, then the subtitle itself — so the stand-in matches the
/// request URL against an ordered route table rather than answering everything
/// the same way. An **empty** table is the "host refuses everything" case the
/// in-band assertion needs.
#[derive(Clone, Debug, Default)]
struct Script {
    routes: Vec<(String, u16, Vec<u8>)>,
    calls: Vec<String>,
}

impl Script {
    fn routed() -> Self {
        Self {
            // Ordered, and the order matters: the release page's URL is a
            // prefix of the subtitle URL beneath it, so the more specific
            // route has to be matched first.
            routes: vec![
                (API_ENDPOINT.to_string(), 200, newznab_feed().into_bytes()),
                (subtitle_url(), 200, SUBTITLE_BYTES.to_vec()),
                (
                    format!("{BASE_URL}/release/{RELEASE_ID}"),
                    200,
                    release_page().into_bytes(),
                ),
            ],
            calls: Vec::new(),
        }
    }

    fn answer(&self, url: &str) -> Option<(u16, Vec<u8>)> {
        self.routes
            .iter()
            .find(|(prefix, _, _)| url.starts_with(prefix.as_str()))
            .map(|(_, status, body)| (*status, body.clone()))
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
                    "base_url" => Some(BASE_URL.to_string()),
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
                match self.script.answer(&request.url) {
                    Some((status, body)) => {
                        PluginHostResponse::Http(PluginResult::Ok(PluginHttpResponse {
                            status,
                            headers: BTreeMap::new(),
                            body,
                        }))
                    }
                    None => PluginHostResponse::Http(PluginResult::Err(refused(&format!(
                        "no route scripted for {}",
                        request.url
                    )))),
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

/// The in-band "this host will not do that" answer.
///
/// Every optional field is populated deliberately: `PluginError` carries
/// `skip_serializing_if` on `debug_message` and `retry_after_seconds`, which a
/// non-self-describing format like postcard cannot round-trip — a `None` there
/// produces bytes the guest decoder rejects outright. Until the SDK drops
/// those attributes, a host answering in-band must fill them in.
fn refused(message: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::UpstreamUnavailable,
        public_message: message.to_string(),
        debug_message: Some(message.to_string()),
        retry_after_seconds: Some(0),
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn search_request() -> SubtitlePluginSearchRequest {
    SubtitlePluginSearchRequest {
        media_kind: SubtitleQueryMediaKind::Episode,
        facet: Some("anime".to_string()),
        file_hash: None,
        imdb_id: None,
        series_imdb_id: None,
        title: "Kinomi Master".to_string(),
        title_aliases: vec![],
        title_candidates: vec![],
        year: None,
        season: Some(1),
        episode: Some(12),
        absolute_episode: None,
        external_ids: BTreeMap::new(),
        languages: vec!["eng".to_string()],
        release_group: None,
        source: None,
        video_codec: None,
        audio_codec: None,
        resolution: None,
        hearing_impaired: None,
        include_ai_translated: false,
        include_machine_translated: false,
    }
}

/// One newznab item, as the shared engine parses it. The `guid` attribute is
/// what ameNZB turns into the release id it then fetches a detail page for.
fn newznab_feed() -> String {
    format!(
        r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
<channel>
  <item>
    <title>[SubsPlease] Kinomi Master - 12 (1080p) [WEB-DL]</title>
    <guid>{RELEASE_ID}</guid>
    <link>{BASE_URL}/release/{RELEASE_ID}</link>
    <pubDate>Tue, 02 Jan 2024 14:00:00 +0000</pubDate>
    <enclosure url="{BASE_URL}/dl/{RELEASE_ID}" length="1048576" type="application/x-nzb"/>
    <newznab:attr name="guid" value="{RELEASE_ID}"/>
    <newznab:attr name="grabs" value="42"/>
    <newznab:attr name="subs" value="English"/>
  </item>
</channel>
</rss>"#
    )
}

/// The subtitle table ameNZB scrapes off a release page.
fn release_page() -> String {
    format!(
        r#"<html><body>
        <div id="subtitlesBody" class="collapse">
          <table><tbody>
            <tr>
              <td><code>eng</code></td>
              <td>English subs <span class="badge">Default</span></td>
              <td><code>srt</code></td>
              <td>36 KB</td>
              <td><a href="/release/{RELEASE_ID}/subtitles/{SUBTITLE_ID}">Download</a></td>
            </tr>
          </tbody></table>
        </div>
        </body></html>"#
    )
}

fn subtitle_url() -> String {
    format!("{BASE_URL}/release/{RELEASE_ID}/subtitles/{SUBTITLE_ID}")
}

/// The reference `search` embeds in `provider_file_id`, as the provider builds
/// it from one subtitle row.
fn download_reference() -> String {
    serde_json::json!({
        "url": subtitle_url(),
        "release_id": RELEASE_ID,
        "subtitle_id": SUBTITLE_ID,
        "filename": "Kinomi.Master.S01E12.eng.srt",
        "language": "eng",
        "format": "srt",
        "label": "English subs",
    })
    .to_string()
}

fn amenzb_plugin_wasm() -> PathBuf {
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
                .expect("run cargo build for the amenzb plugin");
            assert!(status.success(), "amenzb plugin build failed: {status}");

            plugin_root.join("target/wasm32-wasip2/plugin-release/amenzb_subtitles.wasm")
        })
        .clone()
}
