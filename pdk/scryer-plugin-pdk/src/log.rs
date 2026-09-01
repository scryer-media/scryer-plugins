//! World-agnostic guest diagnostics.
//!
//! # Why this is a hook and not a function call
//!
//! Scryer runs guests in shapes that reach a log sink by different routes, and
//! **the build target cannot tell them apart** — an indexer component and a
//! subtitle component are both `wasm32-wasip2`:
//!
//! | Guest shape | Where a log line goes |
//! |---|---|
//! | Indexer component | the indexer world's `log` import, i.e. Scryer's host log service |
//! | Family component (subtitle, download client, notification) | guest **stderr**, which the family component hosts capture as a size-capped tail and re-emit through `tracing` |
//! | Preview 1 command guest | guest stderr, captured the same way by the command host |
//! | Native `cargo test` | stderr, so an assertion failure shows the run that produced it |
//!
//! Shared crates — `newznab-common` and friends — are linked into *several* of
//! those shapes. If they called the indexer world's `log` directly, every
//! family component that depends on them would carry a live
//! `scryer:indexer/host` import its host does not serve, and the artifact would
//! fail to instantiate. The linker keeps a *named* import alive, so it is not
//! enough to avoid taking that branch at run time: nothing reachable from a
//! family component may mention it at all.
//!
//! So this module holds a `fn` pointer, exactly as [`crate::host`] does for the
//! host-services transport and [`crate::component::install_config_get`] does
//! for configuration. [`crate::scryer_indexer_component_main!`] publishes the
//! indexer world's `log` through [`crate::component::install_log`]; the family
//! entry macros publish [`stderr_log`]. Neither names the other.
//!
//! # The default is stderr, not silence
//!
//! With no hook installed, [`log`] writes to stderr. That is deliberate: every
//! guest shape Scryer runs has a stderr its host already collects, so the
//! fallback is the *correct* sink for a Preview 1 command guest and a
//! test-visible one natively. A silent default would have quietly deleted the
//! diagnostics of every plugin that has not yet installed a hook.

use std::fmt;
use std::io::Write;
use std::sync::{PoisonError, RwLock};

/// Severity of one guest diagnostic.
///
/// The variants match the indexer world's `log-level` enum one-for-one, so
/// [`crate::component::install_log`] maps between them without loss, and they
/// carry the names first-party plugins already use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    #[must_use]
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

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One diagnostic line, delivered to whatever sink this guest shape has.
///
/// A plain `fn` pointer rather than a closure or trait object, for the same
/// reasons [`crate::host::HostCall`] is one: it carries no state, is `Copy`,
/// and can be read on any path without allocating.
pub type LogHook = fn(LogLevel, &str);

/// The installed sink, or `None` for the stderr default.
///
/// Written once per component instantiation by the entry macro. A component
/// instance is single-threaded and short-lived, so the lock is uncontended; it
/// exists to keep the registry sound, not to arbitrate.
static LOG: RwLock<Option<LogHook>> = RwLock::new(None);

/// Publish the sink backing [`log`].
///
/// The entry macros call this at the top of every world export, because Scryer
/// instantiates a component once per invocation and a fresh instance starts
/// with an empty registry. Installing twice is harmless; the last writer wins.
pub fn install_log(hook: LogHook) {
    *LOG.write().unwrap_or_else(PoisonError::into_inner) = Some(hook);
}

/// Publish stderr as this guest's log sink.
///
/// This is what the family entry macros install. It is also what [`log`] falls
/// back to when nothing is installed, so calling it is a statement of intent
/// rather than a behaviour change — it pins the family contract in the macro
/// expansion and makes [`log_installed`] true.
pub fn install_stderr_log() {
    install_log(stderr_log);
}

/// Write one diagnostic to guest stderr.
///
/// Exposed so a guest driving a world the PDK ships no entry macro for can
/// install the family behaviour explicitly.
pub fn stderr_log(level: LogLevel, message: &str) {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "[{}] {message}", level.as_str());
}

/// Whether a sink has been installed on this instance.
///
/// False means [`log`] still uses the stderr default, not that logging is off.
#[must_use]
pub fn log_installed() -> bool {
    installed_log().is_some()
}

fn installed_log() -> Option<LogHook> {
    *LOG.read().unwrap_or_else(PoisonError::into_inner)
}

/// Emit one diagnostic through this guest's sink.
///
/// This is the single call shared crates should make. It never fails and never
/// panics: a diagnostic that cannot be delivered is dropped, because losing a
/// log line must not turn into a plugin failure.
pub fn log(level: LogLevel, message: &str) {
    match installed_log() {
        Some(hook) => hook(level, message),
        None => stderr_log(level, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The sink registry is process-wide, so tests that install one take this
    /// lock and clear it again. Guests never need it: a component instance runs
    /// exactly one invocation and is then dropped.
    static REGISTRY: Mutex<()> = Mutex::new(());

    static CAPTURED: Mutex<Vec<(LogLevel, String)>> = Mutex::new(Vec::new());

    fn lock_registry() -> MutexGuard<'static, ()> {
        REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn clear_log() {
        *LOG.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn capture(level: LogLevel, message: &str) {
        CAPTURED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((level, message.to_string()));
    }

    #[test]
    fn an_installed_sink_receives_the_level_and_the_message() {
        let _guard = lock_registry();
        CAPTURED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();

        install_log(capture);
        assert!(log_installed());
        log(LogLevel::Warn, "upstream returned 429");

        let captured = CAPTURED.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            captured.as_slice(),
            [(LogLevel::Warn, "upstream returned 429".to_string())]
        );
        drop(captured);

        clear_log();
        assert!(!log_installed());
    }

    #[test]
    fn a_guest_without_a_sink_falls_back_to_stderr_rather_than_silence() {
        let _guard = lock_registry();
        clear_log();

        assert!(!log_installed());
        // The fallback is what a Preview 1 command guest and a native test both
        // want; the assertion that matters is that this path is reached at all
        // and does not panic.
        log(LogLevel::Info, "no sink installed");
    }

    #[test]
    fn levels_render_their_own_name() {
        assert_eq!(LogLevel::Trace.as_str(), "TRACE");
        assert_eq!(LogLevel::Error.to_string(), "ERROR");
    }
}
