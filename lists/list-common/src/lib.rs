//! Shared plumbing for the `scryer:lists/list-provider@1.0.0` plugins.
//!
//! The PDK ships entry macros for the subtitle, download-client and
//! notification families but not yet for lists, so [`list_component_main!`]
//! carries the same glue here, built only from the PDK's public host, log and
//! runtime hooks. Everything else is provider-neutral: error shaping that the
//! host's failure classes understand ([`error`]), one HTTP seam that tests can
//! replace ([`http`]), item keys and external ids ([`ids`]), RSS and Atom
//! parsing ([`feed`]), and paging plus fingerprints ([`page`]).

pub mod error;
pub mod feed;
pub mod http;
pub mod ids;
pub mod page;
#[cfg(feature = "testing")]
pub mod testing;

use std::future::Future;

pub use scryer_plugin_pdk as pdk;
pub use scryer_plugin_sdk as sdk;

use scryer_plugin_pdk::family::{InvocationFailure, dispatch_command_async};
use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandResult, PluginListCommand, PluginListCommandResult,
};

/// Dispatch one list-provider invocation to an `async` handler.
///
/// Any command that is not a list command is an envelope the world never
/// sends, so it is reported as an invalid invocation rather than a typed
/// plugin error.
pub async fn dispatch_list<H, F>(request: Vec<u8>, handler: H) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginListCommand) -> F,
    F: Future<Output = PluginListCommandResult>,
{
    dispatch_command_async(request, |command| async move {
        match command {
            PluginCommand::List(command) => Ok(PluginCommandResult::List(handler(command).await)),
            _ => Err(InvocationFailure::InvalidResponse),
        }
    })
    .await
}

/// Define a `scryer:lists/list-provider@1.0.0` component from a descriptor
/// factory and an `async` list-command handler.
///
/// Invoke it in the module that ran `wit_bindgen::generate!` for the list
/// world, because it names that module's `Guest`, `InvocationError`,
/// `export!` and `scryer::host::services`.
#[macro_export]
macro_rules! list_component_main {
    (descriptor = $descriptor:expr, handler = $handler:expr $(,)?) => {
        fn __list_host_transport(
            request: &[u8],
        ) -> ::std::result::Result<::std::vec::Vec<u8>, $crate::pdk::host::HostTransportError> {
            self::scryer::host::services::host_call(request).map_err(|error| match error {
                self::scryer::host::services::HostError::InvalidRequest => {
                    $crate::pdk::host::HostTransportError::InvalidRequest
                }
                self::scryer::host::services::HostError::Failed => {
                    $crate::pdk::host::HostTransportError::Failed
                }
            })
        }

        fn __list_install_hooks() {
            $crate::pdk::host::install_host_call(__list_host_transport);
            $crate::pdk::log::install_stderr_log();
            $crate::pdk::runtime::install_runtime_host();
        }

        struct ListComponent;

        impl Guest for ListComponent {
            fn describe() -> ::std::vec::Vec<u8> {
                __list_install_hooks();
                $crate::pdk::family::descriptor_bytes($descriptor())
            }

            async fn process(
                request: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<::std::vec::Vec<u8>, InvocationError> {
                __list_install_hooks();
                $crate::dispatch_list(request, $handler)
                    .await
                    .map_err(|failure| match failure {
                        $crate::pdk::family::InvocationFailure::Failed => InvocationError::Failed,
                        $crate::pdk::family::InvocationFailure::Cancelled => {
                            InvocationError::Cancelled
                        }
                        $crate::pdk::family::InvocationFailure::InvalidResponse => {
                            InvocationError::InvalidResponse
                        }
                    })
            }
        }

        export!(ListComponent);
    };
}
