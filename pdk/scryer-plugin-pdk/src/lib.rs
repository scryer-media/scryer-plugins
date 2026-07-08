//! # scryer-plugin-pdk
//!
//! Guest runtime bindings for **Scryer** WebAssembly plugins. This crate is the
//! guest half of the archive plugin invocation protocol defined in Scryer
//! RFC 123 (WP1): the host runs the plugin as a `wasm32-wasip1` **command**
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
//! A plugin provides a handler `Fn(ArchivePluginProcessRequest) ->
//! ArchivePluginProcessResponse` and hands it to [`run_archive_plugin`] (or the
//! [`scryer_archive_plugin_main`] macro). Operational failures — unsupported
//! format, wrong password, insufficient recovery data — are reported *in-band*
//! through [`ArchivePluginStatus`], never by exiting non-zero. A non-zero exit
//! is reserved for protocol-level faults (malformed request, unwritable stdout)
//! and guest panics.
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
//!         repair: None,
//!         expanded_bytes: None,
//!         copied_bytes: None,
//!         staged_bytes: None,
//!         error_code: None,
//!         message: None,
//!     }
//! }
//!
//! // Either the macro:
//! // scryer_plugin_pdk::scryer_archive_plugin_main!(handle);
//! //
//! // ...or an explicit main:
//! fn main() {
//!     scryer_plugin_pdk::run_archive_plugin(handle);
//! }
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

mod framing;

pub use framing::{FramingError, process};

// One wire-protocol source of truth (RFC 123 §2.6): the protocol types live in
// `scryer-plugin-sdk` and are re-exported here so a plugin can depend on the PDK
// alone for the archive command surface.
pub use scryer_plugin_sdk::{
    ArchivePluginExtractedFile, ArchivePluginFormat, ArchivePluginOperation,
    ArchivePluginProcessRequest, ArchivePluginProcessResponse, ArchivePluginRepairFormat,
    ArchivePluginRepairState, ArchivePluginRepairStatus, ArchivePluginStatus,
};

/// Full access to the SDK for descriptor and other types the PDK does not wrap
/// (e.g. `PluginDescriptor` used by a plugin's own describe path).
pub use scryer_plugin_sdk as sdk;

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

    // Flush again on the way out; `proc_exit` does not run destructors and WASI
    // aborts do not flush libc/std buffers.
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

/// Define the command `main` for an archive plugin from a request handler.
///
/// ```no_run
/// use scryer_plugin_pdk::{ArchivePluginProcessRequest, ArchivePluginProcessResponse};
/// # fn handle(_: ArchivePluginProcessRequest) -> ArchivePluginProcessResponse { unimplemented!() }
/// scryer_plugin_pdk::scryer_archive_plugin_main!(handle);
/// ```
#[macro_export]
macro_rules! scryer_archive_plugin_main {
    ($handler:expr) => {
        fn main() {
            $crate::run_archive_plugin($handler);
        }
    };
}
