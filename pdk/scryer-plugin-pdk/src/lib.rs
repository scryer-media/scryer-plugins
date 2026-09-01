//! # scryer-plugin-pdk
//!
//! Guest runtime bindings for Scryer WebAssembly plugins.
//!
//! ## Transports
//!
//! Every plugin family is moving to WASI Preview 2 components. There are three
//! guest shapes in this crate, and only the first two are current:
//!
//! | Shape | Target | Entry | Host services | Diagnostics |
//! |---|---|---|---|---|
//! | Family component (subtitles, download clients, notifications) | `wasm32-wasip2` `cdylib` | [`scryer_subtitle_component_main!`] and siblings | `scryer:host/services@1.0.0`, through [`host`] | stderr, through [`log`] |
//! | Indexer component | `wasm32-wasip2` `cdylib` | [`scryer_indexer_component_main!`] | `scryer:indexer/host`, through [`component`] | the world's `log` import, through [`log`] |
//! | Preview 1 command (being retired) | `wasm32-wasip1` command | [`run_subtitle_plugin_with_descriptor`] and siblings | none — see below | stderr, through [`log`] |
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
//! plugin body migrates without edits. What changed is that a Preview 1
//! command guest now has no transport at all: those functions report
//! [`host::HostCallError::Unavailable`] there, exactly as they do in a native
//! `cargo test`. A family plugin must therefore ship as a component.
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
//! The handler keeps the SDK types and the dispatch `match` a Preview 1
//! command plugin already had — `process` carries the same
//! [`PluginCommandRequest`]/[`PluginCommandResponse`] JSON envelope that used
//! to travel over stdin/stdout. See [`family`] for the contract.
//!
//! ## Usage — a Preview 1 command plugin
//!
//! A legacy plugin provides a descriptor factory and a typed request handler to
//! the matching descriptor-aware runner, such as
//! [`run_archive_plugin_with_descriptor`] or
//! [`run_subtitle_sync_plugin_with_descriptor`]. The PDK owns the standardized
//! `describe` command; release tooling invokes it and embeds the returned
//! descriptor in the final Wasm artifact. Operational failures are reported
//! *in-band* through the response type, never by exiting non-zero. A non-zero
//! exit is reserved for protocol-level faults (malformed request, unwritable
//! stdout) and guest panics.
//!
//! ```no_run
//! use scryer_plugin_pdk::{
//!     ArchivePluginProcessRequest, ArchivePluginProcessResponse, ArchivePluginStatus,
//! };
//!
//! fn handle(_request: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse {
//!     ArchivePluginProcessResponse {
//!         status: ArchivePluginStatus::Ok,
//!         files: vec![],
//!         expanded_bytes: None,
//!         copied_bytes: None,
//!         staged_bytes: None,
//!         error_code: None,
//!         message: None,
//!     }
//! }
//!
//! # fn descriptor() -> scryer_plugin_pdk::sdk::PluginDescriptor { unimplemented!() }
//! scryer_plugin_pdk::scryer_archive_plugin_main!(
//!     descriptor = descriptor,
//!     handler = handle,
//! );
//! ```
//!
//! ## Building the guest artifact
//!
//! Legacy plugins are **command** binaries, so they must have a `main` (via the
//! macro or an explicit `fn main`) and be built for a `wasm32-wasip1` target.
//! The resulting module exports `_start` and `memory` and — for the archive
//! plugin — imports exactly the two frozen host crypto functions under
//! `extism:host/user` (see RFC 123 §5). Build guests with `panic = "abort"`.
//! Indexers use [`scryer_indexer_component_main!`], a `cdylib`, and the
//! `wasm32-wasip2` target. The component imports single-attempt HTTP and time
//! capabilities; it owns upstream pacing, quotas, retries, and fanout.
//!
//! The host enables the full wasm feature surface Scryer supports, and the
//! catalog `feature_sets` metadata selects a matching flavor per host. Build
//! each flavor as follows (the slugs mirror `required_features` in
//! `[package.metadata.scryer]`):
//!
//! | Flavor | `required_features` | How to build |
//! |---|---|---|
//! | baseline | `[]` | legacy: `cargo build --profile plugin-release --target wasm32-wasip1`; indexer: `--target wasm32-wasip2` |
//! | simd | `["simd128"]` | as baseline with `RUSTFLAGS="-C target-feature=+simd128"` |
//! | relaxed-simd | `["simd128","relaxed-simd"]` | `RUSTFLAGS="-C target-feature=+simd128,+relaxed-simd"` |
//!
//! Exceptions (`wasm_exceptions`) are host-enabled as a forward capability; no
//! current legacy guest emits exception-handling opcodes, so there is no exceptions
//! flavor to build until a toolchain emits them. See `README.md` for the full
//! build matrix and rationale.

pub mod component;
mod download_client_bridge;
mod extism_compat;
pub mod family;
mod framing;
pub mod host;
pub mod log;

pub use download_client_bridge::{
    LegacyDownloadClientFunctions, bridge_download_client_command,
    legacy_download_client_descriptor, run_download_client_bridge_with_descriptor,
};
pub use extism_compat::{Error, FnResult, HttpRequest, HttpResponse, config, http, var};
pub use framing::{FramingError, process, process_json, process_json_result};

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
#[cfg(feature = "archive-extract")]
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
use std::process;

/// Install a best-effort panic hook that reports the panic to stderr.
///
/// Guests build with `panic = "abort"`, so after this hook runs the process
/// aborts (which the host observes as a trap / non-zero exit). Installing the
/// hook still fires under an unwinding build, and does not itself terminate the
/// process, so it is safe to call from native tests. [`run_archive_plugin`]
/// installs it automatically before dispatch.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "scryer-plugin-pdk: guest panic: {info}");
        let _ = stderr.flush();
    }));
}

/// Command entry glue: read one request from stdin, dispatch it to `handler`,
/// write exactly one response to stdout, flush, and exit.
///
/// - Clean success → exit `0`.
/// - A protocol-level fault (malformed request, unwritable stdout, response
///   serialization failure) → message to stderr, non-zero exit.
/// - Operational failures are *not* errors here; the handler reports them
///   in-band via [`ArchivePluginStatus`].
///
/// Never returns.
pub fn run_archive_plugin<H>(handler: H) -> !
where
    H: Fn(ArchivePluginProcessRequest) -> ArchivePluginProcessResponse,
{
    install_panic_hook();

    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = framing::process(stdin.lock(), stdout.lock(), handler);

    let _ = io::stdout().flush();

    match result {
        Ok(()) => process::exit(0),
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "scryer-plugin-pdk: {error}");
            let _ = stderr.flush();
            process::exit(error.exit_code())
        }
    }
}

fn is_describe_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("describe")
}

fn write_descriptor_json<W, T>(mut output: W, descriptor: &T) -> Result<(), String>
where
    W: Write,
    T: serde::Serialize,
{
    let json = serde_json::to_vec(descriptor)
        .map_err(|error| format!("failed to serialize plugin descriptor: {error}"))?;
    output
        .write_all(&json)
        .map_err(|error| format!("failed to write plugin descriptor: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("failed to flush plugin descriptor: {error}"))
}

fn run_descriptor_command<D>(descriptor: D) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
{
    install_panic_hook();
    let stdout = io::stdout();
    match write_descriptor_json(stdout.lock(), &descriptor()) {
        Ok(()) => process::exit(0),
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "scryer-plugin-pdk: {error}");
            let _ = stderr.flush();
            process::exit(2)
        }
    }
}

/// Run an archive plugin and own its standardized `describe` command.
///
/// When the first argument is `describe`, the PDK calls `descriptor` lazily,
/// writes exactly one [`sdk::PluginDescriptor`] JSON document to stdout, and
/// exits. Otherwise it dispatches one archive request to `handler` using the
/// normal command protocol.
///
/// Release tooling is responsible for invoking `describe` and embedding the
/// returned descriptor in the final Wasm artifact.
pub fn run_archive_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(ArchivePluginProcessRequest) -> ArchivePluginProcessResponse,
{
    if is_describe_command() {
        run_descriptor_command(descriptor);
    }
    run_archive_plugin(handler)
}

/// Command entry glue for SDK 3.5 subtitle-sync plugins.
///
/// Never returns.
pub fn run_subtitle_sync_plugin<H>(handler: H) -> !
where
    H: Fn(SubtitleSyncPluginProcessRequest) -> SubtitleSyncPluginProcessResponse,
{
    install_panic_hook();

    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = framing::process_json(stdin.lock(), stdout.lock(), handler);

    let _ = io::stdout().flush();

    match result {
        Ok(()) => process::exit(0),
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "scryer-plugin-pdk: {error}");
            let _ = stderr.flush();
            process::exit(error.exit_code())
        }
    }
}

/// Run a subtitle-sync plugin and own its standardized `describe` command.
///
/// When the first argument is `describe`, the PDK calls `descriptor` lazily,
/// writes exactly one [`sdk::PluginDescriptor`] JSON document to stdout, and
/// exits. Otherwise it dispatches one subtitle-sync request to `handler` using
/// the normal command protocol.
///
/// Release tooling is responsible for invoking `describe` and embedding the
/// returned descriptor in the final Wasm artifact.
pub fn run_subtitle_sync_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(SubtitleSyncPluginProcessRequest) -> SubtitleSyncPluginProcessResponse,
{
    if is_describe_command() {
        run_descriptor_command(descriptor);
    }
    run_subtitle_sync_plugin(handler)
}

fn run_command_plugin<H>(handler: H) -> !
where
    H: Fn(PluginCommand) -> Result<PluginCommandResult, String>,
{
    install_panic_hook();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let result = framing::process_json_result(
        stdin.lock(),
        stdout.lock(),
        |request: PluginCommandRequest| {
            if request.abi_version != sdk::command::COMMAND_ABI_VERSION {
                return Err(FramingError::Dispatch(format!(
                    "unsupported command ABI version {}",
                    request.abi_version
                )));
            }
            let response = handler(request.command).map_err(FramingError::Dispatch)?;
            Ok(PluginCommandResponse::new(response))
        },
    );
    let _ = io::stdout().flush();
    match result {
        Ok(()) => process::exit(0),
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "scryer-plugin-pdk: {error}");
            let _ = stderr.flush();
            process::exit(error.exit_code())
        }
    }
}

fn run_command_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(PluginCommand) -> Result<PluginCommandResult, String>,
{
    if is_describe_command() {
        run_descriptor_command(descriptor);
    }
    run_command_plugin(handler)
}

/// Run a native indexer command plugin.
pub fn run_indexer_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(PluginIndexerCommand) -> PluginIndexerCommandResult,
{
    run_command_plugin_with_descriptor(descriptor, move |command| match command {
        PluginCommand::Indexer(command) => Ok(PluginCommandResult::Indexer(handler(command))),
        _ => Err("indexer command runner received another plugin family".to_string()),
    })
}

/// Run a native download-client command plugin.
pub fn run_download_client_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(PluginDownloadClientCommand) -> PluginDownloadClientCommandResult,
{
    run_command_plugin_with_descriptor(descriptor, move |command| match command {
        PluginCommand::DownloadClient(command) => {
            Ok(PluginCommandResult::DownloadClient(handler(command)))
        }
        _ => Err("download-client command runner received another plugin family".to_string()),
    })
}

/// Run a native notification command plugin.
pub fn run_notification_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(PluginNotificationCommand) -> PluginNotificationCommandResult,
{
    run_command_plugin_with_descriptor(descriptor, move |command| match command {
        PluginCommand::Notification(command) => {
            Ok(PluginCommandResult::Notification(handler(command)))
        }
        _ => Err("notification command runner received another plugin family".to_string()),
    })
}

/// Run a native catalog subtitle command plugin.
pub fn run_subtitle_plugin_with_descriptor<D, H>(descriptor: D, handler: H) -> !
where
    D: FnOnce() -> sdk::PluginDescriptor,
    H: Fn(PluginSubtitleCommand) -> PluginSubtitleCommandResult,
{
    run_command_plugin_with_descriptor(descriptor, move |command| match command {
        PluginCommand::Subtitle(command) => Ok(PluginCommandResult::Subtitle(handler(command))),
        _ => Err("subtitle command runner received another plugin family".to_string()),
    })
}

/// Define the command `main` for an archive plugin from a descriptor factory
/// and request handler.
///
/// ```no_run
/// use scryer_plugin_pdk::{ArchivePluginProcessRequest, ArchivePluginProcessResponse};
/// # fn descriptor() -> scryer_plugin_pdk::sdk::PluginDescriptor { unimplemented!() }
/// # fn handle(_: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse { unimplemented!() }
/// scryer_plugin_pdk::scryer_archive_plugin_main!(
///     descriptor = descriptor,
///     handler = handle,
/// );
/// ```
#[macro_export]
macro_rules! scryer_archive_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        fn main() {
            $crate::run_archive_plugin_with_descriptor($descriptor, $handler);
        }
    };
    ($handler:expr) => {
        fn main() {
            $crate::run_archive_plugin($handler);
        }
    };
}

/// Define the command `main` for a subtitle-sync plugin from a descriptor
/// factory and request handler.
///
/// ```no_run
/// use scryer_plugin_pdk::{
///     SubtitleSyncPluginProcessRequest, SubtitleSyncPluginProcessResponse,
/// };
/// # fn descriptor() -> scryer_plugin_pdk::sdk::PluginDescriptor { unimplemented!() }
/// # fn handle(_: SubtitleSyncPluginProcessRequest) -> SubtitleSyncPluginProcessResponse { unimplemented!() }
/// scryer_plugin_pdk::scryer_subtitle_sync_plugin_main!(
///     descriptor = descriptor,
///     handler = handle,
/// );
/// ```
#[macro_export]
macro_rules! scryer_subtitle_sync_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        fn main() {
            $crate::run_subtitle_sync_plugin_with_descriptor($descriptor, $handler);
        }
    };
    ($handler:expr) => {
        fn main() {
            $crate::run_subtitle_sync_plugin($handler);
        }
    };
}

/// Emit the command-ABI marker used by Scryer to select the native command
/// runtime. Each command macro expands this once in the guest artifact.
#[doc(hidden)]
#[macro_export]
macro_rules! __scryer_command_abi_marker {
    () => {
        #[used]
        #[cfg_attr(
            target_arch = "wasm32",
            unsafe(link_section = "scryer.plugin.command_abi")
        )]
        static SCRYER_PLUGIN_COMMAND_ABI_V1: [u8; 2] =
            $crate::sdk::command::COMMAND_ABI_VERSION.to_le_bytes();
    };
}

#[macro_export]
macro_rules! scryer_indexer_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_indexer_plugin_with_descriptor($descriptor, $handler);
        }
    };
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

#[macro_export]
macro_rules! scryer_download_client_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_download_client_plugin_with_descriptor($descriptor, $handler);
        }
    };
}

// `scryer_download_client_bridge_main!` was deleted in 0.6.0. It was the
// short-lived macro that wrapped a client's legacy JSON exports into
// `LegacyDownloadClientFunctions` and handed them to
// `run_download_client_bridge_with_descriptor`. All sixteen first-party clients
// now build that table directly — they have to, because the macro's initializer
// never learned `mark_imported_non_destructive` and so could no longer even
// construct the struct: any invocation was an E0063, which is why zero
// invocations remained. `run_download_client_bridge_with_descriptor` and
// `LegacyDownloadClientFunctions` are kept and still exported; only the
// uncompilable wrapper is gone.

#[macro_export]
macro_rules! scryer_notification_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_notification_plugin_with_descriptor($descriptor, $handler);
        }
    };
}

#[macro_export]
macro_rules! scryer_subtitle_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_subtitle_plugin_with_descriptor($descriptor, $handler);
        }
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
/// The handler is the same one a Preview 1 subtitle command plugin passes to
/// [`run_subtitle_plugin_with_descriptor`]; only the transport changes.
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

#[cfg(test)]
mod tests {
    use super::*;

    crate::__scryer_command_abi_marker!();

    #[derive(serde::Serialize)]
    struct TestDescriptor<'a> {
        id: &'a str,
    }

    #[test]
    fn descriptor_writer_emits_one_json_document() {
        let mut output = Vec::new();
        write_descriptor_json(&mut output, &TestDescriptor { id: "test" }).unwrap();
        assert_eq!(output, br#"{"id":"test"}"#);
    }

    #[test]
    fn command_abi_marker_compiles_on_host_targets() {
        assert_eq!(
            SCRYER_PLUGIN_COMMAND_ABI_V1,
            sdk::command::COMMAND_ABI_VERSION.to_le_bytes()
        );
    }
}
