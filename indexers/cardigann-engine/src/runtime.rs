use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Datelike, Utc};
use html5ever::{LocalName, QualName, ns};
use roxmltree::{Document as XmlDocument, Node as XmlNode};
use scraper::{ElementRef, Html, Node, Selector, StrTendril};
use scryer_plugin_pdk::{codes, component, plugin_error};
use scryer_plugin_sdk::host::PluginHttpRequest;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, ConfigFieldValueSource, IndexerProtocol, IndexerSourceKind,
    PluginError, PluginErrorCode, PluginSearchRequest, PluginSearchResponse, PluginSearchResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::definition::{
    COMPILED_IR_VERSION, CompiledIr, Definition, DownloadSelector, LoginBlock, RequestBlock,
    ResponseType, ScalarMap, SearchPath, SelectorField, scalar_to_string,
};
use crate::filters::{apply_filters_with_encoding, normalize_unknown_date};
use crate::template::{Variables, render, render_search_path};

mod nab;
#[cfg(test)]
mod nab_tests;

#[derive(Debug)]
pub enum Operation {
    TestConnection,
    Search(Box<PluginSearchRequest>),
    Grab(String),
}

const MAX_OPERATION_STEPS: usize = 64;
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Bound on compare-and-swap retries when persisting the session, so a hot
/// state key can never spin a component invocation indefinitely.
const STATE_WRITE_ATTEMPTS: usize = 8;

#[derive(Debug, Clone)]
pub enum EngineAction {
    CheckCaptcha,
    Grab(String),
}

/// Narrow host boundary for the operation driver. Production drives it through
/// the WASIp2 component imports; tests inject this boundary to keep tracker I/O
/// local. Only the driver touches it — the flow state machine below
/// (`begin`/`resume`) stays pure and host-independent.
pub trait EngineHost {
    /// One host-policy-checked HTTP attempt, with repeated response header
    /// fields preserved so the engine can own its own cookie jar.
    ///
    /// A failed attempt is already classified, so the tracker being
    /// unreachable or timing out keeps its code instead of turning generic.
    async fn http(&mut self, request: PluginHttpRequest)
    -> Result<PluginHttpResponse, EngineError>;
    async fn state_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String>;
    async fn state_set(&mut self, key: &str, value: Vec<u8>) -> Result<(), String>;
    /// UTC Unix milliseconds. Only the persisted session's expiry uses it;
    /// request pacing goes through [`EngineHost::pace_request`], which the
    /// component backs with the host's monotonic clock.
    async fn wall_now_millis(&mut self) -> Result<u64, String>;
    /// Wait until this component instance may start its next tracker request.
    async fn pace_request(&mut self, state_key: &str, delay: Duration) -> Result<(), String>;
    /// Write one line of the debug-level `http_trace` every indexer plugin
    /// emits around its tracker requests. The line is already masked.
    fn trace(&mut self, _line: &str) {}
}

/// The plugin name the `http_trace` lines carry.
const TRACE_PLUGIN_ID: &str = "cardigann";

/// The tracker session this configured indexer carries between operations.
///
/// Request pacing is no longer part of it: under the component ABI that is a
/// monotonic, instance-wide start gate ([`EngineHost::pace_request`]) rather
/// than a wall-clock timestamp the engine has to persist itself.
#[derive(Debug, Default, Serialize, Deserialize)]
struct EngineSession {
    #[serde(default)]
    cookies: BTreeMap<String, String>,
    #[serde(default)]
    expires_at_millis: Option<u64>,
}

/// One tracker response as the engine consumes it: single-valued fields in
/// `headers`, and every `Set-Cookie` field kept separately and in order so the
/// cookie jar sees all of them.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginHttpResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub set_cookie_headers: Vec<String>,
    #[serde(default)]
    pub body: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ResumeInput {
    Http(PluginHttpResponse),
    ManualInteraction(BTreeMap<String, String>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Step {
    NeedHttp {
        request: PluginHttpRequest,
        continuation: Vec<u8>,
    },
    NeedManualInteraction {
        prompt: String,
        fields: Vec<ConfigFieldDef>,
        continuation: Vec<u8>,
    },
    Complete {
        output: Value,
    },
    /// A failure the tracker or the definition caused — a refused login, an
    /// HTTP error, a page the definition cannot read — carried to the
    /// component boundary as the SDK's typed error instead of a generic one.
    Rejected {
        error: PluginError,
    },
}

/// Why an engine operation did not complete.
#[derive(Debug)]
pub enum EngineError {
    /// An engine, transport or definition failure. The component reports it
    /// as its generic error, with this text as the debug detail.
    Message(String),
    /// A typed SDK error whose `public_message` is safe to show the operator.
    Plugin(PluginError),
}

impl From<String> for EngineError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Plugin(error) => formatter.write_str(&error.public_message),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum StoredOperation {
    TestConnection,
    Search(Box<PluginSearchRequest>),
    Grab(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Context {
    definition: Definition,
    config: BTreeMap<String, String>,
    cookies: BTreeMap<String, String>,
    operation: StoredOperation,
    #[serde(default)]
    search_path: usize,
    #[serde(default)]
    results: Vec<PluginSearchResult>,
    #[serde(default)]
    variables: Variables,
    #[serde(default)]
    grab_before_body: Option<Vec<u8>>,
    #[serde(default)]
    grab_selector_page_body: Option<Vec<u8>>,
    #[serde(default)]
    grab_selector_index: usize,
    #[serde(default)]
    relogin_attempts: u8,
    #[serde(default)]
    redirect_hops: u8,
    #[serde(default)]
    current_request: Option<RequestState>,
    #[serde(default)]
    seen_get_urls: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RequestState {
    url: String,
    method: String,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", content = "context", rename_all = "snake_case")]
enum Continuation {
    LoginLanding(Context),
    LoginSubmit(Context),
    SimpleCaptcha(Context),
    CaptchaImage(Context),
    ManualCaptcha(Context),
    TestConnection(Context),
    SearchResponse(Context),
    GrabResolveBefore(Context),
    GrabBeforeResponse(Context),
    GrabPage(Context),
    GrabDownload(Context),
}

impl Continuation {
    fn context_mut(&mut self) -> &mut Context {
        match self {
            Self::LoginLanding(context)
            | Self::LoginSubmit(context)
            | Self::SimpleCaptcha(context)
            | Self::CaptchaImage(context)
            | Self::ManualCaptcha(context)
            | Self::TestConnection(context)
            | Self::SearchResponse(context)
            | Self::GrabResolveBefore(context)
            | Self::GrabBeforeResponse(context)
            | Self::GrabPage(context)
            | Self::GrabDownload(context) => context,
        }
    }
}

pub fn begin(
    compiled_ir: String,
    operation: Operation,
    config: BTreeMap<String, String>,
) -> Result<Step, EngineError> {
    let definition = parse_compiled_ir(&compiled_ir)?;
    let stored_operation = match operation {
        Operation::TestConnection => StoredOperation::TestConnection,
        Operation::Search(request) => StoredOperation::Search(request),
        Operation::Grab(url) => StoredOperation::Grab(url),
    };
    // The reference Cardigann engine skips a search whose criteria use an id
    // the definition's `caps.modes` does not list. Without that gate an id-only
    // strategy renders an empty keyword search on every definition that cannot
    // read the id, and the tracker's front page comes back as results.
    if let StoredOperation::Search(request) = &stored_operation
        && (!nab::request_is_supported(&definition, request)?
            || !id_search_is_supported(&definition, request))
    {
        return Ok(Step::Complete {
            output: serde_json::to_value(PluginSearchResponse::default())
                .map_err(|error| format!("could not encode search response: {error}"))?,
        });
    }
    let cookies = initial_cookies(&definition, &config);
    let mut variables = base_variables(&definition, &config);
    if let StoredOperation::Grab(url) = &stored_operation {
        add_uri_variables(&mut variables, url, ".DownloadUri");
    }
    let context = Context {
        definition: definition.clone(),
        config,
        cookies,
        operation: stored_operation,
        search_path: 0,
        results: Vec::new(),
        variables,
        grab_before_body: None,
        grab_selector_page_body: None,
        grab_selector_index: 0,
        relogin_attempts: 0,
        redirect_hops: 0,
        current_request: None,
        seen_get_urls: BTreeSet::new(),
    };
    begin_after_optional_login(&definition, context)
}

pub async fn search_with_host<H: EngineHost>(
    host: &mut H,
    definition: Definition,
    request: PluginSearchRequest,
    config: BTreeMap<String, String>,
) -> Result<PluginSearchResponse, EngineError> {
    let output = drive_operation(
        host,
        definition,
        Operation::Search(Box::new(request)),
        config,
        false,
    )
    .await?;
    serde_json::from_value(output).map_err(|error| {
        EngineError::Message(format!("search produced an invalid response: {error}"))
    })
}

pub async fn action_with_host<H: EngineHost>(
    host: &mut H,
    definition: Definition,
    action: EngineAction,
    config: BTreeMap<String, String>,
) -> Result<Value, EngineError> {
    let wants_captcha = matches!(action, EngineAction::CheckCaptcha);
    let operation = match action {
        EngineAction::CheckCaptcha => Operation::TestConnection,
        EngineAction::Grab(url) => Operation::Grab(url),
    };
    drive_operation(host, definition, operation, config, wants_captcha).await
}

pub async fn search(
    definition: Definition,
    request: PluginSearchRequest,
    config: BTreeMap<String, String>,
) -> Result<PluginSearchResponse, EngineError> {
    let mut host = ComponentEngineHost;
    search_with_host(&mut host, definition, request, config).await
}

pub async fn action(
    definition: Definition,
    action: EngineAction,
    config: BTreeMap<String, String>,
) -> Result<Value, EngineError> {
    let mut host = ComponentEngineHost;
    action_with_host(&mut host, definition, action, config).await
}

/// The production boundary: Scryer's WASIp2 indexer component imports.
struct ComponentEngineHost;

impl EngineHost for ComponentEngineHost {
    async fn http(
        &mut self,
        request: PluginHttpRequest,
    ) -> Result<PluginHttpResponse, EngineError> {
        // The list-shaped response keeps every `Set-Cookie` field the tracker
        // sent, which is what lets the cookie jar below live in the guest.
        let response = component::http_fields(request)
            .await
            .map_err(host_http_error)?;
        let set_cookie_headers = response
            .field_values("set-cookie")
            .map(str::to_string)
            .collect();
        let headers = response
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("set-cookie"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Ok(PluginHttpResponse {
            status: response.status,
            headers,
            set_cookie_headers,
            body: response.body,
        })
    }

    async fn state_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(component::state_get(key))
    }

    async fn state_set(&mut self, key: &str, value: Vec<u8>) -> Result<(), String> {
        // The component host only offers compare-and-swap. One configured
        // indexer can run several operations at once, so retry until this
        // write lands rather than clobbering a concurrent reader's expectation.
        for _ in 0..STATE_WRITE_ATTEMPTS {
            let expected = component::state_get(key);
            if component::state_cas(key, expected, Some(value.clone())) {
                return Ok(());
            }
        }
        Err(format!(
            "could not persist Cardigann session state `{key}` after {STATE_WRITE_ATTEMPTS} attempts"
        ))
    }

    async fn wall_now_millis(&mut self) -> Result<u64, String> {
        Ok(component::wall_now_ms())
    }

    async fn pace_request(&mut self, state_key: &str, delay: Duration) -> Result<(), String> {
        let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        if delay_ms == 0 {
            return Ok(());
        }
        // A plugin-owned, monotonic, instance-wide start gate: it spaces the
        // definition's `requestdelay` across every concurrent guest future,
        // and refuses a wait the operation deadline cannot absorb.
        component::StartRateGate::new(format!("{state_key}/start-rate"), 1, delay_ms)
            .acquire()
            .await
            .map_err(|wait| {
                format!(
                    "tracker request pacing needs {} ms, which exceeds the remaining operation budget",
                    wait.retry_after_ms
                )
            })
    }

    fn trace(&mut self, line: &str) {
        component::log(component::LogLevel::Debug, line);
    }
}

/// A failed host HTTP attempt as the PDK classifies it: an unreachable
/// tracker is `UpstreamUnavailable`, a timeout `Temporary`, each with the
/// PDK's neutral sentence and the host's text as the debug detail.
fn host_http_error(error: component::HostError) -> EngineError {
    EngineError::Plugin(scryer_plugin_pdk::error::plugin_error_from(
        &scryer_plugin_pdk::Error::new(error),
    ))
}

async fn drive_operation<H: EngineHost>(
    host: &mut H,
    definition: Definition,
    operation: Operation,
    mut config: BTreeMap<String, String>,
    wants_captcha: bool,
) -> Result<Value, EngineError> {
    let state_key = session_state_key(&definition, &config).map_err(|error| {
        rejection_error(
            PluginErrorCode::InvalidConfig,
            "The indexer has no valid tracker URL.",
            error,
        )
    })?;
    let mut session: EngineSession = host
        .state_get(&state_key)
        .await?
        .map(|value| serde_json::from_slice(&value))
        .transpose()
        .map_err(|error| format!("invalid persisted Cardigann session: {error}"))?
        .unwrap_or_default();
    let now = host.wall_now_millis().await?;
    if session
        .expires_at_millis
        .is_some_and(|expires| expires <= now)
    {
        session.cookies.clear();
        session.expires_at_millis = None;
    }
    if let Some(cookie) = config.get("cookie") {
        merge_configured_cookies(&mut session.cookies, cookie);
    }
    if !session.cookies.is_empty() {
        config.insert("cookie".to_string(), cookie_header(&session.cookies));
    }
    if let Some(answer) = config
        .get("cardigannCaptcha")
        .or_else(|| config.get("CAPTCHA"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
    {
        config.insert("CAPTCHA".to_string(), answer);
    }
    if wants_captcha {
        // A login without a CAPTCHA asks for nothing; the host shows nothing.
        let Some(captcha) = definition
            .login
            .as_ref()
            .and_then(|login| login.captcha.as_ref())
        else {
            return Ok(serde_json::json!({}));
        };
        if !captcha.captcha_type.eq_ignore_ascii_case("image") {
            return Err(rejection_error(
                PluginErrorCode::Unsupported,
                "The tracker uses a kind of CAPTCHA the plugin cannot show.",
                format!(
                    "unsupported Cardigann CAPTCHA type `{}`",
                    captcha.captcha_type
                ),
            ));
        }
        config.remove("CAPTCHA");
    }
    let compiled = serde_json::to_string(&CompiledIr {
        ir_version: COMPILED_IR_VERSION,
        definition: definition.clone(),
    })
    .map_err(|error| format!("could not encode the Cardigann compiled IR: {error}"))?;
    let mut step = begin(compiled, operation, config.clone())?;
    let min_delay = configured_request_delay(&definition, &config);
    let mut last_response = None;
    for _ in 0..MAX_OPERATION_STEPS {
        match step {
            Step::NeedHttp {
                request,
                continuation,
            } => {
                host.pace_request(&state_key, min_delay)
                    .await
                    .map_err(|error| {
                        rejection_error(
                            PluginErrorCode::RateLimited,
                            "The tracker's request spacing does not fit in the time left for \
                             this request. Try again shortly.",
                            error,
                        )
                    })?;
                let trace = format!(
                    "plugin={TRACE_PLUGIN_ID} definition={} method={} url={}",
                    definition.id,
                    request.method.as_deref().unwrap_or("GET"),
                    mask_setting_values(&definition, &config, &request.url)
                );
                host.trace(&format!("http_trace {trace}"));
                let response = match host.http(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        let error_text =
                            mask_setting_values(&definition, &config, &error.to_string());
                        host.trace(&format!("http_trace_error {trace} error={error_text}"));
                        return Err(error);
                    }
                };
                host.trace(&format!(
                    "http_trace_response {trace} status={}",
                    response.status
                ));
                let cookies_changed = merge_response_cookies(&mut session.cookies, &response);
                if cookies_changed {
                    let response_now = host.wall_now_millis().await?;
                    session.expires_at_millis = (!session.cookies.is_empty()).then_some(
                        response_now.saturating_add(SESSION_LIFETIME.as_millis() as u64),
                    );
                    persist_session(host, &state_key, &session)
                        .await
                        .map_err(session_save_error)?;
                }
                last_response = Some(response.clone());
                step = resume(&continuation, ResumeInput::Http(response))?;
            }
            Step::NeedManualInteraction { .. } if wants_captcha => {
                let response = last_response.ok_or_else(|| {
                    "CAPTCHA challenge did not produce an image response".to_string()
                })?;
                let content_type = response
                    .headers
                    .iter()
                    .find_map(|(name, value)| {
                        name.eq_ignore_ascii_case("content-type")
                            .then_some(value.clone())
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                return Ok(serde_json::json!({
                    "captchaRequest": {
                        "type": "image",
                        "contentType": content_type,
                        "imageData": BASE64.encode(response.body),
                    }
                }));
            }
            Step::NeedManualInteraction { continuation, .. } => {
                let answer = config
                    .get("CAPTCHA")
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        EngineError::Plugin(
                            plugin_error(
                                codes::CAPTCHA_REQUIRED,
                                "The tracker asks for a CAPTCHA. Solve it from the indexer's \
                                 settings, then try again.",
                            )
                            .debug(
                                "tracker CAPTCHA requires cardigannCaptcha (or CAPTCHA) \
                                 configuration",
                            )
                            .build(),
                        )
                    })?
                    .clone();
                step = resume(
                    &continuation,
                    ResumeInput::ManualInteraction(BTreeMap::from([(
                        "CAPTCHA".to_string(),
                        answer,
                    )])),
                )?;
            }
            Step::Complete { output } => {
                persist_session(host, &state_key, &session)
                    .await
                    .map_err(session_save_error)?;
                return Ok(output);
            }
            Step::Rejected { error } => return Err(EngineError::Plugin(error)),
        }
    }
    Err(EngineError::Message(format!(
        "Cardigann operation exceeded the {MAX_OPERATION_STEPS}-step limit"
    )))
}

/// The tracker session could not be saved between requests.
fn session_save_error(debug: String) -> EngineError {
    rejection_error(
        PluginErrorCode::Temporary,
        "The tracker login could not be saved. Try again shortly.",
        debug,
    )
}

fn session_state_key(
    definition: &Definition,
    config: &BTreeMap<String, String>,
) -> Result<String, String> {
    let base = config
        .get("base_url")
        .or_else(|| definition.links.first())
        .ok_or_else(|| "definition has no base URL".to_string())?;
    let url = Url::parse(base).map_err(|error| format!("invalid base URL `{base}`: {error}"))?;
    let namespace = format!("{}-{}", definition.id, url.origin().ascii_serialization());
    Ok(format!(
        "cardigann-engine/session/{}",
        url::form_urlencoded::byte_serialize(namespace.as_bytes()).collect::<String>()
    ))
}

fn configured_request_delay(
    definition: &Definition,
    config: &BTreeMap<String, String>,
) -> Duration {
    let hint = config
        .get("requestDelay")
        .or_else(|| config.get("request_delay"))
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or_default();
    Duration::from_secs_f64(
        definition
            .request_delay
            .unwrap_or_default()
            .max(hint)
            .max(0.0),
    )
}

async fn persist_session<H: EngineHost>(
    host: &mut H,
    key: &str,
    session: &EngineSession,
) -> Result<(), String> {
    let value = serde_json::to_vec(session)
        .map_err(|error| format!("could not encode Cardigann session: {error}"))?;
    host.state_set(key, value).await
}

pub fn resume(continuation: &[u8], input: ResumeInput) -> Result<Step, EngineError> {
    let continuation: Continuation = serde_json::from_slice(continuation)
        .map_err(|error| format!("invalid Cardigann continuation: {error}"))?;
    match (continuation, input) {
        (Continuation::LoginLanding(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(&definition, &mut context, &response, true)? {
                return need_redirect(request, Continuation::LoginLanding(context));
            }
            // The landing page is the login form itself, so the definition's
            // error blocks — which often match a link back to it — only run on
            // the submit response.
            if !(200..300).contains(&response.status) {
                return Ok(login_http_failure(
                    response.status,
                    "login landing page",
                    PluginErrorCode::AuthFailed,
                ));
            }
            submit_form_login(context, &response)
        }
        (Continuation::SimpleCaptcha(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            if let Some(step) = http_status_failure(
                &context,
                &response,
                "simple CAPTCHA",
                codes::ENDPOINT_NOT_FOUND,
            ) {
                return Ok(step);
            }
            let selection =
                serde_json::from_str::<Value>(&decode_body(&context.definition, &response.body))
                    .map_err(|error| format!("invalid simple CAPTCHA response: {error}"))?
                    .pointer("/images/0/hash")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        "simple CAPTCHA response did not contain images[0].hash".to_string()
                    })?
                    .to_string();
            context
                .config
                .insert("simpleCaptchaSelection".to_string(), selection);
            let landing = context
                .grab_before_body
                .take()
                .ok_or_else(|| "simple CAPTCHA continuation lost its login page".to_string())?;
            submit_form_login(
                context,
                &PluginHttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    set_cookie_headers: Vec::new(),
                    body: landing,
                },
            )
        }
        (Continuation::LoginSubmit(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(&definition, &mut context, &response, true)? {
                return need_redirect(request, Continuation::LoginSubmit(context));
            }
            if let Some(step) = login_failure(&definition, &context, &response)? {
                return Ok(step);
            }
            continue_operation(&definition, context)
        }
        (Continuation::CaptchaImage(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            if let Some(step) = http_status_failure(
                &context,
                &response,
                "captcha image",
                codes::ENDPOINT_NOT_FOUND,
            ) {
                return Ok(step);
            }
            need_manual(
                "The tracker CAPTCHA image was fetched. Enter the value shown in the challenge."
                    .to_string(),
                vec![ConfigFieldDef {
                    key: "CAPTCHA".to_string(),
                    label: "Captcha".to_string(),
                    field_type: ConfigFieldType::String,
                    required: true,
                    default_value: None,
                    value_source: ConfigFieldValueSource::User,
                    role: None,
                    host_binding: None,
                    options: Vec::new(),
                    help_text: None,
                    ..Default::default()
                }],
                Continuation::ManualCaptcha(context),
            )
        }
        (Continuation::ManualCaptcha(mut context), ResumeInput::ManualInteraction(values)) => {
            let value = values
                .get("CAPTCHA")
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "captcha response is required".to_string())?;
            context.config.insert("CAPTCHA".to_string(), value.clone());
            let landing = context
                .grab_before_body
                .take()
                .ok_or_else(|| "captcha continuation lost its login page".to_string())?;
            submit_form_login(
                context,
                &PluginHttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    set_cookie_headers: Vec::new(),
                    body: landing,
                },
            )
        }
        (Continuation::TestConnection(mut context), ResumeInput::Http(response)) => {
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(
                &definition,
                &mut context,
                &response,
                definition.followredirect,
            )? {
                return need_redirect(request, Continuation::TestConnection(context));
            }
            if (200..300).contains(&response.status) {
                if let Some(selector_text) = definition
                    .login
                    .as_ref()
                    .and_then(|login| login.test.as_ref())
                    .and_then(|test| test.selector.as_deref())
                {
                    let selector = parse_selector(selector_text)?;
                    let mut document =
                        Html::parse_document(&decode_body(&definition, &response.body));
                    mark_contains(&mut document, &selector.needles);
                    if select_matching_element(&document.root_element(), &selector).is_none() {
                        return Ok(rejected(
                            login_refusal(&context).code,
                            "The tracker did not show a logged-in page. Check the indexer's \
                             credentials."
                                .to_string(),
                            format!("login test selector `{selector_text}` did not match"),
                        ));
                    }
                }
                Ok(Step::Complete {
                    output: serde_json::json!({
                        "message": "Connection successful",
                        "status": response.status,
                    }),
                })
            } else {
                Ok(http_status_failure(
                    &context,
                    &response,
                    "connection test",
                    codes::ENDPOINT_NOT_FOUND,
                )
                .expect("a non-2xx connection test response is a failure"))
            }
        }
        (Continuation::SearchResponse(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            let should_follow_redirect = definition.followredirect
                || definition
                    .search
                    .paths
                    .get(context.search_path)
                    .is_some_and(|path| path.follow_redirect);
            if let Some(request) =
                follow_redirect(&definition, &mut context, &response, should_follow_redirect)?
            {
                return need_redirect(request, Continuation::SearchResponse(context));
            }
            if let Some(step) = retry_login_if_needed(&definition, context.clone(), &response)? {
                return Ok(step);
            }
            if let Some(step) =
                http_status_failure(&context, &response, "search", codes::ENDPOINT_NOT_FOUND)
            {
                return Ok(step);
            }
            if let Some(step) = search_error_block(&definition, &context, &response)? {
                return Ok(step);
            }
            let path = definition
                .search
                .paths
                .get(context.search_path)
                .ok_or_else(|| "search continuation refers to a missing path".to_string())?;
            let parsed =
                match parse_search_response(&definition, path, &response, &mut context.variables) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        return Ok(rejected(
                            codes::INVALID_RESPONSE.into(),
                            "The tracker's search page could not be read with this definition."
                                .to_string(),
                            format!("Cardigann search response: {error}"),
                        ));
                    }
                };
            context.results.extend(parsed);
            context.search_path += 1;
            next_search(&definition, context)
        }
        (Continuation::GrabResolveBefore(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(
                &definition,
                &mut context,
                &response,
                definition.followredirect,
            )? {
                return need_redirect(request, Continuation::GrabResolveBefore(context));
            }
            if let Some(step) = retry_login_if_needed(&definition, context.clone(), &response)? {
                return Ok(step);
            }
            if let Some(step) = http_status_failure(
                &context,
                &response,
                "download details",
                codes::ITEM_NOT_FOUND,
            ) {
                return Ok(step);
            }
            let before = definition
                .download
                .as_ref()
                .and_then(|download| download.before.as_ref())
                .ok_or_else(|| "download before block disappeared".to_string())?;
            let mut resolved = before.clone();
            let selector = before
                .path_selector
                .as_ref()
                .ok_or_else(|| "download path selector disappeared".to_string())?;
            resolved.path = select_html_value(
                &definition,
                &response.body,
                selector,
                &context.variables,
                true,
            )?;
            let referer = grab_url(&context)?;
            let request = request_block_request(&definition, &resolved, &context, Some(&referer))?;
            need_http(request, Continuation::GrabBeforeResponse(context))
        }
        (Continuation::GrabBeforeResponse(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(
                &definition,
                &mut context,
                &response,
                definition.followredirect,
            )? {
                return need_redirect(request, Continuation::GrabBeforeResponse(context));
            }
            if let Some(step) = retry_login_if_needed(&definition, context.clone(), &response)? {
                return Ok(step);
            }
            if let Some(step) = http_status_failure(
                &context,
                &response,
                "download before request",
                codes::ITEM_NOT_FOUND,
            ) {
                return Ok(step);
            }
            context.grab_before_body = Some(response.body);
            continue_grab_after_before(context)
        }
        (Continuation::GrabPage(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(
                &definition,
                &mut context,
                &response,
                definition.followredirect,
            )? {
                return need_redirect(request, Continuation::GrabPage(context));
            }
            if let Some(step) = retry_login_if_needed(&definition, context.clone(), &response)? {
                return Ok(step);
            }
            if let Some(step) =
                http_status_failure(&context, &response, "download page", codes::ITEM_NOT_FOUND)
            {
                return Ok(step);
            }
            finish_grab(context, &response.body)
        }
        (Continuation::GrabDownload(mut context), ResumeInput::Http(response)) => {
            merge_response_cookies(&mut context.cookies, &response);
            let definition = context.definition.clone();
            if let Some(request) = follow_redirect(
                &definition,
                &mut context,
                &response,
                definition.followredirect,
            )? {
                return need_redirect(request, Continuation::GrabDownload(context));
            }
            if let Some(step) = retry_login_if_needed(&definition, context.clone(), &response)? {
                return Ok(step);
            }
            if let Some(step) = http_status_failure(
                &context,
                &response,
                "torrent download",
                codes::ITEM_NOT_FOUND,
            ) {
                return Ok(step);
            }
            if context.grab_selector_page_body.is_none()
                || !definition.test_link_torrent
                || response.body.first() == Some(&b'd')
            {
                complete_grab_response(&context, response)
            } else {
                next_grab_selector(context)
            }
        }
        (Continuation::ManualCaptcha(_), ResumeInput::Http(_)) => {
            Err("captcha expected manual input".to_string().into())
        }
        (Continuation::CaptchaImage(_), ResumeInput::ManualInteraction(_)) => {
            Err("captcha image expected an HTTP response".to_string().into())
        }
        (_, ResumeInput::ManualInteraction(_)) => {
            Err("flow expected an HTTP response".to_string().into())
        }
    }
}

fn begin_after_optional_login(
    definition: &Definition,
    context: Context,
) -> Result<Step, EngineError> {
    let Some(login) = definition.login.as_ref() else {
        return continue_operation(definition, context);
    };
    match login.method.to_ascii_lowercase().as_str() {
        "cookie" => continue_operation(definition, context),
        "form" => {
            let url = resolve_url(
                definition,
                &context,
                &render(&login.path, &context.variables)?,
                None,
            )?;
            let request = http_request(
                "GET",
                url,
                Vec::new(),
                login_headers(definition, &context, None)?,
            );
            need_http(request, Continuation::LoginLanding(context))
        }
        "post" | "get" | "oneurl" => direct_login(definition, login, context),
        method => Ok(rejected(
            PluginErrorCode::Unsupported,
            "This definition logs in a way the plugin does not support.".to_string(),
            format!("unsupported Cardigann login method `{method}`"),
        )),
    }
}

fn direct_login(
    definition: &Definition,
    login: &LoginBlock,
    context: Context,
) -> Result<Step, EngineError> {
    let method = login.method.to_ascii_uppercase();
    let mut path = render(&login.path, &context.variables)?;
    let mut inputs = render_map(&login.inputs, &context.variables)?;
    if method == "ONEURL"
        && let Some(index) = inputs.iter().position(|(name, _)| name == "oneurl")
    {
        path.push_str(&inputs.remove(index).1);
    }
    let actual_method = if method == "POST" { "POST" } else { "GET" };
    let mut url = resolve_url(definition, &context, &path, None)?;
    let body = if actual_method == "POST" {
        encoded_form_body(definition, &inputs).into_bytes()
    } else {
        append_query_encoded(definition, &mut url, &inputs, None);
        Vec::new()
    };
    let headers = login_headers(
        definition,
        &context,
        Some("application/x-www-form-urlencoded"),
    )?;
    need_http(
        http_request(actual_method, url, body, headers),
        Continuation::LoginSubmit(context),
    )
}

fn submit_form_login(
    mut context: Context,
    response: &PluginHttpResponse,
) -> Result<Step, EngineError> {
    let definition = context.definition.clone();
    let login = definition
        .login
        .as_ref()
        .ok_or_else(|| "login block disappeared".to_string())?;
    let body = decode_body(&definition, &response.body);
    let mut document = Html::parse_document(&body);
    mark_contains(&mut document, &login_needles(login, &context.variables));
    if !context.config.contains_key("simpleCaptchaSelection") {
        let selector = parse_selector(r#"script[src*="simpleCaptcha"]"#)?;
        if select_matching_element(&document.root_element(), &selector).is_some() {
            context.grab_before_body = Some(response.body.clone());
            let login_url = resolve_url(&definition, &context, &login.path, None)?;
            let captcha_url =
                resolve_url(&definition, &context, "simpleCaptcha.php?numImages=1", None)?;
            let mut headers = login_headers(&definition, &context, None)?;
            headers.insert("referer".to_string(), login_url);
            return need_http(
                http_request("GET", captcha_url, Vec::new(), headers),
                Continuation::SimpleCaptcha(context),
            );
        }
    }
    if let Some(captcha) = login.captcha.as_ref()
        && !context.config.contains_key("CAPTCHA")
    {
        context.grab_before_body = Some(response.body.clone());
        if captcha.captcha_type.eq_ignore_ascii_case("image") {
            let selector = parse_selector(&captcha.selector)?;
            if let Some(element) = select_matching_element(&document.root_element(), &selector)
                && let Some(source) = element.value().attr("src")
            {
                let landing_url = resolve_url(&definition, &context, &login.path, None)?;
                let image_url = resolve_url(&definition, &context, source, Some(&landing_url))?;
                let landing_origin = Url::parse(&landing_url)
                    .map_err(|error| format!("invalid login URL `{landing_url}`: {error}"))?
                    .origin();
                let image_origin = Url::parse(&image_url)
                    .map_err(|error| format!("invalid CAPTCHA image URL `{image_url}`: {error}"))?
                    .origin();
                if landing_origin == image_origin {
                    let mut headers = login_headers(&definition, &context, None)?;
                    headers.insert("referer".to_string(), landing_url);
                    return need_http(
                        http_request("GET", image_url, Vec::new(), headers),
                        Continuation::CaptchaImage(context),
                    );
                }
                return Ok(rejected(
                    PluginErrorCode::Unsupported,
                    "The tracker's CAPTCHA image is served from another site, which the plugin \
                     does not load."
                        .to_string(),
                    "cross-origin CAPTCHA image is blocked by the configured-origin policy"
                        .to_string(),
                ));
            }
        }
        return need_manual(
            "The tracker requires a captcha. Enter the value shown on its login page.".to_string(),
            vec![ConfigFieldDef {
                key: "CAPTCHA".to_string(),
                label: "Captcha".to_string(),
                field_type: ConfigFieldType::String,
                required: true,
                default_value: None,
                value_source: ConfigFieldValueSource::User,
                role: None,
                host_binding: None,
                options: Vec::new(),
                help_text: None,
                ..Default::default()
            }],
            Continuation::ManualCaptcha(context),
        );
    }

    let form_selector_text = login.form.as_deref().unwrap_or("form");
    let form_selector = parse_selector(form_selector_text)?;
    let Some(form) = document.select(&form_selector.css).next() else {
        return Ok(login_page_mismatch(format!(
            "login form selector `{form_selector_text}` did not match"
        )));
    };
    let input_selector = parse_selector("input")?;
    let mut inputs = Vec::new();
    for input in form.select(&input_selector.css) {
        let Some(name) = input.value().attr("name") else {
            continue;
        };
        if input.value().attr("disabled").is_some() {
            continue;
        }
        let input_type = input.value().attr("type").unwrap_or_default();
        if matches!(input_type, "checkbox" | "radio") && input.value().attr("checked").is_none() {
            continue;
        }
        inputs.push((
            name.to_string(),
            input.value().attr("value").unwrap_or_default().to_string(),
        ));
    }
    for (key, value) in render_map(&login.inputs, &context.variables)? {
        let input_key = if login.selectors {
            let selector = parse_selector(&key)?;
            let Some(name) = document
                .select(&selector.css)
                .next()
                .and_then(|element| element.value().attr("name"))
            else {
                return Ok(login_page_mismatch(format!(
                    "login input selector `{key}` did not match a named input"
                )));
            };
            name.to_string()
        } else {
            key
        };
        inputs.push((input_key, value));
    }
    for (key, selector) in &login.selector_inputs {
        let value = select_element_value(
            &definition,
            &form,
            selector,
            &context.variables,
            !selector.optional,
        )?;
        if !selector.optional || !value.is_empty() {
            inputs.push((key.clone(), value));
        }
    }
    let mut query_inputs = Vec::new();
    for (key, selector) in &login.get_selector_inputs {
        let value = select_element_value(
            &definition,
            &form,
            selector,
            &context.variables,
            !selector.optional,
        )?;
        if !selector.optional || !value.is_empty() {
            query_inputs.push((key.clone(), value));
        }
    }
    if let Some(selection) = context.config.get("simpleCaptchaSelection") {
        inputs.retain(|(name, _)| name != "captchaSelection" && name != "submitme");
        inputs.push(("captchaSelection".to_string(), selection.clone()));
        inputs.push(("submitme".to_string(), "X".to_string()));
    }
    if let Some(captcha) = login.captcha.as_ref()
        && let Some(value) = context.config.get("CAPTCHA")
    {
        let input = if login.selectors {
            let selector = parse_selector(&captcha.input)?;
            let Some(name) = document
                .select(&selector.css)
                .next()
                .and_then(|element| element.value().attr("name"))
            else {
                return Ok(login_page_mismatch(format!(
                    "login captcha selector `{}` did not match a named input",
                    captcha.input
                )));
            };
            name.to_string()
        } else {
            captcha.input.clone()
        };
        inputs.push((input, value.clone()));
    }
    let landing_url = resolve_url(&definition, &context, &login.path, None)?;
    let action = login
        .submit_path
        .clone()
        .or_else(|| form.value().attr("action").map(str::to_string))
        .unwrap_or_else(|| login.path.clone());
    let mut submit_url = resolve_url(&definition, &context, &action, Some(&landing_url))?;
    append_query_encoded(&definition, &mut submit_url, &query_inputs, None);
    let multipart = form
        .value()
        .attr("enctype")
        .is_some_and(|value| value.eq_ignore_ascii_case("multipart/form-data"));
    let (body, content_type) = if multipart {
        let boundary = "----CardigannRuntimeBoundary";
        (
            multipart_form_body(&inputs, boundary).into_bytes(),
            format!("multipart/form-data; boundary={boundary}"),
        )
    } else {
        (
            encoded_form_body(&definition, &inputs).into_bytes(),
            "application/x-www-form-urlencoded".to_string(),
        )
    };
    let headers = login_headers(&definition, &context, Some(&content_type))?;
    need_http(
        http_request("POST", submit_url, body, headers),
        Continuation::LoginSubmit(context),
    )
}

/// Start the operation the caller asked for.
///
/// The definition's `ratio:` block is deliberately not consulted. In Cardigann
/// it is a Jackett-era display of the operator's own account ratio, and the
/// reference engine parses it without ever evaluating it. Fetching it cost one extra
/// authenticated request before every search and grab on 328 definitions and
/// stamped a fabricated `minimum_seed_ratio` onto every release.
fn continue_operation(definition: &Definition, context: Context) -> Result<Step, EngineError> {
    match &context.operation {
        StoredOperation::TestConnection => {
            let path = definition
                .login
                .as_ref()
                .and_then(|login| login.test.as_ref())
                .and_then(|test| test.path.as_deref())
                .unwrap_or("");
            let url = if !path.is_empty() {
                resolve_url(definition, &context, path, None)?
            } else {
                configured_base_url(definition, &context)?
            };
            let request = http_request("GET", url, Vec::new(), common_headers(&context, None));
            need_http(request, Continuation::TestConnection(context))
        }
        StoredOperation::Search(_) => next_search(definition, context),
        StoredOperation::Grab(_) => begin_grab(definition, context),
    }
}

fn next_search(definition: &Definition, mut context: Context) -> Result<Step, EngineError> {
    if context.search_path >= definition.search.paths.len() {
        let response = PluginSearchResponse {
            results: context.results,
            ..PluginSearchResponse::default()
        };
        return Ok(Step::Complete {
            output: serde_json::to_value(response)
                .map_err(|error| format!("could not encode search response: {error}"))?,
        });
    }
    let request = match &context.operation {
        StoredOperation::Search(search) => search.clone(),
        _ => {
            return Err("search continuation lost its search request"
                .to_string()
                .into());
        }
    };
    context.variables = match search_variables(definition, &context.config, &request) {
        Ok(variables) => variables,
        Err(error) => return Ok(search_not_buildable(error)),
    };
    let path = &definition.search.paths[context.search_path];
    if !path.categories.is_empty()
        && let Some(Value::Array(categories)) = context.variables.get(".Categories")
        && !categories.is_empty()
    {
        let categories = categories
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let excluded = path
            .categories
            .first()
            .is_some_and(|category| category == "!");
        let path_categories = path
            .categories
            .iter()
            .filter(|category| category.as_str() != "!")
            .collect::<Vec<_>>();
        let intersects = categories
            .iter()
            .any(|category| path_categories.contains(&category));
        if (!excluded && !intersects) || (excluded && intersects) {
            context.search_path += 1;
            return next_search(definition, context);
        }
        let selected = categories
            .into_iter()
            .filter(|category| {
                let contains = path_categories.contains(&category);
                if excluded { !contains } else { contains }
            })
            .map(Value::String)
            .collect();
        context
            .variables
            .insert(".Categories".to_string(), Value::Array(selected));
    }
    let request = match build_search_request(definition, path, &context) {
        Ok(request) => request,
        Err(error) => {
            return Ok(search_not_buildable(format!(
                "search path {}: {error}",
                context.search_path + 1
            )));
        }
    };
    if request
        .method
        .as_deref()
        .is_some_and(|method| method == "GET")
        && !context.seen_get_urls.insert(request.url.clone())
    {
        context.search_path += 1;
        return next_search(definition, context);
    }
    add_uri_variables(&mut context.variables, &request.url, ".SearchUri");
    need_http(request, Continuation::SearchResponse(context))
}

fn build_search_request(
    definition: &Definition,
    path: &SearchPath,
    context: &Context,
) -> Result<PluginHttpRequest, String> {
    let method = render(&path.method, &context.variables)?.to_ascii_uppercase();
    let rendered_path = render_search_path(&path.path, &context.variables)?;
    let mut url = resolve_url(definition, context, &rendered_path, None)?;
    let mut inputs = Vec::new();
    if path.inherit_inputs {
        inputs.extend(render_map_allow_empty(
            &definition.search.inputs,
            &context.variables,
            definition.search.allow_empty_inputs,
        )?);
    }
    inputs.extend(render_map_allow_empty(
        &path.inputs,
        &context.variables,
        definition.search.allow_empty_inputs,
    )?);
    let body = if method == "POST" {
        encoded_form_body(definition, &inputs).into_bytes()
    } else {
        append_query_encoded(
            definition,
            &mut url,
            &inputs,
            path.query_separator.as_deref(),
        );
        Vec::new()
    };
    let mut headers = common_headers(
        context,
        if method == "POST" {
            Some("application/x-www-form-urlencoded")
        } else {
            None
        },
    );
    headers.extend(render_headers(
        &definition.search.headers,
        &context.variables,
    )?);
    Ok(http_request(&method, url, body, headers))
}

/// Render every search path of `definition` for `request` without driving the
/// flow, returning each path's rendered path and inputs.
///
/// The search flow renders a path only once the previous one has been
/// fetched, so a template error in a later path would otherwise only surface
/// mid-search, against the live site.
#[cfg(test)]
pub(crate) fn render_all_search_paths(
    definition: &Definition,
    config: &BTreeMap<String, String>,
    request: &PluginSearchRequest,
) -> Result<Vec<String>, String> {
    let variables = search_variables(definition, config, request)?;
    let mut rendered = Vec::with_capacity(definition.search.paths.len());
    for (index, path) in definition.search.paths.iter().enumerate() {
        let describe = |error: String| format!("search path {}: {error}", index + 1);
        render(&path.method, &variables).map_err(describe)?;
        let mut line = render_search_path(&path.path, &variables).map_err(describe)?;
        let mut inputs = Vec::new();
        if path.inherit_inputs {
            inputs.extend(
                render_map_allow_empty(
                    &definition.search.inputs,
                    &variables,
                    definition.search.allow_empty_inputs,
                )
                .map_err(describe)?,
            );
        }
        inputs.extend(
            render_map_allow_empty(
                &path.inputs,
                &variables,
                definition.search.allow_empty_inputs,
            )
            .map_err(describe)?,
        );
        render_headers(&definition.search.headers, &variables).map_err(describe)?;
        for (key, value) in inputs {
            line.push_str(&format!(" {key}={value}"));
        }
        rendered.push(line);
    }
    Ok(rendered)
}

fn begin_grab(definition: &Definition, context: Context) -> Result<Step, EngineError> {
    let Some(download) = definition.download.as_ref() else {
        return complete_grab(&context, &grab_url(&context)?, "GET", Vec::new());
    };
    if let Some(before) = download.before.as_ref() {
        if before.path_selector.is_some() {
            let url = grab_url(&context)?;
            let request = http_request(
                "GET",
                url,
                Vec::new(),
                download_headers(definition, &context, None)?,
            );
            return need_http(request, Continuation::GrabResolveBefore(context));
        }
        let referer = grab_url(&context)?;
        let request = request_block_request(definition, before, &context, Some(&referer))?;
        return need_http(request, Continuation::GrabBeforeResponse(context));
    }
    continue_grab_after_before(context)
}

fn continue_grab_after_before(context: Context) -> Result<Step, EngineError> {
    let definition = context.definition.clone();
    let download = definition
        .download
        .as_ref()
        .ok_or_else(|| "download block disappeared".to_string())?;
    let can_use_before = context.grab_before_body.is_some()
        && (download
            .selectors
            .iter()
            .any(|selector| selector.use_before_response)
            || download
                .infohash
                .as_ref()
                .is_some_and(|block| block.use_before_response));
    if can_use_before {
        let body = context.grab_before_body.clone().expect("checked body");
        finish_grab(context, &body)
    } else if download.selectors.is_empty() && download.infohash.is_none() {
        complete_grab(&context, &grab_url(&context)?, &download.method, Vec::new())
    } else {
        let url = grab_url(&context)?;
        let request = http_request(
            "GET",
            url,
            Vec::new(),
            download_headers(&definition, &context, None)?,
        );
        need_http(request, Continuation::GrabPage(context))
    }
}

fn finish_grab(mut context: Context, body: &[u8]) -> Result<Step, EngineError> {
    let definition = context.definition.clone();
    let download = definition
        .download
        .as_ref()
        .ok_or_else(|| "download block disappeared".to_string())?;
    if let Some(infohash) = download.infohash.as_ref() {
        let hash = select_html_value(&definition, body, &infohash.hash, &context.variables, true)?;
        let title =
            select_html_value(&definition, body, &infohash.title, &context.variables, true)?;
        let magnet = public_magnet_link(&hash, &title);
        return complete_grab(&context, &magnet, &download.method, Vec::new());
    }
    context.grab_selector_page_body = Some(body.to_vec());
    context.grab_selector_index = 0;
    next_grab_selector(context)
}

fn next_grab_selector(mut context: Context) -> Result<Step, EngineError> {
    let definition = context.definition.clone();
    let download = definition
        .download
        .as_ref()
        .ok_or_else(|| "download block disappeared".to_string())?;
    let body = context
        .grab_selector_page_body
        .as_deref()
        .ok_or_else(|| "download selector continuation lost its page body".to_string())?;
    let base =
        Url::parse(&grab_url(&context)?).map_err(|error| format!("invalid grab URL: {error}"))?;
    while let Some(selector) = download.selectors.get(context.grab_selector_index) {
        context.grab_selector_index += 1;
        if let Ok(value) = select_download_value(&definition, body, selector, &context.variables) {
            let url = base
                .join(&value)
                .map(|url| url.to_string())
                .unwrap_or(value);
            return complete_grab(&context, &url, &download.method, Vec::new());
        }
    }
    Ok(rejected(
        codes::INVALID_RESPONSE.into(),
        "The tracker's download page did not show a download link this definition recognises."
            .to_string(),
        format!("download selectors did not match `{base}`"),
    ))
}

fn request_block_request(
    definition: &Definition,
    block: &RequestBlock,
    context: &Context,
    referer: Option<&str>,
) -> Result<PluginHttpRequest, String> {
    let method = render(&block.method, &context.variables)?.to_ascii_uppercase();
    let path = render(&block.path, &context.variables)?;
    let mut url = resolve_url(definition, context, &path, None)?;
    let inputs = render_map(&block.inputs, &context.variables)?;
    let body = if method == "POST" {
        encoded_form_body(definition, &inputs).into_bytes()
    } else {
        append_query_encoded(
            definition,
            &mut url,
            &inputs,
            block.query_separator.as_deref(),
        );
        Vec::new()
    };
    let mut headers = download_headers(
        definition,
        context,
        if method == "POST" {
            Some("application/x-www-form-urlencoded")
        } else {
            None
        },
    )?;
    if let Some(referer) = referer {
        headers.insert("referer".to_string(), referer.to_string());
    }
    Ok(http_request(&method, url, body, headers))
}

fn complete_grab(
    context: &Context,
    url: &str,
    method: &str,
    body: Vec<u8>,
) -> Result<Step, EngineError> {
    let definition = context.definition.clone();
    let method = render(method, &context.variables)?.to_ascii_uppercase();
    if url.starts_with("magnet:") {
        return Ok(Step::Complete {
            output: serde_json::json!({
                "url": url,
                "method": method,
                "headers": download_headers_for_url(&definition, context, url)?,
                "body": body,
            }),
        });
    }
    let unusable_link = |debug: String| {
        rejected(
            codes::INVALID_RESPONSE.into(),
            "The tracker gave a download link the plugin cannot fetch.".to_string(),
            debug,
        )
    };
    let parsed = match Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(unusable_link(format!(
                "invalid download URL `{url}`: {error}"
            )));
        }
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Ok(unusable_link(format!(
            "unsupported download URL scheme `{}`",
            parsed.scheme()
        )));
    }
    let request = http_request(
        &method,
        url.to_string(),
        body,
        download_headers_for_url(&definition, context, url)?,
    );
    need_http(request, Continuation::GrabDownload(context.clone()))
}

fn complete_grab_response(
    context: &Context,
    response: PluginHttpResponse,
) -> Result<Step, EngineError> {
    // A multi-file torrent's piece table alone can run past a megabyte, so the
    // old 2 MiB ceiling rejected legitimate season packs.
    const MAX_TORRENT_BYTES: usize = 64 * 1024 * 1024;
    if response.body.len() > MAX_TORRENT_BYTES {
        return Ok(rejected(
            PluginErrorCode::Permanent,
            "The tracker's torrent is larger than the plugin accepts.".to_string(),
            format!("torrent response exceeded the {MAX_TORRENT_BYTES}-byte runtime limit"),
        ));
    }
    // An HTML-typed download is rejected outright: it is a login wall, a
    // rate-limit notice or an interstitial, never a torrent. A tracker that
    // mislabels a real bencoded body is still believed.
    let html_typed = response.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.to_ascii_lowercase().contains("text/html")
    });
    if html_typed && response.body.first() != Some(&b'd') {
        return Ok(rejected(
            codes::INVALID_RESPONSE.into(),
            "The tracker sent a web page instead of the torrent. The login session has \
             probably expired."
                .to_string(),
            "tracker returned an HTML page instead of a torrent; the session is probably expired"
                .to_string(),
        ));
    }
    let request = context
        .current_request
        .as_ref()
        .ok_or_else(|| "download continuation lost its request state".to_string())?;
    Ok(Step::Complete {
        output: serde_json::json!({
            "url": request.url,
            "method": request.method,
            "headers": request.headers,
            "body": response.body,
        }),
    })
}

#[cfg(test)]
pub(crate) fn parse_definition(source: &str) -> Result<Definition, String> {
    serde_yaml::from_str(source.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("invalid Cardigann v12 runtime definition: {error}"))
}

fn parse_compiled_ir(input: &str) -> Result<Definition, String> {
    let compiled: CompiledIr = serde_json::from_str(input)
        .map_err(|error| format!("invalid Cardigann compiled IR: {error}"))?;
    if compiled.ir_version != COMPILED_IR_VERSION {
        return Err(format!(
            "unsupported Cardigann compiled IR version {}; expected {}",
            compiled.ir_version, COMPILED_IR_VERSION
        ));
    }
    Ok(compiled.definition)
}

fn configured_base_url(definition: &Definition, context: &Context) -> Result<String, String> {
    context
        .config
        .get("base_url")
        .cloned()
        .or_else(|| definition.links.first().cloned())
        .ok_or_else(|| "definition has no base URL".to_string())
}

fn resolve_url(
    definition: &Definition,
    context: &Context,
    value: &str,
    relative_to: Option<&str>,
) -> Result<String, String> {
    if let Ok(url) = Url::parse(value) {
        return Ok(url.to_string());
    }
    let base = relative_to
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(|| configured_base_url(definition, context))?;
    Url::parse(&base)
        .map_err(|error| format!("invalid base URL `{base}`: {error}"))?
        .join(value)
        .map(|url| url.to_string())
        .map_err(|error| format!("could not resolve `{value}` against `{base}`: {error}"))
}

/// Every endpoint template a definition declares, each with whether it is
/// optional: an optional one is left out of the network endpoints when its
/// authority cannot render from configuration.
pub(crate) fn declared_endpoints(definition: &Definition) -> Vec<(&str, bool)> {
    let mut endpoints: Vec<(&str, bool)> = definition
        .search
        .paths
        .iter()
        .map(|path| (path.path.as_str(), false))
        .collect();
    if let Some(login) = &definition.login {
        endpoints.push((&login.path, false));
        endpoints.extend(login.submit_path.as_deref().map(|path| (path, false)));
        endpoints.extend(
            login
                .test
                .as_ref()
                .and_then(|test| test.path.as_deref())
                .map(|path| (path, false)),
        );
    }
    if let Some(before) = definition
        .download
        .as_ref()
        .and_then(|download| download.before.as_ref())
    {
        endpoints.push((&before.path, true));
    }
    // A literal download template is an endpoint declaration. Extracted links
    // and selector/filter output remain subject to the existing host policy.
    if let Some(field) = definition.search.fields.get("download")
        && let Some(value) = field.text.as_ref().and_then(serde_yaml::Value::as_str)
    {
        endpoints.push((value, true));
    }
    endpoints
}

pub fn resolve_network_endpoints(
    definition: &Definition,
    config: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let variables = base_variables(definition, config);
    let base = config
        .get("base_url")
        .or_else(|| definition.links.first())
        .ok_or("Missing base URL")?;
    let base = Url::parse(base).map_err(|_| "Invalid base URL")?;
    let mut endpoints: Vec<&str> = Vec::new();
    for (endpoint, optional) in declared_endpoints(definition) {
        if optional {
            let authority = crate::template::endpoint_authority_prefix(endpoint)?;
            if crate::template::render_endpoint_authority(authority, &variables).is_err() {
                continue;
            }
        }
        endpoints.push(endpoint);
    }
    let mut origins = std::collections::BTreeSet::new();
    origins.insert(base.origin().ascii_serialization());
    // Every site link and legacy link is a root the definition names: an
    // operator may pick any of them as the base URL, and trackers redirect
    // between them or serve downloads from them. Each declared host also
    // covers its own subdomains on the host side. A link that is not a plain
    // HTTP(S) URL is skipped rather than failing the whole definition.
    for link in definition.links.iter().chain(&definition.legacy_links) {
        let Ok(url) = Url::parse(link.trim()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            continue;
        }
        origins.insert(url.origin().ascii_serialization());
    }
    for endpoint in endpoints {
        if endpoint.starts_with("magnet:") {
            continue;
        }
        let prefix = crate::template::endpoint_authority_prefix(endpoint)?;
        let rendered = crate::template::render_endpoint_authority(prefix, &variables)?;
        if rendered.contains(['\\', '*', '@']) || rendered.chars().any(char::is_whitespace) {
            return Err("Invalid configured endpoint authority".into());
        }
        let url = base
            .join(&rendered)
            .map_err(|_| "Invalid configured endpoint authority")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return Err("Endpoint authorities must be HTTP(S) URLs without credentials".into());
        }
        origins.insert(url.origin().ascii_serialization());
    }
    Ok(origins.into_iter().collect())
}

fn base_variables(definition: &Definition, config: &BTreeMap<String, String>) -> Variables {
    let mut variables = Variables::new();
    variables.insert(".True".to_string(), Value::String("True".to_string()));
    variables.insert(".False".to_string(), Value::Null);
    let now: chrono::DateTime<Utc> = std::time::SystemTime::now().into();
    variables.insert(".Today.Year".to_string(), Value::from(now.year()));
    let site_link = config
        .get("base_url")
        .cloned()
        .or_else(|| definition.links.first().cloned())
        .unwrap_or_default();
    variables.insert(".Config.sitelink".to_string(), Value::String(site_link));
    for setting in &definition.settings {
        let value = config
            .get(&setting.name)
            .cloned()
            .or_else(|| setting.default_value.as_ref().and_then(scalar_to_string));
        variables.insert(
            format!(".Config.{}", setting.name),
            match (setting.setting_type.as_str(), value) {
                ("checkbox" | "bool", Some(value))
                    if value == "1"
                        || value.eq_ignore_ascii_case("true")
                        || value.eq_ignore_ascii_case("on") =>
                {
                    Value::String("true".to_string())
                }
                ("checkbox" | "bool", _) => Value::Null,
                (_, Some(value)) => Value::String(value),
                _ => Value::Null,
            },
        );
    }
    variables
}

fn add_uri_variables(variables: &mut Variables, value: &str, prefix: &str) {
    let Ok(url) = Url::parse(value) else {
        return;
    };
    variables.insert(
        format!("{prefix}.AbsoluteUri"),
        Value::String(url.to_string()),
    );
    variables.insert(
        format!("{prefix}.AbsolutePath"),
        Value::String(url.path().to_string()),
    );
    variables.insert(
        format!("{prefix}.Scheme"),
        Value::String(url.scheme().to_string()),
    );
    variables.insert(
        format!("{prefix}.Host"),
        Value::String(url.host_str().unwrap_or_default().to_string()),
    );
    variables.insert(
        format!("{prefix}.Port"),
        Value::String(url.port_or_known_default().unwrap_or_default().to_string()),
    );
    variables.insert(
        format!("{prefix}.PathAndQuery"),
        Value::String(format!(
            "{}{}",
            url.path(),
            url.query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default()
        )),
    );
    variables.insert(
        format!("{prefix}.Query"),
        Value::String(
            url.query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default(),
        ),
    );
    for (key, value) in url.query_pairs() {
        variables.insert(
            format!("{prefix}.Query.{key}"),
            Value::String(value.into_owned()),
        );
    }
}

fn search_variables(
    definition: &Definition,
    config: &BTreeMap<String, String>,
    request: &PluginSearchRequest,
) -> Result<Variables, String> {
    let mut variables = base_variables(definition, config);
    // The host keys external ids `<source>_id` (`imdb_id`, `tvdb_id`, …), which
    // is what the sibling Newznab and Torznab plugins read. Cardigann
    // definitions predate that spelling, so accept the bare and `id`-suffixed
    // forms too rather than silently binding every `.Query.*ID` to null.
    let id = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| {
                request
                    .ids
                    .get(*key)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    };
    let imdb = id(&["imdb_id", "imdb", "imdbid"]);
    let query_values = [
        ("Type", nab::query_type(request)),
        ("Q", request.query.clone()),
        ("Keywords", request.query.clone()),
        ("Limit", request.limit.to_string()),
        ("Offset", "0".to_string()),
        (
            "Season",
            request
                .season
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        (
            "Ep",
            request
                .episode
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        (
            "Episode",
            request
                .episode
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        // `.Query.IMDBID` is the `tt`-prefixed, seven-digit form and
        // `.Query.IMDBIDShort` the bare id, whichever spelling reached us. 238 definitions read one of the two.
        ("IMDBID", full_imdb_id(&imdb)),
        ("IMDBIDShort", short_imdb_id(&imdb)),
        ("TMDBID", id(&["tmdb_id", "tmdb", "tmdbid"])),
        ("TVDBID", id(&["tvdb_id", "tvdb", "tvdbid"])),
        ("TVRageID", id(&["tvrage_id", "tvrage", "rageid"])),
        ("TVMazeID", id(&["tvmaze_id", "tvmaze", "tvmazeid"])),
        ("TraktID", id(&["trakt_id", "trakt", "traktid"])),
        ("DoubanID", id(&["douban_id", "douban", "doubanid"])),
        // Ordinary title searches use the host's known release year. Exact
        // Nab requests bind their operation-specific fields below.
        (
            "Year",
            request
                .context
                .as_ref()
                .and_then(|context| context.year)
                .map(|year| year.to_string())
                .unwrap_or_default(),
        ),
    ];
    for (key, value) in query_values {
        variables.insert(
            format!(".Query.{key}"),
            if value.is_empty() {
                Value::Null
            } else {
                Value::String(value)
            },
        );
    }
    let categories = map_categories(definition, request);
    variables.insert(
        ".Categories".to_string(),
        Value::Array(categories.iter().cloned().map(Value::String).collect()),
    );
    variables.insert(
        ".Query.Categories".to_string(),
        Value::Array(categories.into_iter().map(Value::String).collect()),
    );
    let query = nab::bind_query_variables(request, &mut variables);
    let keywords = apply_definition_filters(
        definition,
        &query,
        &definition.search.keyword_filters,
        &variables,
    )?;
    variables.insert(".Keywords".to_string(), Value::String(keywords));
    Ok(variables)
}

/// The Cardigann mode name for a Scryer search facet.
///
/// These are definition `caps.modes` keys. The template's `.Query.Type` uses
/// the incoming protocol operation instead; see [`nab::query_type`].
fn cardigann_search_type(facet: Option<&str>) -> String {
    // Scryer's media facets are `movie`, `series`, and `anime`. A `collection`
    // subject is a movie collection and a `special` is a series special;
    // `title` carries no medium, so it stays a plain search.
    match facet.map(str::trim).unwrap_or_default() {
        "movie" | "collection" => "movie-search",
        "series" | "anime" | "special" => "tv-search",
        _ => "search",
    }
    .to_string()
}

/// The Cardigann `caps.modes` parameter name for a host external-id key.
///
/// The host keys ids `<source>_id`; the bare and `id`-suffixed spellings are
/// accepted for the same reason [`search_variables`] accepts them.
fn cardigann_id_parameter(key: &str) -> Option<&'static str> {
    match key.trim().to_ascii_lowercase().as_str() {
        "imdb_id" | "imdb" | "imdbid" => Some("imdbid"),
        "tmdb_id" | "tmdb" | "tmdbid" => Some("tmdbid"),
        "tvdb_id" | "tvdb" | "tvdbid" => Some("tvdbid"),
        "tvrage_id" | "tvrage" | "tvrageid" | "rageid" => Some("rid"),
        "tvmaze_id" | "tvmaze" | "tvmazeid" => Some("tvmazeid"),
        "trakt_id" | "trakt" | "traktid" => Some("traktid"),
        "douban_id" | "douban" | "doubanid" => Some("doubanid"),
        _ => None,
    }
}

/// The parameters `caps.modes` lists for one Cardigann search mode.
///
/// A missing or malformed `modes` block, or a mode the definition does not
/// declare, yields nothing — which is what "this definition supports no id for
/// that facet" means.
fn caps_mode_parameters(definition: &Definition, mode: &str) -> Vec<String> {
    definition
        .caps
        .get("modes")
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|modes| modes.get(serde_yaml::Value::String(mode.to_string())))
        .and_then(serde_yaml::Value::as_sequence)
        .map(|values| values.iter().filter_map(scalar_to_string).collect())
        .unwrap_or_default()
}

/// Whether this request may be sent to this definition at all.
///
/// An id-only search (no keywords) against a definition whose `caps.modes` lists
/// none of the supplied ids renders an empty keyword search and returns the
/// tracker's front page, so it is skipped outright. A request that also
/// carries keywords still scopes the search, so it proceeds either way.
fn id_search_is_supported(definition: &Definition, request: &PluginSearchRequest) -> bool {
    if !request.query.trim().is_empty() {
        return true;
    }
    let supplied = request
        .ids
        .iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    if supplied.is_empty() {
        return true;
    }
    let parameters = caps_mode_parameters(definition, &nab::search_type(request));
    supplied.into_iter().any(|key| {
        cardigann_id_parameter(key).is_some_and(|parameter| {
            parameters
                .iter()
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(parameter))
        })
    })
}

/// The digits of an IMDb id, with a `tt` prefix or its absence both accepted.
fn imdb_digits(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("tt")
        .trim_start_matches("TT")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect()
}

/// The full IMDb id: `tt` plus the id padded to seven digits.
fn full_imdb_id(value: &str) -> String {
    let digits = imdb_digits(value);
    if digits.is_empty() || digits.chars().all(|digit| digit == '0') {
        return String::new();
    }
    let digits = digits.trim_start_matches('0');
    format!("tt{digits:0>7}")
}

fn short_imdb_id(value: &str) -> String {
    imdb_digits(value)
}

fn map_categories(definition: &Definition, request: &PluginSearchRequest) -> Vec<String> {
    let requested = if request.categories.is_empty() {
        request.category.clone().into_iter().collect::<Vec<_>>()
    } else {
        request.categories.clone()
    };
    let mappings = definition
        .caps
        .get("categorymappings")
        .and_then(serde_yaml::Value::as_sequence);
    let mut mapped = Vec::new();
    let mut defaults = Vec::new();
    if let Some(mappings) = mappings {
        for mapping in mappings {
            let Some(mapping) = mapping.as_mapping() else {
                continue;
            };
            let id = mapping
                .get(serde_yaml::Value::String("id".into()))
                .and_then(scalar_to_string);
            let category = mapping
                .get(serde_yaml::Value::String("cat".into()))
                .and_then(scalar_to_string);
            let is_default = mapping
                .get(serde_yaml::Value::String("default".into()))
                .and_then(serde_yaml::Value::as_bool)
                .unwrap_or(false);
            if let Some(id) = id {
                if is_default {
                    defaults.push(id.clone());
                }
                if category.is_some_and(|category| category_matches(&requested, &category)) {
                    mapped.push(id);
                }
            }
        }
    } else if let Some(categories) = definition
        .caps
        .get("categories")
        .and_then(serde_yaml::Value::as_mapping)
    {
        for (id, category) in categories {
            if let (Some(id), Some(category)) = (scalar_to_string(id), scalar_to_string(category))
                && category_matches(&requested, &category)
            {
                mapped.push(id);
            }
        }
    }
    if mapped.is_empty() {
        mapped = defaults;
    }
    // The reference engine keeps the definition's mapping order and only
    // de-duplicates, which is the order `{{ range .Categories }}`
    // renders. Sorting reordered every multi-category request.
    let mut seen = BTreeSet::new();
    mapped.retain(|id| seen.insert(id.clone()));
    mapped
}

/// Whether a definition's mapped category answers one of the request's.
///
/// A request category may be a torznab id (`5040`), a standard name (`TV/HD`) or
/// a bare parent name (`Movies`). A parent matches the parent itself and every
/// child under it — a parent id expands to its whole subtree — while a
/// child matches only its own name.
fn category_matches(requested: &[String], category: &str) -> bool {
    requested.iter().any(|requested| {
        let requested = torznab_category_path(requested).unwrap_or(requested);
        if requested.eq_ignore_ascii_case(category) {
            return true;
        }
        // Only a parent expands; `TV/HD` must never claim `TV/HD Something`.
        !requested.contains('/')
            && category
                .to_ascii_lowercase()
                .starts_with(&format!("{}/", requested.to_ascii_lowercase()))
    })
}

/// The standard newznab category tree request categories are routed
/// through. Custom (`>= 100000`) categories are a
/// per-indexer construct the engine does not synthesize.
const STANDARD_CATEGORIES: &[(&str, &str)] = &[
    ("1000", "Console"),
    ("1010", "Console/NDS"),
    ("1020", "Console/PSP"),
    ("1030", "Console/Wii"),
    ("1040", "Console/XBox"),
    ("1050", "Console/XBox 360"),
    ("1060", "Console/Wiiware"),
    ("1070", "Console/XBox 360 DLC"),
    ("1080", "Console/PS3"),
    ("1090", "Console/Other"),
    ("1110", "Console/3DS"),
    ("1120", "Console/PS Vita"),
    ("1130", "Console/WiiU"),
    ("1140", "Console/XBox One"),
    ("1180", "Console/PS4"),
    ("2000", "Movies"),
    ("2010", "Movies/Foreign"),
    ("2020", "Movies/Other"),
    ("2030", "Movies/SD"),
    ("2040", "Movies/HD"),
    ("2045", "Movies/UHD"),
    ("2050", "Movies/BluRay"),
    ("2060", "Movies/3D"),
    ("2070", "Movies/DVD"),
    ("2080", "Movies/WEB-DL"),
    ("2090", "Movies/x265"),
    ("3000", "Audio"),
    ("3010", "Audio/MP3"),
    ("3020", "Audio/Video"),
    ("3030", "Audio/Audiobook"),
    ("3040", "Audio/Lossless"),
    ("3050", "Audio/Other"),
    ("3060", "Audio/Foreign"),
    ("4000", "PC"),
    ("4010", "PC/0day"),
    ("4020", "PC/ISO"),
    ("4030", "PC/Mac"),
    ("4040", "PC/Mobile-Other"),
    ("4050", "PC/Games"),
    ("4060", "PC/Mobile-iOS"),
    ("4070", "PC/Mobile-Android"),
    ("5000", "TV"),
    ("5010", "TV/WEB-DL"),
    ("5020", "TV/Foreign"),
    ("5030", "TV/SD"),
    ("5040", "TV/HD"),
    ("5045", "TV/UHD"),
    ("5050", "TV/Other"),
    ("5060", "TV/Sport"),
    ("5070", "TV/Anime"),
    ("5080", "TV/Documentary"),
    ("5090", "TV/x265"),
    ("6000", "XXX"),
    ("6010", "XXX/DVD"),
    ("6020", "XXX/WMV"),
    ("6030", "XXX/XviD"),
    ("6040", "XXX/x264"),
    ("6045", "XXX/UHD"),
    ("6050", "XXX/Pack"),
    ("6060", "XXX/ImageSet"),
    ("6070", "XXX/Other"),
    ("6080", "XXX/SD"),
    ("6090", "XXX/WEB-DL"),
    ("7000", "Books"),
    ("7010", "Books/Mags"),
    ("7020", "Books/EBook"),
    ("7030", "Books/Comics"),
    ("7040", "Books/Technical"),
    ("7050", "Books/Other"),
    ("7060", "Books/Foreign"),
    ("8000", "Other"),
    ("8010", "Other/Misc"),
    ("8020", "Other/Hashed"),
];

fn torznab_category_path(category: &str) -> Option<&'static str> {
    let category = category.trim();
    STANDARD_CATEGORIES
        .iter()
        .find(|(id, _)| *id == category)
        .map(|(_, name)| *name)
}

fn initial_cookies(
    definition: &Definition,
    config: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut cookies = BTreeMap::new();
    if let Some(login) = definition.login.as_ref() {
        for cookie in &login.cookies {
            merge_configured_cookies(&mut cookies, cookie);
        }
    }
    if let Some(cookie) = config.get("cookie") {
        merge_configured_cookies(&mut cookies, cookie);
    }
    cookies
}

fn merge_response_cookies(
    cookies: &mut BTreeMap<String, String>,
    response: &PluginHttpResponse,
) -> bool {
    let mut changed = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("set-cookie") {
            changed |= merge_cookie_header(cookies, value);
        }
    }
    for header in &response.set_cookie_headers {
        changed |= merge_cookie_header(cookies, header);
    }
    changed
}

/// The attribute names a `Cookie` header may not be carrying as a cookie.
///
/// The reference engine's attribute list, with the RFC 2965 `Discard`
/// attribute spelled correctly.
const COOKIE_ATTRIBUTE_NAMES: [&str; 12] = [
    "COMMENT",
    "COMMENTURL",
    "DISCARD",
    "DOMAIN",
    "EXPIRES",
    "MAX-AGE",
    "PATH",
    "PORT",
    "SECURE",
    "VERSION",
    "HTTPONLY",
    "SAMESITE",
];

/// Every cookie in a `Cookie`-header value (`uid=1; pass=abc; …`), the way
/// the reference engine reads one.
///
/// This is not [`merge_cookie_header`]: a `Set-Cookie` field carries one cookie
/// followed by its attributes, while the operator's `cookie` setting and a
/// definition's `login.cookies` are browser `Cookie` headers carrying every
/// cookie of the session. Parsing the latter as the former kept only the first
/// pair and silently dropped the rest.
fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r#"([^()<>@,;:\\"/\[\]?={}\s]+)=([^,;\\"\s]+)"#)
            .expect("static cookie header regex")
    });
    pattern
        .captures_iter(header)
        .filter_map(|captures| {
            let name = captures.get(1)?.as_str();
            if COOKIE_ATTRIBUTE_NAMES.contains(&name.to_ascii_uppercase().as_str()) {
                return None;
            }
            Some((name.to_string(), captures.get(2)?.as_str().to_string()))
        })
        .collect()
}

/// Merge a configured or definition-supplied `Cookie` header into the jar.
fn merge_configured_cookies(cookies: &mut BTreeMap<String, String>, header: &str) {
    for (name, value) in parse_cookie_header(header) {
        cookies.insert(name, value);
    }
}

fn merge_cookie_header(cookies: &mut BTreeMap<String, String>, header: &str) -> bool {
    let mut parts = header.split(';');
    let Some((name, value)) = parts.next().and_then(|pair| pair.trim().split_once('=')) else {
        return false;
    };
    let expired = parts.any(|attribute| {
        let Some((key, value)) = attribute.trim().split_once('=') else {
            return false;
        };
        if key.eq_ignore_ascii_case("max-age") {
            return value.trim().parse::<i64>().is_ok_and(|age| age <= 0);
        }
        key.eq_ignore_ascii_case("expires")
            && DateTime::parse_from_rfc2822(value.trim())
                .map(|expires| expires.timestamp_millis() <= unix_time_millis())
                .unwrap_or(false)
    });
    if expired {
        cookies.remove(name).is_some()
    } else if cookies.get(name).is_some_and(|existing| existing == value) {
        false
    } else {
        cookies.insert(name.to_string(), value.to_string());
        true
    }
}

fn unix_time_millis() -> i64 {
    // Cookie expiry is a pure function of the parsed cookie, evaluated in the
    // host-independent flow machine, so it uses the standard clock (WASI wall
    // time inside the component) rather than a host import.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn common_headers(context: &Context, content_type: Option<&str>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([(
        "accept".to_string(),
        "text/html,application/json,application/xml;q=0.9,*/*;q=0.8".to_string(),
    )]);
    if !context.cookies.is_empty() {
        headers.insert("cookie".to_string(), cookie_header(&context.cookies));
    }
    if let Some(content_type) = content_type {
        headers.insert("content-type".to_string(), content_type.to_string());
    }
    headers
}

fn login_headers(
    definition: &Definition,
    context: &Context,
    content_type: Option<&str>,
) -> Result<BTreeMap<String, String>, String> {
    let mut headers = common_headers(context, content_type);
    headers.extend(render_headers(
        login_header_templates(definition),
        &context.variables,
    )?);
    headers.insert(
        "referer".to_string(),
        configured_base_url(definition, context)?,
    );
    Ok(headers)
}

/// The header templates every login request renders: the login block's own
/// `headers:` when it has the key, otherwise the search headers. That fallback
/// is why an API tracker whose only `Authorization` header sits under
/// `search:` still authenticates its login.
fn login_header_templates(definition: &Definition) -> &ScalarMap {
    definition
        .login
        .as_ref()
        .and_then(|login| login.headers.as_ref())
        .unwrap_or(&definition.search.headers)
}

fn download_headers(
    definition: &Definition,
    context: &Context,
    content_type: Option<&str>,
) -> Result<BTreeMap<String, String>, String> {
    let mut headers = common_headers(context, content_type);
    let custom = definition
        .download
        .as_ref()
        .filter(|download| !download.headers.is_empty())
        .map(|download| &download.headers)
        .unwrap_or(&definition.search.headers);
    headers.extend(render_headers(custom, &context.variables)?);
    Ok(headers)
}

fn download_headers_for_url(
    definition: &Definition,
    context: &Context,
    url: &str,
) -> Result<BTreeMap<String, String>, String> {
    let base = configured_base_url(definition, context)?;
    if same_site_urls(&base, url) {
        download_headers(definition, context, None)
    } else {
        Ok(BTreeMap::from([(
            "accept".to_string(),
            "application/x-bittorrent,application/octet-stream,*/*;q=0.8".to_string(),
        )]))
    }
}

fn render_headers(
    headers: &ScalarMap,
    variables: &Variables,
) -> Result<BTreeMap<String, String>, String> {
    let mut rendered = BTreeMap::new();
    for (name, value) in headers {
        let values = match value {
            serde_yaml::Value::Sequence(values) => values
                .iter()
                .filter_map(scalar_to_string)
                .collect::<Vec<_>>(),
            value => scalar_to_string(value).into_iter().collect(),
        };
        rendered.insert(
            name.to_ascii_lowercase(),
            values
                .into_iter()
                .map(|value| render(&value, variables))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "),
        );
    }
    Ok(rendered)
}

fn render_map(map: &ScalarMap, variables: &Variables) -> Result<Vec<(String, String)>, String> {
    render_map_allow_empty(map, variables, false)
}

fn render_map_allow_empty(
    map: &ScalarMap,
    variables: &Variables,
    include_empty: bool,
) -> Result<Vec<(String, String)>, String> {
    let mut rendered = Vec::new();
    for (name, value) in map {
        let Some(value) = scalar_to_string(value) else {
            continue;
        };
        let value = if name == "$raw" {
            render_search_path(&value, variables)?
        } else {
            render(&value, variables)?
        };
        if name == "$raw" {
            for part in value.split('&') {
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                if !key.is_empty() {
                    rendered.push((key.to_string(), value.to_string()));
                }
            }
        } else if include_empty || !value.is_empty() {
            rendered.push((name.clone(), value));
        }
    }
    Ok(rendered)
}

fn append_query_encoded(
    definition: &Definition,
    url: &mut String,
    inputs: &[(String, String)],
    separator: Option<&str>,
) {
    if inputs.is_empty() {
        return;
    }
    if !url.contains('?') {
        url.push('?');
    } else if !url.ends_with('?') && !url.ends_with('&') {
        url.push_str(separator.unwrap_or("&"));
    }
    url.push_str(&encoded_form_body(definition, inputs));
}

fn encoded_form_body(definition: &Definition, inputs: &[(String, String)]) -> String {
    inputs
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                form_component(definition, name),
                form_component(definition, value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn form_component(definition: &Definition, value: &str) -> String {
    let (encoded, _, _) = definition.encoding.encoding().encode(value);
    encoded
        .iter()
        .flat_map(|&byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                vec![char::from(byte).to_string()]
            }
            b' ' => vec!["+".to_string()],
            byte => vec![format!("%{byte:02X}")],
        })
        .collect()
}

fn multipart_form_body(inputs: &[(String, String)], boundary: &str) -> String {
    let mut body = String::new();
    for (name, value) in inputs {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n",
            name.replace('"', "%22"),
            value
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

fn http_request(
    method: &str,
    url: String,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
) -> PluginHttpRequest {
    PluginHttpRequest {
        url,
        method: Some(method.to_string()),
        headers,
        body,
    }
}

fn need_http(
    mut request: PluginHttpRequest,
    mut continuation: Continuation,
) -> Result<Step, EngineError> {
    let context = continuation.context_mut();
    if !same_configured_site(&context.definition, context, &request.url) {
        request.headers.remove("cookie");
    }
    context.redirect_hops = 0;
    context.current_request = Some(RequestState {
        url: request.url.clone(),
        method: request.method.clone().unwrap_or_else(|| "GET".to_string()),
        body: request.body.clone(),
        headers: request.headers.clone(),
    });
    Ok(Step::NeedHttp {
        request,
        continuation: serde_json::to_vec(&continuation)
            .map_err(|error| format!("could not encode Cardigann continuation: {error}"))?,
    })
}

/// Whether two hosts belong to the same registrable site, the way a cookie
/// container with a `Domain=.tracker.example` attribute treats them.
///
/// The reference engine keeps one cookie jar per indexer and sends a domain
/// cookie to every host under it, so a tracker whose download or API endpoint lives on a
/// sibling subdomain keeps its session. Same-origin stripping lost it.
fn same_site_host(base_host: &str, target_host: &str) -> bool {
    let base = base_host.to_ascii_lowercase();
    let target = target_host.to_ascii_lowercase();
    if base.is_empty() || target.is_empty() {
        return false;
    }
    base == target || target.ends_with(&format!(".{base}")) || base.ends_with(&format!(".{target}"))
}

fn same_site_urls(left: &str, right: &str) -> bool {
    Url::parse(left)
        .ok()
        .zip(Url::parse(right).ok())
        .is_some_and(|(left, right)| {
            same_site_host(
                left.host_str().unwrap_or_default(),
                right.host_str().unwrap_or_default(),
            )
        })
}

fn same_configured_site(definition: &Definition, context: &Context, url: &str) -> bool {
    configured_base_url(definition, context)
        .ok()
        .is_some_and(|base| same_site_urls(&base, url))
}

fn need_redirect(
    request: PluginHttpRequest,
    mut continuation: Continuation,
) -> Result<Step, EngineError> {
    let context = continuation.context_mut();
    context.current_request = Some(RequestState {
        url: request.url.clone(),
        method: request.method.clone().unwrap_or_else(|| "GET".to_string()),
        body: request.body.clone(),
        headers: request.headers.clone(),
    });
    Ok(Step::NeedHttp {
        request,
        continuation: serde_json::to_vec(&continuation)
            .map_err(|error| format!("could not encode Cardigann continuation: {error}"))?,
    })
}

fn need_manual(
    prompt: String,
    fields: Vec<ConfigFieldDef>,
    continuation: Continuation,
) -> Result<Step, EngineError> {
    Ok(Step::NeedManualInteraction {
        prompt,
        fields,
        continuation: serde_json::to_vec(&continuation)
            .map_err(|error| format!("could not encode Cardigann continuation: {error}"))?,
    })
}

/// A tracker request outside the login flow that the tracker answered with
/// an HTTP error, as the code it stands for: 401 and 403 refused the
/// credential the definition sends (or blocked a definition that sends
/// none), 404 is `not_found` (the page for a search, the release for a
/// download), 429 is rate limiting with its `Retry-After`, a 5xx is the
/// tracker being down, and anything else is a temporary failure.
fn http_status_failure(
    context: &Context,
    response: &PluginHttpResponse,
    phase: &str,
    not_found: &str,
) -> Option<Step> {
    let status = response.status;
    if (200..300).contains(&status) {
        return None;
    }
    let error = match status {
        401 | 403 => match refused_credential(context) {
            Some(code) => plugin_error(
                code,
                format!(
                    "The tracker refused the request (HTTP {status}). Check the indexer's \
                     credentials."
                ),
            ),
            None => plugin_error(
                PluginErrorCode::Temporary,
                format!(
                    "The tracker blocked the request (HTTP {status}). It may sit behind a \
                     challenge that needs a challenge solver."
                ),
            ),
        },
        404 => plugin_error(
            not_found,
            "The tracker has no page at that address (HTTP 404).",
        ),
        429 => {
            let error = plugin_error(
                PluginErrorCode::RateLimited,
                "The tracker is rate limiting requests (HTTP 429).",
            );
            match retry_after_seconds(response) {
                Some(seconds) => error.retry_after_seconds(seconds),
                None => error,
            }
        }
        500..=599 => plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            format!("The tracker answered with a server error (HTTP {status})."),
        ),
        _ => plugin_error(
            PluginErrorCode::Temporary,
            format!("The tracker request failed (HTTP {status})."),
        ),
    };
    Some(Step::Rejected {
        error: error
            .debug(format!("Cardigann {phase} returned HTTP {status}"))
            .build(),
    })
}

/// The credential a 401 or 403 refused: the login's own credential when the
/// definition logs in, or the key or passkey setting a login-less definition
/// sends with every request. A definition that sends no credential at all was
/// blocked rather than refused, so there is nothing for the operator to fix
/// in its settings.
fn refused_credential(context: &Context) -> Option<PluginErrorCode> {
    let definition = &context.definition;
    if definition.login.is_some() {
        return Some(login_refusal(context).code);
    }
    let names: Vec<String> = definition
        .settings
        .iter()
        .map(|setting| setting.name.to_ascii_lowercase())
        .collect();
    if names.iter().any(|name| name.contains("passkey")) {
        return Some(codes::INVALID_PASSKEY.into());
    }
    if names
        .iter()
        .any(|name| name.contains("apikey") || name.contains("api_key") || name.contains("rsskey"))
    {
        return Some(codes::INVALID_API_KEY.into());
    }
    definition
        .settings
        .iter()
        .any(|setting| setting.setting_type.eq_ignore_ascii_case("password"))
        .then_some(PluginErrorCode::AuthFailed)
}

/// The response's `Retry-After` in seconds, when the tracker sent one in that
/// form.
fn retry_after_seconds(response: &PluginHttpResponse) -> Option<i64> {
    response.headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("retry-after")
            .then(|| value.trim().parse::<i64>().ok())
            .flatten()
    })
}

fn follow_redirect(
    definition: &Definition,
    context: &mut Context,
    response: &PluginHttpResponse,
    enabled: bool,
) -> Result<Option<PluginHttpRequest>, EngineError> {
    if !(300..400).contains(&response.status) || !enabled {
        return Ok(None);
    }
    let Some(location) = response
        .headers
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case("location").then_some(value))
    else {
        return Err(rejection_error(
            codes::INVALID_RESPONSE.into(),
            "The tracker answered with a redirect that names no destination.",
            format!(
                "HTTP {} redirect response did not include a Location header",
                response.status
            ),
        ));
    };
    if context.redirect_hops >= 5 {
        return Err(rejection_error(
            codes::INVALID_RESPONSE.into(),
            "The tracker redirected more times than the plugin follows.",
            "Cardigann redirect limit (5 hops) exceeded".to_string(),
        ));
    }
    let previous = context
        .current_request
        .clone()
        .ok_or_else(|| "redirect continuation lost its originating request".to_string())?;
    let target = resolve_url(definition, context, location, Some(&previous.url))?;
    let same_origin = Url::parse(&previous.url)
        .ok()
        .zip(Url::parse(&target).ok())
        .is_some_and(|(base, target)| base.origin() == target.origin());
    context.redirect_hops += 1;
    let (method, body) = if same_origin && (response.status == 307 || response.status == 308) {
        (previous.method, previous.body)
    } else {
        ("GET".to_string(), Vec::new())
    };
    // Cookies and the definition's own headers follow the session's site, not
    // only its origin, so a redirect onto a sibling subdomain keeps them.
    let headers = if same_site_urls(&previous.url, &target) {
        let mut headers = previous.headers;
        // The jar belongs to the configured site, so a hop that leaves it drops
        // the cookies even when it stays within the previous request's site.
        if !same_configured_site(definition, context, &target) {
            headers.remove("cookie");
        } else if !context.cookies.is_empty() {
            headers.insert("cookie".to_string(), cookie_header(&context.cookies));
        }
        headers
    } else {
        BTreeMap::from([(
            "accept".to_string(),
            "text/html,application/json,application/xml;q=0.9,*/*;q=0.8".to_string(),
        )])
    };
    Ok(Some(http_request(&method, target, body, headers)))
}

fn retry_login_if_needed(
    definition: &Definition,
    mut context: Context,
    response: &PluginHttpResponse,
) -> Result<Option<Step>, EngineError> {
    let Some(login) = definition.login.as_ref() else {
        return Ok(None);
    };
    let cookie_login = login.method.eq_ignore_ascii_case("cookie");
    if context.relogin_attempts >= 1 && !cookie_login {
        return Ok(None);
    }

    // Login is needed again on any redirect that was not followed, any
    // HTTP error status, or an HTML body missing the login test selector. A 3xx
    // can only reach here unfollowed — `follow_redirect` returns a fresh request
    // when redirects are enabled for this request — so the status test is the
    // whole rule. Narrowing it to 401/403 and a `Location` naming `login.path`
    // left every other session expiry as a plain search failure.
    let needs_login = response.status >= 300;
    let content_type = response.headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            .then_some(value.as_str())
    });
    let missing_login_test = login
        .test
        .as_ref()
        .and_then(|test| test.selector.as_deref())
        .is_some_and(|selector_text| {
            content_type
                .map(|value| value.contains("html"))
                .unwrap_or(true)
                && parse_selector(selector_text).is_ok_and(|selector| {
                    let mut document =
                        Html::parse_document(&decode_body(definition, &response.body));
                    mark_contains(&mut document, &selector.needles);
                    select_matching_element(&document.root_element(), &selector).is_none()
                })
        });
    if !(needs_login || missing_login_test) {
        return Ok(None);
    }
    if context.relogin_attempts >= 1 {
        // A cookie login has nothing to log in again with: the reissued
        // request still carried the configured cookie, so the tracker refused
        // that cookie. Any other status stays the search failure it was.
        if missing_login_test || matches!(response.status, 300..=399 | 401 | 403) {
            return Ok(Some(rejected(
                codes::INVALID_COOKIE.into(),
                "The tracker did not accept the configured cookie. Update the cookie setting."
                    .to_string(),
                format!(
                    "Cardigann cookie login: HTTP {} still had no logged-in session after the retry",
                    response.status
                ),
            )));
        }
        return Ok(None);
    }

    context.relogin_attempts += 1;
    context.cookies = initial_cookies(definition, &context.config);
    // The request that provoked the re-login is reissued once the session is
    // back, so it must not still count as a URL this operation has already
    // asked for.
    if let Some(request) = context.current_request.as_ref() {
        let url = request.url.clone();
        context.seen_get_urls.remove(&url);
    }
    begin_after_optional_login(definition, context).map(Some)
}

fn grab_url(context: &Context) -> Result<String, String> {
    match &context.operation {
        StoredOperation::Grab(url) => Ok(url.clone()),
        _ => Err("grab continuation lost its URL".to_string()),
    }
}

/// A Cardigann selector, with every `:contains(…)` pseudo rewritten into a
/// marker attribute.
///
/// `scraper` has no `:contains`. Stripping the pseudo out of the selector text
/// and testing the finally matched element's text instead is right for
/// `a:contains("Download")` and wrong everywhere else: `tr:not(:contains("x"))`
/// became `tr:not()` and inverted its meaning, and `tr:has(td:contains("x"))`
/// degraded to "the row mentions x somewhere". Rewriting each occurrence into
/// `[data-cardigann-contains-…]` keeps the test in the position the definition
/// wrote it, at the cost of one document pass ([`mark_contains`]) that stamps
/// the attribute onto every element whose text contains the needle.
struct CardigannSelector {
    css: Selector,
    /// The needles a document must be marked with before this selector can
    /// match anything.
    needles: Vec<String>,
}

impl CardigannSelector {
    fn matches_element(&self, element: &ElementRef<'_>) -> bool {
        self.css.matches(element)
    }
}

fn select_matching_element<'a>(
    root: &ElementRef<'a>,
    selector: &CardigannSelector,
) -> Option<ElementRef<'a>> {
    selector
        .matches_element(root)
        .then_some(*root)
        .or_else(|| root.select(&selector.css).next())
}

fn contains_pattern() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(r#":contains\(\s*(?:"([^"]*)"|'([^']*)'|([^)]*))\s*\)"#)
            .expect("static contains selector regex")
    })
}

/// The needle of one `:contains(…)`. A quoted argument is taken verbatim —
/// `:contains(" GB")` is a different test from `:contains("GB")` — while the
/// unquoted form carries no way to express leading or trailing space.
fn contains_needle(captures: &regex::Captures<'_>) -> String {
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| {
            captures
                .get(3)
                .map(|value| value.as_str().trim().to_string())
                .unwrap_or_default()
        })
}

fn contains_needles(selector: &str) -> Vec<String> {
    contains_pattern()
        .captures_iter(selector)
        .map(|captures| contains_needle(&captures))
        .collect()
}

/// The attribute name that stands for one needle. Derived from the needle
/// (FNV-1a) so marking is idempotent and two selectors over the same document
/// can never claim each other's marker.
fn contains_marker(needle: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in needle.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("data-cardigann-contains-{hash:016x}")
}

/// Stamp each needle's marker attribute onto every element of `document` whose
/// text contains it. Must run before a selector carrying that needle is used
/// against the document; running it twice, or with overlapping needle sets, is
/// harmless.
fn mark_contains(document: &mut Html, needles: &[String]) {
    if needles.is_empty() {
        return;
    }
    let markers = needles
        .iter()
        .map(|needle| {
            (
                needle.clone(),
                QualName::new(
                    None,
                    ns!(),
                    LocalName::from(contains_marker(needle).as_str()),
                ),
            )
        })
        .collect::<Vec<_>>();
    let ids = document
        .tree
        .nodes()
        .filter(|node| node.value().is_element())
        .map(|node| node.id())
        .collect::<Vec<_>>();
    for id in ids {
        let Some(element) = document.tree.get(id).and_then(ElementRef::wrap) else {
            continue;
        };
        let text = element.text().collect::<String>();
        let matched = markers
            .iter()
            .filter(|(needle, name)| {
                text.contains(needle.as_str())
                    && !element.value().attrs.iter().any(|(key, _)| key == name)
            })
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        if matched.is_empty() {
            continue;
        }
        let Some(mut node) = document.tree.get_mut(id) else {
            continue;
        };
        let Node::Element(element) = node.value() else {
            continue;
        };
        for name in matched {
            element.attrs.push((name, StrTendril::new()));
        }
        // `Element::attr` binary-searches this list, so it has to stay ordered.
        element
            .attrs
            .sort_unstable_by(|left, right| left.0.cmp(&right.0));
    }
}

/// Every `:contains(…)` needle a selector field can reach, rendered the way the
/// field's own evaluation renders it.
///
/// A needle whose text is itself templated cannot always be resolved here (the
/// row's `.Result.*` bindings do not exist yet), so the raw form is harvested
/// alongside the rendered one.
fn field_needles(field: &SelectorField, variables: &Variables, needles: &mut Vec<String>) {
    let mut harvest = |selector: &str| {
        let rendered = render(selector, variables).unwrap_or_else(|_| selector.to_string());
        needles.extend(contains_needles(&rendered));
        if rendered != selector {
            needles.extend(contains_needles(selector));
        }
    };
    if let Some(selector) = field.selector.as_deref() {
        harvest(selector);
    }
    if let Some(cases) = field.case.as_ref() {
        for selector in cases.keys() {
            harvest(selector);
        }
    }
    match field.remove.as_ref() {
        Some(serde_yaml::Value::Sequence(values)) => {
            for value in values.iter().filter_map(scalar_to_string) {
                harvest(&value);
            }
        }
        Some(value) => {
            if let Some(value) = scalar_to_string(value) {
                harvest(&value);
            }
        }
        None => {}
    }
}

/// Every needle the search-response parse can use, so one pass over the parsed
/// document serves the row selector and all of its fields.
fn search_response_needles(definition: &Definition, variables: &Variables) -> Vec<String> {
    let rows = &definition.search.rows;
    let mut needles = contains_needles(
        &render(&rows.selector, variables).unwrap_or_else(|_| rows.selector.clone()),
    );
    needles.extend(contains_needles(&rows.selector));
    for field in rows
        .count
        .iter()
        .chain(rows.date_headers.iter())
        .chain(definition.search.fields.values())
    {
        field_needles(field, variables, &mut needles);
    }
    needles.sort();
    needles.dedup();
    needles
}

fn login_needles(login: &LoginBlock, variables: &Variables) -> Vec<String> {
    let mut needles = contains_needles(login.form.as_deref().unwrap_or("form"));
    if login.selectors {
        for key in login.inputs.keys() {
            needles.extend(contains_needles(key));
        }
    }
    if let Some(captcha) = login.captcha.as_ref() {
        needles.extend(contains_needles(&captcha.selector));
        if login.selectors {
            needles.extend(contains_needles(&captcha.input));
        }
    }
    for field in login
        .selector_inputs
        .values()
        .chain(login.get_selector_inputs.values())
    {
        field_needles(field, variables, &mut needles);
    }
    needles.sort();
    needles.dedup();
    needles
}

fn error_needles(errors: &[crate::definition::ErrorBlock], variables: &Variables) -> Vec<String> {
    let mut needles = Vec::new();
    for error in errors {
        needles.extend(contains_needles(&error.selector));
        if let Some(message) = error.message.as_ref() {
            field_needles(message, variables, &mut needles);
        }
    }
    needles.sort();
    needles.dedup();
    needles
}

fn parse_selector(selector: &str) -> Result<CardigannSelector, String> {
    let mut needles = Vec::new();
    let css = contains_pattern()
        .replace_all(selector, |captures: &regex::Captures<'_>| {
            let needle = contains_needle(captures);
            let marker = contains_marker(&needle);
            needles.push(needle);
            format!("[{marker}]")
        })
        .into_owned();
    Selector::parse(&css)
        .map(|css| CardigannSelector { css, needles })
        .map_err(|error| format!("invalid CSS selector `{selector}`: {error:?}"))
}

/// The definition's error check for a login response.
///
/// A non-2xx response still reaches the definition's `error:` blocks. They run first
/// here for every status, which keeps a rejected login reported as refused
/// whatever status the tracker answered it with. A non-2xx response no block
/// matches is reported as the HTTP failure it is.
///
/// A matching block only says the login was refused. Which credential was
/// refused comes from the definition and the configuration, never from the
/// response, and the operator sentence is fixed for that code. The tracker's
/// own text goes to the debug message only.
fn login_failure(
    definition: &Definition,
    context: &Context,
    response: &PluginHttpResponse,
) -> Result<Option<Step>, EngineError> {
    if let Some(login) = definition.login.as_ref()
        && let Some(message) = error_block_message(
            definition,
            &login.error,
            response,
            &context.variables,
            "login",
        )?
    {
        let refusal = login_refusal(context);
        return Ok(Some(rejected(
            refusal.code,
            refusal.public_message.to_string(),
            format!(
                "Cardigann login returned HTTP {} and matched the definition's error block: {}",
                response.status,
                tracker_text(&mask_credentials(context, &message))
            ),
        )));
    }
    if !(200..300).contains(&response.status) {
        return Ok(Some(login_http_failure(
            response.status,
            "login",
            login_refusal(context).code,
        )));
    }
    Ok(None)
}

/// A refused login: the code it carries and the fixed operator sentence for
/// that code.
struct LoginRefusal {
    code: PluginErrorCode,
    public_message: &'static str,
}

/// What a refused login refused, from how the definition logs in and what the
/// configuration supplies, all known before the request is sent: a cookie
/// login was refused its cookie, a login that declares a CAPTCHA with no
/// answer configured wants one, and a definition with username and password
/// settings was refused that pair. Anything else, such as an API-key login,
/// stays the generic auth failure. Nothing in the tracker's response decides
/// it.
fn login_refusal(context: &Context) -> LoginRefusal {
    let definition = &context.definition;
    let refused_sign_in = LoginRefusal {
        code: PluginErrorCode::AuthFailed,
        public_message: "The tracker refused the sign-in.",
    };
    let Some(login) = definition.login.as_ref() else {
        return refused_sign_in;
    };
    if login.method.eq_ignore_ascii_case("cookie") {
        return LoginRefusal {
            code: codes::INVALID_COOKIE.into(),
            public_message: "The tracker refused the cookie.",
        };
    }
    let has_captcha_answer = ["cardigannCaptcha", "CAPTCHA"].iter().any(|key| {
        context
            .config
            .get(*key)
            .is_some_and(|value| !value.trim().is_empty())
    });
    if login.captcha.is_some() && !has_captcha_answer {
        return LoginRefusal {
            code: codes::CAPTCHA_REQUIRED.into(),
            public_message: "The tracker asks for a CAPTCHA answer.",
        };
    }
    let has_setting = |name: &str| {
        definition
            .settings
            .iter()
            .any(|setting| setting.name == name)
    };
    if has_setting("username") && has_setting("password") {
        return LoginRefusal {
            code: codes::INVALID_USERNAME_PASSWORD.into(),
            public_message: "The tracker refused the username and password.",
        };
    }
    refused_sign_in
}

/// A login request the tracker answered with an HTTP error and nothing the
/// definition explains. `auth_code` is what a 401 or 403 means for this
/// request.
fn login_http_failure(status: u16, phase: &str, auth_code: PluginErrorCode) -> Step {
    let (code, public_message) = match status {
        401 | 403 => (
            auth_code,
            format!("Tracker login was rejected (HTTP {status}). Check the indexer's credentials."),
        ),
        408 | 429 => (
            PluginErrorCode::Temporary,
            format!("Tracker login was temporarily refused (HTTP {status}). Try again later."),
        ),
        _ => (
            PluginErrorCode::UpstreamUnavailable,
            format!("Tracker login failed: the tracker returned HTTP {status}."),
        ),
    };
    rejected(
        code,
        public_message,
        format!("Cardigann {phase} returned HTTP {status}"),
    )
}

fn rejected(code: PluginErrorCode, public_message: String, debug_message: String) -> Step {
    Step::Rejected {
        error: plugin_error(code, public_message)
            .debug(debug_message)
            .build(),
    }
}

/// The typed failure of a step that reports through `Err` rather than a
/// rejected `Step`. The public message is fixed wording and carries no
/// tracker or setting text.
fn rejection_error(
    code: PluginErrorCode,
    public_message: &str,
    debug_message: String,
) -> EngineError {
    EngineError::Plugin(
        plugin_error(code, public_message)
            .debug(debug_message)
            .build(),
    )
}

/// The definition's search templates do not evaluate with these settings.
fn search_not_buildable(debug_message: String) -> Step {
    rejected(
        PluginErrorCode::InvalidConfig,
        "The definition's search could not be built from the indexer's settings.".to_string(),
        debug_message,
    )
}

/// The tracker's login page does not hold what the definition looks for.
fn login_page_mismatch(debug_message: String) -> Step {
    rejected(
        codes::INVALID_RESPONSE.into(),
        "The tracker's login page did not show the login form this definition expects.".to_string(),
        debug_message,
    )
}

/// Upper bound on tracker-supplied text in a debug message. An error block
/// without a `message:` reports the matched element's text, which for a
/// `:root:contains(…)` selector is the whole page.
const TRACKER_TEXT_LIMIT: usize = 200;

/// Tracker or definition text with every credential-bearing setting value the
/// operator configured masked, in case a definition's message template or the
/// tracker's page echoes one back.
fn mask_credentials(context: &Context, text: &str) -> String {
    mask_setting_values(&context.definition, &context.config, text)
}

/// `text` with every credential-bearing setting value masked, raw and in the
/// percent-encoded form a rendered URL carries it in. A passkey a definition
/// puts in the URL path has no parameter name to mark it, so masking goes by
/// value.
fn mask_setting_values(
    definition: &Definition,
    config: &BTreeMap<String, String>,
    text: &str,
) -> String {
    let mut text = text.to_string();
    let mut secrets = ["username", "password", "cookie", "cardigannCaptcha"]
        .into_iter()
        .chain(
            definition
                .settings
                .iter()
                .filter(|setting| matches!(setting.setting_type.as_str(), "text" | "password"))
                .map(|setting| setting.name.as_str()),
        )
        .filter_map(|key| config.get(key))
        .map(|value| value.trim())
        .filter(|value| value.len() >= 4)
        .flat_map(|value| {
            let encoded =
                percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
                    .to_string();
            [value.to_string(), encoded]
        })
        .collect::<Vec<_>>();
    // Longest first, so a value that contains another is masked whole.
    secrets.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    secrets.dedup();
    for secret in secrets {
        text = text.replace(&secret, "[redacted]");
    }
    text
}

/// Masked tracker text made fit for a `debug_message`: whitespace collapsed
/// and length bounded. Tracker text never goes into a `public_message`.
fn tracker_text(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > TRACKER_TEXT_LIMIT {
        text.chars().take(TRACKER_TEXT_LIMIT).collect::<String>() + "…"
    } else {
        text
    }
}

/// The text an element shows a reader: its text nodes, without the contents
/// of `<style>` and `<script>` elements inside it.
fn visible_text(element: &ElementRef<'_>) -> String {
    let mut text = String::new();
    let mut pending = vec![**element];
    while let Some(node) = pending.pop() {
        match node.value() {
            Node::Text(fragment) => text.push_str(fragment),
            Node::Element(child)
                if child.name().eq_ignore_ascii_case("style")
                    || child.name().eq_ignore_ascii_case("script") => {}
            _ => pending.extend(node.children().rev()),
        }
    }
    text
}

/// The definition's `error:` block a search response matched. The operator
/// sentence is fixed; the tracker's own explanation, credentials masked, goes
/// to the debug message.
fn search_error_block(
    definition: &Definition,
    context: &Context,
    response: &PluginHttpResponse,
) -> Result<Option<Step>, EngineError> {
    let Some(message) = error_block_message(
        definition,
        &definition.search.error,
        response,
        &context.variables,
        "search",
    )?
    else {
        return Ok(None);
    };
    Ok(Some(rejected(
        PluginErrorCode::Temporary,
        "The tracker reported an error for the search.".to_string(),
        format!(
            "tracker search failed: {}",
            tracker_text(&mask_credentials(context, &message))
        ),
    )))
}

/// The message of the first `error:` block that matches the response: the
/// block's `message:` value when it declares one, otherwise the matched
/// element's visible text.
fn error_block_message(
    definition: &Definition,
    errors: &[crate::definition::ErrorBlock],
    response: &PluginHttpResponse,
    variables: &Variables,
    phase: &str,
) -> Result<Option<String>, String> {
    let mut document = Html::parse_document(&decode_body(definition, &response.body));
    mark_contains(&mut document, &error_needles(errors, variables));
    for error in errors {
        if error.path.is_some() {
            return Err(format!(
                "tracker {phase} error path requests are unsupported by the definition runtime"
            ));
        }
        let root = document.root_element();
        let selector = parse_selector(&error.selector)?;
        if let Some(element) = select_matching_element(&root, &selector) {
            let message = error
                .message
                .as_ref()
                .and_then(|field| {
                    select_element_value(definition, &element, field, variables, false).ok()
                })
                .unwrap_or_else(|| visible_text(&element).trim().to_string());
            return Ok(Some(message));
        }
    }
    Ok(None)
}

fn select_download_value(
    definition: &Definition,
    body: &[u8],
    field: &DownloadSelector,
    variables: &Variables,
) -> Result<String, String> {
    select_html_value(
        definition,
        body,
        &SelectorField {
            selector: Some(field.selector.clone()),
            attribute: field.attribute.clone(),
            filters: field.filters.clone(),
            ..SelectorField::default()
        },
        variables,
        true,
    )
}

fn select_html_value(
    definition: &Definition,
    body: &[u8],
    field: &SelectorField,
    variables: &Variables,
    required: bool,
) -> Result<String, String> {
    let mut document = Html::parse_document(&decode_body(definition, body));
    let mut needles = Vec::new();
    field_needles(field, variables, &mut needles);
    mark_contains(&mut document, &needles);
    select_element_value(
        definition,
        &document.root_element(),
        field,
        variables,
        required,
    )
}

fn select_element_value(
    definition: &Definition,
    root: &ElementRef<'_>,
    field: &SelectorField,
    variables: &Variables,
    required: bool,
) -> Result<String, String> {
    if let Some(text) = field.text.as_ref().and_then(scalar_to_string) {
        return apply_definition_filters(
            definition,
            &render(&text, variables)?,
            &field.filters,
            variables,
        );
    }
    if let Some(cases) = field.case.as_ref() {
        for (selector, value) in cases {
            let selector = parse_selector(&render(selector, variables)?)?;
            if select_matching_element(root, &selector).is_some() {
                return apply_definition_filters(
                    definition,
                    &render(value, variables)?,
                    &field.filters,
                    variables,
                );
            }
        }
        // When no case selector matches, the field is null (an error when
        // required). Falling through to the field's own selector would
        // hand back the row's text instead.
        if required {
            return Err("none of the case selectors matched".to_string());
        }
        return Ok(String::new());
    }
    let selector_text = render(field.selector.as_deref().unwrap_or(":scope"), variables)?;
    let selector = parse_selector(&selector_text)?;
    let Some(element) = select_matching_element(root, &selector) else {
        if required {
            return Err(format!("selector `{selector_text}` did not match"));
        }
        return Ok(String::new());
    };
    let mut value = if let Some(attribute) = field.attribute.as_ref() {
        element
            .value()
            .attr(attribute)
            .unwrap_or_default()
            .to_string()
    } else {
        element.text().collect::<String>().trim().to_string()
    };
    if field.attribute.is_none() {
        let remove_selectors = match field.remove.as_ref() {
            Some(serde_yaml::Value::Sequence(values)) => values
                .iter()
                .filter_map(scalar_to_string)
                .collect::<Vec<_>>(),
            Some(value) => scalar_to_string(value).into_iter().collect(),
            None => Vec::new(),
        };
        for remove_selector in remove_selectors {
            let remove_selector = parse_selector(&render(&remove_selector, variables)?)?;
            for removed in element.select(&remove_selector.css) {
                let removed_text = removed.text().collect::<String>();
                value = value.replace(removed_text.trim(), "");
            }
        }
        value = value.trim().to_string();
    }
    value = apply_definition_filters(definition, &value, &field.filters, variables)?;
    Ok(value)
}

fn parse_search_response(
    definition: &Definition,
    path: &SearchPath,
    response: &PluginHttpResponse,
    variables: &mut Variables,
) -> Result<Vec<PluginSearchResult>, String> {
    let body = decode_body(definition, &response.body);
    match path.response.response_type {
        ResponseType::Json => {
            if matches_no_results_message(&body, path.response.no_results_message.as_ref()) {
                Ok(Vec::new())
            } else {
                parse_json_results(definition, &body, variables)
            }
        }
        ResponseType::Xml => parse_xml_results(definition, &body, variables),
        ResponseType::Html => parse_markup_results(definition, &body, variables),
    }
}

fn decode_body(definition: &Definition, body: &[u8]) -> String {
    let (decoded, _, _) = definition.encoding.encoding().decode(body);
    decoded.into_owned()
}

fn apply_definition_filters(
    definition: &Definition,
    input: &str,
    filters: &[crate::definition::FilterBlock],
    variables: &Variables,
) -> Result<String, String> {
    apply_filters_with_encoding(input, filters, variables, definition.encoding.encoding())
}

fn matches_no_results_message(body: &str, message: Option<&String>) -> bool {
    message.is_some_and(|message| {
        if message.is_empty() {
            body.is_empty()
        } else {
            body.contains(message)
        }
    })
}

fn parse_xml_results(
    definition: &Definition,
    body: &str,
    variables: &mut Variables,
) -> Result<Vec<PluginSearchResult>, String> {
    let preprocessed = apply_definition_filters(
        definition,
        body,
        &definition.search.preprocessing_filters,
        variables,
    )?;
    let document = XmlDocument::parse(&preprocessed)
        .map_err(|error| format!("invalid XML search response: {error}"))?;
    let rows = xml_select_many(
        document.root(),
        &render(&definition.search.rows.selector, variables)?,
    )?;
    let mut results = Vec::new();
    for row in rows {
        let mut row_variables = variables.clone();
        // A row that will not parse is skipped on its own, and a feed whose
        // rows all fail is an empty result rather than an indexer failure.
        match parse_xml_row(definition, row, &mut row_variables) {
            Ok(parsed) => {
                if let Some(mut result) = parsed {
                    normalize_result(definition, &mut result);
                    if should_keep_row(definition, &result, &row_variables) {
                        results.push(result);
                    }
                }
                *variables = row_variables;
            }
            Err(_) => continue,
        }
    }
    Ok(results)
}

fn parse_xml_row(
    definition: &Definition,
    row: XmlNode<'_, '_>,
    variables: &mut Variables,
) -> Result<Option<PluginSearchResult>, String> {
    let mut result = PluginSearchResult::default();
    for (field_name, field) in &definition.search.fields {
        let (name, modifiers) = split_field_name(field_name);
        let required = !field.optional && !is_implicitly_optional(name);
        let value = xml_field_value(definition, row, field, variables, required)
            .map_err(|error| format!("field `{field_name}`: {error}"))?;
        variables.insert(
            format!(".Result.{name}"),
            if value.is_empty() {
                Value::Null
            } else {
                Value::String(value.clone())
            },
        );
        let base_url = variables
            .get(".SearchUri.AbsoluteUri")
            .and_then(Value::as_str)
            .or_else(|| definition.links.first().map(String::as_str))
            .unwrap_or_default();
        apply_result_field(&mut result, name, &modifiers, value, base_url)?;
    }
    if result.title.is_empty() {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}

fn xml_field_value(
    definition: &Definition,
    root: XmlNode<'_, '_>,
    field: &SelectorField,
    variables: &Variables,
    required: bool,
) -> Result<String, String> {
    if let Some(text) = field.text.as_ref().and_then(scalar_to_string) {
        return apply_definition_filters(
            definition,
            &render(&text, variables)?,
            &field.filters,
            variables,
        );
    }
    if let Some(cases) = field.case.as_ref() {
        for (selector, value) in cases {
            if !xml_select_many(root, &render(selector, variables)?)?.is_empty() {
                return apply_definition_filters(
                    definition,
                    &render(value, variables)?,
                    &field.filters,
                    variables,
                );
            }
        }
        if required {
            return Err("none of the case selectors matched".to_string());
        }
        return Ok(String::new());
    }
    let selector = render(field.selector.as_deref().unwrap_or(""), variables)?;
    let node = xml_select_many(root, &selector)?.into_iter().next();
    let value = match node {
        Some(node) => match field.attribute.as_deref() {
            Some(attribute) => node
                .attribute(attribute)
                .map(str::to_string)
                .ok_or_else(|| format!("attribute `{attribute}` did not match"))?,
            None => node.text().unwrap_or_default().trim().to_string(),
        },
        None if !required => field
            .default_value
            .as_ref()
            .and_then(scalar_to_string)
            .map(|value| render(&value, variables))
            .transpose()?
            .unwrap_or_default(),
        None => return Err(format!("selector `{selector}` did not match")),
    };
    apply_definition_filters(definition, &value, &field.filters, variables)
}

fn xml_select_many<'a>(
    root: XmlNode<'a, 'a>,
    selector: &str,
) -> Result<Vec<XmlNode<'a, 'a>>, String> {
    let mut steps = Vec::new();
    let mut relation = false;
    for token in selector.split_whitespace() {
        if token == ">" {
            relation = true;
            continue;
        }
        steps.push((relation, token));
        relation = false;
    }
    if steps.is_empty() {
        return Ok(Vec::new());
    }
    let mut current = vec![root];
    for (child_only, token) in steps {
        let mut next = Vec::new();
        for node in current {
            let candidates: Vec<_> = if child_only {
                node.children().filter(|child| child.is_element()).collect()
            } else {
                node.descendants()
                    .filter(|child| child.is_element())
                    .collect()
            };
            next.extend(
                candidates
                    .into_iter()
                    .filter(|candidate| xml_matches(*candidate, token)),
            );
        }
        current = next;
    }
    Ok(current)
}

fn xml_matches(node: XmlNode<'_, '_>, selector: &str) -> bool {
    let contains = regex::Regex::new(r#":contains\((?:\"([^\"]*)\"|'([^']*)'|([^)]*))\)"#)
        .expect("static XML contains selector regex");
    let expected_text = contains
        .captures(selector)
        .and_then(|captures| (1..=3).find_map(|index| captures.get(index)))
        .map(|value| value.as_str().trim().to_string());
    let selector = contains.replace(selector, "");
    let (tag, attribute) = selector.split_once('[').unwrap_or((selector.as_ref(), ""));
    let tag = tag.trim();
    let tag_matches = tag.is_empty()
        || tag == "*"
        || node.tag_name().name() == tag.rsplit(':').next().unwrap_or(tag);
    let attribute_matches = if attribute.is_empty() {
        true
    } else {
        let attribute = attribute.trim_end_matches(']').trim();
        match attribute.split_once('=') {
            Some((name, value)) => node
                .attribute(name.trim())
                .is_some_and(|actual| actual == value.trim().trim_matches('\"').trim_matches('\'')),
            None => node.attribute(attribute).is_some(),
        }
    };
    tag_matches
        && attribute_matches
        && expected_text.is_none_or(|expected| node.text().unwrap_or_default().contains(&expected))
}

fn parse_markup_results(
    definition: &Definition,
    body: &str,
    variables: &mut Variables,
) -> Result<Vec<PluginSearchResult>, String> {
    let preprocessed = apply_definition_filters(
        definition,
        body,
        &definition.search.preprocessing_filters,
        variables,
    )?;
    let needles = search_response_needles(definition, variables);
    let mut document = Html::parse_document(&preprocessed);
    mark_contains(&mut document, &needles);
    if let Some(count) = definition.search.rows.count.as_ref() {
        let value = select_element_value(
            definition,
            &document.root_element(),
            count,
            variables,
            false,
        )?;
        if parse_i64(&value).is_some_and(|count| count < 1) {
            return Ok(Vec::new());
        }
    }
    let row_selector_text = render(&definition.search.rows.selector, variables)?;
    let row_selector = parse_selector(&row_selector_text)?;
    let rows = document.select(&row_selector.css).collect::<Vec<_>>();
    let mut results = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let row = &rows[index];
        let mut row_variables = variables.clone();
        let parsed = (|| -> Result<Option<PluginSearchResult>, String> {
            let parsed = if definition.search.rows.after == 0 {
                parse_markup_row(definition, row, &mut row_variables)?
            } else {
                let trailing = rows
                    .iter()
                    .skip(index)
                    .take(definition.search.rows.after + 1)
                    .map(ElementRef::html)
                    .collect::<String>();
                let mut merged = Html::parse_fragment(&trailing);
                mark_contains(&mut merged, &needles);
                parse_markup_row(definition, &merged.root_element(), &mut row_variables)?
            };
            if let Some(mut result) = parsed {
                if result.published_at.is_none()
                    && let Some(date_headers) = definition.search.rows.date_headers.as_ref()
                {
                    result.published_at =
                        previous_date_header(definition, row, date_headers, &row_variables)?;
                }
                normalize_result(definition, &mut result);
                if should_keep_row(definition, &result, &row_variables) {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })();
        // A row that fails outside field parsing — a required `dateheaders`
        // selector, say — is skipped on its own, and a page whose rows all fail is an empty page, never an indexer error:
        // a header row matched by `rows.selector`, a "no results" row, or a
        // Cloudflare interstitial must not turn into a failing indexer.
        if let Ok(result) = parsed {
            *variables = row_variables;
            if let Some(result) = result {
                results.push(result);
            }
        }
        index += definition.search.rows.after + 1;
    }
    Ok(results)
}

fn previous_date_header(
    definition: &Definition,
    row: &ElementRef<'_>,
    field: &SelectorField,
    variables: &Variables,
) -> Result<Option<String>, String> {
    let mut current = Some(*row);
    while let Some(element) = current {
        for sibling in element.prev_siblings().filter_map(ElementRef::wrap) {
            let value = select_element_value(definition, &sibling, field, variables, false)?;
            if !value.is_empty() {
                return Ok(Some(value));
            }
        }
        current = element.parent().and_then(ElementRef::wrap);
    }
    Ok(None)
}

/// Whether this search carried an external id, read back from the `.Query.*ID`
/// bindings [`search_variables`] made. Nothing new is exposed to definitions:
/// these are the same names a definition already reads.
fn is_id_search(variables: &Variables) -> bool {
    [
        ".Query.IMDBID",
        ".Query.TMDBID",
        ".Query.TVDBID",
        ".Query.TVRageID",
        ".Query.TVMazeID",
        ".Query.TraktID",
        ".Query.DoubanID",
    ]
    .iter()
    .any(|name| {
        variables
            .get(*name)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// The release filter `andmatch` means.
///
/// The query is split on runs of non-word characters, one-character terms and
/// the common words are dropped, and a release passes when a term appears in its
/// title *or its description*. With more than one term two must match; with one,
/// one. An RSS search and an id search are not filtered at all. Requiring every
/// whitespace-separated token in the title alone dropped releases the reference
/// engine keeps.
fn should_keep_row(
    definition: &Definition,
    result: &PluginSearchResult,
    variables: &Variables,
) -> bool {
    if !definition
        .search
        .rows
        .filters
        .iter()
        .any(|filter| filter.name.eq_ignore_ascii_case("andmatch"))
    {
        return true;
    }
    let query = variables
        .get(".Query.Q")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if query.trim().is_empty() || is_id_search(variables) {
        return true;
    }
    const COMMON_WORDS: [&str; 4] = ["and", "the", "an", "of"];
    static SEPARATOR: OnceLock<regex::Regex> = OnceLock::new();
    let separator = SEPARATOR
        .get_or_init(|| regex::Regex::new(r"[^\w]+").expect("static query separator regex"));
    let terms = separator
        .split(query)
        .filter(|term| {
            term.chars().count() > 1
                && !COMMON_WORDS
                    .iter()
                    .any(|common| common.eq_ignore_ascii_case(term))
        })
        .collect::<Vec<_>>();
    let title = result.title.to_lowercase();
    let description = result
        .provider_extra
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let matches = terms
        .iter()
        .filter(|term| {
            let term = term.to_lowercase();
            title.contains(&term) || description.contains(&term)
        })
        .count();
    let required = if terms.len() > 1 { 2 } else { 1 };
    matches >= required
}

/// Parse one HTML row into a release.
///
/// A field that cannot be read — a selector that missed a required field, a
/// filter that errored on the tracker's own malformed value — binds
/// `.Result.<name>` to null and is skipped; the row is still emitted, because
/// one unreadable date or size is not a reason to lose the release. A row that
/// ends up with no title is the row that gets dropped.
fn parse_markup_row(
    definition: &Definition,
    row: &ElementRef<'_>,
    variables: &mut Variables,
) -> Result<Option<PluginSearchResult>, String> {
    let mut result = PluginSearchResult::default();
    for (field_name, field) in &definition.search.fields {
        let (name, modifiers) = split_field_name(field_name);
        let required = !field.optional && !is_implicitly_optional(name);
        let default_value = || {
            field
                .default_value
                .as_ref()
                .and_then(scalar_to_string)
                .map(|value| render(&value, variables))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let value = match select_element_value(definition, row, field, variables, required) {
            Ok(value) if value.is_empty() && !required => default_value()?,
            Ok(value) => value,
            Err(_) if !required => default_value()?,
            Err(_) => {
                variables.insert(format!(".Result.{name}"), Value::Null);
                continue;
            }
        };
        variables.insert(
            format!(".Result.{name}"),
            if value.is_empty() {
                Value::Null
            } else {
                Value::String(value.clone())
            },
        );
        let base_url = variables
            .get(".SearchUri.AbsoluteUri")
            .and_then(Value::as_str)
            .or_else(|| variables.get(".Config.sitelink").and_then(Value::as_str))
            .or_else(|| definition.links.first().map(String::as_str))
            .unwrap_or_default();
        if apply_result_field(&mut result, name, &modifiers, value, base_url).is_err() {
            variables.insert(format!(".Result.{name}"), Value::Null);
        }
    }
    if result.title.is_empty() {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}

fn parse_json_results(
    definition: &Definition,
    body: &str,
    variables: &mut Variables,
) -> Result<Vec<PluginSearchResult>, String> {
    let document: Value = serde_json::from_str(body)
        .map_err(|error| format!("invalid JSON search response: {error}"))?;
    if let Some(count) = definition.search.rows.count.as_ref() {
        let count = json_field_value(definition, &document, count, variables, false)?;
        if parse_i64(&count).is_some_and(|count| count < 1) {
            return Ok(Vec::new());
        }
    }
    let rows_selector = render(&definition.search.rows.selector, variables)?;
    let rows = match json_select_rows(&document, &rows_selector) {
        Ok(rows) => rows,
        Err(_) if definition.search.rows.missing_attribute_equals_no_results => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let rows = rows
        .into_iter()
        .flat_map(|row| match row {
            Value::Array(rows) => rows.iter().collect::<Vec<_>>(),
            row => vec![row],
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for parent in rows {
        let selected = if let Some(attribute) = definition.search.rows.attribute.as_deref() {
            match json_select_one(parent, attribute) {
                Some(value) => value,
                None if definition.search.rows.missing_attribute_equals_no_results => continue,
                None => return Err(format!("JSON rows attribute `{attribute}` did not match")),
            }
        } else {
            parent
        };
        let values = if definition.search.rows.multiple {
            match selected {
                Value::Array(values) => values.iter().collect::<Vec<_>>(),
                Value::Object(values) => values.values().collect::<Vec<_>>(),
                _ => vec![selected],
            }
        } else {
            vec![selected]
        };
        for row in values {
            let mut result = PluginSearchResult::default();
            for (field_name, field) in &definition.search.fields {
                let (name, modifiers) = split_field_name(field_name);
                let required = !field.optional && !is_implicitly_optional(name);
                let root = if field
                    .selector
                    .as_deref()
                    .is_some_and(|selector| selector.trim_start().starts_with(".."))
                {
                    parent
                } else {
                    row
                };
                let value = json_field_value(definition, root, field, variables, required)
                    .map_err(|error| format!("JSON field `{field_name}`: {error}"))?;
                variables.insert(
                    format!(".Result.{name}"),
                    if value.is_empty() {
                        Value::Null
                    } else {
                        Value::String(value.clone())
                    },
                );
                let base_url = variables
                    .get(".SearchUri.AbsoluteUri")
                    .and_then(Value::as_str)
                    .or_else(|| definition.links.first().map(String::as_str))
                    .unwrap_or_default();
                apply_result_field(&mut result, name, &modifiers, value, base_url)?;
            }
            normalize_result(definition, &mut result);
            if !result.title.is_empty() && should_keep_row(definition, &result, variables) {
                results.push(result);
            }
        }
    }
    Ok(results)
}

fn json_field_value(
    definition: &Definition,
    root: &Value,
    field: &SelectorField,
    variables: &Variables,
    required: bool,
) -> Result<String, String> {
    if let Some(text) = field.text.as_ref().and_then(scalar_to_string) {
        return apply_definition_filters(
            definition,
            &render(&text, variables)?,
            &field.filters,
            variables,
        );
    }
    let selector = field
        .selector
        .as_deref()
        .unwrap_or("")
        .trim_start_matches("..");
    let selected = json_select_one(root, &render(selector, variables)?);
    let mut value = match selected {
        Some(value) if let Some(attribute) = field.attribute.as_deref() => {
            json_select_one(value, attribute)
                .map(json_value_string)
                .ok_or_else(|| format!("attribute `{attribute}` did not match"))?
        }
        Some(value) => json_value_string(value),
        None if !required => field
            .default_value
            .as_ref()
            .and_then(scalar_to_string)
            .map(|value| render(&value, variables))
            .transpose()?
            .unwrap_or_default(),
        None => return Err(format!("selector `{selector}` did not match")),
    };
    if let Some(mapped) = field.case.as_ref().and_then(|cases| {
        cases
            .get(&value)
            .or_else(|| cases.get("*"))
            .map(|mapped| render(mapped, variables))
    }) {
        value = mapped?;
    }
    apply_definition_filters(definition, &value, &field.filters, variables)
}

fn json_select_many<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>, String> {
    let (path, pseudos) = split_json_selector(path)?;
    let values = json_select_path_many(root, &path)?;
    let values = values
        .into_iter()
        .filter(|value| json_pseudos_match(value, &pseudos))
        .collect::<Vec<_>>();
    if values.is_empty() {
        Err(format!("JSON rows selector `{path}` did not match"))
    } else {
        Ok(values)
    }
}

fn json_select_path_many<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>, String> {
    let path = path.trim().trim_start_matches('$').trim_start_matches('.');
    if path.is_empty() {
        return Ok(vec![root]);
    }
    let mut values = vec![root];
    for segment in path.split('.') {
        let (name, accessor) = segment.split_once('[').unwrap_or((segment, ""));
        let accessor = accessor.strip_suffix(']');
        let mut next = Vec::new();
        for value in values {
            let value = if name.is_empty() {
                Some(value)
            } else {
                value.get(name)
            };
            let Some(value) = value else {
                continue;
            };
            match accessor {
                Some("*") | Some("") if segment.contains('[') => match value {
                    Value::Array(values) => next.extend(values),
                    Value::Object(values) => next.extend(values.values()),
                    _ => {}
                },
                Some(index) => {
                    if let Ok(index) = index.parse::<usize>()
                        && let Some(value) = value.get(index)
                    {
                        next.push(value);
                    }
                }
                None => next.push(value),
            }
        }
        values = next;
    }
    if values.is_empty() {
        Err(format!("JSON rows selector `{path}` did not match"))
    } else {
        Ok(values)
    }
}

fn json_select_rows<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>, String> {
    let (path, pseudos) = split_json_selector(path)?;
    let selected = json_select_path_many(root, &path)?;
    let mut rows = Vec::new();
    for value in selected {
        match value {
            Value::Array(values) => rows.extend(
                values
                    .iter()
                    .filter(|row| json_pseudos_match(row, &pseudos)),
            ),
            Value::Object(values) if !pseudos.is_empty() => rows.extend(
                values
                    .values()
                    .filter(|row| json_pseudos_match(row, &pseudos)),
            ),
            value if json_pseudos_match(value, &pseudos) => rows.push(value),
            _ => {}
        }
    }
    Ok(rows)
}

#[derive(Debug)]
enum JsonPseudo {
    Has(String),
    Not(String),
    Contains(String),
}

fn split_json_selector(path: &str) -> Result<(String, Vec<JsonPseudo>), String> {
    let path = path.trim();
    let mut base_end = path.len();
    let mut quote = None;
    let mut depth = 0usize;
    for (index, character) in path.char_indices() {
        if quote.is_some() {
            if Some(character) == quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '\"' => quote = Some(character),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => {
                base_end = index;
                break;
            }
            _ => {}
        }
    }
    let base = path[..base_end].to_string();
    let mut remaining = &path[base_end..];
    let mut pseudos = Vec::new();
    while !remaining.is_empty() {
        let Some(rest) = remaining.strip_prefix(':') else {
            return Err(format!("invalid JSON selector `{path}`"));
        };
        let Some(open) = rest.find('(') else {
            return Err(format!("invalid JSON selector `{path}`"));
        };
        let name = &rest[..open];
        let mut quote = None;
        let mut depth = 0usize;
        let mut close = None;
        for (index, character) in rest[open..].char_indices() {
            if quote.is_some() {
                if Some(character) == quote {
                    quote = None;
                }
                continue;
            }
            match character {
                '\'' | '\"' => quote = Some(character),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(open + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| format!("invalid JSON selector `{path}`"))?;
        let key = rest[open + 1..close].to_string();
        match name {
            "has" => pseudos.push(JsonPseudo::Has(key)),
            "not" => pseudos.push(JsonPseudo::Not(key)),
            "contains" => pseudos.push(JsonPseudo::Contains(key)),
            _ => return Err(format!("unsupported JSON selector `:{name}`")),
        }
        remaining = &rest[close + 1..];
    }
    Ok((base, pseudos))
}

fn json_pseudos_match(value: &Value, pseudos: &[JsonPseudo]) -> bool {
    pseudos.iter().all(|pseudo| match pseudo {
        JsonPseudo::Has(selector) => json_selector_matches(value, selector),
        JsonPseudo::Not(selector) => !json_selector_matches(value, selector),
        JsonPseudo::Contains(needle) => value.to_string().contains(needle),
    })
}

fn json_selector_matches(root: &Value, selector: &str) -> bool {
    let Ok((path, pseudos)) = split_json_selector(selector) else {
        return false;
    };
    json_select_path_many(root, &path).is_ok_and(|values| {
        values
            .into_iter()
            .any(|value| json_pseudos_match(value, &pseudos))
    })
}

fn json_select_one<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    json_select_many(root, path).ok()?.into_iter().next()
}

fn json_value_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => values
            .iter()
            .map(json_value_string)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

fn split_field_name(field: &str) -> (&str, Vec<&str>) {
    let mut parts = field.split('|');
    (parts.next().unwrap_or(field), parts.collect())
}

fn is_implicitly_optional(name: &str) -> bool {
    matches!(
        name,
        "category"
            | "categorydesc"
            | "comments"
            | "description"
            | "download"
            | "downloadvolumefactor"
            | "files"
            | "genre"
            | "grabs"
            | "imdb"
            | "imdbid"
            | "infohash"
            | "leechers"
            | "magnet"
            | "minimumratio"
            | "minimumseedtime"
            | "poster"
            | "seeders"
            | "tmdbid"
            | "tvdbid"
            | "uploadvolumefactor"
            | "year"
    ) || name.starts_with('_')
}

fn apply_result_field(
    result: &mut PluginSearchResult,
    name: &str,
    modifiers: &[&str],
    value: String,
    base_url: &str,
) -> Result<(), String> {
    let absolute = |value: &str| {
        Url::parse(value)
            .map(|url| url.to_string())
            .or_else(|_| {
                Url::parse(base_url)
                    .and_then(|base| base.join(value))
                    .map(|url| url.to_string())
            })
            .unwrap_or_else(|_| value.to_string())
    };
    match name {
        "title" => {
            if modifiers.contains(&"append") {
                result.title.push_str(&value)
            } else {
                result.title = value
            }
        }
        "details" => result.info_url = Some(absolute(&value)),
        "comments" if result.comment_url.is_none() => result.comment_url = Some(absolute(&value)),
        "download" => {
            if value.is_empty() {
                result.download_url = None;
            } else if value.starts_with("magnet:") {
                result.magnet_url = Some(value.clone())
            } else {
                result.download_url = Some(absolute(&value))
            }
            if !value.is_empty() {
                result.guid = Some(value);
            }
        }
        "magnet" => result.magnet_url = Some(value),
        "infohash" => result.info_hash_v1 = Some(value),
        "size" => result.size_bytes = parse_size(&value),
        "date" => result.published_at = Some(value),
        "seeders" => result.seeders = parse_i64(&value),
        "leechers" => result.leechers = parse_i64(&value),
        "grabs" => result.grabs = parse_i64(&value),
        "downloadvolumefactor" => result.download_volume_factor = value.parse().ok(),
        "uploadvolumefactor" => result.upload_volume_factor = value.parse().ok(),
        "minimumratio" => result.minimum_seed_ratio = value.parse().ok(),
        "minimumseedtime" => {
            result.minimum_seed_time_minutes =
                parse_i64(&value).map(|seconds| (seconds.saturating_add(59)).div_euclid(60))
        }
        "category" | "categorydesc" => {
            if !value.is_empty() {
                result.provider_categories.push(value)
            }
        }
        "imdb" | "imdbid" | "tmdbid" | "tvdbid" | "tvmazeid" | "traktid" | "doubanid" => {
            result
                .external_ids
                .insert(name.trim_end_matches("id").to_string(), value);
        }
        other => {
            result
                .provider_extra
                .insert(other.to_string(), Value::String(value));
        }
    }
    result.source_kind = Some(IndexerSourceKind::Torrent);
    result.protocol = Some(IndexerProtocol::Torrent);
    if result.seeders.is_some() || result.leechers.is_some() {
        result.peers =
            Some(result.seeders.unwrap_or_default() + result.leechers.unwrap_or_default());
    }
    Ok(())
}

fn normalize_result(definition: &Definition, result: &mut PluginSearchResult) {
    if let Some(published_at) = result.published_at.as_deref() {
        result.published_at = Some(normalize_unknown_date(published_at));
    }
    if let Some(mappings) = definition
        .caps
        .get("categorymappings")
        .and_then(serde_yaml::Value::as_sequence)
    {
        for provider_category in &result.provider_categories {
            for mapping in mappings {
                let Some(mapping) = mapping.as_mapping() else {
                    continue;
                };
                let category = mapping
                    .get(serde_yaml::Value::String("cat".into()))
                    .and_then(scalar_to_string);
                let id = mapping
                    .get(serde_yaml::Value::String("id".into()))
                    .and_then(scalar_to_string);
                let description = mapping
                    .get(serde_yaml::Value::String("desc".into()))
                    .and_then(scalar_to_string);
                if (id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(provider_category))
                    || description.as_deref().is_some_and(|description| {
                        description.eq_ignore_ascii_case(provider_category)
                    }))
                    && let Some(category) = category
                {
                    result.categories.push(category);
                }
            }
        }
        result.categories.sort();
        result.categories.dedup();
    }
    if result.info_hash_v1.is_none() {
        result.info_hash_v1 = result.magnet_url.as_deref().and_then(magnet_info_hash);
    }
    if result.magnet_url.is_none()
        && definition.definition_type.eq_ignore_ascii_case("public")
        && let Some(hash) = result.info_hash_v1.as_deref()
    {
        result.magnet_url = Some(public_magnet_link(hash, &result.title));
    }
    if result.guid.is_none() {
        result.guid = result
            .download_url
            .clone()
            .or_else(|| result.magnet_url.clone())
            .or_else(|| result.info_url.clone())
            .or_else(|| result.info_hash_v1.clone());
    }
    if result
        .provider_extra
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|description| description.starts_with("Internal"))
        && !result.indexer_flags.iter().any(|flag| flag == "Internal")
    {
        result.indexer_flags.push("Internal".to_string());
    }
}

/// The public tracker list put on every magnet synthesized from a bare
/// infohash. A magnet with no `tr=` entry is only
/// resolvable through DHT, which several clients do not run.
const PUBLIC_MAGNET_TRACKERS: [&str; 20] = [
    "http://tracker.opentrackr.org:1337/announce",
    "udp://tracker.auctor.tv:6969/announce",
    "udp://opentracker.i2p.rocks:6969/announce",
    "https://opentracker.i2p.rocks:443/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.moeking.me:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://uploads.gamecoast.net:6969/announce",
    "udp://tracker1.bt.moack.co.kr:80/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://tracker.theoks.net:6969/announce",
    "udp://tracker.skyts.net:6969/announce",
    "udp://tracker-udp.gbitt.info:80/announce",
    "udp://open.tracker.ink:6969/announce",
    "udp://movies.zsw.ca:6969/announce",
];

/// A magnet link for a bare infohash, carrying the public tracker list.
fn public_magnet_link(info_hash: &str, title: &str) -> String {
    let encode =
        |value: &str| url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let mut magnet = format!("magnet:?xt=urn:btih:{info_hash}&dn={}", encode(title));
    for tracker in PUBLIC_MAGNET_TRACKERS {
        magnet.push_str("&tr=");
        magnet.push_str(&encode(tracker));
    }
    magnet
}

fn magnet_info_hash(value: &str) -> Option<String> {
    Url::parse(value)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| {
            (key.eq_ignore_ascii_case("xt"))
                .then(|| value.strip_prefix("urn:btih:").map(str::to_string))
                .flatten()
        })
}

fn parse_i64(value: &str) -> Option<i64> {
    value
        .chars()
        .filter(|character| character.is_ascii_digit() || *character == '-')
        .collect::<String>()
        .parse()
        .ok()
}

/// Normalize a non-integer number the way the reference engine does.
///
/// Everything but digits, `.` and `,` is dropped; `,` becomes `.`; and when more
/// than one `.` survives, all but the last are removed, so the thousands
/// separators of `1.234,5` fall away and the value reads `1234.5`.
fn normalize_number(value: &str) -> Option<f64> {
    let digits = value
        .chars()
        .filter(|character| character.is_ascii_digit() || *character == '.' || *character == ',')
        .collect::<String>()
        .replace(',', ".");
    if digits.is_empty() {
        return None;
    }
    let normalized = if digits.matches('.').count() > 1 {
        let last = digits.rfind('.').expect("counted at least two");
        format!("{}{}", digits[..last].replace('.', ""), &digits[last..])
    } else {
        digits
    };
    normalized.parse::<f64>().ok()
}

/// Parse a size into bytes the way the reference engine does.
///
/// The unit is every letter of the value, lower-cased, with `i` removed, so
/// `GiB`, `GIB` and `GB` are the same unit — and every unit named is a power
/// of 1024, never of 1000. (The reference engine strips the `i` before lower-casing,
/// which reads an all-caps `GIB` as bytes; lower-casing first is the intent.)
/// The number is read the `NormalizeNumber` way, which is what makes `1,5 GB`
/// and `1.234,5 MB` parse.
fn parse_size(value: &str) -> Option<i64> {
    if let Ok(bytes) = value.trim().parse::<i64>()
        && bytes >= 0
    {
        return Some(bytes);
    }
    let amount = normalize_number(value)?;
    let unit = value
        .chars()
        .filter(|character| character.is_alphabetic())
        .collect::<String>()
        .to_lowercase()
        .replace('i', "");
    let multiplier = if unit.contains("kb") {
        1_024f64
    } else if unit.contains("mb") {
        1_048_576f64
    } else if unit.contains("gb") {
        1_073_741_824f64
    } else if unit.contains("tb") {
        1_099_511_627_776f64
    } else if unit.contains("pb") {
        1_125_899_906_842_624f64
    } else if unit.contains("eb") {
        1_152_921_504_606_846_976f64
    } else {
        1f64
    };
    Some((amount * multiplier).round() as i64)
}

#[cfg(test)]
mod tests {
    use scryer_plugin_sdk::PluginSearchContext;

    use super::*;

    #[test]
    fn network_endpoints_skip_result_derived_download_links() {
        for id in ["hdforever-api", "nyaasi", "pwtorrents", "hdturk", "tmghub"] {
            let definition =
                parse_definition(&crate::baked::definition_yaml(id).unwrap().unwrap()).unwrap();
            let origins = resolve_network_endpoints(&definition, &BTreeMap::new())
                .unwrap_or_else(|error| panic!("{id}: {error}"));
            assert!(
                origins.contains(
                    &Url::parse(&definition.links[0])
                        .unwrap()
                        .origin()
                        .ascii_serialization()
                )
            );
        }
    }

    #[test]
    fn network_endpoints_resolve_reported_definition_defaults() {
        for (id, host, setting) in [
            ("abnormal-api", "api.abn.lol", "apiurl"),
            ("animetosho-xyz", "feed.animetosho.net", "api-url"),
            ("thepiratebay", "apibay.org", "apiurl"),
            ("v3x", "api.v3x.club", "apiurl"),
            ("yggreborn-api", "api.yggreborn.org", "apiurl"),
            ("yts", "movies-api.accel.li", "apiurl"),
        ] {
            let definition =
                parse_definition(&crate::baked::definition_yaml(id).unwrap().unwrap()).unwrap();
            let origins = resolve_network_endpoints(&definition, &BTreeMap::new())
                .unwrap_or_else(|error| panic!("{id}: {error}"));
            assert!(
                origins.contains(&format!("https://{host}")),
                "{id}: {origins:?}"
            );
            let config = BTreeMap::from([(setting.into(), "alternate.example:8443".into())]);
            let origins = resolve_network_endpoints(&definition, &config).unwrap();
            assert!(
                origins.contains(&"https://alternate.example:8443".into()),
                "{id}"
            );
            assert!(
                !origins.contains(&format!("https://{host}")),
                "stale host for {id}"
            );
        }
    }

    #[test]
    fn network_endpoints_declare_every_link_and_legacy_link() {
        let mut definition = parse_definition(
            &crate::baked::definition_yaml("thepiratebay")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        definition.search.fields.clear();
        definition.search.paths[0].path = "/search/{{ .Keywords }}".into();
        definition.links = vec![
            "https://tracker.example/".into(),
            "https://mirror.example:8443/".into(),
            "magnet:?not-a-site".into(),
        ];
        definition.legacy_links = vec![
            "http://old-tracker.example/".into(),
            "https://user:secret@credentialed.example/".into(),
        ];
        let config = BTreeMap::from([("base_url".into(), "https://mirror.example:8443/".into())]);
        assert_eq!(
            resolve_network_endpoints(&definition, &config).unwrap(),
            vec![
                "http://old-tracker.example".to_string(),
                "https://mirror.example:8443".to_string(),
                "https://tracker.example".to_string(),
            ]
        );
    }

    /// Every bundled definition declares each of its links and legacy links.
    #[test]
    fn network_endpoints_for_every_bundled_definition_include_its_links() {
        for row in crate::baked::index().unwrap() {
            if row.id == crate::baked::CUSTOM_DEFINITION_ID {
                continue;
            }
            let definition =
                parse_definition(&crate::baked::definition_yaml(&row.id).unwrap().unwrap())
                    .unwrap();
            let Ok(origins) = resolve_network_endpoints(&definition, &BTreeMap::new()) else {
                // Definitions whose authorities need operator settings are
                // covered by the tests above.
                continue;
            };
            for link in definition.links.iter().chain(&definition.legacy_links) {
                if let Ok(url) = Url::parse(link)
                    && matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                {
                    assert!(
                        origins.contains(&url.origin().ascii_serialization()),
                        "{}: {link}",
                        row.id
                    );
                }
            }
        }
    }

    #[test]
    fn network_endpoints_custom_relative_and_unavailable_authorities() {
        let mut definition = parse_definition(
            &crate::baked::definition_yaml("thepiratebay")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        definition.search.fields.clear();
        definition.links = vec!["https://tracker.example/".into()];
        definition.legacy_links.clear();
        definition.search.paths[0].path = "/search/{{ .Keywords }}".into();
        assert_eq!(
            resolve_network_endpoints(&definition, &BTreeMap::new()).unwrap(),
            vec!["https://tracker.example".to_string()]
        );
        definition.search.paths[0].path =
            "{{ if .Keywords }}search/{{ .Keywords }}{{ else }}recent{{ end }}".into();
        assert_eq!(
            resolve_network_endpoints(&definition, &BTreeMap::new())
                .unwrap()
                .len(),
            1
        );
        for path in [
            "https://{{ .Result.host }}/file",
            "https://{{ .Keywords }}/search",
            "https://{{ .Config.missing }}/search",
            "https://user:password@example.test/path",
            "ftp://example.test/path",
            "https://*.example.test/path",
        ] {
            definition.search.paths[0].path = path.into();
            assert!(
                resolve_network_endpoints(&definition, &BTreeMap::new()).is_err(),
                "{path}"
            );
        }
        definition.search.paths[0].path =
            "//alternate.example:8080/{{ if .Keywords }}search{{ else }}recent{{ end }}".into();
        assert!(
            resolve_network_endpoints(&definition, &BTreeMap::new())
                .unwrap()
                .contains(&"https://alternate.example:8080".into())
        );
    }

    /// Drive one engine future to completion. Every mock boundary below
    /// resolves immediately, so a bare poll loop is enough and keeps the crate
    /// free of an async runtime it would never ship.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    #[derive(Default)]
    struct MockEngineHost {
        responses: Vec<PluginHttpResponse>,
        requests: Vec<PluginHttpRequest>,
        state: Option<Vec<u8>>,
        persisted: Option<Vec<u8>>,
        now: u64,
        paced: Vec<Duration>,
        pace_error: Option<String>,
        traces: Vec<String>,
    }

    impl EngineHost for MockEngineHost {
        async fn http(
            &mut self,
            request: PluginHttpRequest,
        ) -> Result<PluginHttpResponse, EngineError> {
            self.requests.push(request);
            if self.responses.is_empty() {
                return Err(EngineError::Message("unexpected HTTP request".to_string()));
            }
            Ok(self.responses.remove(0))
        }

        async fn state_get(&mut self, _key: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.state.clone())
        }

        async fn state_set(&mut self, _key: &str, value: Vec<u8>) -> Result<(), String> {
            self.persisted = Some(value);
            Ok(())
        }

        async fn wall_now_millis(&mut self) -> Result<u64, String> {
            Ok(self.now)
        }

        async fn pace_request(&mut self, _state_key: &str, delay: Duration) -> Result<(), String> {
            self.paced.push(delay);
            self.pace_error.clone().map_or(Ok(()), Err)
        }

        fn trace(&mut self, line: &str) {
            self.traces.push(line.to_string());
        }
    }

    fn typed_search_failure(
        host: &mut MockEngineHost,
        definition: &str,
        config: BTreeMap<String, String>,
    ) -> PluginError {
        let definition = parse_definition(definition).unwrap();
        match block_on(search_with_host(
            host,
            definition,
            PluginSearchRequest::default(),
            config,
        )) {
            Err(EngineError::Plugin(error)) => error,
            other => panic!("a typed failure expected, got {other:?}"),
        }
    }

    fn redirect(location: Option<&str>) -> PluginHttpResponse {
        PluginHttpResponse {
            status: 302,
            headers: location
                .map(|location| BTreeMap::from([("location".into(), location.into())]))
                .unwrap_or_default(),
            set_cookie_headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn a_redirect_without_a_destination_is_an_invalid_response() {
        let definition = PUBLIC_DEFINITION.replace("search:\n", "followredirect: true\nsearch:\n");
        let mut host = MockEngineHost {
            responses: vec![redirect(None)],
            ..Default::default()
        };
        let error = typed_search_failure(&mut host, &definition, BTreeMap::new());
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_RESPONSE));
        assert_eq!(
            error.public_message,
            "The tracker answered with a redirect that names no destination."
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("HTTP 302 redirect response did not include a Location header")
        );
    }

    #[test]
    fn a_redirect_chain_past_the_hop_limit_is_an_invalid_response() {
        let definition = PUBLIC_DEFINITION.replace("search:\n", "followredirect: true\nsearch:\n");
        let mut host = MockEngineHost {
            responses: (0..6).map(|_| redirect(Some("/loop"))).collect(),
            ..Default::default()
        };
        let error = typed_search_failure(&mut host, &definition, BTreeMap::new());
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_RESPONSE));
        assert_eq!(
            error.public_message,
            "The tracker redirected more times than the plugin follows."
        );
        assert_eq!(host.requests.len(), 6);
    }

    #[test]
    fn a_login_page_without_the_expected_form_is_a_definition_mismatch() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  inputs: { username: \"{{ .Config.username }}\" }\nsearch:\n",
        );
        let mut host = MockEngineHost {
            responses: vec![html_response(200, "<html><p>Maintenance</p></html>")],
            ..Default::default()
        };
        let error = typed_search_failure(
            &mut host,
            &definition,
            BTreeMap::from([("username".to_string(), "synthetic-user".to_string())]),
        );
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_RESPONSE));
        assert_eq!(
            error.public_message,
            "The tracker's login page did not show the login form this definition expects."
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("login form selector `form` did not match")
        );
        assert_eq!(host.requests.len(), 1);
    }

    #[test]
    fn a_login_input_selector_without_a_named_input_is_a_definition_mismatch() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  selectors: true\n  inputs: { \"input.user\": alice }\nsearch:\n",
        );
        let mut host = MockEngineHost {
            responses: vec![html_response(
                200,
                "<form><input class=pass name=pass></form>",
            )],
            ..Default::default()
        };
        let error = typed_search_failure(&mut host, &definition, BTreeMap::new());
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_RESPONSE));
        assert_eq!(
            error.debug_message.as_deref(),
            Some("login input selector `input.user` did not match a named input")
        );
    }

    #[test]
    fn an_unparseable_tracker_url_is_invalid_config_before_any_request() {
        let mut host = MockEngineHost::default();
        let error = typed_search_failure(
            &mut host,
            PUBLIC_DEFINITION,
            BTreeMap::from([("base_url".to_string(), "not a url".to_string())]),
        );
        assert_eq!(error.code, PluginErrorCode::InvalidConfig);
        assert_eq!(
            error.public_message,
            "The indexer has no valid tracker URL."
        );
        assert!(host.requests.is_empty());
    }

    #[test]
    fn request_spacing_past_the_deadline_is_rate_limited() {
        let mut host = MockEngineHost {
            pace_error: Some("tracker request pacing needs 9000 ms".to_string()),
            ..Default::default()
        };
        let error = typed_search_failure(&mut host, PUBLIC_DEFINITION, BTreeMap::new());
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(
            error.debug_message.as_deref(),
            Some("tracker request pacing needs 9000 ms")
        );
        assert!(host.requests.is_empty());
    }

    #[test]
    fn a_captcha_kind_the_plugin_cannot_show_is_unsupported() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  captcha:\n    type: recaptcha\n    selector: div.g-recaptcha\n    input: captcha\nsearch:\n",
        ))
        .unwrap();
        let mut host = MockEngineHost::default();
        let Err(EngineError::Plugin(error)) = block_on(action_with_host(
            &mut host,
            definition,
            EngineAction::CheckCaptcha,
            BTreeMap::new(),
        )) else {
            panic!("a typed failure expected")
        };
        assert_eq!(error.code, PluginErrorCode::Unsupported);
        assert_eq!(
            error.public_message,
            "The tracker uses a kind of CAPTCHA the plugin cannot show."
        );
        assert!(host.requests.is_empty());
    }

    /// Every tracker request writes an `http_trace` line before it and an
    /// `http_trace_response` or `http_trace_error` line after it, with the
    /// configured passkey masked wherever the URL carries it.
    #[test]
    fn tracker_requests_write_a_masked_http_trace() {
        let definition = parse_definition(
            &PUBLIC_DEFINITION
                .replace(
                    "caps:\n",
                    "settings:\n  - { name: passkey, type: text }\ncaps:\n",
                )
                .replace(
                    "    - path: search\n",
                    "    - path: \"rss/{{ .Config.passkey }}/search\"\n",
                ),
        )
        .unwrap();
        let config = BTreeMap::from([("passkey".to_string(), "synthetic+key".to_string())]);
        let mut host = MockEngineHost {
            responses: vec![PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<table></table>".to_vec(),
            }],
            ..Default::default()
        };
        block_on(search_with_host(
            &mut host,
            definition.clone(),
            PluginSearchRequest::default(),
            config.clone(),
        ))
        .unwrap();

        assert_eq!(host.traces.len(), 2, "{:?}", host.traces);
        assert!(
            host.traces[0].starts_with("http_trace plugin=cardigann definition=fixture method=GET url=https://tracker.example/rss/[redacted]/search"),
            "{:?}",
            host.traces
        );
        assert!(
            host.traces[1].starts_with("http_trace_response plugin=cardigann definition=fixture"),
            "{:?}",
            host.traces
        );
        assert!(host.traces[1].ends_with(" status=200"), "{:?}", host.traces);
        for line in &host.traces {
            assert!(!line.contains("synthetic"), "{line}");
        }

        let mut host = MockEngineHost::default();
        assert!(
            block_on(search_with_host(
                &mut host,
                definition,
                PluginSearchRequest::default(),
                config,
            ))
            .is_err()
        );
        assert_eq!(host.traces.len(), 2, "{:?}", host.traces);
        assert!(
            host.traces[1].starts_with("http_trace_error plugin=cardigann definition=fixture"),
            "{:?}",
            host.traces
        );
        assert!(
            host.traces[1].ends_with(" error=unexpected HTTP request"),
            "{:?}",
            host.traces
        );
        assert!(!host.traces[1].contains("synthetic"), "{:?}", host.traces);
    }

    fn compiled(definition: &str) -> String {
        serde_json::to_string(&CompiledIr {
            ir_version: COMPILED_IR_VERSION,
            definition: parse_definition(definition).unwrap(),
        })
        .unwrap()
    }

    const PUBLIC_DEFINITION: &str = r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps:
  categorymappings:
    - { id: 7, cat: Movies }
search:
  paths:
    - path: search
      inputs: { q: "{{ .Keywords }}" }
  rows:
    selector: tr.result
  fields:
    title: { selector: a.title }
    details: { selector: a.title, attribute: href }
    download: { selector: a.download, attribute: href }
    size: { selector: td.size }
    seeders: { selector: td.seeders }
    minimumseedtime: { selector: td.seedtime }
"#;

    /// Match `selector` against `html` the way the engine does: mark the
    /// document with the selector's `:contains` needles, then select.
    fn select_count(html: &str, selector: &str) -> usize {
        let mut document = Html::parse_document(html);
        let selector = parse_selector(selector).unwrap();
        mark_contains(&mut document, &selector.needles);
        document.select(&selector.css).count()
    }

    #[test]
    fn supports_has_and_cardigann_contains_selectors() {
        assert_eq!(
            select_count(
                "<table><tr><td><a>needle</a></td></tr><tr><td>other</td></tr></table>",
                "tr:has(a):contains(\"needle\")",
            ),
            1
        );
    }

    /// `:contains` is evaluated where it is written. Stripping it from the
    /// selector text turned `tr:not(:contains("Sticky"))` into `tr:not()` — the
    /// opposite test, on 51 definitions — and reduced
    /// `tr:has(td.name:contains(…))` to "the row mentions it somewhere".
    #[test]
    fn evaluates_contains_inside_not_and_has_where_it_is_written() {
        const ROWS: &str = r#"<table>
            <tr class=result><td class=name>Sticky Announcement</td><td class=uploader>needle</td></tr>
            <tr class=result><td class=name>Fixture Release needle</td><td class=uploader>someone</td></tr>
            <tr class=result><td class=name>Another Release</td><td class=uploader>someone</td></tr>
        </table>"#;

        // `:not(:contains(...))` excludes only the rows whose text contains it.
        assert_eq!(
            select_count(ROWS, r#"tr.result:not(:contains("Sticky"))"#),
            2
        );
        // `:has(td.name:contains(...))` requires the needle in that cell, not
        // anywhere in the row: row one carries "needle" in the uploader cell.
        assert_eq!(
            select_count(ROWS, r#"tr.result:has(td.name:contains("needle"))"#),
            1
        );
        // Plain top-level `:contains` still means "this element's text".
        assert_eq!(select_count(ROWS, r#"td.name:contains("Release")"#), 2);
        // Stacked negations compose.
        assert_eq!(
            select_count(
                ROWS,
                r#"tr.result:not(:contains("Sticky")):not(:contains("Another"))"#,
            ),
            1
        );
        // A quoted needle keeps its whitespace: " GB" is not "GB".
        assert_eq!(
            select_count(
                "<div><span>12GB</span><span>12 GB</span></div>",
                r#"span:contains(" GB")"#
            ),
            1
        );
    }

    /// The whole search parse has to see the marked document: 12 definitions
    /// put `:contains` inside a pseudo in `rows.selector`, and the rest use it
    /// in field and `case:` selectors.
    #[test]
    fn positional_contains_reaches_row_and_field_selectors() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps: {}
search:
  paths:
    - path: search
  rows:
    selector: 'tr.result:not(:contains("Sticky"))'
  fields:
    title: { selector: 'td.name' }
    download: { selector: 'a.download', attribute: href }
    downloadvolumefactor:
      case:
        'td.flags:contains("Free")': "0"
        '*': "1"
"#,
        )
        .unwrap();
        let results = parse_search_response(
            &definition,
            &definition.search.paths[0],
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table>
                    <tr class=result><td class=name>Sticky Announcement</td><td class=flags>Free</td><td><a class=download href='/download/0'>DL</a></td></tr>
                    <tr class=result><td class=name>Fixture One</td><td class=flags>Free</td><td><a class=download href='/download/1'>DL</a></td></tr>
                    <tr class=result><td class=name>Fixture Two</td><td class=flags>Paid</td><td><a class=download href='/download/2'>DL</a></td></tr>
                </table>"#.to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.title.as_str())
                .collect::<Vec<_>>(),
            ["Fixture One", "Fixture Two"],
            "the sticky row is the only one excluded"
        );
        assert_eq!(results[0].download_volume_factor, Some(0.0));
        assert_eq!(results[1].download_volume_factor, Some(1.0));
    }

    /// The `"*"` catch-all is written last in the corpus and must stay last:
    /// a sorted map tests it first, and every freeleech case collapses to the
    /// fallback.
    #[test]
    fn html_and_xml_case_maps_take_the_first_matching_selector() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let html_field: SelectorField =
            serde_yaml::from_str("case:\n  img[src*=\"free\"]: \"0\"\n  \"*\": \"1\"\n").unwrap();
        for (markup, expected) in [
            (
                "<tr class=result><td><img src='/badge-free.png'></td></tr>",
                "0",
            ),
            (
                "<tr class=result><td><img src='/badge-paid.png'></td></tr>",
                "1",
            ),
        ] {
            let document = Html::parse_document(&format!("<table>{markup}</table>"));
            let row = document
                .select(&Selector::parse("tr.result").unwrap())
                .next()
                .unwrap();
            assert_eq!(
                select_element_value(&definition, &row, &html_field, &Variables::new(), true)
                    .unwrap(),
                expected,
                "{markup}"
            );
        }

        // Without a catch-all, an unmatched case is null: an error when the
        // field is required, empty otherwise, never the row's own text.
        let no_fallback: SelectorField =
            serde_yaml::from_str("case:\n  img[src*=\"free\"]: \"0\"\n").unwrap();
        let document = Html::parse_document(
            "<table><tr class=result><td><img src='/badge-paid.png'>row text</td></tr></table>",
        );
        let row = document
            .select(&Selector::parse("tr.result").unwrap())
            .next()
            .unwrap();
        assert!(
            select_element_value(&definition, &row, &no_fallback, &Variables::new(), true).is_err()
        );
        assert_eq!(
            select_element_value(&definition, &row, &no_fallback, &Variables::new(), false)
                .unwrap(),
            ""
        );

        let xml_field: SelectorField =
            serde_yaml::from_str("case:\n  freeleech: \"0\"\n  \"*\": \"1\"\n").unwrap();
        for (markup, expected) in [
            ("<item><freeleech>yes</freeleech></item>", "0"),
            ("<item><paid>yes</paid></item>", "1"),
        ] {
            let source = format!("<rss>{markup}</rss>");
            let document = XmlDocument::parse(&source).unwrap();
            let item = xml_select_many(document.root(), "item")
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                xml_field_value(&definition, item, &xml_field, &Variables::new(), true).unwrap(),
                expected,
                "{markup}"
            );
        }
    }

    /// The host keys ids `<source>_id`. Reading only the bare Cardigann
    /// spellings left every `.Query.*ID` null, which silently turned an id
    /// search into an empty keyword search.
    #[test]
    fn binds_host_id_keys_and_cardigann_search_types() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let request = PluginSearchRequest {
            query: "fixture".to_string(),
            ids: std::collections::HashMap::from([
                ("imdb_id".to_string(), "tt0111161".to_string()),
                ("tmdb_id".to_string(), "278".to_string()),
                ("tvdb_id".to_string(), "81189".to_string()),
                ("tvrage_id".to_string(), "18164".to_string()),
                ("tvmaze_id".to_string(), "169".to_string()),
            ]),
            facet: Some("series".to_string()),
            category: None,
            categories: Vec::new(),
            limit: 100,
            season: None,
            episode: None,
            absolute_episode: None,
            tagged_aliases: Vec::new(),
            context: None,
            ..PluginSearchRequest::default()
        };
        let variables = search_variables(&definition, &BTreeMap::new(), &request).unwrap();
        let value = |name: &str| {
            variables
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(value(".Query.IMDBID"), "tt0111161");
        assert_eq!(value(".Query.IMDBIDShort"), "0111161");
        assert_eq!(value(".Query.TMDBID"), "278");
        assert_eq!(value(".Query.TVDBID"), "81189");
        assert_eq!(value(".Query.TVRageID"), "18164");
        assert_eq!(value(".Query.TVMazeID"), "169");
        assert_eq!(value(".Query.Type"), "tvsearch");

        // The bare spellings a pasted definition may still carry keep working,
        // and a bare id is promoted to the `tt` form the template binds.
        let mut bare = request.clone();
        bare.ids = std::collections::HashMap::from([("imdbid".to_string(), "111161".to_string())]);
        bare.facet = Some("movie".to_string());
        let variables = search_variables(&definition, &BTreeMap::new(), &bare).unwrap();
        assert_eq!(
            variables.get(".Query.IMDBID").and_then(Value::as_str),
            Some("tt0111161")
        );
        assert_eq!(
            variables.get(".Query.IMDBIDShort").and_then(Value::as_str),
            Some("111161")
        );
        assert_eq!(
            variables.get(".Query.Type").and_then(Value::as_str),
            Some("movie")
        );

        for (facet, expected) in [
            (Some("movie"), "movie-search"),
            (Some("series"), "tv-search"),
            (Some("anime"), "tv-search"),
            (Some("special"), "tv-search"),
            (Some("collection"), "movie-search"),
            (Some("title"), "search"),
            (Some("music"), "search"),
            (None, "search"),
        ] {
            assert_eq!(cardigann_search_type(facet), expected, "{facet:?}");
        }

        let mut empty = request.clone();
        empty.ids = std::collections::HashMap::from([("imdb_id".to_string(), "  ".to_string())]);
        let variables = search_variables(&definition, &BTreeMap::new(), &empty).unwrap();
        assert_eq!(variables.get(".Query.IMDBID"), Some(&Value::Null));
        assert_eq!(variables.get(".Query.IMDBIDShort"), Some(&Value::Null));
    }

    /// With `supported_ids` declared plugin-wide the host routes id-only
    /// strategies to every configured Cardigann indexer. A definition whose
    /// `caps.modes` cannot read the id would render an empty keyword search and
    /// hand back its front page, so it must not be asked at all.
    #[test]
    fn gates_id_only_searches_on_the_definitions_caps_modes() {
        let with_modes = |modes: &str| {
            PUBLIC_DEFINITION.replace("caps:\n", &format!("caps:\n  modes:\n{modes}"))
        };
        let id_request = |facet: &str, key: &str, value: &str| PluginSearchRequest {
            query: String::new(),
            ids: std::collections::HashMap::from([(key.to_string(), value.to_string())]),
            facet: Some(facet.to_string()),
            ..Default::default()
        };
        let begin_search = |definition: &str, request: PluginSearchRequest| {
            begin(
                compiled(definition),
                Operation::Search(Box::new(request)),
                BTreeMap::new(),
            )
            .unwrap()
        };

        // The mode lists the id: the search proceeds.
        assert!(matches!(
            begin_search(
                &with_modes("    search: [q]\n    movie-search: [q, imdbid]\n"),
                id_request("movie", "imdb_id", "tt0111161"),
            ),
            Step::NeedHttp { .. }
        ));
        // `tv-search` with a season/episode parameter list still gates on the id.
        assert!(matches!(
            begin_search(
                &with_modes("    search: [q]\n    tv-search: [q, season, ep, tvdbid]\n"),
                id_request("series", "tvdb_id", "81189"),
            ),
            Step::NeedHttp { .. }
        ));

        // The mode does not list the id: zero results, no HTTP request.
        for (definition, request) in [
            (
                with_modes("    search: [q]\n    movie-search: [q]\n"),
                id_request("movie", "imdb_id", "tt0111161"),
            ),
            // The facet's mode is missing entirely.
            (
                with_modes("    search: [q]\n"),
                id_request("movie", "imdb_id", "tt0111161"),
            ),
            // A malformed `modes` block supports nothing.
            (
                PUBLIC_DEFINITION.replace("caps:\n", "caps:\n  modes: notamapping\n"),
                id_request("series", "tvdb_id", "81189"),
            ),
            // No `modes` block at all.
            (
                PUBLIC_DEFINITION.to_string(),
                id_request("series", "tvdb_id", "81189"),
            ),
        ] {
            let Step::Complete { output } = begin_search(&definition, request) else {
                panic!("an unsupported id search must complete without a request")
            };
            assert_eq!(output["results"], serde_json::json!([]));
        }

        // Keywords scope the search on their own, so an unsupported id alongside
        // a query still proceeds.
        let mut with_query = id_request("movie", "imdb_id", "tt0111161");
        with_query.query = "fixture".to_string();
        assert!(matches!(
            begin_search(&with_modes("    movie-search: [q]\n"), with_query),
            Step::NeedHttp { .. }
        ));

        // An RSS request carries neither, and is never gated.
        assert!(matches!(
            begin_search(
                &with_modes("    search: [q]\n"),
                PluginSearchRequest::default()
            ),
            Step::NeedHttp { .. }
        ));
    }

    #[test]
    fn selector_extraction_accepts_the_current_element() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let document = Html::parse_document("<div class=row>current element</div>");
        let row = document
            .select(&Selector::parse("div.row").unwrap())
            .next()
            .unwrap();
        let value = select_element_value(
            &definition,
            &row,
            &SelectorField {
                selector: Some("div.row".to_string()),
                ..SelectorField::default()
            },
            &Variables::new(),
            true,
        )
        .unwrap();
        assert_eq!(value, "current element");
    }

    #[test]
    fn skips_malformed_markup_and_xml_rows_and_treats_no_rows_as_empty() {
        let html = parse_definition(PUBLIC_DEFINITION).unwrap();
        let html_path = &html.search.paths[0];
        let mut variables = Variables::new();
        let results = parse_search_response(
            &html,
            html_path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=download href='/broken'>DL</a></td></tr><tr class=result><td><a class=title>valid</a><a class=download href='/valid'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            },
            &mut variables,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "valid");

        let xml = parse_definition(
            &PUBLIC_DEFINITION
                .replace("    - path: search", "    - path: search\n      response: { type: xml }")
                .replace("    selector: tr.result", "    selector: item")
                .replace(
                    "    title: { selector: a.title }\n    details: { selector: a.title, attribute: href }\n    download: { selector: a.download, attribute: href }\n    size: { selector: td.size }\n    seeders: { selector: td.seeders }\n    minimumseedtime: { selector: td.seedtime }",
                    "    title: { selector: title }\n    download: { selector: link }",
                ),
        )
        .unwrap();
        let xml_path = &xml.search.paths[0];
        let results = parse_search_response(
            &xml,
            xml_path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<rss><item><link>/broken</link></item><item><title>xml valid</title><link>/valid</link></item></rss>".to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "xml valid");

        // A page whose only matched row yields no title is an empty result, the
        // way the reference engine reports it. The previous "all HTML rows failed" error
        // turned a decorative header row, a "no results" row, or a Cloudflare
        // interstitial into a failing indexer.
        let results = parse_search_response(
            &html,
            html_path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=download href='/broken'>DL</a></td></tr></table>"#.to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert!(results.is_empty());

        let results = parse_search_response(
            &xml,
            xml_path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<rss><item><link>/broken</link></item></rss>".to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert!(results.is_empty());
    }

    /// The reference engine records a field error, binds `.Result.<name>` to
    /// null, and still emits the release. Dropping the row instead is how a single
    /// divergent filter — a date layout, a regex dialect — turned into missing
    /// releases.
    #[test]
    fn a_failing_field_filter_keeps_the_release_without_that_field() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps: {}
search:
  paths:
    - path: search
  rows:
    selector: tr.result
  fields:
    title: { selector: a.title }
    download: { selector: a.download, attribute: href }
    date:
      selector: td.date
      filters:
        - { name: timeago }
    size: { selector: td.size }
"#,
        )
        .unwrap();
        let results = parse_search_response(
            &definition,
            &definition.search.paths[0],
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title>Fixture Release</a><a class=download href='/download/1'>DL</a></td><td class=date>not a date</td><td class=size>1 GiB</td></tr></table>"#.to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Fixture Release");
        assert_eq!(results[0].size_bytes, Some(1_073_741_824));
        assert!(results[0].published_at.is_none());
    }

    #[test]
    fn applies_no_results_message_only_to_json_responses() {
        let mut definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        definition.search.paths[0].response.no_results_message = Some("No results".to_string());
        let path = &definition.search.paths[0];
        let results = parse_search_response(
            &definition,
            path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table>No results<tr class=result><td><a class=title>still valid</a><a class=download href='/download'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            },
            &mut Variables::new(),
        )
        .unwrap();
        assert_eq!(results[0].title, "still valid");
    }

    #[test]
    fn removes_expired_cookies_and_expires_persisted_sessions() {
        let mut cookies = BTreeMap::from([("session".to_string(), "live".to_string())]);
        assert!(merge_cookie_header(
            &mut cookies,
            "session=; Max-Age=0; Path=/"
        ));
        assert!(cookies.is_empty());
        cookies.insert("session".to_string(), "live".to_string());
        assert!(merge_cookie_header(
            &mut cookies,
            "session=; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Path=/",
        ));
        assert!(cookies.is_empty());
        cookies.insert("session".to_string(), "live".to_string());
        assert!(merge_response_cookies(
            &mut cookies,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::from([(
                    "set-cookie".to_string(),
                    "session=; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Path=/".to_string(),
                )]),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            },
        ));
        assert!(cookies.is_empty());

        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let session = EngineSession {
            cookies: BTreeMap::from([("stale".to_string(), "cookie".to_string())]),
            expires_at_millis: Some(99),
        };
        let mut host = MockEngineHost {
            state: Some(serde_json::to_vec(&session).unwrap()),
            now: 100,
            responses: vec![PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title>fresh</a><a class=download href='/download'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            }],
            ..Default::default()
        };
        block_on(search_with_host(
            &mut host,
            definition,
            PluginSearchRequest::default(),
            BTreeMap::new(),
        ))
        .unwrap();
        assert!(!host.requests[0].headers.contains_key("cookie"));
    }

    /// The operator's `cookie` setting and a definition's `login.cookies` are
    /// browser `Cookie` headers, not `Set-Cookie` fields. Reading them with the
    /// `Set-Cookie` parser kept the first pair and dropped every other cookie of
    /// the session, which is the login method of most cookie-login definitions.
    #[test]
    fn parses_the_cookie_setting_and_login_cookies_as_a_cookie_header() {
        assert_eq!(
            parse_cookie_header("uid=1; pass=abc; Path=/"),
            [
                ("uid".to_string(), "1".to_string()),
                ("pass".to_string(), "abc".to_string()),
            ]
        );
        // Every attribute name the header may carry is skipped, whatever its
        // case, and the cookies around it survive.
        assert_eq!(
            parse_cookie_header(
                "uid=1; Domain=.tracker.example; HttpOnly; secure; SameSite=Lax; max-age=600; session=xyz",
            ),
            [
                ("uid".to_string(), "1".to_string()),
                ("session".to_string(), "xyz".to_string()),
            ]
        );

        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: cookie\n  cookies:\n    - 'first=one; second=two'\nsearch:\n",
        ))
        .unwrap();
        let cookies = initial_cookies(
            &definition,
            &BTreeMap::from([("cookie".to_string(), "uid=1; pass=abc; Path=/".to_string())]),
        );
        assert_eq!(
            cookies,
            BTreeMap::from([
                ("first".to_string(), "one".to_string()),
                ("second".to_string(), "two".to_string()),
                ("uid".to_string(), "1".to_string()),
                ("pass".to_string(), "abc".to_string()),
            ])
        );

        // The driver's own read of the setting agrees with the flow's.
        let mut host = MockEngineHost {
            responses: vec![PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title>one</a><a class=download href='/download'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            }],
            ..Default::default()
        };
        block_on(search_with_host(
            &mut host,
            parse_definition(PUBLIC_DEFINITION).unwrap(),
            PluginSearchRequest::default(),
            BTreeMap::from([("cookie".to_string(), "uid=1; pass=abc; Path=/".to_string())]),
        ))
        .unwrap();
        let sent = host.requests[0].headers["cookie"].clone();
        assert!(sent.contains("uid=1"), "{sent}");
        assert!(sent.contains("pass=abc"), "{sent}");
        assert!(!sent.to_ascii_lowercase().contains("path"), "{sent}");
    }

    #[test]
    fn deduplicates_identical_get_search_urls() {
        let definition = PUBLIC_DEFINITION.replace(
            "    - path: search\n      inputs: { q: \"{{ .Keywords }}\" }",
            "    - path: search\n      inputs: { q: \"{{ .Keywords }}\" }\n    - path: search\n      inputs: { q: \"{{ .Keywords }}\" }",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let complete = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title>one</a><a class=download href='/download'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            }),
        )
        .unwrap();
        assert!(matches!(complete, Step::Complete { .. }));
    }

    #[test]
    fn resolves_simple_captcha_before_submitting_form_login() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("login landing request expected")
        };
        let Step::NeedHttp {
            request,
            continuation,
        } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<form action='/session'><input name='user' value='alice'></form><script src='/simpleCaptcha.js'></script>"#.to_vec(),
            }),
        )
        .unwrap() else {
            panic!("simple CAPTCHA request expected")
        };
        assert_eq!(
            request.url,
            "https://tracker.example/simpleCaptcha.php?numImages=1"
        );
        let Step::NeedHttp { request, .. } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"{"images":[{"hash":"selection"}]}"#.to_vec(),
            }),
        )
        .unwrap() else {
            panic!("login submit expected")
        };
        let body = String::from_utf8(request.body).unwrap();
        assert!(body.contains("captchaSelection=selection"));
        assert!(body.contains("submitme=X"));
    }

    #[test]
    fn rejects_cross_origin_captcha_images_without_sending_a_request() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  captcha:\n    type: image\n    selector: img.captcha\n    input: captcha\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("login landing request expected")
        };
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<form><img class=captcha src='https://captcha.example/challenge.png'></form>"#.to_vec(),
            }),
        ));
        assert_eq!(error.code, PluginErrorCode::Unsupported);
        assert_eq!(
            error.public_message,
            "The tracker's CAPTCHA image is served from another site, which the plugin does \
             not load."
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("cross-origin CAPTCHA image is blocked by the configured-origin policy")
        );
    }

    #[test]
    fn engine_driver_persists_cookies_and_paces_requests() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replacen(
            "type: public",
            "requestDelay: 2\ntype: public",
            1,
        ))
        .unwrap();
        let session = EngineSession {
            cookies: BTreeMap::from([("old".to_string(), "cookie".to_string())]),
            expires_at_millis: Some(31 * 24 * 60 * 60 * 1_000),
        };
        let mut host = MockEngineHost {
            state: Some(serde_json::to_vec(&session).unwrap()),
            now: 1_500,
            responses: vec![PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                // Repeated `Set-Cookie` fields reach the guest intact under the
                // component ABI, so the jar is built here rather than from a
                // host-synthesized side-channel header.
                set_cookie_headers: vec![
                    "session=new; Path=/".to_string(),
                    "theme=dark; Path=/".to_string(),
                ],
                body: br#"<table><tr class=result><td><a class=title>Release</a><a class=download href='/download/1'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            }],
            ..Default::default()
        };
        let response = block_on(search_with_host(
            &mut host,
            definition,
            PluginSearchRequest::default(),
            BTreeMap::new(),
        ))
        .unwrap();
        assert_eq!(response.results[0].title, "Release");
        assert_eq!(host.paced, [Duration::from_secs(2)]);
        assert!(
            host.requests[0]
                .headers
                .get("cookie")
                .is_some_and(|value| value.contains("old=cookie"))
        );
        let persisted: EngineSession =
            serde_json::from_slice(host.persisted.as_ref().unwrap()).unwrap();
        assert_eq!(persisted.cookies["session"], "new");
        assert_eq!(persisted.cookies["theme"], "dark");

        let mut grab_host = MockEngineHost {
            responses: vec![PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"torrent".to_vec(),
            }],
            ..Default::default()
        };
        let grab = block_on(action_with_host(
            &mut grab_host,
            parse_definition(PUBLIC_DEFINITION).unwrap(),
            EngineAction::Grab("https://tracker.example/download/1".to_string()),
            BTreeMap::new(),
        ))
        .unwrap();
        assert_eq!(
            grab["body"],
            serde_json::json!([116, 111, 114, 114, 101, 110, 116])
        );
    }

    /// A login without a CAPTCHA has nothing to show: the reply is empty and
    /// the tracker is not contacted.
    #[test]
    fn captcha_action_without_a_captcha_asks_for_nothing() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let mut host = MockEngineHost::default();
        let payload = block_on(action_with_host(
            &mut host,
            definition,
            EngineAction::CheckCaptcha,
            BTreeMap::new(),
        ))
        .unwrap();
        assert_eq!(payload, serde_json::json!({}));
        assert!(host.requests.is_empty());
    }

    #[test]
    fn captcha_action_returns_fetched_image_payload() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  captcha:\n    type: image\n    selector: img.captcha\n    input: captcha\nsearch:\n",
        ))
        .unwrap();
        let mut host = MockEngineHost {
            responses: vec![
                PluginHttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    set_cookie_headers: Vec::new(),
                    body: br#"<form><img class=captcha src='/captcha.png'></form>"#.to_vec(),
                },
                PluginHttpResponse {
                    status: 200,
                    headers: BTreeMap::from([(
                        "content-type".to_string(),
                        "image/png".to_string(),
                    )]),
                    set_cookie_headers: Vec::new(),
                    body: vec![1, 2, 3],
                },
            ],
            ..Default::default()
        };
        let payload = block_on(action_with_host(
            &mut host,
            definition,
            EngineAction::CheckCaptcha,
            BTreeMap::new(),
        ))
        .unwrap();
        assert_eq!(payload["captchaRequest"]["type"], "image");
        assert_eq!(payload["captchaRequest"]["contentType"], "image/png");
        assert_eq!(payload["captchaRequest"]["imageData"], "AQID");
        assert_eq!(host.requests.len(), 2);
    }

    #[test]
    fn every_repeated_set_cookie_field_reaches_the_jar() {
        let mut cookies = BTreeMap::new();
        assert!(merge_response_cookies(
            &mut cookies,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::from([("content-type".to_string(), "text/html".to_string())]),
                set_cookie_headers: vec![
                    "session=one; Path=/; HttpOnly".to_string(),
                    "theme=dark; Path=/".to_string(),
                ],
                body: b"body".to_vec(),
            },
        ));
        assert_eq!(cookies["session"], "one");
        assert_eq!(cookies["theme"], "dark");
    }

    #[test]
    fn finds_date_headers_through_parent_previous_siblings() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let document = Html::parse_document(
            "<div class=header><span class=date>2026-08-22</span></div><table><tr class=result><td>row</td></tr></table>",
        );
        let row = document
            .select(&Selector::parse("tr.result").unwrap())
            .next()
            .unwrap();
        let value = previous_date_header(
            &definition,
            &row,
            &SelectorField {
                selector: Some("span.date".into()),
                ..SelectorField::default()
            },
            &Variables::new(),
        )
        .unwrap();
        assert_eq!(value.as_deref(), Some("2026-08-22"));
    }

    #[test]
    fn normalizes_category_ids_descriptions_and_infohash_guids() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps:
  categorymappings:
    - { id: 5070, cat: TV, desc: Anime }
search:
  paths: [{ path: search }]
"#,
        )
        .unwrap();
        let mut result = PluginSearchResult {
            title: "Example".into(),
            provider_categories: vec!["5070".into(), "Anime".into()],
            info_hash_v1: Some("0123456789abcdef0123456789abcdef01234567".into()),
            ..PluginSearchResult::default()
        };
        normalize_result(&definition, &mut result);
        assert_eq!(result.provider_categories, ["5070", "Anime"]);
        assert_eq!(result.categories, ["TV"]);
        assert!(
            result
                .magnet_url
                .as_deref()
                .is_some_and(|url| url.contains("btih:"))
        );
        assert_eq!(result.guid, result.magnet_url);
    }

    /// A magnet synthesized from a bare infohash carries the public
    /// tracker list; without it the link only resolves through DHT.
    #[test]
    fn synthesized_magnets_carry_the_public_tracker_list() {
        let expect_trackers = |magnet: &str| {
            for tracker in [
                PUBLIC_MAGNET_TRACKERS[0],
                PUBLIC_MAGNET_TRACKERS[PUBLIC_MAGNET_TRACKERS.len() - 1],
            ] {
                let encoded =
                    url::form_urlencoded::byte_serialize(tracker.as_bytes()).collect::<String>();
                assert!(magnet.contains(&format!("&tr={encoded}")), "{magnet}");
            }
            assert_eq!(magnet.matches("&tr=").count(), PUBLIC_MAGNET_TRACKERS.len());
        };

        // A public definition's `infohash` result field.
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let mut result = PluginSearchResult {
            title: "Fixture Release".into(),
            info_hash_v1: Some("0123456789abcdef0123456789abcdef01234567".into()),
            ..PluginSearchResult::default()
        };
        normalize_result(&definition, &mut result);
        let magnet = result
            .magnet_url
            .expect("a public infohash yields a magnet");
        assert!(
            magnet.starts_with(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Fixture+Release"
            ),
            "{magnet}"
        );
        expect_trackers(&magnet);

        // The `download.infohash` grab path.
        let with_infohash = PUBLIC_DEFINITION.replace(
            "search:\n",
            "download:\n  infohash:\n    hash: { selector: span.hash }\n    title: { selector: span.name }\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&with_infohash),
            Operation::Grab("https://tracker.example/details/1".into()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("details request expected")
        };
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<div><span class=hash>0123456789abcdef0123456789abcdef01234567</span><span class=name>Fixture Release</span></div>"#.to_vec(),
            }),
        )
        .unwrap() else {
            panic!("an infohash grab must complete")
        };
        let magnet = output["url"].as_str().expect("a magnet URL").to_string();
        assert!(magnet.starts_with("magnet:?xt=urn:btih:"), "{magnet}");
        expect_trackers(&magnet);
    }

    const YEAR_DEFINITION: &str = r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps: {}
search:
  paths: [{ path: search }]
"#;

    #[test]
    fn year_reaches_definitions_through_the_typed_search_context() {
        let definition = parse_definition(YEAR_DEFINITION).unwrap();
        let variables = search_variables(
            &definition,
            &BTreeMap::from([(
                "base_url".to_string(),
                "https://tracker.example/".to_string(),
            )]),
            &PluginSearchRequest {
                query: "the thing".to_string(),
                context: Some(PluginSearchContext {
                    year: Some(1982),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            variables.get(".Query.Year"),
            Some(&Value::String("1982".to_string()))
        );
        assert_eq!(
            render(
                "{{ .Keywords }}{{ if .Query.Year }} {{ .Query.Year }}{{ end }}",
                &variables,
            )
            .unwrap(),
            "the thing 1982"
        );
    }

    #[test]
    fn an_omitted_year_stays_null_and_renders_as_a_go_template_would() {
        let definition = parse_definition(YEAR_DEFINITION).unwrap();
        // A host that sends no context at all and a host that sends a context
        // whose year it could not determine have to behave identically.
        for context in [
            None,
            Some(PluginSearchContext::default()),
            Some(PluginSearchContext {
                year: None,
                ..Default::default()
            }),
        ] {
            let variables = search_variables(
                &definition,
                &BTreeMap::from([(
                    "base_url".to_string(),
                    "https://tracker.example/".to_string(),
                )]),
                &PluginSearchRequest {
                    query: "the thing".to_string(),
                    context,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(variables.get(".Query.Year"), Some(&Value::Null));
            assert_eq!(
                render(
                    "{{ .Keywords }}{{ if .Query.Year }} {{ .Query.Year }}{{ end }}",
                    &variables,
                )
                .unwrap(),
                "the thing"
            );
            assert_eq!(render("[{{ .Query.Year }}]", &variables).unwrap(), "[]");
        }
    }

    /// Music and book search is out of scope for Scryer, so the engine binds no
    /// `.Query` variable for it. A definition that still names one must behave
    /// exactly as Go's `text/template` does for a missing map key: falsy in a
    /// conditional and empty when expanded, never a render error.
    #[test]
    fn unbound_music_and_book_query_names_are_falsy_and_expand_to_nothing() {
        let definition = parse_definition(YEAR_DEFINITION).unwrap();
        let variables = search_variables(
            &definition,
            &BTreeMap::from([(
                "base_url".to_string(),
                "https://tracker.example/".to_string(),
            )]),
            &PluginSearchRequest {
                query: "the thing".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        for name in [
            ".Query.Artist",
            ".Query.Album",
            ".Query.Author",
            ".Query.Title",
            ".Query.Publisher",
            ".Query.Genre",
        ] {
            assert!(!variables.contains_key(name), "{name} must not be bound");
            assert_eq!(
                render(
                    &format!("{{{{ if {name} }}}}yes{{{{ else }}}}no{{{{ end }}}}"),
                    &variables
                )
                .unwrap(),
                "no",
                "{name}"
            );
            assert_eq!(
                render(&format!("[{{{{ {name} }}}}]"), &variables).unwrap(),
                "[]",
                "{name}"
            );
        }
    }

    /// `ParseUtil.GetBytes` reads every unit as a power of 1024 and normalizes
    /// the number before parsing it. Treating `GB` as 1000³ understated every
    /// size by 7–10 %, and `1.234,5 GB` did not parse at all.
    #[test]
    fn parses_sizes_the_reference_way() {
        for (value, expected) in [
            ("1 GB", Some(1_073_741_824)),
            ("1.5 GiB", Some(1_610_612_736)),
            ("1,5 GB", Some(1_610_612_736)),
            ("1.234,5 MB", Some(1_294_467_072)),
            ("700 MB", Some(734_003_200)),
            ("12345", Some(12_345)),
            ("2 TB", Some(2_199_023_255_552)),
            ("512 KB", Some(524_288)),
            ("1 GIB", Some(1_073_741_824)),
            // No unit at all is a byte count, whatever surrounds it.
            ("1 048 576", Some(1_048_576)),
            // Nothing numeric is no size.
            ("unknown", None),
            ("", None),
        ] {
            assert_eq!(parse_size(value), expected, "{value}");
        }
    }

    #[test]
    fn maps_modern_categories_and_uses_defaults_only_as_a_fallback() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps:
  categorymappings:
    - { id: 10, cat: Movies }
    - { id: 20, cat: TV, default: true }
    - { id: 30, cat: TV/Anime }
search:
  paths: [{ path: search }]
"#,
        )
        .unwrap();
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec!["Movies".into()],
                    ..Default::default()
                },
            ),
            ["10"]
        );
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec!["2000".into()],
                    ..Default::default()
                },
            ),
            ["10"]
        );
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec!["5000".into()],
                    ..Default::default()
                },
            ),
            ["20", "30"]
        );
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec!["5070".into()],
                    ..Default::default()
                },
            ),
            ["30"]
        );
        assert_eq!(
            map_categories(&definition, &PluginSearchRequest::default()),
            ["20"]
        );
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    category: Some("Music".into()),
                    ..Default::default()
                },
            ),
            ["20"]
        );
    }

    /// The request's torznab ids are translated through the standard
    /// category tree: a parent id claims its whole subtree, a child id claims
    /// only its own name, and the definition's mapping order is preserved.
    #[test]
    fn translates_request_categories_through_the_standard_category_tree() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps:
  categorymappings:
    - { id: 40, cat: Movies/UHD }
    - { id: 10, cat: Movies }
    - { id: 30, cat: Movies/HD }
    - { id: 20, cat: TV/HD }
    - { id: 50, cat: TV/HD Remux }
search:
  paths: [{ path: search }]
"#,
        )
        .unwrap();
        let mapped = |category: &str| {
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec![category.to_string()],
                    ..Default::default()
                },
            )
        };
        // A child id matches its own name only: `TV/HD Remux` is a different
        // category, not a child of `TV/HD`.
        assert_eq!(mapped("5040"), ["20"]);
        assert_eq!(mapped("TV/HD"), ["20"]);
        // A parent id claims the parent and every child, in definition order.
        assert_eq!(mapped("2000"), ["40", "10", "30"]);
        assert_eq!(mapped("Movies"), ["40", "10", "30"]);
        // Ids the tree does not name fall through to the literal comparison.
        assert_eq!(mapped("2045"), ["40"]);
        assert!(mapped("7020").is_empty());
    }

    #[test]
    fn maps_legacy_categories_without_inventing_defaults() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps:
  categories: { 1: Movies, 2: TV }
search:
  paths: [{ path: search }]
"#,
        )
        .unwrap();
        assert_eq!(
            map_categories(
                &definition,
                &PluginSearchRequest {
                    categories: vec!["TV".into()],
                    ..Default::default()
                },
            ),
            ["2"]
        );
        assert!(map_categories(&definition, &PluginSearchRequest::default()).is_empty());
    }

    /// `andmatch` is the reference release filter: two of the query's
    /// terms have to appear in the title *or* the description, common words and
    /// one-character tokens do not count, and RSS and id searches are exempt.
    #[test]
    fn andmatch_filters_rows_the_way_the_reference_engine_filters_releases() {
        let definition = parse_definition(
            r#"
id: fixture
name: Fixture
type: public
links: [https://tracker.example/]
caps: {}
search:
  paths: [{ path: search }]
  rows:
    selector: tr.result
    filters:
      - { name: andmatch }
"#,
        )
        .unwrap();
        let row = |title: &str, description: Option<&str>| {
            let mut result = PluginSearchResult {
                title: title.to_string(),
                ..PluginSearchResult::default()
            };
            if let Some(description) = description {
                result.provider_extra.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            result
        };
        let variables = |query: &str| {
            search_variables(
                &definition,
                &BTreeMap::new(),
                &PluginSearchRequest {
                    query: query.to_string(),
                    ..Default::default()
                },
            )
            .unwrap()
        };

        // Two terms, one present: dropped.
        assert!(!should_keep_row(
            &definition,
            &row("Fixture Release 1080p", None),
            &variables("fixture missing"),
        ));
        // Three terms, two present: kept.
        assert!(should_keep_row(
            &definition,
            &row("Fixture Release 1080p", None),
            &variables("fixture release missing"),
        ));
        // The description counts as much as the title.
        assert!(should_keep_row(
            &definition,
            &row("Fixture 1080p", Some("A release of the second season")),
            &variables("fixture season"),
        ));
        // A single term needs a single match.
        assert!(should_keep_row(
            &definition,
            &row("Fixture Release", None),
            &variables("fixture"),
        ));
        assert!(!should_keep_row(
            &definition,
            &row("Fixture Release", None),
            &variables("missing"),
        ));
        // Common words and one-character tokens never count, so `the` alone
        // leaves a single real term.
        assert!(should_keep_row(
            &definition,
            &row("Fixture Release", None),
            &variables("the fixture"),
        ));
        // Punctuation splits terms the way `[^\w]+` does.
        assert!(should_keep_row(
            &definition,
            &row("Fixture Release 1080p", None),
            &variables("fixture.release-1080p"),
        ));

        // An RSS search has no query to filter by.
        assert!(should_keep_row(
            &definition,
            &row("Anything At All", None),
            &variables(""),
        ));
        // An id search is exempt even with a query the title does not answer.
        let id_variables = search_variables(
            &definition,
            &BTreeMap::new(),
            &PluginSearchRequest {
                query: "fixture missing".to_string(),
                ids: std::collections::HashMap::from([(
                    "imdb_id".to_string(),
                    "tt0111161".to_string(),
                )]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(should_keep_row(
            &definition,
            &row("Something Else", None),
            &id_variables,
        ));
    }

    #[test]
    fn preserves_typed_date_empty_download_and_first_comment_semantics() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let mut result = PluginSearchResult::default();
        apply_result_field(
            &mut result,
            "date",
            &[],
            "Sat, 22 Aug 2026 12:34:56 +0000".into(),
            "https://tracker.example/search",
        )
        .unwrap();
        apply_result_field(
            &mut result,
            "comments",
            &[],
            "/comments/first".into(),
            "https://tracker.example/search",
        )
        .unwrap();
        apply_result_field(
            &mut result,
            "comments",
            &[],
            "/comments/second".into(),
            "https://tracker.example/search",
        )
        .unwrap();
        apply_result_field(
            &mut result,
            "download",
            &[],
            "/download/one".into(),
            "https://tracker.example/search",
        )
        .unwrap();
        apply_result_field(
            &mut result,
            "download",
            &[],
            String::new(),
            "https://tracker.example/search",
        )
        .unwrap();
        normalize_result(&definition, &mut result);
        assert_eq!(
            result.published_at.as_deref(),
            Some("2026-08-22T12:34:56+00:00")
        );
        assert_eq!(
            result.comment_url.as_deref(),
            Some("https://tracker.example/comments/first")
        );
        assert!(result.download_url.is_none());
        assert_eq!(result.guid.as_deref(), Some("/download/one"));
    }

    #[test]
    fn executes_search_request_and_parses_markup_response() {
        let first = begin(
            compiled(PUBLIC_DEFINITION),
            Operation::Search(Box::new(PluginSearchRequest {
                query: "debian".into(),
                ..Default::default()
            })),
            BTreeMap::new(),
        )
        .unwrap();
        let continuation = match first {
            Step::NeedHttp {
                request,
                continuation,
            } => {
                assert_eq!(request.url, "https://tracker.example/search?q=debian");
                continuation
            }
            _ => panic!("search must yield HTTP"),
        };
        let complete = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title href='/details/1'>Debian 13</a><a class=download href='/download/1'>DL</a></td><td class=size>2 GiB</td><td class=seeders>42</td><td class=seedtime>172800</td></tr></table>"#.to_vec(),
            }),
        )
        .unwrap();
        let Step::Complete { output } = complete else {
            panic!("search must complete")
        };
        assert_eq!(output["results"][0]["title"], "Debian 13");
        assert_eq!(output["results"][0]["size_bytes"], 2_147_483_648i64);
        assert_eq!(output["results"][0]["seeders"], 42);
        assert_eq!(output["results"][0]["minimum_seed_time_minutes"], 2880);
        assert_eq!(
            output["results"][0]["download_url"],
            "https://tracker.example/download/1"
        );
    }

    /// Cardigann's `ratio:` block is a Jackett-era display of the operator's own
    /// account ratio; the reference engine parses it and never evaluates it. Fetching it
    /// spent an extra authenticated request before every search and grab and
    /// stamped a seeding requirement the tracker never stated onto every
    /// release.
    #[test]
    fn a_ratio_block_costs_no_request_and_stamps_no_seed_requirement() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "ratio:\n  path: account\n  selector: span.ratio\nsearch:\n",
        );
        // The block still parses, so the definition stays admissible.
        assert!(parse_definition(&definition).unwrap().ratio.is_some());

        let first = begin(
            compiled(&definition),
            Operation::Search(Box::new(PluginSearchRequest {
                query: "fixture".into(),
                ..Default::default()
            })),
            BTreeMap::new(),
        )
        .unwrap();
        let Step::NeedHttp {
            request,
            continuation,
        } = first
        else {
            panic!("search must yield HTTP")
        };
        assert_eq!(request.url, "https://tracker.example/search?q=fixture");

        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title href='/details/1'>Fixture Release</a><a class=download href='/download/1'>DL</a></td><td class=size>2 GiB</td><td class=seeders>42</td></tr></table>"#.to_vec(),
            }),
        )
        .unwrap() else {
            panic!("search must complete")
        };
        assert_eq!(output["results"][0]["title"], "Fixture Release");
        assert!(output["results"][0]["minimum_seed_ratio"].is_null());
        assert!(output["results"][0]["minimum_seed_time_minutes"].is_null());
    }

    #[test]
    fn executes_form_login_before_search_and_carries_cookie() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "settings:\n  - { name: username, type: text }\n  - { name: password, type: password }\nlogin:\n  method: form\n  path: login\n  inputs: { user: '{{ .Config.username }}', pass: '{{ .Config.password }}' }\nsearch:\n",
        );
        let first = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::from([
                ("username".into(), "alice".into()),
                ("password".into(), "secret".into()),
            ]),
        )
        .unwrap();
        let landing_continuation = match first {
            Step::NeedHttp { continuation, .. } => continuation,
            _ => panic!(),
        };
        let submit = resume(
            &landing_continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: vec!["session=one; Path=/".into(), "theme=dark; Path=/".into()],
                body: br#"<form action='/session'><input name='csrf' value='token'></form>"#
                    .to_vec(),
            }),
        )
        .unwrap();
        let (submit_request, submit_continuation) = match submit {
            Step::NeedHttp {
                request,
                continuation,
            } => (request, continuation),
            _ => panic!(),
        };
        assert_eq!(submit_request.method.as_deref(), Some("POST"));
        assert_eq!(
            submit_request.headers.get("cookie").map(String::as_str),
            Some("session=one; theme=dark")
        );
        assert_eq!(
            String::from_utf8(submit_request.body).unwrap(),
            "csrf=token&pass=secret&user=alice"
        );
        let search = resume(
            &submit_continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: vec!["session=two; Path=/".into()],
                body: Vec::new(),
            }),
        )
        .unwrap();
        let Step::NeedHttp { request, .. } = search else {
            panic!()
        };
        assert_eq!(
            request.headers.get("cookie").map(String::as_str),
            Some("session=two; theme=dark")
        );
    }

    /// A UNIT3D-style API definition: the API key is only a search header, and
    /// the login block, which checks the key, declares no headers of its own.
    fn api_definition(login: &str) -> String {
        PUBLIC_DEFINITION
            .replace(
                "search:\n  paths:\n",
                &format!(
                    "settings:\n  - {{ name: apikey, type: text, label: API Key }}\n{login}search:\n  headers:\n    Authorization: [\"Bearer {{{{ .Config.apikey }}}}\"]\n  paths:\n"
                ),
            )
            .replace("id: fixture", "id: example-api")
    }

    fn api_key_config() -> BTreeMap<String, String> {
        BTreeMap::from([("apikey".to_string(), "synthetic-key".to_string())])
    }

    fn first_request(definition: &str) -> PluginHttpRequest {
        let Step::NeedHttp { request, .. } = begin(
            compiled(definition),
            Operation::Search(Box::default()),
            api_key_config(),
        )
        .unwrap() else {
            panic!("login request expected")
        };
        request
    }

    /// A login block without a `headers:` key sends the search headers, so the API key reaches the
    /// login check instead of the tracker rejecting an anonymous request.
    #[test]
    fn get_login_without_headers_sends_the_search_headers() {
        let request = first_request(&api_definition(
            "login:\n  path: api/torrents\n  method: get\n",
        ));
        assert_eq!(request.url, "https://tracker.example/api/torrents");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer synthetic-key")
        );
    }

    #[test]
    fn form_login_landing_without_headers_sends_the_search_headers() {
        let request = first_request(&api_definition("login:\n  path: login\n  method: form\n"));
        assert_eq!(request.url, "https://tracker.example/login");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer synthetic-key")
        );
    }

    /// The fallback only covers an absent key: login headers the definition
    /// does declare, even an empty map, replace the search headers.
    #[test]
    fn explicit_login_headers_replace_the_search_headers() {
        let request = first_request(&api_definition(
            "login:\n  path: api/torrents\n  method: get\n  headers:\n    Authorization: \"Bearer login-{{ .Config.apikey }}\"\n",
        ));
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer login-synthetic-key")
        );

        let request = first_request(&api_definition(
            "login:\n  path: api/torrents\n  method: get\n  headers: {}\n",
        ));
        assert!(!request.headers.contains_key("authorization"));
    }

    const API_LOGIN_WITH_ERRORS: &str = "login:\n  path: api/torrents\n  method: get\n  error:\n    - selector: a[href*=\"/login\"]\n      message:\n        text: \"The API key was not accepted by {{ .Config.sitelink }}.\"\n    - selector: :root:contains(\"Account is Banned\")\n";

    /// Answer the definition's login request with `response` and return the
    /// typed error the flow ends with.
    fn login_rejection(definition: &str, response: PluginHttpResponse) -> PluginError {
        login_rejection_with(definition, api_key_config(), response)
    }

    /// [`login_rejection`] with the given configuration.
    fn login_rejection_with(
        definition: &str,
        config: BTreeMap<String, String>,
        response: PluginHttpResponse,
    ) -> PluginError {
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(definition),
            Operation::Search(Box::default()),
            config,
        )
        .unwrap() else {
            panic!("login request expected")
        };
        match resume(&continuation, ResumeInput::Http(response)).unwrap() {
            Step::Rejected { error } => error,
            step => panic!("login rejection expected, got {step:?}"),
        }
    }

    fn html_response(status: u16, body: &str) -> PluginHttpResponse {
        PluginHttpResponse {
            status,
            headers: BTreeMap::from([("content-type".to_string(), "text/html".to_string())]),
            set_cookie_headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn debug(error: &PluginError) -> &str {
        error.debug_message.as_deref().unwrap_or_default()
    }

    /// Login `error:` messages render with the definition's variables, so a
    /// `{{ .Config.sitelink }}` names the configured site instead of rendering
    /// empty. That text is the debug message; the operator sees the fixed
    /// sentence for the code.
    #[test]
    fn login_error_block_message_renders_config_variables() {
        let error = login_rejection(
            &api_definition(API_LOGIN_WITH_ERRORS),
            html_response(200, "<html><a href='/login'>Sign in</a></html>"),
        );
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        assert!(debug(&error).contains("HTTP 200"), "{}", debug(&error));
        assert!(
            debug(&error).contains("The API key was not accepted by https://tracker.example/."),
            "{}",
            debug(&error)
        );
    }

    /// The definition's own explanation survives an error status; a status no block explains is
    /// reported by what the status means.
    #[test]
    fn login_error_status_prefers_the_definitions_message() {
        let definition = api_definition(API_LOGIN_WITH_ERRORS);
        let error = login_rejection(
            &definition,
            html_response(403, "<html><a href='/login'>Sign in</a></html>"),
        );
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        assert!(
            debug(&error).contains("The API key was not accepted"),
            "{}",
            debug(&error)
        );

        for (status, code) in [
            (401, PluginErrorCode::AuthFailed),
            (403, PluginErrorCode::AuthFailed),
            (429, PluginErrorCode::Temporary),
            (503, PluginErrorCode::UpstreamUnavailable),
        ] {
            let error = login_rejection(&definition, html_response(status, "<html></html>"));
            assert_eq!(error.code, code, "HTTP {status}");
            assert!(
                error.public_message.contains(&format!("HTTP {status}")),
                "{}",
                error.public_message
            );
            assert_eq!(
                error.debug_message.as_deref(),
                Some(format!("Cardigann login returned HTTP {status}").as_str())
            );
        }

        // A matching block on a successful status still rejects the login.
        let error = login_rejection(
            &definition,
            html_response(200, "<html><p>Account is Banned</p></html>"),
        );
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        assert!(
            debug(&error).ends_with("Account is Banned"),
            "{}",
            debug(&error)
        );
    }

    /// A username/password login, optionally with an image captcha.
    fn password_login_definition(captcha: &str) -> String {
        PUBLIC_DEFINITION.replace(
            "search:\n  paths:\n",
            &format!(
                "settings:\n  - {{ name: username, type: text, label: Username }}\n  - {{ name: password, type: password, label: Password }}\nlogin:\n  path: takelogin.php\n  method: post\n  inputs:\n    username: \"{{{{ .Config.username }}}}\"\n    password: \"{{{{ .Config.password }}}}\"\n{captcha}  error:\n    - selector: div.login-error\nsearch:\n  paths:\n"
            ),
        )
    }

    /// A rejected username/password login says which credential failed, and
    /// a login that declares a captcha with no answer configured asks for the
    /// captcha instead. The response body decides neither.
    #[test]
    fn login_rejections_carry_the_credential_code() {
        let definition = password_login_definition("");
        let error = login_rejection(
            &definition,
            html_response(200, "<html><div class=login-error>Wrong</div></html>"),
        );
        assert_eq!(
            error.code,
            PluginErrorCode::from(codes::INVALID_USERNAME_PASSWORD)
        );
        assert_eq!(
            error.public_message,
            "The tracker refused the username and password."
        );
        assert!(debug(&error).ends_with(": Wrong"), "{}", debug(&error));
        let error = login_rejection(&definition, html_response(401, "<html></html>"));
        assert_eq!(
            error.code,
            PluginErrorCode::from(codes::INVALID_USERNAME_PASSWORD)
        );
        // A status that is not a refusal keeps its own meaning.
        let error = login_rejection(&definition, html_response(503, "<html></html>"));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);

        let definition = password_login_definition(
            "  captcha:\n    type: image\n    selector: img.captcha\n    input: captcha\n",
        );
        let error = login_rejection(
            &definition,
            html_response(
                200,
                "<html><div class=login-error>Wrong</div><img class=captcha src='/c.png'></html>",
            ),
        );
        assert_eq!(error.code, PluginErrorCode::from(codes::CAPTCHA_REQUIRED));
        assert_eq!(
            error.public_message,
            "The tracker asks for a CAPTCHA answer."
        );
        // Without the captcha image in the response the declared captcha
        // still has no answer, so the code is the same.
        let error = login_rejection(
            &definition,
            html_response(200, "<html><div class=login-error>Wrong</div></html>"),
        );
        assert_eq!(error.code, PluginErrorCode::from(codes::CAPTCHA_REQUIRED));
    }

    /// With the captcha answered, a refused login refused the username and
    /// password, even when the tracker shows its captcha again.
    #[test]
    fn an_answered_captcha_login_rejection_is_invalid_username_password() {
        let definition = password_login_definition(
            "  captcha:\n    type: image\n    selector: img.captcha\n    input: captcha\n",
        );
        for key in ["cardigannCaptcha", "CAPTCHA"] {
            let error = login_rejection_with(
                &definition,
                BTreeMap::from([
                    ("username".to_string(), "synthetic-user".to_string()),
                    ("password".to_string(), "synthetic-pass".to_string()),
                    (key.to_string(), "answer".to_string()),
                ]),
                html_response(
                    200,
                    "<html><div class=login-error>Wrong</div><img class=captcha src='/c.png'></html>",
                ),
            );
            assert_eq!(
                error.code,
                PluginErrorCode::from(codes::INVALID_USERNAME_PASSWORD),
                "{key}"
            );
            assert_eq!(
                error.public_message,
                "The tracker refused the username and password."
            );
        }
    }

    /// A matched login error element that carries an inline stylesheet and a
    /// script reports the fixed sentence, and its debug text holds only what
    /// a reader sees, credentials masked.
    #[test]
    fn a_login_error_element_with_style_and_script_reports_only_visible_text() {
        let definition = password_login_definition("");
        let error = login_rejection_with(
            &definition,
            BTreeMap::from([
                ("username".to_string(), "synthetic-user".to_string()),
                ("password".to_string(), "synthetic-pass".to_string()),
            ]),
            html_response(
                200,
                "<html><div class=login-error><style>.msg-wrap { width: 80%; margin: 0 auto; }</style>\
                 <script>var shown = true;</script><p>Wrong   password for\n synthetic-user</p></div></html>",
            ),
        );
        assert_eq!(
            error.code,
            PluginErrorCode::from(codes::INVALID_USERNAME_PASSWORD)
        );
        assert_eq!(
            error.public_message,
            "The tracker refused the username and password."
        );
        let debug = debug(&error);
        assert!(
            debug.ends_with(": Wrong password for [redacted]"),
            "{debug}"
        );
        for leaked in ["width", "margin", "{", "var shown", "synthetic-user"] {
            assert!(!debug.contains(leaked), "{leaked} in {debug}");
        }
    }

    /// A login with no username and password settings, no cookie and no
    /// captcha is refused as a plain sign-in failure.
    #[test]
    fn a_login_without_credential_settings_is_auth_failed() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n  paths:\n",
            "login:\n  path: takelogin.php\n  method: post\n  error:\n    - selector: div.login-error\nsearch:\n  paths:\n",
        );
        let error = login_rejection_with(
            &definition,
            BTreeMap::new(),
            html_response(200, "<html><div class=login-error>Refused</div></html>"),
        );
        assert_eq!(error.code, PluginErrorCode::AuthFailed);
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        assert!(debug(&error).ends_with(": Refused"), "{}", debug(&error));
    }

    /// A cookie login whose reissued request still lands on a logged-out page
    /// reports the cookie as refused instead of an empty search.
    #[test]
    fn cookie_login_rejection_carries_invalid_cookie() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: cookie\n  test:\n    path: index.php\n    selector: a[href*=\"logout\"]\nsearch:\n",
        ))
        .unwrap();
        let logged_out = || html_response(200, "<html><a href='/login.php'>Sign in</a></html>");
        let mut host = MockEngineHost {
            responses: vec![logged_out(), logged_out()],
            ..Default::default()
        };
        let Err(EngineError::Plugin(error)) = block_on(search_with_host(
            &mut host,
            definition,
            PluginSearchRequest::default(),
            BTreeMap::from([("cookie".to_string(), "uid=1; pass=synthetic".to_string())]),
        )) else {
            panic!("cookie rejection expected")
        };
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_COOKIE));
        assert!(!error.public_message.contains("synthetic"));
        assert_eq!(host.requests.len(), 2);
    }

    /// A cookie login the tracker still answers with 401 after the reissued
    /// request refused the cookie, whatever the page shows.
    #[test]
    fn a_refused_cookie_login_status_is_invalid_cookie() {
        let definition = parse_definition(
            &PUBLIC_DEFINITION.replace("search:\n", "login:\n  method: cookie\nsearch:\n"),
        )
        .unwrap();
        let refused = || html_response(401, "<html><img class=captcha src='/c.png'></html>");
        let mut host = MockEngineHost {
            responses: vec![refused(), refused()],
            ..Default::default()
        };
        let Err(EngineError::Plugin(error)) = block_on(search_with_host(
            &mut host,
            definition,
            PluginSearchRequest::default(),
            BTreeMap::from([("cookie".to_string(), "uid=1; pass=synthetic".to_string())]),
        )) else {
            panic!("cookie rejection expected")
        };
        assert_eq!(error.code, PluginErrorCode::from(codes::INVALID_COOKIE));
        assert!(!error.public_message.contains("synthetic"));
    }

    /// Neither message carries a configured credential back out, the public
    /// message carries no tracker text, and a block without a `message:`
    /// cannot dump a whole page into the debug message.
    #[test]
    fn login_error_public_message_masks_credentials_and_stays_short() {
        let definition = api_definition(
            "login:\n  path: api/torrents\n  method: get\n  error:\n    - selector: p.denied\n      message:\n        text: \"Key {{ .Config.apikey }} refused\"\n    - selector: :root:contains(\"Account is Banned\")\n",
        );
        let error = login_rejection(
            &definition,
            html_response(200, "<html><p class=denied>no</p></html>"),
        );
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        assert!(
            debug(&error).ends_with(": Key [redacted] refused"),
            "{}",
            debug(&error)
        );
        assert!(!debug(&error).contains("synthetic-key"));

        let page = format!(
            "<html><p>Account is Banned</p>{}</html>",
            "filler ".repeat(200)
        );
        let error = login_rejection(&definition, html_response(200, &page));
        assert_eq!(error.public_message, "The tracker refused the sign-in.");
        let tracker_part = debug(&error).split_once("error block: ").unwrap().1;
        assert!(tracker_part.chars().count() <= TRACKER_TEXT_LIMIT + 1);
        assert!(tracker_part.ends_with('…'));
    }

    /// Login is redone on any unfollowed redirect and any HTTP error, not
    /// only on 401/403 or a `Location` naming the login path, and then reissues
    /// the request that provoked it.
    #[test]
    fn re_logs_in_once_on_any_unfollowed_redirect_or_error_status() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\nsearch:\n",
        );
        let landing = || PluginHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            set_cookie_headers: Vec::new(),
            body: br#"<form action='/session'><input name='csrf' value='token'></form>"#.to_vec(),
        };
        let logged_in = || PluginHttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            set_cookie_headers: vec!["session=fresh; Path=/".into()],
            body: Vec::new(),
        };

        for challenge in [
            // A redirect to somewhere that is not the login path at all.
            PluginHttpResponse {
                status: 302,
                headers: BTreeMap::from([("location".into(), "/maintenance".into())]),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            },
            PluginHttpResponse {
                status: 404,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            },
            PluginHttpResponse {
                status: 503,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            },
        ] {
            let Step::NeedHttp { continuation, .. } = begin(
                compiled(&definition),
                Operation::Search(Box::default()),
                BTreeMap::new(),
            )
            .unwrap() else {
                panic!("login landing request expected")
            };
            let Step::NeedHttp { continuation, .. } =
                resume(&continuation, ResumeInput::Http(landing())).unwrap()
            else {
                panic!("login submit expected")
            };
            let Step::NeedHttp {
                request,
                continuation,
            } = resume(&continuation, ResumeInput::Http(logged_in())).unwrap()
            else {
                panic!("search request expected")
            };
            let search_url = request.url.clone();

            // The challenge triggers exactly one re-login, which lands back on
            // the same search request.
            let status = challenge.status;
            let Step::NeedHttp { continuation, .. } =
                resume(&continuation, ResumeInput::Http(challenge.clone())).unwrap()
            else {
                panic!("re-login landing expected for {status}")
            };
            let Step::NeedHttp { continuation, .. } =
                resume(&continuation, ResumeInput::Http(landing())).unwrap()
            else {
                panic!("re-login submit expected for {status}")
            };
            let Step::NeedHttp {
                request,
                continuation,
            } = resume(&continuation, ResumeInput::Http(logged_in())).unwrap()
            else {
                panic!("retried search request expected for {status}")
            };
            assert_eq!(request.url, search_url, "{status}");

            // The second failure is not retried again.
            assert!(
                matches!(
                    resume(&continuation, ResumeInput::Http(challenge)),
                    Ok(Step::Rejected { .. })
                ),
                "{status} must fail after one re-login"
            );
        }

        // Without a login block there is nothing to retry: the error stands.
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(PUBLIC_DEFINITION),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let error = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 404,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            }),
        )
        .unwrap();
        let Step::Rejected { error } = error else {
            panic!("a 404 search must be rejected")
        };
        assert_eq!(error.code.as_str(), codes::ENDPOINT_NOT_FOUND);
        assert_eq!(
            error.debug_message.as_deref(),
            Some("Cardigann search returned HTTP 404")
        );
    }

    #[test]
    fn rejects_an_unsupported_compiled_ir_version() {
        let invalid = serde_json::json!({
            "ir_version": COMPILED_IR_VERSION + 1,
            "definition": parse_definition(PUBLIC_DEFINITION).unwrap(),
        })
        .to_string();
        let error = begin(invalid, Operation::TestConnection, BTreeMap::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Cardigann compiled IR version")
        );
    }

    #[test]
    fn submits_form_get_selector_inputs_and_multipart_body() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  getselectorinputs:\n    next: { selector: 'input[name=next]', attribute: value }\nsearch:\n",
        );
        let first = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap();
        let Step::NeedHttp { continuation, .. } = first else {
            panic!("login landing request expected")
        };
        let submit = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<form action='/session?existing=1' enctype='multipart/form-data'><input name='csrf' value='token'><input name='next' value='catalog'></form>"#.to_vec(),
            }),
        )
        .unwrap();
        let Step::NeedHttp { request, .. } = submit else {
            panic!("login submit request expected")
        };
        assert_eq!(
            request.url,
            "https://tracker.example/session?existing=1&next=catalog"
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("multipart/form-data; boundary=----CardigannRuntimeBoundary")
        );
        assert!(
            String::from_utf8(request.body)
                .unwrap()
                .contains("name=\"csrf\"")
        );
    }

    #[test]
    fn parses_json_row_attribute_multiple_and_missing_rows_as_no_results() {
        let definition = PUBLIC_DEFINITION.replace(
            "  rows:\n    selector: tr.result\n",
            "  rows:\n    selector: data.items\n    attribute: releases\n    multiple: true\n    missingAttributeEqualsNoResults: true\n    count: { selector: meta.total }\n",
        ).replace(
            "    title: { selector: a.title }\n    details: { selector: a.title, attribute: href }\n    download: { selector: a.download, attribute: href }\n    size: { selector: td.size }\n    seeders: { selector: td.seeders }\n",
            "    title: { selector: name }\n    download: { selector: url }\n",
        ).replace(
            "      inputs: { q: \"{{ .Keywords }}\" }\n",
            "      inputs: { q: \"{{ .Keywords }}\" }\n      response: { type: json }\n",
        );
        let first = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap();
        let Step::NeedHttp { continuation, .. } = first else {
            panic!("search request expected")
        };
        let complete = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"{"meta":{"total":1},"data":{"items":[{"releases":[{"name":"JSON release","url":"/dl/1"}]}]}}"#.to_vec(),
            }),
        )
        .unwrap();
        let Step::Complete { output } = complete else {
            panic!("JSON search must complete")
        };
        assert_eq!(output["results"][0]["title"], "JSON release");
        assert_eq!(
            output["results"][0]["download_url"],
            "https://tracker.example/dl/1"
        );
    }

    #[test]
    fn parses_case_preserving_namespaced_xml_results() {
        let definition = PUBLIC_DEFINITION
            .replace(
                "      inputs: { q: \"{{ .Keywords }}\" }\n",
                "      inputs: { q: \"{{ .Keywords }}\" }\n      response: { type: xml }\n",
            )
            .replace(
                "    selector: tr.result\n",
                "    selector: rss > channel > item\n",
            )
            .replace(
                "    title: { selector: a.title }\n    details: { selector: a.title, attribute: href }\n    download: { selector: a.download, attribute: href }\n    size: { selector: td.size }\n    seeders: { selector: td.seeders }\n",
                "    title: { selector: title }\n    download: { selector: enclosure, attribute: url }\n    seeders: { selector: 'torznab:attr[name=seeders]', attribute: value }\n",
            );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("XML search request expected")
        };
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<rss xmlns:torznab="urn:torznab"><channel><item><title>XML release</title><enclosure url="/download/1"/><torznab:attr name="seeders" value="5"/></item></channel></rss>"#.to_vec(),
            }),
        )
        .unwrap()
        else {
            panic!("XML search must complete")
        };
        assert_eq!(output["results"][0]["title"], "XML release");
        assert_eq!(output["results"][0]["seeders"], 5);
    }

    #[test]
    fn resolves_download_before_selector_and_preserves_same_origin_headers() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "download:\n  method: '{{ .DownloadUri.Query.method }}'\n  headers: { x-token: '{{ .DownloadUri.Query.token }}' }\n  before:\n    path: gate?existing=1\n    method: get\n    queryseparator: ';'\n    inputs: { token: '{{ .DownloadUri.Query.token }}' }\n  selectors:\n    - { selector: a.torrent, attribute: href, usebeforeresponse: true }\nsearch:\n",
        );
        let first = begin(
            compiled(&definition),
            Operation::Grab("https://tracker.example/details/1?token=abc&method=post".into()),
            BTreeMap::new(),
        )
        .unwrap();
        let Step::NeedHttp {
            request,
            continuation,
        } = first
        else {
            panic!("before request expected")
        };
        assert_eq!(
            request.url,
            "https://tracker.example/gate?existing=1;token=abc"
        );
        assert_eq!(
            request.headers.get("x-token").map(String::as_str),
            Some("abc")
        );
        let Step::NeedHttp {
            request,
            continuation,
        } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<a class=torrent href='/download/1'>torrent</a>"#.to_vec(),
            }),
        )
        .unwrap()
        else {
            panic!("resolved download must traverse the HTTP state machine")
        };
        assert_eq!(request.url, "https://tracker.example/download/1");
        assert_eq!(request.method.as_deref(), Some("POST"));
        assert_eq!(request.headers["x-token"], "abc");
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"d4:infotorrent-bytes".to_vec(),
            }),
        )
        .unwrap() else {
            panic!("download response must complete")
        };
        assert_eq!(output["url"], "https://tracker.example/download/1");
        assert_eq!(output["method"], "POST");
        assert_eq!(output["headers"]["x-token"], "abc");
        assert_eq!(
            output["body"],
            serde_json::json!([
                100, 52, 58, 105, 110, 102, 111, 116, 111, 114, 114, 101, 110, 116, 45, 98, 121,
                116, 101, 115
            ])
        );
    }

    #[test]
    fn retries_download_selectors_when_torrent_validation_fails() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "download:\n  selectors:\n    - { selector: a.first, attribute: href }\n    - { selector: a.second, attribute: href }\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Grab("https://tracker.example/details/1".into()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("details request expected")
        };
        let Step::NeedHttp {
            request,
            continuation,
        } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<a class=first href='/bad.torrent'>bad</a><a class=second href='/good.torrent'>good</a>"#.to_vec(),
            }),
        )
        .unwrap()
        else {
            panic!("first selector request expected")
        };
        assert_eq!(request.url, "https://tracker.example/bad.torrent");
        let Step::NeedHttp {
            request,
            continuation,
        } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<html>challenge</html>".to_vec(),
            }),
        )
        .unwrap()
        else {
            panic!("second selector request expected")
        };
        assert_eq!(request.url, "https://tracker.example/good.torrent");
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"d4:info".to_vec(),
            }),
        )
        .unwrap() else {
            panic!("valid torrent must complete")
        };
        assert_eq!(output["url"], "https://tracker.example/good.torrent");
    }

    #[test]
    fn accepts_non_torrent_selector_responses_when_validation_is_disabled() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "testlinktorrent: false\ndownload:\n  selectors:\n    - { selector: a.download, attribute: href }\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Grab("https://tracker.example/details/1".into()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("details request expected")
        };
        let Step::NeedHttp { continuation, .. } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<a class=download href='/download'>download</a>"#.to_vec(),
            }),
        )
        .unwrap() else {
            panic!("selector request expected")
        };
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<html>download portal</html>".to_vec(),
            }),
        )
        .unwrap() else {
            panic!("disabled validation must complete")
        };
        assert_eq!(
            output["body"],
            serde_json::json!(b"<html>download portal</html>")
        );
    }

    #[test]
    fn direct_http_grabs_traverse_the_proxy_facing_state_machine() {
        let Step::NeedHttp {
            request,
            continuation,
        } = begin(
            compiled(PUBLIC_DEFINITION),
            Operation::Grab("https://tracker.example/download/1".into()),
            BTreeMap::new(),
        )
        .unwrap()
        else {
            panic!("direct download must yield HTTP")
        };
        assert_eq!(request.url, "https://tracker.example/download/1");
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"torrent".to_vec(),
            }),
        )
        .unwrap() else {
            panic!("direct download response must complete")
        };
        assert_eq!(
            output["body"],
            serde_json::json!([116, 111, 114, 114, 101, 110, 116])
        );
    }

    /// An HTML-typed download is a login wall, not a torrent, and a large
    /// multi-file torrent is not a runtime abuse. The old 2 MiB ceiling rejected
    /// legitimate season packs and an HTML body reached the download client.
    #[test]
    fn rejects_html_typed_downloads_and_accepts_large_torrents() {
        let grab = |content_type: Option<&str>, body: Vec<u8>| {
            let Step::NeedHttp { continuation, .. } = begin(
                compiled(PUBLIC_DEFINITION),
                Operation::Grab("https://tracker.example/download/1".into()),
                BTreeMap::new(),
            )
            .unwrap() else {
                panic!("direct download request expected")
            };
            resume(
                &continuation,
                ResumeInput::Http(PluginHttpResponse {
                    status: 200,
                    headers: content_type
                        .map(|value| {
                            BTreeMap::from([("content-type".to_string(), value.to_string())])
                        })
                        .unwrap_or_default(),
                    set_cookie_headers: Vec::new(),
                    body,
                }),
            )
        };

        let Step::Rejected { error } = grab(
            Some("text/html; charset=utf-8"),
            b"<html>login</html>".to_vec(),
        )
        .unwrap() else {
            panic!("an HTML download must be rejected")
        };
        assert_eq!(error.code.as_str(), codes::INVALID_RESPONSE);
        assert!(
            error
                .debug_message
                .as_deref()
                .is_some_and(|debug| debug.contains("HTML page instead of a torrent")),
            "{error:?}"
        );

        // A tracker mislabelling a real bencoded body is still believed.
        assert!(matches!(
            grab(Some("text/html"), b"d4:infoe".to_vec()).unwrap(),
            Step::Complete { .. }
        ));

        // Three megabytes of torrent is an ordinary season pack.
        let large = std::iter::once(b'd')
            .chain(std::iter::repeat_n(b'0', 3 * 1024 * 1024))
            .collect::<Vec<_>>();
        assert!(matches!(
            grab(Some("application/x-bittorrent"), large).unwrap(),
            Step::Complete { .. }
        ));

        // Past the ceiling it is still refused.
        let oversized = vec![b'd'; 64 * 1024 * 1024 + 1];
        let Step::Rejected { error } = grab(None, oversized).unwrap() else {
            panic!("an oversized torrent must be rejected")
        };
        assert_eq!(error.code, PluginErrorCode::Permanent);
        assert!(
            error
                .debug_message
                .as_deref()
                .is_some_and(|debug| debug.contains("runtime limit")),
            "{error:?}"
        );
    }

    #[test]
    fn follows_bounded_same_origin_search_redirects() {
        let definition = PUBLIC_DEFINITION.replace("search:\n", "followredirect: true\nsearch:\n");
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let Step::NeedHttp {
            request,
            continuation,
        } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 302,
                headers: BTreeMap::from([("location".into(), "/redirected-search".into())]),
                set_cookie_headers: vec!["session=redirected; Path=/".into()],
                body: Vec::new(),
            }),
        )
        .unwrap()
        else {
            panic!("redirect must yield a new HTTP request")
        };
        assert_eq!(request.url, "https://tracker.example/redirected-search");
        assert_eq!(
            request.headers.get("cookie").map(String::as_str),
            Some("session=redirected")
        );
        let Step::Complete { output } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: br#"<table><tr class=result><td><a class=title>Redirected</a><a class=download href='/download/1'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>"#.to_vec(),
            }),
        )
        .unwrap()
        else {
            panic!("redirected response must complete")
        };
        assert_eq!(output["results"][0]["title"], "Redirected");
    }

    #[test]
    fn resolves_login_redirects_relative_to_the_login_request() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: account/login\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!()
        };
        let Step::NeedHttp { request, .. } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 302,
                headers: BTreeMap::from([("location".into(), "next".into())]),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            }),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.url, "https://tracker.example/account/next");
    }

    #[test]
    fn preserves_same_origin_post_body_for_307_redirects() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: post\n  path: login\n  inputs: { user: alice }\nsearch:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!()
        };
        let Step::NeedHttp { request, .. } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 307,
                headers: BTreeMap::from([("location".into(), "/new-login".into())]),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            }),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.method.as_deref(), Some("POST"));
        assert_eq!(request.url, "https://tracker.example/new-login");
        assert_eq!(request.body, b"user=alice");
    }

    #[test]
    fn empty_json_search_rows_are_successful_without_missing_attribute_override() {
        let mut definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        definition.search.rows.selector = "data".into();
        definition.search.rows.count = None;
        definition.search.rows.missing_attribute_equals_no_results = false;
        let body = r#"{"data":[],"links":{},"meta":{"total":0}}"#;
        let results = parse_json_results(&definition, body, &mut Variables::new())
            .expect("an existing empty result array is a successful search");
        assert!(results.is_empty());
        for invalid in [r#"{"links":{},"meta":{"total":0}}"#, "not JSON"] {
            assert!(parse_json_results(&definition, invalid, &mut Variables::new()).is_err());
        }
        for (document, selector) in [
            (serde_json::json!([]), ""),
            (serde_json::json!({"data": []}), "$.data"),
            (
                serde_json::json!({"data": [{"name": "Normal release"}]}),
                "data:contains(Free)",
            ),
        ] {
            assert!(json_select_rows(&document, selector).unwrap().is_empty());
        }
    }

    #[test]
    fn parses_nested_json_pseudos_for_rows_fields_and_empty_prefixes() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let document = serde_json::json!({
            "items": [
                {"name": "Free release", "meta": {"label": "free"}},
                {"name": "Hidden release", "meta": {"label": "free"}, "hidden": true},
                {"name": "Normal release", "meta": {"label": "normal"}}
            ],
            "genres": ["drama", "comedy"]
        });
        let rows = json_select_rows(
            &document,
            "items:has(meta:contains(free)):not(hidden):contains(Free)",
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "Free release");
        let array = serde_json::json!([
            {"meta": {"label": "free"}},
            {"meta": {"label": "normal"}}
        ]);
        assert_eq!(
            json_select_rows(&array, ":has(meta:contains(free))")
                .unwrap()
                .len(),
            1
        );
        let field: SelectorField =
            serde_yaml::from_str("selector: status\ncase: { 'True': freeleech, '*': normal }\n")
                .unwrap();
        assert_eq!(
            json_field_value(
                &definition,
                &serde_json::json!({"status": true}),
                &field,
                &Variables::new(),
                true,
            )
            .unwrap(),
            "normal"
        );
        let field: SelectorField =
            serde_yaml::from_str("selector: status\ncase: { 'true': freeleech, '*': normal }\n")
                .unwrap();
        assert_eq!(
            json_field_value(
                &definition,
                &serde_json::json!({"status": true}),
                &field,
                &Variables::new(),
                true,
            )
            .unwrap(),
            "freeleech"
        );
        assert_eq!(json_value_string(&document["genres"]), "drama,comedy");
    }

    #[test]
    fn raw_query_matrix_matches_the_reference_split_then_encode_semantics() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let variables = BTreeMap::from([(
            ".Keywords".to_string(),
            Value::String("space & slash/".to_string()),
        )]);
        let cases = [
            (
                "$raw: categories%5B%5D=1&categories%5B%5D=2\npage: '3'\n",
                vec![
                    ("categories%5B%5D", "1"),
                    ("categories%5B%5D", "2"),
                    ("page", "3"),
                ],
                "categories%255B%255D=1&categories%255B%255D=2&page=3",
            ),
            (
                "$raw: '&&flag&literal=a+b&escaped=%20&=skip&repeated=x&repeated=y'\n",
                vec![
                    ("flag", ""),
                    ("literal", "a+b"),
                    ("escaped", "%20"),
                    ("repeated", "x"),
                    ("repeated", "y"),
                ],
                "flag=&literal=a%2Bb&escaped=%2520&repeated=x&repeated=y",
            ),
            (
                "$raw: q={{ .Keywords }}&equals=a=b=c\n",
                vec![("q", "space%20%26%20slash%2F"), ("equals", "a=b=c")],
                "q=space%2520%2526%2520slash%252F&equals=a%3Db%3Dc",
            ),
        ];
        for (source, expected_pairs, expected_encoded) in cases {
            let map: ScalarMap = serde_yaml::from_str(source).unwrap();
            let pairs = render_map(&map, &variables).unwrap();
            assert_eq!(
                pairs,
                expected_pairs
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(encoded_form_body(&definition, &pairs), expected_encoded);
            let mut url = "https://tracker.example/search".to_string();
            append_query_encoded(&definition, &mut url, &pairs, None);
            assert_eq!(
                url,
                format!("https://tracker.example/search?{expected_encoded}")
            );
        }
    }

    #[test]
    fn treats_empty_no_results_markers_as_empty_bodies_only() {
        let marker = Some(String::new());
        assert!(!matches_no_results_message(
            "<html>results</html>",
            marker.as_ref()
        ));
        assert!(matches_no_results_message("", marker.as_ref()));
    }

    #[test]
    fn decodes_responses_and_encodes_form_pairs_with_definition_encoding() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replacen(
            "type: public",
            "encoding: windows-1251\ntype: public",
            1,
        ))
        .unwrap();
        let path = &definition.search.paths[0];
        let mut variables = Variables::new();
        let results = parse_search_response(
            &definition,
            path,
            &PluginHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                set_cookie_headers: Vec::new(),
                body: b"<table><tr class=result><td><a class=title>\xcf\xf0\xe8\xe2\xe5\xf2</a><a class=download href='/download/1'>DL</a></td><td class=size>1 GiB</td><td class=seeders>1</td></tr></table>".to_vec(),
            },
            &mut variables,
        )
        .unwrap();
        assert_eq!(results[0].title, "Привет");

        let latin = parse_definition(&PUBLIC_DEFINITION.replacen(
            "type: public",
            "encoding: iso-8859-1\ntype: public",
            1,
        ))
        .unwrap();
        let pairs = [("q".to_string(), "café au lait".to_string())];
        assert_eq!(encoded_form_body(&latin, &pairs), "q=caf%E9+au+lait");
        let mut url = "https://tracker.example/search".to_string();
        append_query_encoded(&latin, &mut url, &pairs, None);
        assert_eq!(url, "https://tracker.example/search?q=caf%E9+au+lait");
    }

    #[test]
    fn strips_cookies_from_cross_origin_initial_requests() {
        let definition = parse_definition(PUBLIC_DEFINITION).unwrap();
        let context = Context {
            definition: definition.clone(),
            config: BTreeMap::new(),
            cookies: BTreeMap::from([("session".to_string(), "secret".to_string())]),
            operation: StoredOperation::TestConnection,
            search_path: 0,
            results: Vec::new(),
            variables: Variables::new(),
            grab_before_body: None,
            grab_selector_page_body: None,
            grab_selector_index: 0,
            relogin_attempts: 0,
            redirect_hops: 0,
            current_request: None,
            seen_get_urls: BTreeSet::new(),
        };
        let headers = common_headers(&context, None);
        let Step::NeedHttp { request, .. } = need_http(
            http_request(
                "GET",
                "https://elsewhere.example/search".to_string(),
                Vec::new(),
                headers,
            ),
            Continuation::TestConnection(context),
        )
        .unwrap() else {
            panic!("HTTP request expected")
        };
        assert!(!request.headers.contains_key("cookie"));
    }

    /// A tracker whose download endpoint lives on a sibling subdomain shares the
    /// session cookie with its base host, exactly as a `Domain=` cookie does in
    /// the reference engine's cookie jar. Same-origin stripping lost it on every one.
    #[test]
    fn attaches_cookies_across_the_configured_site_but_never_beyond_it() {
        assert!(same_site_host("tracker.example", "dl.tracker.example"));
        assert!(same_site_host("dl.tracker.example", "tracker.example"));
        assert!(same_site_host("tracker.example", "TRACKER.example"));
        assert!(!same_site_host("tracker.example", "elsewhere.example"));
        assert!(!same_site_host("tracker.example", "nottracker.example"));
        assert!(!same_site_host("tracker.example", ""));

        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "download:\n  headers: { x-token: abc }\nsearch:\n",
        ))
        .unwrap();
        let mut context = Context {
            definition: definition.clone(),
            config: BTreeMap::new(),
            cookies: BTreeMap::from([("session".to_string(), "secret".to_string())]),
            operation: StoredOperation::TestConnection,
            search_path: 0,
            results: Vec::new(),
            variables: Variables::new(),
            grab_before_body: None,
            grab_selector_page_body: None,
            grab_selector_index: 0,
            relogin_attempts: 0,
            redirect_hops: 0,
            current_request: None,
            seen_get_urls: BTreeSet::new(),
        };

        let sibling = download_headers_for_url(
            &definition,
            &context,
            "https://dl.tracker.example/download/1",
        )
        .unwrap();
        assert_eq!(
            sibling.get("cookie").map(String::as_str),
            Some("session=secret")
        );
        assert_eq!(sibling.get("x-token").map(String::as_str), Some("abc"));

        let elsewhere = download_headers_for_url(
            &definition,
            &context,
            "https://elsewhere.example/download/1",
        )
        .unwrap();
        assert!(!elsewhere.contains_key("cookie"));
        assert!(!elsewhere.contains_key("x-token"));

        // The same rule gates the outgoing request itself.
        context.operation = StoredOperation::TestConnection;
        for (url, expected) in [
            ("https://dl.tracker.example/search", true),
            ("https://tracker.example/search", true),
            ("https://elsewhere.example/search", false),
        ] {
            let headers = common_headers(&context, None);
            let Step::NeedHttp { request, .. } = need_http(
                http_request("GET", url.to_string(), Vec::new(), headers),
                Continuation::TestConnection(context.clone()),
            )
            .unwrap() else {
                panic!("HTTP request expected")
            };
            assert_eq!(request.headers.contains_key("cookie"), expected, "{url}");
        }
    }

    #[test]
    fn starts_each_redirect_chain_with_a_fresh_hop_budget() {
        let definition = PUBLIC_DEFINITION.replace("search:\n", "followredirect: true\nsearch:\n");
        for _ in 0..2 {
            let Step::NeedHttp { continuation, .. } = begin(
                compiled(&definition),
                Operation::Search(Box::default()),
                BTreeMap::new(),
            )
            .unwrap() else {
                panic!()
            };
            assert!(matches!(
                resume(
                    &continuation,
                    ResumeInput::Http(PluginHttpResponse {
                        status: 302,
                        headers: BTreeMap::from([("location".into(), "/next".into())]),
                        set_cookie_headers: Vec::new(),
                        body: Vec::new(),
                    }),
                )
                .unwrap(),
                Step::NeedHttp { .. }
            ));
        }
    }

    #[test]
    fn follows_test_connection_redirects() {
        let definition = PUBLIC_DEFINITION.replace("search:\n", "followredirect: true\nsearch:\n");
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::TestConnection,
            BTreeMap::new(),
        )
        .unwrap() else {
            panic!()
        };
        let Step::NeedHttp { request, .. } = resume(
            &continuation,
            ResumeInput::Http(PluginHttpResponse {
                status: 302,
                headers: BTreeMap::from([("location".into(), "/health".into())]),
                set_cookie_headers: Vec::new(),
                body: Vec::new(),
            }),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.url, "https://tracker.example/health");
    }

    fn rejection(step: Result<Step, EngineError>) -> PluginError {
        match step {
            Ok(Step::Rejected { error }) | Err(EngineError::Plugin(error)) => error,
            other => panic!("a typed rejection expected, got {other:?}"),
        }
    }

    fn first_continuation(definition: &str, operation: Operation) -> Vec<u8> {
        match begin(compiled(definition), operation, BTreeMap::new()).unwrap() {
            Step::NeedHttp { continuation, .. } => continuation,
            step => panic!("an HTTP request expected, got {step:?}"),
        }
    }

    fn plain_response(status: u16, headers: &[(&str, &str)], body: &str) -> PluginHttpResponse {
        PluginHttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            set_cookie_headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// A tracker HTTP error outside the login flow carries the code its status
    /// stands for, not the component's generic failure.
    #[test]
    fn tracker_http_errors_carry_the_code_their_status_stands_for() {
        let search = |response| {
            let continuation =
                first_continuation(PUBLIC_DEFINITION, Operation::Search(Box::default()));
            rejection(resume(&continuation, ResumeInput::Http(response)))
        };

        let error = search(plain_response(429, &[("Retry-After", "30")], ""));
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert_eq!(error.retry_after_seconds, Some(30));

        let error = search(plain_response(503, &[], ""));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);

        // A public definition sends no credential, so a 403 is a block, not
        // a refused login.
        let error = search(plain_response(403, &[], ""));
        assert_eq!(error.code, PluginErrorCode::Temporary);
        assert!(error.public_message.contains("blocked"), "{error:?}");

        // A definition that sends an API key had that key refused.
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&api_definition("")),
            Operation::Search(Box::default()),
            api_key_config(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(plain_response(401, &[], "")),
        ));
        assert_eq!(error.code.as_str(), codes::INVALID_API_KEY);

        let error = search(plain_response(418, &[], ""));
        assert_eq!(error.code, PluginErrorCode::Temporary);

        // A download the tracker no longer has is the release, not the page.
        let continuation = first_continuation(
            PUBLIC_DEFINITION,
            Operation::Grab("https://tracker.example/download/1".into()),
        );
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(plain_response(404, &[], "")),
        ));
        assert_eq!(error.code.as_str(), codes::ITEM_NOT_FOUND);
        assert_eq!(
            error.debug_message.as_deref(),
            Some("Cardigann torrent download returned HTTP 404")
        );
    }

    /// A search page the definition's `error:` block explains reports the
    /// tracker's own words, with configured credentials masked.
    #[test]
    fn a_search_error_block_reports_the_tracker_message() {
        let definition = api_definition("").replace(
            "  paths:\n",
            "  error:\n    - selector: p.error\n  paths:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            api_key_config(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(html_response(
                200,
                "<html><p class=error>Key synthetic-key may search again in an hour</p></html>",
            )),
        ));
        assert_eq!(error.code, PluginErrorCode::Temporary);
        assert_eq!(
            error.public_message,
            "The tracker reported an error for the search."
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("tracker search failed: Key [redacted] may search again in an hour")
        );
    }

    /// A search error element's stylesheet and script stay out of the debug
    /// message.
    #[test]
    fn a_search_error_block_debug_text_skips_style_and_script() {
        let definition = api_definition("").replace(
            "  paths:\n",
            "  error:\n    - selector: div.error\n  paths:\n",
        );
        let Step::NeedHttp { continuation, .. } = begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            api_key_config(),
        )
        .unwrap() else {
            panic!("search request expected")
        };
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(html_response(
                200,
                "<html><div class=error><style>.box { color: red; }</style><script>track();</script>Slow down</div></html>",
            )),
        ));
        assert_eq!(error.code, PluginErrorCode::Temporary);
        assert_eq!(
            error.public_message,
            "The tracker reported an error for the search."
        );
        assert_eq!(
            error.debug_message.as_deref(),
            Some("tracker search failed: Slow down")
        );
    }

    /// A search page the definition cannot read is an invalid response.
    #[test]
    fn an_unreadable_search_page_is_an_invalid_response() {
        let definition = PUBLIC_DEFINITION.replace(
            "    - path: search",
            "    - path: search\n      response: { type: xml }",
        );
        let continuation = first_continuation(&definition, Operation::Search(Box::default()));
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(plain_response(200, &[], "<rss><unclosed></rss>")),
        ));
        assert_eq!(error.code.as_str(), codes::INVALID_RESPONSE);
        assert!(
            error
                .debug_message
                .as_deref()
                .is_some_and(|debug| debug.contains("invalid XML search response")),
            "{error:?}"
        );
    }

    /// A connection test that does not find the logged-in page names the
    /// credential the login uses; an HTTP error names its status's code.
    #[test]
    fn connection_test_failures_carry_typed_codes() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: cookie\n  test:\n    path: index\n    selector: a.logout\nsearch:\n",
        );
        let continuation = first_continuation(&definition, Operation::TestConnection);
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(html_response(200, "<html><a class=login>in</a></html>")),
        ));
        assert_eq!(error.code.as_str(), codes::INVALID_COOKIE);

        let continuation = first_continuation(&definition, Operation::TestConnection);
        let error = rejection(resume(
            &continuation,
            ResumeInput::Http(plain_response(503, &[], "")),
        ));
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
    }

    #[test]
    fn an_unsupported_login_method_is_unsupported() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: carrier-pigeon\n  path: login\nsearch:\n",
        );
        let error = rejection(begin(
            compiled(&definition),
            Operation::Search(Box::default()),
            BTreeMap::new(),
        ));
        assert_eq!(error.code, PluginErrorCode::Unsupported);
    }

    /// A download page without a link the definition recognises, or with a
    /// link the plugin cannot fetch, is an invalid response.
    #[test]
    fn unusable_download_pages_are_invalid_responses() {
        let definition = PUBLIC_DEFINITION.replace(
            "search:\n",
            "download:\n  selectors:\n    - { selector: a.torrent, attribute: href }\nsearch:\n",
        );
        let grab = |body: &str| {
            let continuation = first_continuation(
                &definition,
                Operation::Grab("https://tracker.example/details/1".into()),
            );
            rejection(resume(
                &continuation,
                ResumeInput::Http(html_response(200, body)),
            ))
        };

        let error = grab("<html><p>no link here</p></html>");
        assert_eq!(error.code.as_str(), codes::INVALID_RESPONSE);

        let error =
            grab("<html><a class=torrent href='ftp://tracker.example/1.torrent'>t</a></html>");
        assert_eq!(error.code.as_str(), codes::INVALID_RESPONSE);
        assert!(
            error
                .debug_message
                .as_deref()
                .is_some_and(|debug| debug.contains("scheme `ftp`")),
            "{error:?}"
        );
    }

    /// A login that stops for a CAPTCHA the operator has not answered asks for
    /// one by code.
    #[test]
    fn an_unanswered_captcha_is_captcha_required() {
        let definition = parse_definition(&PUBLIC_DEFINITION.replace(
            "search:\n",
            "login:\n  method: form\n  path: login\n  captcha:\n    type: image\n    selector: img.captcha\n    input: captcha\nsearch:\n",
        ))
        .unwrap();
        let mut host = MockEngineHost {
            responses: vec![
                plain_response(
                    200,
                    &[],
                    "<form><img class=captcha src='/captcha.png'></form>",
                ),
                plain_response(200, &[("content-type", "image/png")], "png"),
            ],
            ..Default::default()
        };
        let error = block_on(search_with_host(
            &mut host,
            definition,
            PluginSearchRequest::default(),
            BTreeMap::new(),
        ))
        .unwrap_err();
        let EngineError::Plugin(error) = error else {
            panic!("a typed CAPTCHA error expected, got {error:?}")
        };
        assert_eq!(error.code.as_str(), codes::CAPTCHA_REQUIRED);
    }

    /// A tracker the host could not reach keeps the PDK's classification.
    #[test]
    fn host_http_failures_keep_their_classification() {
        let EngineError::Plugin(error) = host_http_error(component::HostError::Transport) else {
            panic!("a typed host error expected")
        };
        assert_eq!(error.code, PluginErrorCode::UpstreamUnavailable);
        let EngineError::Plugin(error) = host_http_error(component::HostError::Timeout) else {
            panic!("a typed host error expected")
        };
        assert_eq!(error.code, PluginErrorCode::Temporary);
        assert_ne!(
            error.public_message,
            "the plugin could not complete the request"
        );
    }
}
