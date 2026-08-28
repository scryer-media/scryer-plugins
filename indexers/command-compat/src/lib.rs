//! Migration support for indexer plugins moving to the command-style PDK.
//!
//! `scryer-plugin-pdk` already provides everything an indexer needs at runtime —
//! config, HTTP, state, and the `scryer_indexer_plugin_main!` entrypoint — but
//! two things were missing that every indexer relies on and no download client
//! did: guest-side logging, and a uniform way to turn a `Result`-shaped
//! operation into the command ABI's `PluginResult`.
//!
//! Both live here rather than in the published PDK on purpose. Adding them
//! upstream would mean releasing a new PDK before a single plugin could move,
//! which would put a crates.io publish in the critical path of a runtime
//! migration. Once the family has shipped on the command ABI these helpers are
//! the natural candidates to hoist into the PDK proper.

pub use scryer_plugin_pdk as pdk;
pub use scryer_plugin_pdk::sdk;

use sdk::{PluginError, PluginErrorCode, PluginResult};

/// Severity for [`log!`], mirroring the levels the Extism SDK exposed.
///
/// The variants are the ones first-party indexers actually use; keeping the
/// same names means call sites migrate without being rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Emit one diagnostic line, source-compatible with the Extism SDK's `log!`.
///
/// Command guests have no host log service: they are WASI commands, so stderr
/// is the diagnostic channel, and Scryer captures it and re-emits it under
/// `scryer_plugins::command` at debug level. Writing to stderr therefore lands
/// in the same place the old host log call did.
#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {{
        eprintln!("[{}] {}", $crate::LogLevel::as_str($level), format!($($arg)*));
    }};
}

/// Lift a fallible operation into the command ABI's result shape.
///
/// Errors are reported as [`PluginErrorCode::Temporary`] with the detail in
/// `debug_message`, matching what the download-client bridge does: the guest
/// cannot tell a bad API key from a flaky upstream, so the conservative code is
/// the one that lets Scryer retry rather than condemning the indexer.
#[derive(Debug)]
pub struct StructuredPluginError(PluginError);

impl std::fmt::Display for StructuredPluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.public_message)
    }
}

impl std::error::Error for StructuredPluginError {}

impl StructuredPluginError {
    pub fn plugin_error(&self) -> &PluginError {
        &self.0
    }
}

pub fn structured_plugin_error(error: PluginError) -> pdk::Error {
    pdk::Error::new(StructuredPluginError(error))
}

pub fn to_plugin_result<T>(result: Result<T, pdk::Error>) -> PluginResult<T> {
    match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => {
            if let Some(structured) = error.downcast_ref::<StructuredPluginError>() {
                return PluginResult::Err(structured.0.clone());
            }
            PluginResult::Err(PluginError {
                code: PluginErrorCode::Temporary,
                public_message: "indexer command failed".to_string(),
                debug_message: Some(error.to_string()),
                retry_after_seconds: None,
                details: None,
            })
        }
    }
}

/// The error a plugin without an action handler returns.
///
/// The legacy runtime answered this by export lookup — a plugin that never
/// exported `scryer_indexer_action` simply had no such function, and the
/// adapter reported "no output". A command guest always has a handler, so the
/// same "this plugin does not do that" has to be said explicitly.
pub fn action_unsupported(action: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: format!("this indexer does not support the '{action}' action"),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

/// Declare a command-ABI indexer from the functions the plugin already has.
///
/// `descriptor` builds the `PluginDescriptor`, `search` implements the search
/// operation, and `action` — when the plugin has one — implements the action
/// operation. Both operations keep their `Result` signatures, so their bodies
/// go on using `?` exactly as they did under `#[plugin_fn]`.
#[macro_export]
macro_rules! scryer_indexer_main {
    (descriptor = $descriptor:path, search = $search:path $(,)?) => {
        $crate::pdk::scryer_indexer_plugin_main!(
            descriptor = $descriptor,
            handler = |command| match command {
                $crate::pdk::PluginIndexerCommand::Search(request) => {
                    $crate::pdk::PluginIndexerCommandResult::Search($crate::to_plugin_result(
                        $search(request),
                    ))
                }
                $crate::pdk::PluginIndexerCommand::Action(request) => {
                    $crate::pdk::PluginIndexerCommandResult::Action($crate::sdk::PluginResult::Err(
                        $crate::action_unsupported(&request.action),
                    ))
                }
            },
        );
    };
    (descriptor = $descriptor:path, search = $search:path, action = $action:path $(,)?) => {
        $crate::pdk::scryer_indexer_plugin_main!(
            descriptor = $descriptor,
            handler = |command| match command {
                $crate::pdk::PluginIndexerCommand::Search(request) => {
                    $crate::pdk::PluginIndexerCommandResult::Search($crate::to_plugin_result(
                        $search(request),
                    ))
                }
                $crate::pdk::PluginIndexerCommand::Action(request) => {
                    $crate::pdk::PluginIndexerCommandResult::Action($crate::to_plugin_result(
                        $action(request),
                    ))
                }
            },
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_values_pass_through() {
        let result: PluginResult<u8> = to_plugin_result(Ok::<u8, pdk::Error>(7));
        match result {
            PluginResult::Ok(value) => assert_eq!(value, 7),
            PluginResult::Err(error) => panic!("unexpected error: {error:?}"),
        }
    }

    #[test]
    fn errors_stay_retryable_and_keep_their_detail() {
        let result: PluginResult<u8> =
            to_plugin_result(Err::<u8, pdk::Error>(pdk::Error::msg("upstream 503")));
        let PluginResult::Err(error) = result else {
            panic!("expected an error");
        };
        assert!(matches!(error.code, PluginErrorCode::Temporary));
        assert_eq!(error.debug_message.as_deref(), Some("upstream 503"));
        assert!(
            !error.public_message.contains("503"),
            "upstream detail must stay in debug_message"
        );
    }

    #[test]
    fn structured_errors_survive_command_result_conversion() {
        let result: PluginResult<u8> =
            to_plugin_result(Err(structured_plugin_error(PluginError {
                code: PluginErrorCode::RateLimited,
                public_message: "search deferred".to_string(),
                debug_message: Some("quota exhausted".to_string()),
                retry_after_seconds: Some(60),
                details: None,
            })));
        let PluginResult::Err(error) = result else {
            panic!("expected structured error");
        };
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(60));
    }

    #[test]
    fn missing_actions_report_unsupported_by_name() {
        let error = action_unsupported("caps");
        assert!(matches!(error.code, PluginErrorCode::Unsupported));
        assert!(error.public_message.contains("caps"));
    }

    #[test]
    fn log_levels_render_their_own_name() {
        assert_eq!(LogLevel::Warn.as_str(), "WARN");
        // Exercises the macro's own expansion, not just the level type.
        log!(LogLevel::Debug, "probe {}", 1);
    }
}
