//! # scryer-plugin-pdk
//!
//! Guest runtime bindings for Scryer WebAssembly plugins.
//!
//! ## Transports
//!
//! Every plugin family is a WASI Preview 2 component. There are two guest
//! shapes in this crate, and nothing else is loadable:
//!
//! | Shape | Target | Entry | Host services | Diagnostics |
//! |---|---|---|---|---|
//! | Family component (subtitles, download clients, notifications) | `wasm32-wasip2` `cdylib` | [`scryer_subtitle_component_main!`] and siblings | `scryer:host/services@1.0.0`, through [`host`] | stderr, through [`log`] |
//! | Indexer component | `wasm32-wasip2` `cdylib` | [`scryer_indexer_component_main!`] | `scryer:indexer/host`, through [`component`] | the world's `log` import, through [`log`] |
//!
//! Crates shared between shapes — `newznab-common` and its kin — call
//! [`log::log`] and let the installed hook decide where the line goes. That
//! indirection is not decoration: naming the indexer world's `log` import from
//! code a family component can reach keeps that import alive in the artifact,
//! and it then fails to instantiate under a host that does not serve it. See
//! [`log`] for the whole contract.
//!
//! ### What 0.6 changed
//!
//! [`host`] used to reach Scryer through a four-function core-module import
//! (`scryer_host_call` and a response handle, in the `scryer:host/v1` module).
//! A component has no exported linear memory for a host to slice and no
//! handle table of its own, so that ABI is **removed**, not deprecated: the
//! host side went component-only at the same time. Its replacement is one
//! `list<u8>` in, one `list<u8>` out over `scryer:host/services@1.0.0`, with
//! the transport injected by the family entry macro — see [`host`] for why the
//! PDK holds a `fn` pointer rather than generating those bindings itself.
//!
//! The signatures of `host::config_get`, `host::http`, `host::state_*`,
//! `host::socket_*` and `host::process_exec` are unchanged, as are the
//! [`config`], [`var`] and [`http`] convenience modules built on them, so a
//! plugin body migrated without edits. Outside a component — in a native
//! `cargo test`, say — no transport is installed and those functions report
//! [`host::HostCallError::Unavailable`].
//!
//! ## What this crate is (and is not)
//!
//! This crate serves **Scryer's plugin contract**. It is intentionally *not* a
//! general-purpose plugin framework. Its API promise is narrow and concrete:
//! *"what Scryer's host provides"*. It owns only guest runtime bindings — the
//! command entry glue, the stdin/stdout framing, and the panic hook. It carries
//! no wire types of its own: the protocol/descriptor/schema types remain the
//! single source of truth in [`scryer_plugin_sdk`] (which this crate depends on
//! and re-exports).
//!
//! ## Usage — a family component
//!
//! A migrated plugin generates its own family world (the WIT is vendored in
//! the plugin crate, as the archive extractor does) and then hands the entry
//! macro a descriptor factory and its existing command handler. That is the
//! whole of the boilerplate:
//!
//! ```ignore
//! wit_bindgen::generate!({ world: "subtitle-provider", path: "wit" });
//!
//! scryer_plugin_pdk::scryer_subtitle_component_main!(
//!     descriptor = build_descriptor,
//!     handler = handle_subtitle_command,
//! );
//! ```
//!
//! The handler is a plain dispatch `match` over the SDK command types:
//! `process` carries a [`PluginCommandRequest`]/[`PluginCommandResponse`] JSON
//! envelope. See [`family`] for the contract.
//!
//! ## Building the guest artifact
//!
//! Every family builds a `cdylib` for `wasm32-wasip2` — indexers through
//! [`scryer_indexer_component_main!`], the rest through their family entry
//! macro. A component imports single-attempt HTTP and time capabilities; it
//! owns upstream pacing, quotas, retries, and fanout. Build guests with
//! `panic = "abort"`.
//!
//! The host enables the full wasm feature surface Scryer supports, and the
//! catalog `feature_sets` metadata selects a matching flavor per host. Build
//! each flavor as follows (the slugs mirror `required_features` in
//! `[package.metadata.scryer]`):
//!
//! | Flavor | `required_features` | How to build |
//! |---|---|---|
//! | baseline | `[]` | `cargo build --profile plugin-release --target wasm32-wasip2` |
//! | simd | `["simd128"]` | as baseline with `RUSTFLAGS="-C target-feature=+simd128"` |
//! | relaxed-simd | `["simd128","relaxed-simd"]` | `RUSTFLAGS="-C target-feature=+simd128,+relaxed-simd"` |
//!
//! Exceptions (`wasm_exceptions`) are host-enabled as a forward capability; no
//! current guest emits exception-handling opcodes, so there is no exceptions
//! flavor to build until a toolchain emits them. See `README.md` for the full
//! build matrix and rationale.

pub mod component;
mod download_client_bridge;
pub mod family;
pub mod host;
mod host_api;
pub mod log;

pub use download_client_bridge::{
    LegacyDownloadClientFunctions, bridge_download_client_command,
    legacy_download_client_descriptor,
};
pub use host_api::{Error, FnResult, HttpRequest, HttpResponse, config, http, var};

// One wire-protocol source of truth (RFC 123 §2.6): the protocol types live in
// `scryer-plugin-sdk` and are re-exported here so a plugin can depend on the PDK
// alone for the archive command surface.
pub use scryer_plugin_sdk::{
    ArchivePluginExtractedFile, ArchivePluginFormat, ArchivePluginOperation,
    ArchivePluginProcessRequest, ArchivePluginProcessResponse, ArchivePluginStatus,
    AudioStreamSelector, SubtitleSyncAlignSkipReason, SubtitleSyncAudioCodec,
    SubtitleSyncAudioPacket, SubtitleSyncAudioStreamMetadata, SubtitleSyncCapabilities,
    SubtitleSyncCommandAlignRequest, SubtitleSyncCommandAlignResponse,
    SubtitleSyncCommandInputFile, SubtitleSyncCommandOutputSubtitle,
    SubtitleSyncCommandOutputTarget, SubtitleSyncCommandSubtitleFile, SubtitleSyncDecodeStatus,
    SubtitleSyncDecodeWindowRequest, SubtitleSyncDecodeWindowResponse,
    SubtitleSyncDecodeWindowStatus, SubtitleSyncMediaMetadataSnapshot, SubtitleSyncOperation,
    SubtitleSyncOptions, SubtitleSyncPluginOperation, SubtitleSyncPluginProcessRequest,
    SubtitleSyncPluginProcessResponse, SubtitleSyncPluginResponse, SubtitleSyncProbeRequest,
    SubtitleSyncProbeResponse, SubtitleSyncSubtitleStreamMetadata, SubtitleTimingSpan,
};

/// Full access to descriptor and other SDK types that the PDK does not wrap.
///
/// Descriptor-aware runners accept a factory returning [`sdk::PluginDescriptor`].
pub use scryer_plugin_sdk as sdk;
pub use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginDownloadClientCommand, PluginDownloadClientCommandResult,
    PluginDownloadGetCompletedRequest, PluginIndexerCommand, PluginIndexerCommandResult,
    PluginNotificationCommand, PluginNotificationCommandResult, PluginSubtitleCommand,
    PluginSubtitleCommandResult,
};
pub use scryer_plugin_sdk::host::{
    PluginArchiveExtractRequest, PluginArchiveExtractResponse, PluginArchiveExtractedFile,
};
pub use scryer_plugin_sdk::host::{
    PluginConfigGetRequest, PluginConfigGetResponse, PluginHostRequest, PluginHostResponse,
    PluginHttpRequest, PluginHttpResponse, PluginProcessExecRequest, PluginProcessExecResponse,
    PluginStateDeleteRequest, PluginStateGetRequest, PluginStateGetResponse,
    PluginStateMutationResponse, PluginStateSetRequest,
};

use std::io::{self, Write};

/// Install a best-effort panic hook that reports the panic to stderr.
///
/// Guests build with `panic = "abort"`, so after this hook runs the process
/// aborts (which the host observes as a trap / non-zero exit). Installing the
/// hook still fires under an unwinding build, and does not itself terminate the
/// process, so it is safe to call from native tests.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "scryer-plugin-pdk: guest panic: {info}");
        let _ = stderr.flush();
    }));
}

/// Define a WASIp2 component indexer from async search and optional action
/// handlers. The handlers receive the existing SDK request models and return
/// the existing SDK response models, so only their host capability calls need
/// to become `await`-based.
///
/// The component ABI serializes the SDK operation payloads as UTF-8 JSON; WIT
/// remains a small, stable capability boundary rather than duplicating the
/// full SDK model graph.
#[macro_export]
macro_rules! scryer_indexer_component_main {
    (descriptor = $descriptor:path, search = $search:path $(,)?) => {
        struct ScryerIndexerComponent;

        impl $crate::component::Guest for ScryerIndexerComponent {
            fn describe() -> ::std::vec::Vec<u8> {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::descriptor_bytes($descriptor())
            }

            async fn search(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::dispatch_search(request, $search).await
            }

            async fn search_plan(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                let descriptor = $descriptor();
                let parallelism = $crate::component::strategy_plan_parallelism(&descriptor)
                    .ok_or($crate::component::InvocationError::InvalidResponse)?;
                $crate::component::dispatch_search_plan(request, parallelism, $search).await
            }

            async fn action(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::unsupported_action_response(request)
            }
        }

        $crate::component::export!(
            ScryerIndexerComponent with_types_in $crate::component
        );
    };
    (descriptor = $descriptor:path, search = $search:path, action = $action:path $(,)?) => {
        struct ScryerIndexerComponent;

        impl $crate::component::Guest for ScryerIndexerComponent {
            fn describe() -> ::std::vec::Vec<u8> {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::descriptor_bytes($descriptor())
            }

            async fn search(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::dispatch_search(request, $search).await
            }

            async fn search_plan(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                let descriptor = $descriptor();
                let parallelism = $crate::component::strategy_plan_parallelism(&descriptor)
                    .ok_or($crate::component::InvocationError::InvalidResponse)?;
                $crate::component::dispatch_search_plan(request, parallelism, $search).await
            }

            async fn action(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<u8>,
                $crate::component::InvocationError,
            > {
                $crate::component::install_config_get();
                $crate::component::install_log();
                $crate::component::dispatch_action(request, $action).await
            }
        }

        $crate::component::export!(
            ScryerIndexerComponent with_types_in $crate::component
        );
    };
}

/// The shared body of every family component entry macro.
///
/// It expands to four things, in the module that invoked `wit_bindgen`:
///
/// 1. a `fn` adapting the world's `scryer:host/services@1.0.0` import to
///    [`host::HostCall`], which is why the PDK needs no WIT of its own;
/// 2. the component type;
/// 3. its `Guest` impl, installing that transport *and* the family log sink at
///    the top of *both* exports — Scryer instantiates a component once per
///    invocation, so a fresh instance always starts with an empty registry;
/// 4. the `export!` that makes it the component's implementation.
///
/// The log sink is stderr, which every family component host already captures
/// as a size-capped tail and re-emits through `tracing`. A family world has no
/// `log` import of its own, and shared crates must not name the indexer
/// world's — see [`log`] for why that is a linking property rather than a
/// stylistic one.
///
/// It names `Guest`, `InvocationError`, `export!` and `self::scryer::host` —
/// all generated by the plugin's own `wit_bindgen::generate!` — so it must be
/// invoked in the same module as that macro.
#[doc(hidden)]
#[macro_export]
macro_rules! __scryer_family_component_main {
    (
        component = $component:ident,
        transport = $transport:ident,
        dispatch = $dispatch:path,
        descriptor = $descriptor:expr,
        handler = $handler:expr $(,)?
    ) => {
        fn $transport(
            request: &[u8],
        ) -> ::std::result::Result<::std::vec::Vec<u8>, $crate::host::HostTransportError> {
            self::scryer::host::services::host_call(request).map_err(|error| match error {
                self::scryer::host::services::HostError::InvalidRequest => {
                    $crate::host::HostTransportError::InvalidRequest
                }
                self::scryer::host::services::HostError::Failed => {
                    $crate::host::HostTransportError::Failed
                }
            })
        }

        struct $component;

        impl Guest for $component {
            fn describe() -> ::std::vec::Vec<u8> {
                $crate::host::install_host_call($transport);
                $crate::log::install_stderr_log();
                $crate::family::descriptor_bytes($descriptor())
            }

            fn process(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<::std::vec::Vec<u8>, InvocationError> {
                $crate::host::install_host_call($transport);
                $crate::log::install_stderr_log();
                $dispatch(request, $handler).map_err(|failure| match failure {
                    $crate::family::InvocationFailure::Failed => InvocationError::Failed,
                    $crate::family::InvocationFailure::Cancelled => InvocationError::Cancelled,
                    $crate::family::InvocationFailure::InvalidResponse => {
                        InvocationError::InvalidResponse
                    }
                })
            }
        }

        export!($component);
    };
}

/// Define a `scryer:subtitle/subtitle-provider@1.0.0` component from a
/// descriptor factory and a [`PluginSubtitleCommand`] handler.
///
/// ```ignore
/// wit_bindgen::generate!({ world: "subtitle-provider", path: "wit" });
///
/// scryer_plugin_pdk::scryer_subtitle_component_main!(
///     descriptor = build_descriptor,
///     handler = handle_subtitle_command,
/// );
/// ```
#[macro_export]
macro_rules! scryer_subtitle_component_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_family_component_main!(
            component = ScryerSubtitleComponent,
            transport = __scryer_subtitle_host_call,
            dispatch = $crate::family::dispatch_subtitle,
            descriptor = $descriptor,
            handler = $handler,
        );
    };
}

/// Define a `scryer:download-client/download-client@1.0.0` component from a
/// descriptor factory and a [`PluginDownloadClientCommand`] handler.
#[macro_export]
macro_rules! scryer_download_client_component_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_family_component_main!(
            component = ScryerDownloadClientComponent,
            transport = __scryer_download_client_host_call,
            dispatch = $crate::family::dispatch_download_client,
            descriptor = $descriptor,
            handler = $handler,
        );
    };
}

/// Define a `scryer:notification/notification@1.0.0` component from a
/// descriptor factory and a [`PluginNotificationCommand`] handler.
#[macro_export]
macro_rules! scryer_notification_component_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_family_component_main!(
            component = ScryerNotificationComponent,
            transport = __scryer_notification_host_call,
            dispatch = $crate::family::dispatch_notification,
            descriptor = $descriptor,
            handler = $handler,
        );
    };
}
