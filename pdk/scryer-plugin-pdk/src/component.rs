//! WASIp2 component bindings for first-party indexer plugins.
//!
//! Indexers own their upstream pacing, quotas, retries, and fanout. The host
//! provides one cancellable, policy-checked HTTP attempt at a time. Ordinary
//! plugin failures stay in the SDK's typed [`PluginResult`]; this component
//! ABI's `invocation-error` is reserved for malformed ABI payloads and other
//! faults that prevent a typed result from being produced.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::sync::{PoisonError, RwLock};

use scryer_plugin_sdk::command::{PluginActionRequest, PluginActionResponse};
use scryer_plugin_sdk::{
    IndexerSearchIncompleteReason, IndexerSearchPluginError, PluginError, PluginErrorCode,
    PluginErrorDetails, PluginProviderProfile, PluginResult, PluginSearchPlanRequest,
    PluginSearchPlanSummary, PluginSearchRequest, PluginSearchResponse, PluginSearchStrategyEvent,
};

wit_bindgen::generate!({
    world: "indexer-plugin",
    path: "wit",
    pub_export_macro: true,
});

use self::scryer::indexer::host as component_host;
pub use self::scryer::indexer::host::LogLevel;

/// Re-export the minimal future utilities indexer plugins need to bound and
/// collect their own upstream fanout.
pub use futures_util::stream::{self, StreamExt};

/// A component-host error from an imported capability.
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

impl From<component_host::TransportError> for HostError {
    fn from(value: component_host::TransportError) -> Self {
        match value {
            component_host::TransportError::InvalidRequest => Self::InvalidRequest,
            component_host::TransportError::ForbiddenOrigin => Self::ForbiddenOrigin,
            component_host::TransportError::Timeout => Self::Timeout,
            component_host::TransportError::Cancelled => Self::Cancelled,
            component_host::TransportError::ResponseTooLarge => Self::ResponseTooLarge,
            component_host::TransportError::Capacity => Self::Capacity,
            component_host::TransportError::Transport => Self::Transport,
        }
    }
}

/// Perform one host-policy-checked HTTP attempt.
pub async fn http(
    request: scryer_plugin_sdk::host::PluginHttpRequest,
) -> Result<scryer_plugin_sdk::host::PluginHttpResponse, HostError> {
    let response = component_host::http(component_host::HttpRequest {
        method: request.method.unwrap_or_else(|| "GET".to_string()),
        url: request.url,
        headers: request
            .headers
            .into_iter()
            .map(|(name, value)| component_host::Header { name, value })
            .collect(),
        body: request.body,
    })
    .await
    .map_err(HostError::from)?;

    Ok(scryer_plugin_sdk::host::PluginHttpResponse {
        status: response.status,
        headers: response
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect::<BTreeMap<_, _>>(),
        body: response.body,
    })
}

/// Yield the component invocation until the requested monotonic duration has
/// elapsed. Plugins use this for their own rate gates and retry delays.
pub async fn sleep(duration_ms: u64) {
    component_host::sleep(duration_ms).await;
}

/// Process-relative monotonic time for plugin-owned rate and quota gates.
///
/// The value is anchored when Scryer creates this configured indexer host and
/// therefore remains stable across actor recreation but deliberately resets
/// when the application restarts.
pub fn monotonic_now_ms() -> u64 {
    component_host::monotonic_now_ms()
}

/// The absolute monotonic deadline for the current operation.
///
/// This shares [`monotonic_now_ms`]'s origin, so plugins can determine whether
/// an upstream-mandated wait is still useful without consulting wall-clock
/// time. A zero deadline means the operation has already expired.
pub fn operation_deadline_monotonic_ms() -> u64 {
    component_host::operation_deadline_monotonic_ms()
}

/// UTC Unix milliseconds for provider headers that carry an absolute reset
/// instant. Rate gates must use [`monotonic_now_ms`] instead.
pub fn wall_now_ms() -> u64 {
    component_host::wall_now_ms()
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
    structured_plugin_error(PluginError {
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
    let mut value = Vec::with_capacity(12);
    value.extend_from_slice(&bucket.to_le_bytes());
    value.extend_from_slice(&used.to_le_bytes());
    value
}

pub fn config_get(key: impl Into<String>) -> Option<String> {
    component_host::config_get(&key.into())
}

/// One configuration lookup against the indexer world's host.
type ConfigGetHook = fn(&str) -> Option<String>;

/// The indexer world's `config-get`, once an indexer component has published
/// it.
///
/// This registry exists so that nothing outside
/// [`crate::scryer_indexer_component_main!`]
/// mentions [`config_get`] by name. That matters more than it looks: the
/// shared [`crate::config`] shim has to reach *two* different worlds — the
/// indexer host here, and `scryer:host/services` in [`crate::host`] — and both
/// are `wasm32-wasip2`, so the build target cannot tell them apart. If the
/// shim called [`config_get`] directly, every family component would keep a
/// live `scryer:indexer/host` import that its host does not serve, and the
/// artifact would fail to instantiate. Behind a hook, an unused import is
/// linked out.
static CONFIG_GET: RwLock<Option<ConfigGetHook>> = RwLock::new(None);

/// Publish this indexer component's `config-get` to [`crate::config`].
///
/// [`crate::scryer_indexer_component_main!`] calls this; indexer plugins that call
/// [`config_get`] directly never need it.
pub fn install_config_get() {
    fn hook(key: &str) -> Option<String> {
        component_host::config_get(key)
    }

    *CONFIG_GET.write().unwrap_or_else(PoisonError::into_inner) = Some(hook);
}

pub(crate) fn installed_config_get() -> Option<ConfigGetHook> {
    *CONFIG_GET.read().unwrap_or_else(PoisonError::into_inner)
}

/// Return the host-resolved provider profile bytes for this configured
/// component instance.
pub fn provider_profile_bytes() -> Option<Vec<u8>> {
    component_host::provider_profile()
}

/// Decode the host-resolved provider profile through the SDK's stable runtime
/// model. Catalog-only provenance and setup metadata do not cross the
/// component boundary.
pub fn provider_profile() -> Result<Option<PluginProviderProfile>, serde_json::Error> {
    provider_profile_bytes()
        .map(|encoded| serde_json::from_slice(&encoded))
        .transpose()
}

pub fn state_get(key: impl Into<String>) -> Option<Vec<u8>> {
    component_host::state_get(&key.into())
}

pub fn state_cas(
    key: impl Into<String>,
    expected: Option<Vec<u8>>,
    replacement: Option<Vec<u8>>,
) -> bool {
    component_host::state_cas(&key.into(), expected.as_deref(), replacement.as_deref())
}

pub fn log(level: component_host::LogLevel, message: impl AsRef<str>) {
    component_host::log(level, message.as_ref());
}

/// Publish this indexer component's `log` to [`crate::log`].
///
/// [`crate::scryer_indexer_component_main!`] calls this; indexer plugins that
/// call [`log`] directly never need it.
///
/// It exists for exactly the reason [`install_config_get`] does, one dependency
/// further out. `newznab-common` and the other shared search crates are linked
/// into indexer components *and* — through providers like amenzb — into family
/// components, which serve `scryer:host/services` and no indexer world at all.
/// A direct call to [`log`] from shared code would leave every one of those
/// artifacts importing `scryer:indexer/host`, and a named import is kept alive
/// by the linker whether or not the branch runs. Behind this hook, shared code
/// calls [`crate::log::log`], an indexer routes it here, a family component
/// routes it to stderr, and neither names the other's world.
pub fn install_log() {
    fn hook(level: crate::log::LogLevel, message: &str) {
        component_host::log(host_log_level(level), message);
    }

    crate::log::install_log(hook);
}

/// The PDK's world-agnostic level as the indexer world declares it.
///
/// The two enums have the same five cases by construction, so this is total and
/// lossless; a new case on either side is a compile error here rather than a
/// silently downgraded diagnostic.
fn host_log_level(level: crate::log::LogLevel) -> component_host::LogLevel {
    match level {
        crate::log::LogLevel::Trace => component_host::LogLevel::Trace,
        crate::log::LogLevel::Debug => component_host::LogLevel::Debug,
        crate::log::LogLevel::Info => component_host::LogLevel::Info,
        crate::log::LogLevel::Warn => component_host::LogLevel::Warn,
        crate::log::LogLevel::Error => component_host::LogLevel::Error,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrategyEventEmitError {
    NoActivePlan,
    Closed,
}

impl From<component_host::StrategyEventError> for StrategyEventEmitError {
    fn from(value: component_host::StrategyEventError) -> Self {
        match value {
            component_host::StrategyEventError::NoActivePlan => Self::NoActivePlan,
            component_host::StrategyEventError::Closed => Self::Closed,
        }
    }
}

pub async fn emit_strategy_event(
    event: &PluginSearchStrategyEvent,
) -> Result<(), StrategyEventEmitError> {
    let encoded = serde_json::to_vec(event).map_err(|_| StrategyEventEmitError::Closed)?;
    component_host::emit_strategy_event(encoded)
        .await
        .map_err(StrategyEventEmitError::from)
}

/// Convert an ordinary plugin handler error into the SDK's stable typed result
/// shape. The component invocation itself still succeeds in this case.
pub fn to_plugin_result<T>(result: Result<T, crate::Error>) -> PluginResult<T> {
    match result {
        Ok(value) => PluginResult::Ok(value),
        Err(error) => {
            if let Some(structured) = error.downcast_ref::<StructuredPluginError>() {
                return PluginResult::Err(structured.0.clone());
            }
            PluginResult::Err(PluginError {
                code: PluginErrorCode::Temporary,
                public_message: "indexer component failed".to_string(),
                debug_message: Some(error.to_string()),
                retry_after_seconds: None,
                details: None,
            })
        }
    }
}

/// Preserve an explicit SDK error through the component result adapter.
#[derive(Debug)]
pub struct StructuredPluginError(PluginError);

impl StructuredPluginError {
    pub fn plugin_error(&self) -> &PluginError {
        &self.0
    }
}

impl std::fmt::Display for StructuredPluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.public_message)
    }
}

impl std::error::Error for StructuredPluginError {}

pub fn structured_plugin_error(error: PluginError) -> crate::Error {
    crate::Error::new(StructuredPluginError(error))
}

pub fn action_unsupported(action: &str) -> PluginError {
    PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: format!("this indexer does not support the '{action}' action"),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

pub fn descriptor_bytes(descriptor: scryer_plugin_sdk::PluginDescriptor) -> Vec<u8> {
    // Components use UTF-8 JSON for descriptors and typed operation payloads.
    // The SDK models contain JSON-oriented omission rules and arbitrary JSON
    // action values, so a positional codec cannot round-trip them safely.
    serde_json::to_vec(&descriptor).unwrap_or_default()
}

pub fn strategy_plan_parallelism(descriptor: &scryer_plugin_sdk::PluginDescriptor) -> Option<u32> {
    let scryer_plugin_sdk::ProviderDescriptor::Indexer(indexer) = &descriptor.provider else {
        return None;
    };
    let capability = indexer.strategy_plan.as_ref()?;
    (capability.version == 1 && capability.max_parallel_strategies > 0)
        .then_some(capability.max_parallel_strategies)
}

pub async fn dispatch_search<H, F>(encoded: Vec<u8>, handler: H) -> Result<Vec<u8>, InvocationError>
where
    H: FnOnce(PluginSearchRequest) -> F,
    F: Future<Output = Result<PluginSearchResponse, crate::Error>>,
{
    let request = serde_json::from_slice::<PluginSearchRequest>(&encoded)
        .map_err(|_| InvocationError::InvalidResponse)?;
    serde_json::to_vec(&to_plugin_result(handler(request).await))
        .map_err(|_| InvocationError::Failed)
}

pub async fn dispatch_search_plan<H, F>(
    encoded: Vec<u8>,
    max_parallel_strategies: u32,
    handler: H,
) -> Result<Vec<u8>, InvocationError>
where
    H: Fn(PluginSearchRequest) -> F + Clone,
    F: Future<Output = Result<PluginSearchResponse, crate::Error>>,
{
    let plan = serde_json::from_slice::<PluginSearchPlanRequest>(&encoded)
        .map_err(|_| InvocationError::InvalidResponse)?;
    let summary = execute_search_plan(plan, max_parallel_strategies, handler, |event| async move {
        emit_strategy_event(&event)
            .await
            .map_err(|_| InvocationError::Cancelled)
    })
    .await?;
    serde_json::to_vec(&summary).map_err(|_| InvocationError::Failed)
}

async fn execute_search_plan<H, F, E, EF>(
    plan: PluginSearchPlanRequest,
    max_parallel_strategies: u32,
    handler: H,
    emit: E,
) -> Result<PluginSearchPlanSummary, InvocationError>
where
    H: Fn(PluginSearchRequest) -> F + Clone,
    F: Future<Output = Result<PluginSearchResponse, crate::Error>>,
    E: Fn(PluginSearchStrategyEvent) -> EF,
    EF: Future<Output = Result<(), InvocationError>>,
{
    if plan.plan_id.trim().is_empty()
        || plan
            .strategies
            .iter()
            .any(|strategy| strategy.strategy_id.trim().is_empty())
        || plan
            .strategies
            .iter()
            .map(|strategy| strategy.strategy_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != plan.strategies.len()
    {
        return Err(InvocationError::InvalidResponse);
    }

    let plan_id = plan.plan_id;
    let parallelism = usize::try_from(max_parallel_strategies.max(1)).unwrap_or(1);
    let futures = plan.strategies.into_iter().map(|strategy| {
        let handler = handler.clone();
        async move {
            PluginSearchStrategyEvent {
                strategy_id: strategy.strategy_id,
                result: to_plugin_result(handler(strategy.request).await),
            }
        }
    });
    let mut events = stream::iter(futures).buffer_unordered(parallelism);
    let mut emitted_strategy_ids = Vec::new();
    while let Some(event) = events.next().await {
        let strategy_id = event.strategy_id.clone();
        emit(event).await?;
        emitted_strategy_ids.push(strategy_id);
    }

    Ok(PluginSearchPlanSummary {
        plan_id,
        emitted_strategy_ids,
    })
}

pub async fn dispatch_action<H, F>(encoded: Vec<u8>, handler: H) -> Result<Vec<u8>, InvocationError>
where
    H: FnOnce(PluginActionRequest) -> F,
    F: Future<Output = Result<PluginActionResponse, crate::Error>>,
{
    let request = serde_json::from_slice::<PluginActionRequest>(&encoded)
        .map_err(|_| InvocationError::InvalidResponse)?;
    serde_json::to_vec(&to_plugin_result(handler(request).await))
        .map_err(|_| InvocationError::Failed)
}

pub fn unsupported_action_response(encoded: Vec<u8>) -> Result<Vec<u8>, InvocationError> {
    let request = serde_json::from_slice::<PluginActionRequest>(&encoded)
        .map_err(|_| InvocationError::InvalidResponse)?;
    serde_json::to_vec(&PluginResult::<PluginActionResponse>::Err(
        action_unsupported(&request.action),
    ))
    .map_err(|_| InvocationError::Failed)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    use futures_util::future::poll_fn;
    use scryer_plugin_sdk::PluginSearchStrategyRequest;

    use super::*;

    #[derive(Default)]
    struct PlanState {
        active: usize,
        max_active: usize,
        timeline: Vec<String>,
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => {}
            }
        }
    }

    fn request(query: String) -> PluginSearchRequest {
        PluginSearchRequest {
            query,
            ids: Default::default(),
            facet: None,
            category: None,
            categories: vec![],
            limit: 100,
            season: None,
            episode: None,
            absolute_episode: None,
            tagged_aliases: vec![],
            context: None,
        }
    }

    #[test]
    fn every_world_agnostic_level_has_an_indexer_world_level() {
        for (level, expected) in [
            (crate::log::LogLevel::Trace, component_host::LogLevel::Trace),
            (crate::log::LogLevel::Debug, component_host::LogLevel::Debug),
            (crate::log::LogLevel::Info, component_host::LogLevel::Info),
            (crate::log::LogLevel::Warn, component_host::LogLevel::Warn),
            (crate::log::LogLevel::Error, component_host::LogLevel::Error),
        ] {
            // The mapping is total by construction; this pins that the two
            // enums stay the same five cases in the same order, so a level
            // added to one side cannot silently arrive at the host as
            // something else.
            assert_eq!(host_log_level(level), expected);
        }
    }

    #[test]
    fn strategy_plan_uses_a_rolling_parallelism_window() {
        let state = Rc::new(RefCell::new(PlanState::default()));
        let plan = PluginSearchPlanRequest {
            plan_id: "plan".into(),
            strategies: (0..6)
                .map(|index| PluginSearchStrategyRequest {
                    strategy_id: format!("strategy-{index}"),
                    labels: vec![],
                    request: request(index.to_string()),
                })
                .collect(),
        };

        let handler_state = Rc::clone(&state);
        let handler = move |request: PluginSearchRequest| {
            let state = Rc::clone(&handler_state);
            let mut started = false;
            poll_fn(move |context| {
                let mut state = state.borrow_mut();
                if !started {
                    started = true;
                    state.active += 1;
                    state.max_active = state.max_active.max(state.active);
                    state.timeline.push(format!("start:{}", request.query));
                    context.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    state.active -= 1;
                    state.timeline.push(format!("finish:{}", request.query));
                    Poll::Ready(Ok(PluginSearchResponse::default()))
                }
            })
        };

        let emit_state = Rc::clone(&state);
        let summary = block_on(execute_search_plan(plan, 4, handler, move |event| {
            emit_state
                .borrow_mut()
                .timeline
                .push(format!("emit:{}", event.strategy_id));
            std::future::ready(Ok(()))
        }))
        .unwrap();

        let state = state.borrow();
        assert_eq!(state.max_active, 4);
        assert_eq!(summary.emitted_strategy_ids.len(), 6);
        assert!(
            state.timeline[..4]
                .iter()
                .all(|entry| entry.starts_with("start:"))
        );
        let first_finish = state
            .timeline
            .iter()
            .position(|entry| entry.starts_with("finish:"))
            .unwrap();
        let fifth_start = state
            .timeline
            .iter()
            .position(|entry| entry == "start:4")
            .unwrap();
        assert!(fifth_start > first_finish);
    }
}
