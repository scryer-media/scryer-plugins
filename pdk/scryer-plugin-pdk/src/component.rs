//! WASIp2 component bindings for first-party indexer plugins.
//!
//! Indexers own their upstream pacing, quotas, retries, and fanout. The host
//! provides one cancellable, policy-checked HTTP attempt at a time. Ordinary
//! plugin failures stay in the SDK's typed [`PluginResult`]; this component
//! ABI's `invocation-error` is reserved for malformed ABI payloads and other
//! faults that prevent a typed result from being produced.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use scryer_plugin_sdk::command::{PluginActionRequest, PluginActionResponse};
use scryer_plugin_sdk::{
    PluginError, PluginErrorCode, PluginResult, PluginSearchPlanRequest, PluginSearchPlanSummary,
    PluginSearchRequest, PluginSearchResponse, PluginSearchStrategyEvent,
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

/// The family-neutral runtime surface, re-exported unchanged.
///
/// These types and functions used to be declared here, against
/// `scryer:indexer/host@1.1.0` directly. They now live in [`crate::runtime`],
/// which dispatches through whichever capability world the running guest
/// installed, so that one rate gate, one quota window and one deadline model
/// serve indexer components and family components alike. Indexer plugins keep
/// calling `component::http`, `component::StartRateGate` and the rest exactly
/// as before; only the layer underneath changed.
///
/// `config_get`, `provider_profile`, `state_get` and `state_cas` are here for a
/// sharper reason than tidiness. `newznab-common` calls all four — the API key
/// lookup, the provider profile, and the hit-budget window it keeps in plugin
/// state — and that crate is linked into a subtitle component as well as the
/// five indexer ones. A direct `component_host::state_get` in shared code puts
/// a live `scryer:indexer/host@1.1.0` import in the subtitle artifact, whose
/// host serves no such interface, and the component then fails to instantiate
/// rather than failing to work. Routed through the installed backend, the same
/// call reaches `scryer:indexer/host` from an indexer and
/// `scryer:runtime/host` from a family component.
pub use crate::runtime::{
    CooldownGate, DeadlineExceeded, HostError, PluginHttpFieldsResponse, QuotaExhausted,
    StartRateGate, WindowQuota, config_get, deadline_deferred_error, http, http_fields,
    monotonic_now_ms, operation_deadline_monotonic_ms, provider_profile, provider_profile_bytes,
    sleep, sleep_until_deadline, state_cas, state_get, wall_now_ms,
};

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

/// The `scryer:indexer/host@1.1.0` backend for [`crate::runtime`].
///
/// Every method is the plain imported call. It exists so that shared crates —
/// `newznab-common` above all, which is linked into indexer components *and*,
/// through amenzb, into a subtitle component — can reach these capabilities
/// without naming a world. See [`crate::runtime`] for why a named import is
/// not something a branch can avoid.
struct IndexerHost;

impl crate::runtime::HostRuntime for IndexerHost {
    fn http_fields(
        &self,
        request: scryer_plugin_sdk::host::PluginHttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<PluginHttpFieldsResponse, HostError>> + '_>> {
        Box::pin(async move {
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

    fn sleep(&self, duration_ms: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(component_host::sleep(duration_ms))
    }

    fn monotonic_now_ms(&self) -> u64 {
        component_host::monotonic_now_ms()
    }

    fn operation_deadline_monotonic_ms(&self) -> u64 {
        component_host::operation_deadline_monotonic_ms()
    }

    fn wall_now_ms(&self) -> u64 {
        component_host::wall_now_ms()
    }

    fn config_get(&self, key: &str) -> Option<String> {
        component_host::config_get(key)
    }

    fn provider_profile_bytes(&self) -> Option<Vec<u8>> {
        component_host::provider_profile()
    }

    fn state_get(&self, key: &str) -> Option<Vec<u8>> {
        component_host::state_get(key)
    }

    fn state_cas(&self, key: &str, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> bool {
        component_host::state_cas(key, expected, replacement)
    }

    fn log(&self, level: crate::log::LogLevel, message: &str) {
        component_host::log(host_log_level(level), message);
    }
}

static INDEXER_HOST: IndexerHost = IndexerHost;

/// Publish `scryer:indexer/host@1.1.0` as this guest's runtime backend.
///
/// [`crate::scryer_indexer_component_main!`] calls this at the top of every
/// world export, for the same reason it calls [`install_config_get`] and
/// [`install_log`]: a component instance is created per invocation and starts
/// with an empty registry.
pub fn install_indexer_runtime() {
    crate::runtime::install_host_runtime(&INDEXER_HOST);
}

/// Publish this indexer component's `config-get` to [`crate::config`].
///
/// [`crate::scryer_indexer_component_main!`] calls this; indexer plugins that
/// call [`config_get`] directly never need it.
///
/// The registry itself lives in [`crate::runtime`], because the shared
/// [`crate::config`] shim has to reach *two* different worlds — the indexer
/// host here, and `scryer:host/services` in [`crate::host`] — and both are
/// `wasm32-wasip2`, so the build target cannot tell them apart. If the shim
/// called [`config_get`] directly, every family component would keep a live
/// `scryer:indexer/host` import that its host does not serve, and the artifact
/// would fail to instantiate. Behind a hook, an unused import is linked out.
pub fn install_config_get() {
    fn hook(key: &str) -> Option<String> {
        component_host::config_get(key)
    }

    crate::runtime::install_config_get_hook(hook);
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
    fn repeated_response_fields_survive_the_list_shaped_response() {
        let response = PluginHttpFieldsResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "text/html".to_string()),
                ("Set-Cookie".to_string(), "session=one; Path=/".to_string()),
                ("set-cookie".to_string(), "csrf=two; Path=/".to_string()),
            ],
            body: Vec::new(),
        };

        assert_eq!(
            response.field_values("set-cookie").collect::<Vec<_>>(),
            vec!["session=one; Path=/", "csrf=two; Path=/"]
        );
        assert_eq!(
            response.field_values("content-type").collect::<Vec<_>>(),
            vec!["text/html"]
        );
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
