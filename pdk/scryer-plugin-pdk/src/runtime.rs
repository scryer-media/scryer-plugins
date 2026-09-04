//! The family-neutral runtime capabilities, and the hook that decides which
//! world serves them.
//!
//! # Why this module exists
//!
//! Scryer has two typed capability interfaces that say the same thing:
//! `scryer:indexer/host@1.1.0`, which the indexer world has carried since 1.1,
//! and `scryer:runtime/host@1.0.0`, the family-neutral surface the subtitle
//! world adopted at `scryer:subtitle@1.1.0`. They declare the same records,
//! the same enums, and the same functions — minus the two strategy-event
//! functions, which are an indexer concept.
//!
//! Shared crates are linked into *both* shapes. `newznab-common` backs the
//! newznab and torznab indexers **and**, since amenzb became a subtitle
//! provider over the same Newznab API, a family component. It cannot name
//! either interface directly: a named import stays alive in the artifact
//! whether or not the branch runs, so an indexer that named the runtime host
//! (or a subtitle provider that named the indexer host) would carry an import
//! its host does not serve and would fail to instantiate. That is not a
//! hypothesis — it is the failure the migration started from, reproduced in
//! full before any of this was written.
//!
//! So the capability calls go behind a hook, exactly as [`crate::host`] does
//! for the encoded services door, [`crate::log`] does for diagnostics, and
//! [`crate::component::install_config_get`] does for configuration:
//!
//! | Guest shape | Backend | Installed by |
//! |---|---|---|
//! | Indexer component | `scryer:indexer/host@1.1.0` | [`crate::component::install_indexer_runtime`], from [`crate::scryer_indexer_component_main!`] |
//! | Subtitle component (`scryer:subtitle@1.1.0`) | `scryer:runtime/host@1.0.0` | [`install_runtime_host`], from [`crate::scryer_subtitle_component_main!`] |
//! | Native `cargo test`, or a guest that installed nothing | none | — |
//!
//! With nothing installed every capability degrades in the way its return type
//! already allows: [`http`] reports [`HostError::Transport`], the clocks read
//! zero, the lookups return `None`, and [`state_cas`] fails. A native unit test
//! therefore runs the same code the guest runs, and sees a host that answers
//! nothing rather than a link error.
//!
//! # Why the gates live here
//!
//! [`StartRateGate`], [`CooldownGate`] and [`WindowQuota`] are plugin-owned
//! policy built on `state-cas` and the monotonic clock. They were written for
//! indexers and they are what a subtitle provider hitting the same upstream
//! needs, so they have exactly one implementation — this one — and
//! [`crate::component`] re-exports it. Anything else would be two copies of a
//! CAS loop that must agree on its encoding for ever.
//!
//! # Where the PDK's own import comes from
//!
//! [`install_runtime_host`] is backed by a `wit_bindgen::generate!` the PDK
//! owns, of a private import-only world (`scryer:pdk/runtime-imports@1.0.0`,
//! in `wit/pdk-v1.0.0`). `component.rs` already generates the whole
//! `indexer-plugin` world and that one cannot be reused: it is bound to
//! `scryer:indexer/host@1.1.0` and carries the indexer world's exports with
//! it. The two `generate!` invocations coexist because the component-type
//! metadata of a linked-in crate contributes only what the final artifact
//! actually names — the reason a subtitle component built against this PDK
//! imports `scryer:host/services` and `scryer:runtime/host` and nothing else,
//! even though `component.rs` is compiled into it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{PoisonError, RwLock};
use std::{fmt, mem};

// Re-exported, not merely imported: [`http`] and [`http_fields`] take and
// return these, so a plugin that calls them has to be able to name them
// without depending on the SDK's module layout.
pub use scryer_plugin_sdk::host::{PluginHttpRequest, PluginHttpResponse};
use scryer_plugin_sdk::{
    IndexerSearchIncompleteReason, IndexerSearchPluginError, PluginError, PluginErrorCode,
    PluginErrorDetails, PluginProviderProfile,
};

mod imports {
    wit_bindgen::generate!({
        world: "scryer:pdk/runtime-imports@1.0.0",
        path: ["wit/runtime-v1.0.0", "wit/pdk-v1.0.0"],
        generate_all,
    });
}

use self::imports::scryer::runtime::host as runtime_host;

/// A boxed guest future. The capability trait has to be object-safe, and an
/// `async fn` in a trait is not, so the two suspending capabilities return
/// this instead.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A component-host error from an imported capability.
///
/// The variants are the `transport-error` enum both capability interfaces
/// declare, one for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
    InvalidRequest,
    ForbiddenOrigin,
    Timeout,
    Cancelled,
    ResponseTooLarge,
    Capacity,
    Transport,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidRequest => "host rejected the HTTP request",
            Self::ForbiddenOrigin => "host denied the HTTP origin",
            Self::Timeout => "host HTTP request timed out",
            Self::Cancelled => "host HTTP request was cancelled",
            Self::ResponseTooLarge => "host HTTP response exceeded its limit",
            Self::Capacity => "host HTTP capacity is exhausted",
            Self::Transport => "host HTTP transport failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for HostError {}

impl From<runtime_host::TransportError> for HostError {
    fn from(value: runtime_host::TransportError) -> Self {
        match value {
            runtime_host::TransportError::InvalidRequest => Self::InvalidRequest,
            runtime_host::TransportError::ForbiddenOrigin => Self::ForbiddenOrigin,
            runtime_host::TransportError::Timeout => Self::Timeout,
            runtime_host::TransportError::Cancelled => Self::Cancelled,
            runtime_host::TransportError::ResponseTooLarge => Self::ResponseTooLarge,
            runtime_host::TransportError::Capacity => Self::Capacity,
            runtime_host::TransportError::Transport => Self::Transport,
        }
    }
}

/// One HTTP response with its header fields kept exactly as the host received
/// them, in upstream order and with repeats preserved.
///
/// [`http`] returns the SDK's map-shaped response, which is the right model for
/// single-valued fields but keeps only one value per field name. A guest that
/// owns a cookie jar or otherwise reads a repeated field — `Set-Cookie` above
/// all — needs the unmerged list instead.
#[derive(Clone, Debug)]
pub struct PluginHttpFieldsResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl PluginHttpFieldsResponse {
    /// Every value of one header field, compared case-insensitively.
    pub fn field_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter_map(move |(field, value)| {
            field.eq_ignore_ascii_case(name).then_some(value.as_str())
        })
    }
}

/// The capabilities one Scryer world serves a guest.
///
/// Implemented once per capability interface — [`crate::component`] for the
/// indexer host, this module for the family runtime host — and selected at run
/// time by whichever entry macro the guest expanded. Callers use the free
/// functions below rather than this trait.
pub trait HostRuntime: Sync {
    /// One host-policy-checked HTTP attempt, with repeated response header
    /// fields preserved.
    fn http_fields(
        &self,
        request: PluginHttpRequest,
    ) -> BoxFuture<'_, Result<PluginHttpFieldsResponse, HostError>>;

    /// Yield until the requested monotonic duration has elapsed.
    fn sleep(&self, duration_ms: u64) -> BoxFuture<'_, ()>;

    fn monotonic_now_ms(&self) -> u64;
    fn operation_deadline_monotonic_ms(&self) -> u64;
    fn wall_now_ms(&self) -> u64;
    fn config_get(&self, key: &str) -> Option<String>;
    fn provider_profile_bytes(&self) -> Option<Vec<u8>>;
    fn state_get(&self, key: &str) -> Option<Vec<u8>>;
    fn state_cas(&self, key: &str, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> bool;
    fn log(&self, level: crate::log::LogLevel, message: &str);
}

/// The installed backend, or `None` outside a guest that published one.
///
/// A component instance is single-threaded and lives for one invocation, so
/// this lock is uncontended; it exists to keep the registry sound, not to
/// arbitrate.
static RUNTIME: RwLock<Option<&'static dyn HostRuntime>> = RwLock::new(None);

/// Publish the backend serving [`http`] and its siblings.
///
/// The entry macros call this at the top of every world export, because Scryer
/// instantiates a component once per invocation and a fresh instance starts
/// with an empty registry. Installing twice is harmless; the last writer wins.
pub fn install_host_runtime(runtime: &'static dyn HostRuntime) {
    *RUNTIME.write().unwrap_or_else(PoisonError::into_inner) = Some(runtime);
}

/// Whether a backend has been installed on this instance.
///
/// False means every capability below answers with its inert default, not that
/// the host refused the call.
#[must_use]
pub fn host_runtime_installed() -> bool {
    installed().is_some()
}

fn installed() -> Option<&'static dyn HostRuntime> {
    *RUNTIME.read().unwrap_or_else(PoisonError::into_inner)
}

/// The `scryer:runtime/host@1.0.0` backend, for family components.
struct RuntimeHost;

impl HostRuntime for RuntimeHost {
    fn http_fields(
        &self,
        request: PluginHttpRequest,
    ) -> BoxFuture<'_, Result<PluginHttpFieldsResponse, HostError>> {
        Box::pin(async move {
            let response = runtime_host::http(runtime_host::HttpRequest {
                method: request.method.unwrap_or_else(|| "GET".to_string()),
                url: request.url,
                headers: request
                    .headers
                    .into_iter()
                    .map(|(name, value)| runtime_host::Header { name, value })
                    .collect(),
                body: request.body,
            })
            .await
            .map_err(HostError::from)?;

            Ok(PluginHttpFieldsResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|header| (header.name, header.value))
                    .collect(),
                body: response.body,
            })
        })
    }

    fn sleep(&self, duration_ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(runtime_host::sleep(duration_ms))
    }

    fn monotonic_now_ms(&self) -> u64 {
        runtime_host::monotonic_now_ms()
    }

    fn operation_deadline_monotonic_ms(&self) -> u64 {
        runtime_host::operation_deadline_monotonic_ms()
    }

    fn wall_now_ms(&self) -> u64 {
        runtime_host::wall_now_ms()
    }

    fn config_get(&self, key: &str) -> Option<String> {
        runtime_host::config_get(key)
    }

    fn provider_profile_bytes(&self) -> Option<Vec<u8>> {
        runtime_host::provider_profile()
    }

    fn state_get(&self, key: &str) -> Option<Vec<u8>> {
        runtime_host::state_get(key)
    }

    fn state_cas(&self, key: &str, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> bool {
        runtime_host::state_cas(key, expected, replacement)
    }

    fn log(&self, level: crate::log::LogLevel, message: &str) {
        runtime_host::log(runtime_log_level(level), message);
    }
}

static RUNTIME_HOST: RuntimeHost = RuntimeHost;

/// Publish `scryer:runtime/host@1.0.0` as this guest's backend.
///
/// This is the only place in the PDK that names that import, which is what
/// keeps it out of indexer artifacts: nothing an indexer component reaches
/// calls this function, so the linker drops the import along with it.
pub fn install_runtime_host() {
    install_host_runtime(&RUNTIME_HOST);
}

/// The PDK's world-agnostic level as the runtime host declares it.
///
/// The two enums have the same five cases by construction, so this is total
/// and lossless; a new case on either side is a compile error here rather than
/// a silently downgraded diagnostic.
fn runtime_log_level(level: crate::log::LogLevel) -> runtime_host::LogLevel {
    match level {
        crate::log::LogLevel::Trace => runtime_host::LogLevel::Trace,
        crate::log::LogLevel::Debug => runtime_host::LogLevel::Debug,
        crate::log::LogLevel::Info => runtime_host::LogLevel::Info,
        crate::log::LogLevel::Warn => runtime_host::LogLevel::Warn,
        crate::log::LogLevel::Error => runtime_host::LogLevel::Error,
    }
}

/// Publish the runtime host's `config-get` to [`crate::config`].
///
/// The family entry macros do **not** call this: a family component reaches
/// configuration through `scryer:host/services@1.0.0`, the encoded door the
/// macro already installs, and both doors read the same host-side map. It is
/// public for a guest driving the runtime host without that door.
pub fn install_config_get() {
    fn hook(key: &str) -> Option<String> {
        RUNTIME_HOST.config_get(key)
    }

    install_config_get_hook(hook);
}

/// One configuration lookup against whichever world published it.
pub(crate) type ConfigGetHook = fn(&str) -> Option<String>;

/// The typed `config-get` a guest published, if any.
///
/// This registry is world-agnostic on purpose: [`crate::config`] reads it
/// without naming either capability interface, so a family component that
/// never publishes one falls through to `scryer:host/services` instead of
/// carrying an indexer import it cannot satisfy.
static CONFIG_GET: RwLock<Option<ConfigGetHook>> = RwLock::new(None);

pub(crate) fn install_config_get_hook(hook: ConfigGetHook) {
    *CONFIG_GET.write().unwrap_or_else(PoisonError::into_inner) = Some(hook);
}

pub(crate) fn installed_config_get() -> Option<ConfigGetHook> {
    *CONFIG_GET.read().unwrap_or_else(PoisonError::into_inner)
}

/// Publish the runtime host's `log` to [`crate::log`].
///
/// The family entry macros install stderr instead — the family component hosts
/// already capture a size-capped stderr tail and re-emit it through `tracing`,
/// and that stays the family contract for this release. This is public so a
/// guest can opt into the structured sink.
pub fn install_log() {
    fn hook(level: crate::log::LogLevel, message: &str) {
        RUNTIME_HOST.log(level, message);
    }

    crate::log::install_log(hook);
}

/// Perform one host-policy-checked HTTP attempt, preserving repeated response
/// header fields.
pub async fn http_fields(
    request: PluginHttpRequest,
) -> Result<PluginHttpFieldsResponse, HostError> {
    match installed() {
        Some(runtime) => runtime.http_fields(request).await,
        None => Err(HostError::Transport),
    }
}

/// Perform one host-policy-checked HTTP attempt.
pub async fn http(request: PluginHttpRequest) -> Result<PluginHttpResponse, HostError> {
    let response = http_fields(request).await?;

    Ok(PluginHttpResponse {
        status: response.status,
        headers: response.headers.into_iter().collect(),
        body: response.body,
    })
}

/// Yield the component invocation until the requested monotonic duration has
/// elapsed. Plugins use this for their own rate gates and retry delays.
pub async fn sleep(duration_ms: u64) {
    if let Some(runtime) = installed() {
        runtime.sleep(duration_ms).await;
    }
}

/// Process-relative monotonic time for plugin-owned rate and quota gates.
///
/// The value is anchored when Scryer creates this configured host and
/// therefore remains stable across actor recreation but deliberately resets
/// when the application restarts.
#[must_use]
pub fn monotonic_now_ms() -> u64 {
    installed().map_or(0, HostRuntime::monotonic_now_ms)
}

/// The absolute monotonic deadline for the current operation.
///
/// This shares [`monotonic_now_ms`]'s origin, so plugins can determine whether
/// an upstream-mandated wait is still useful without consulting wall-clock
/// time. A zero deadline means the operation has already expired.
#[must_use]
pub fn operation_deadline_monotonic_ms() -> u64 {
    installed().map_or(0, HostRuntime::operation_deadline_monotonic_ms)
}

/// UTC Unix milliseconds for provider headers that carry an absolute reset
/// instant. Rate gates must use [`monotonic_now_ms`] instead.
#[must_use]
pub fn wall_now_ms() -> u64 {
    installed().map_or(0, HostRuntime::wall_now_ms)
}

/// One configuration lookup against whichever world serves this guest.
pub fn config_get(key: impl AsRef<str>) -> Option<String> {
    installed().and_then(|runtime| runtime.config_get(key.as_ref()))
}

/// The host-resolved provider profile bytes for this configured instance.
///
/// A world that carries no provider profile — every family world today —
/// answers `None`, which is the same answer an unconfigured indexer gives.
#[must_use]
pub fn provider_profile_bytes() -> Option<Vec<u8>> {
    installed().and_then(HostRuntime::provider_profile_bytes)
}

/// Decode the host-resolved provider profile through the SDK's stable runtime
/// model. Catalog-only provenance and setup metadata do not cross the
/// component boundary.
pub fn provider_profile() -> Result<Option<PluginProviderProfile>, serde_json::Error> {
    provider_profile_bytes()
        .map(|encoded| serde_json::from_slice(&encoded))
        .transpose()
}

/// Read one plugin-owned state value.
pub fn state_get(key: impl AsRef<str>) -> Option<Vec<u8>> {
    installed().and_then(|runtime| runtime.state_get(key.as_ref()))
}

/// Replace one plugin-owned state value if it still holds `expected`.
///
/// The host applies the comparison and the write as one step, which is what
/// makes the gates below safe when several guest futures race.
pub fn state_cas(
    key: impl AsRef<str>,
    expected: Option<Vec<u8>>,
    replacement: Option<Vec<u8>>,
) -> bool {
    installed().is_some_and(|runtime| {
        runtime.state_cas(key.as_ref(), expected.as_deref(), replacement.as_deref())
    })
}

/// Emit one diagnostic through the installed capability world.
///
/// Prefer [`crate::log::log`], which reaches whatever sink this guest shape
/// has. This is the direct call, for a guest that wants the host's structured
/// log specifically.
pub fn log(level: crate::log::LogLevel, message: impl AsRef<str>) {
    if let Some(runtime) = installed() {
        runtime.log(level, message.as_ref());
    }
}

/// A required plugin-owned wait that cannot complete before the operation
/// deadline. Callers convert this into the SDK's typed deferred result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadlineExceeded {
    pub retry_after_ms: u64,
}

/// Sleep only when the current operation has enough remaining budget.
pub async fn sleep_until_deadline(duration_ms: u64) -> Result<(), DeadlineExceeded> {
    if duration_ms == 0 {
        return Ok(());
    }

    let now = monotonic_now_ms();
    let deadline = operation_deadline_monotonic_ms();
    let remaining_ms = deadline.saturating_sub(now);
    if duration_ms > remaining_ms {
        return Err(DeadlineExceeded {
            retry_after_ms: duration_ms,
        });
    }

    sleep(duration_ms).await;
    Ok(())
}

/// Encode a deadline-aware pacing decision as Scryer's typed deferred search
/// outcome. It is intentionally an ordinary plugin result rather than an ABI
/// invocation failure, allowing the host to retain any earlier candidates.
pub fn deadline_deferred_error(wait: DeadlineExceeded) -> crate::Error {
    let retry_after_seconds =
        i64::try_from(wait.retry_after_ms.div_ceil(1_000)).unwrap_or(i64::MAX);
    crate::component::structured_plugin_error(PluginError {
        code: PluginErrorCode::UpstreamUnavailable,
        public_message: "indexer search deferred by upstream pacing".to_string(),
        debug_message: Some(
            "the operation deadline expires before the next upstream request may start".to_string(),
        ),
        retry_after_seconds: Some(retry_after_seconds),
        details: Some(PluginErrorDetails::IndexerSearch(
            IndexerSearchPluginError::Deferred {
                reason: IndexerSearchIncompleteReason::RateLimited,
                retry_after_seconds: Some(retry_after_seconds),
            },
        )),
    })
}

/// A plugin-owned, component-instance start-rate gate.
///
/// The host only supplies atomic state and monotonic time. This type chooses
/// the upstream policy and waits itself, including when several guest futures
/// race to start requests.
#[derive(Clone, Debug)]
pub struct StartRateGate {
    state_key: String,
    interval_ms: u64,
}

impl StartRateGate {
    /// Create an evenly-spaced start-rate gate. `starts` must be non-zero.
    pub fn new(state_key: impl Into<String>, starts: u32, interval_ms: u64) -> Self {
        let starts = u64::from(starts.max(1));
        Self {
            state_key: state_key.into(),
            interval_ms: interval_ms.max(1).div_ceil(starts).max(1),
        }
    }

    /// Wait until this request may begin according to the plugin's policy.
    pub async fn acquire(&self) -> Result<(), DeadlineExceeded> {
        loop {
            let expected = state_get(&self.state_key);
            let previous = expected
                .as_deref()
                .and_then(|value| value.try_into().ok().map(u64::from_le_bytes));
            let now = monotonic_now_ms();
            let eligible_at = previous
                .unwrap_or(now.saturating_sub(self.interval_ms))
                .saturating_add(self.interval_ms);
            if eligible_at > now {
                sleep_until_deadline(eligible_at.saturating_sub(now)).await?;
                continue;
            }
            if state_cas(&self.state_key, expected, Some(now.to_le_bytes().to_vec())) {
                return Ok(());
            }
        }
    }
}

/// The result when a plugin-owned fixed-window quota cannot admit work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaExhausted {
    pub retry_after_ms: u64,
}

/// A plugin-owned quota window backed by component instance state.
#[derive(Clone, Debug)]
pub struct WindowQuota {
    state_key: String,
    limit: u32,
    window_ms: u64,
}

/// A plugin-owned cooldown shared by every concurrent future in one configured
/// component. Providers can apply it after a 429 or an upstream reset hint.
#[derive(Clone, Debug)]
pub struct CooldownGate {
    state_key: String,
}

impl CooldownGate {
    pub fn new(state_key: impl Into<String>) -> Self {
        Self {
            state_key: state_key.into(),
        }
    }

    /// Delay a request until any recorded cooldown has elapsed.
    pub async fn wait(&self) -> Result<(), DeadlineExceeded> {
        loop {
            let until = state_get(&self.state_key)
                .as_deref()
                .and_then(|value| value.try_into().ok().map(u64::from_le_bytes))
                .unwrap_or_default();
            let now = monotonic_now_ms();
            if until <= now {
                return Ok(());
            }
            sleep_until_deadline(until.saturating_sub(now)).await?;
        }
    }

    /// Extend the shared cooldown without shortening a longer concurrent one.
    pub fn defer_for(&self, duration_ms: u64) {
        let requested_until = monotonic_now_ms().saturating_add(duration_ms);
        loop {
            let expected = state_get(&self.state_key);
            let existing_until = expected
                .as_deref()
                .and_then(|value| value.try_into().ok().map(u64::from_le_bytes))
                .unwrap_or_default();
            let replacement = existing_until.max(requested_until);
            if state_cas(
                &self.state_key,
                expected,
                Some(replacement.to_le_bytes().to_vec()),
            ) {
                return;
            }
        }
    }
}

impl WindowQuota {
    pub fn new(state_key: impl Into<String>, limit: u32, window_ms: u64) -> Self {
        Self {
            state_key: state_key.into(),
            limit,
            window_ms: window_ms.max(1),
        }
    }

    /// Atomically reserve `uses` requests or report when the current window
    /// ends. A zero limit is intentionally never admitted.
    pub fn reserve(&self, uses: u32) -> Result<(), QuotaExhausted> {
        loop {
            let expected = state_get(&self.state_key);
            // Provider quota windows normally reset against wall-clock
            // boundaries; rate gates and cooldowns remain monotonic.
            let now = wall_now_ms();
            let bucket = now / self.window_ms;
            let (stored_bucket, used) = expected
                .as_deref()
                .and_then(decode_quota_state)
                .unwrap_or((bucket, 0));
            let used = if stored_bucket == bucket { used } else { 0 };
            if uses > self.limit.saturating_sub(used) {
                let retry_after_ms = self.window_ms.saturating_sub(now % self.window_ms).max(1);
                return Err(QuotaExhausted { retry_after_ms });
            }
            let replacement = encode_quota_state(bucket, used.saturating_add(uses));
            if state_cas(&self.state_key, expected, Some(replacement)) {
                return Ok(());
            }
        }
    }
}

fn decode_quota_state(value: &[u8]) -> Option<(u64, u32)> {
    let bytes: [u8; 12] = value.try_into().ok()?;
    let bucket = u64::from_le_bytes(bytes[..8].try_into().ok()?);
    let used = u32::from_le_bytes(bytes[8..].try_into().ok()?);
    Some((bucket, used))
}

fn encode_quota_state(bucket: u64, used: u32) -> Vec<u8> {
    let mut value = Vec::with_capacity(mem::size_of::<u64>() + mem::size_of::<u32>());
    value.extend_from_slice(&bucket.to_le_bytes());
    value.extend_from_slice(&used.to_le_bytes());
    value
}
