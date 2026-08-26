//! # scryer-plugin-pdk
//!
//! Guest runtime bindings for **Scryer** WebAssembly plugins. This crate is the
//! guest half of Scryer's command-model plugin invocation protocol: the host
//! runs the plugin as a `wasm32-wasip1` **command**
//! (a `_start` entry), hands it one request document on stdin, and reads
//! exactly one response document from stdout.
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
//! ## Usage
//!
//! A plugin provides a descriptor factory and a typed request handler to the
//! matching descriptor-aware runner, such as
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
//! The plugin is a **command** binary, so it must have a `main` (via the macro
//! or an explicit `fn main`) and be built for a `wasm32-wasip1` target. The
//! resulting module exports `_start` and `memory` and — for the archive plugin
//! — imports exactly the two frozen host crypto functions under
//! `extism:host/user` (see RFC 123 §5). Build guests with `panic = "abort"`.
//!
//! The host enables the full wasm feature surface Scryer supports, and the
//! catalog `feature_sets` metadata selects a matching flavor per host. Build
//! each flavor as follows (the slugs mirror `required_features` in
//! `[package.metadata.scryer]`):
//!
//! | Flavor | `required_features` | How to build |
//! |---|---|---|
//! | baseline | `[]` | `cargo build --profile plugin-release --target wasm32-wasip1` |
//! | simd | `["simd128"]` | as baseline with `RUSTFLAGS="-C target-feature=+simd128"` |
//! | relaxed-simd | `["simd128","relaxed-simd"]` | `RUSTFLAGS="-C target-feature=+simd128,+relaxed-simd"` |
//! | threads | `["threads"]` | build for `--target wasm32-wasip1-threads` with `RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals"` |
//!
//! Exceptions (`wasm_exceptions`) are host-enabled as a forward capability; no
//! current guest emits exception-handling opcodes, so there is no exceptions
//! flavor to build until a toolchain emits them. See `README.md` for the full
//! build matrix and rationale.

mod download_client_bridge;
mod extism_compat;
mod framing;
pub mod host;

pub use download_client_bridge::{
    LegacyDownloadClientFunctions, legacy_download_client_descriptor,
    run_download_client_bridge_with_descriptor,
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
pub use scryer_plugin_sdk::host::{
    PluginConfigGetRequest, PluginConfigGetResponse, PluginHostRequest, PluginHostResponse,
    PluginHttpBatchRequest, PluginHttpBatchResponse, PluginHttpRequest, PluginHttpResponse,
    PluginHttpStartRate, PluginProcessExecRequest, PluginProcessExecResponse,
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

#[macro_export]
macro_rules! scryer_download_client_plugin_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_download_client_plugin_with_descriptor($descriptor, $handler);
        }
    };
}

/// Bridge the former first-party DLC JSON exports into the native command ABI.
///
/// This is intentionally a short-lived migration macro: it preserves each
/// client's operation implementation while moving framing, descriptor handling,
/// and exact completed-download lookup into PDK 0.5.
#[macro_export]
macro_rules! scryer_download_client_bridge_main {
    (
        describe = $describe:path,
        add = $add:path,
        list_queue = $list_queue:path,
        list_history = $list_history:path,
        list_completed = $list_completed:path,
        list_recent_completed = $list_recent_completed:expr,
        control = $control:path,
        mark_imported = $mark_imported:path,
        mark_imported_non_destructive = $mark_imported_non_destructive:expr,
        status = $status:path,
        test_connection = $test_connection:path $(,)?
    ) => {
        $crate::__scryer_command_abi_marker!();
        fn main() {
            $crate::run_download_client_bridge_with_descriptor(
                $crate::LegacyDownloadClientFunctions {
                    describe: $describe,
                    add: $add,
                    list_queue: $list_queue,
                    list_history: $list_history,
                    list_completed: $list_completed,
                    list_recent_completed: $list_recent_completed,
                    control: $control,
                    mark_imported: $mark_imported,
                    mark_imported_non_destructive: $mark_imported_non_destructive,
                    status: $status,
                    test_connection: $test_connection,
                },
            );
        }
    };
    (
        describe = $describe:path,
        add = $add:path,
        list_queue = $list_queue:path,
        list_history = $list_history:path,
        list_completed = $list_completed:path,
        list_recent_completed = $list_recent_completed:expr,
        control = $control:path,
        mark_imported = $mark_imported:path,
        status = $status:path,
        test_connection = $test_connection:path $(,)?
    ) => {
        $crate::scryer_download_client_bridge_main!(
            describe = $describe,
            add = $add,
            list_queue = $list_queue,
            list_history = $list_history,
            list_completed = $list_completed,
            list_recent_completed = $list_recent_completed,
            control = $control,
            mark_imported = $mark_imported,
            mark_imported_non_destructive = None,
            status = $status,
            test_connection = $test_connection,
        );
    };
}

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
