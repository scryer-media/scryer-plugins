//! Scryer's host-owned services, as a guest sees them.
//!
//! The guest never receives unrestricted WASI network, filesystem, or process
//! authority. Every request is postcard-encoded, evaluated by Scryer's
//! descriptor-scoped policy, and returned as a postcard-encoded response.
//!
//! # One door, an injected transport
//!
//! The *encoding* lives here and nowhere else: a plugin calls [`config_get`],
//! [`http`], `archive_extract` and friends, and this module builds the
//! [`PluginHostRequest`], hands the bytes to whatever transport is installed,
//! and decodes the [`PluginHostResponse`]. No plugin ever hand-builds
//! postcard.
//!
//! The *transport* is injected rather than compiled in. A WASI Preview 2
//! family component imports `scryer:host/services@1.0.0`, and its
//! `wit_bindgen`-generated `host-call` lives in the **plugin** crate, not in
//! the PDK — bindings are generated per world, and this crate serves several.
//! So the family entry macros ([`crate::scryer_subtitle_component_main`] and
//! its siblings) install a one-line shim over that generated import through
//! [`install_host_call`], and this module holds nothing but a `fn` pointer.
//! The pattern is `unrar_rs::component_abi`'s: the crate that owns the
//! encoding never learns what the transport is.
//!
//! Two consequences worth stating plainly:
//!
//! * The PDK depends on no WIT world for host services, so a future family
//!   world reuses this module unchanged by installing its own shim.
//! * With no transport installed — native `cargo test`, or an indexer
//!   component that talks to `scryer:indexer/host` instead — every function
//!   here reports [`HostCallError::Unavailable`] rather than failing to link.
//!
//! # Capability availability is in-band
//!
//! A host with no provider for a service does not fail the transport. It
//! returns a well-formed [`PluginHostResponse`] carrying
//! `PluginResult::Err(PluginError { code: Unsupported, .. })`, which surfaces
//! here as [`HostCallError::Service`]. [`HostTransportError`] is reserved for
//! the call never reaching the service layer at all, which a guest cannot
//! recover from by asking for something else.

use std::fmt;
use std::sync::{PoisonError, RwLock};

#[cfg(feature = "archive-extract")]
use scryer_plugin_sdk::host::{PluginArchiveExtractRequest, PluginArchiveExtractResponse};
use scryer_plugin_sdk::host::{
    PluginConfigGetRequest, PluginHostRequest, PluginHostResponse, PluginHttpRequest,
    PluginHttpResponse, PluginProcessExecRequest, PluginProcessExecResponse,
    PluginStateDeleteRequest, PluginStateGetRequest, PluginStateSetRequest,
};
use scryer_plugin_sdk::{
    PluginError, PluginResult, SocketCloseRequest, SocketCloseResponse, SocketOpenRequest,
    SocketOpenResponse, SocketReadRequest, SocketReadResponse, SocketStartTlsRequest,
    SocketStartTlsResponse, SocketWriteRequest, SocketWriteResponse,
};

/// Maximum one-response payload accepted by the guest binding.
///
/// This is independent of host-side service limits and prevents a compromised
/// or malformed host from making a guest allocate without bound.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// A transport-level failure of the host-call itself.
///
/// Both variants mean the guest never reached the service layer, so no
/// [`PluginHostResponse`] exists to carry a typed [`PluginError`]. These are
/// exactly the two `host-error` cases of `scryer:host/services@1.0.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostTransportError {
    /// The request was not a decodable [`PluginHostRequest`], or exceeded the
    /// host's encoded-request cap.
    InvalidRequest,
    /// The host could not run the service or encode its response.
    Failed,
}

impl fmt::Display for HostTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "the host rejected the encoded request",
            Self::Failed => "the host could not run the service",
        })
    }
}

impl std::error::Error for HostTransportError {}

/// One encoded [`PluginHostRequest`] in, one encoded [`PluginHostResponse`]
/// out.
///
/// A plain `fn` pointer rather than a closure or trait object: it carries no
/// state, is `Copy`, and can be read on any path without allocating. Whatever
/// state a transport needs belongs to the component instance, which Scryer
/// creates fresh per invocation.
pub type HostCall = fn(&[u8]) -> Result<Vec<u8>, HostTransportError>;

/// The installed transport, or `None` when this guest has no host services.
///
/// Written once per component instantiation by the family entry macro. A
/// component instance is single-threaded and short-lived, so the lock is
/// uncontended; it exists to keep the registry sound, not to arbitrate.
static HOST_CALL: RwLock<Option<HostCall>> = RwLock::new(None);

/// Install the transport backing every function in this module.
///
/// The family entry macros call this at the top of each world export, because
/// Scryer instantiates a component once per invocation and a fresh instance
/// starts with an empty registry. Installing twice is harmless; the last
/// writer wins.
///
/// A plugin calls this directly only when it drives a world the PDK ships no
/// entry macro for.
pub fn install_host_call(host_call: HostCall) {
    *HOST_CALL.write().unwrap_or_else(PoisonError::into_inner) = Some(host_call);
}

/// Whether a host-services transport has been installed on this instance.
///
/// This distinguishes a family component (transport installed) from a guest
/// that reaches Scryer some other way, without inspecting the build target.
#[must_use]
pub fn host_call_installed() -> bool {
    installed_host_call().is_some()
}

fn installed_host_call() -> Option<HostCall> {
    *HOST_CALL.read().unwrap_or_else(PoisonError::into_inner)
}

/// A transport or protocol failure while using Scryer's host services.
#[derive(Debug)]
pub enum HostCallError {
    /// No transport is installed: this guest has no Scryer host services.
    Unavailable,
    Encode(postcard::Error),
    Decode(postcard::Error),
    ResponseTooLarge(usize),
    /// The host-call itself failed, so no typed service result exists.
    Transport(HostTransportError),
    UnexpectedResponse(&'static str),
    /// The service ran and reported a typed failure — including the in-band
    /// `Unsupported` a host without that capability returns.
    Service(PluginError),
}

impl fmt::Display for HostCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(f, "Scryer host services are unavailable"),
            Self::Encode(error) => write!(f, "failed to encode host request: {error}"),
            Self::Decode(error) => write!(f, "failed to decode host response: {error}"),
            Self::ResponseTooLarge(size) => {
                write!(
                    f,
                    "host response exceeds the {MAX_RESPONSE_BYTES}-byte limit: {size}"
                )
            }
            Self::Transport(error) => write!(f, "host call failed: {error}"),
            Self::UnexpectedResponse(operation) => {
                write!(
                    f,
                    "host returned a response for another operation while handling {operation}"
                )
            }
            Self::Service(error) => write!(f, "host service error: {}", error.public_message),
        }
    }
}

impl std::error::Error for HostCallError {}

/// Invoke a typed host service. Most plugins should prefer the specific
/// convenience functions below, which also verify the response operation.
pub fn call(request: PluginHostRequest) -> Result<PluginHostResponse, HostCallError> {
    let host_call = installed_host_call().ok_or(HostCallError::Unavailable)?;
    let encoded = postcard::to_allocvec(&request).map_err(HostCallError::Encode)?;
    let response = host_call(&encoded).map_err(HostCallError::Transport)?;
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(HostCallError::ResponseTooLarge(response.len()));
    }
    postcard::from_bytes(&response).map_err(HostCallError::Decode)
}

pub fn config_get(key: impl Into<String>) -> Result<Option<String>, HostCallError> {
    match call(PluginHostRequest::ConfigGet(PluginConfigGetRequest {
        key: key.into(),
    }))? {
        PluginHostResponse::ConfigGet(result) => {
            result_value(result).map(|response| response.value)
        }
        _ => Err(HostCallError::UnexpectedResponse("config_get")),
    }
}

pub fn state_get(key: impl Into<String>) -> Result<Option<Vec<u8>>, HostCallError> {
    match call(PluginHostRequest::StateGet(PluginStateGetRequest {
        key: key.into(),
    }))? {
        PluginHostResponse::StateGet(result) => result_value(result).map(|response| response.value),
        _ => Err(HostCallError::UnexpectedResponse("state_get")),
    }
}

pub fn state_set(key: impl Into<String>, value: Vec<u8>) -> Result<bool, HostCallError> {
    match call(PluginHostRequest::StateSet(PluginStateSetRequest {
        key: key.into(),
        value,
    }))? {
        PluginHostResponse::StateSet(result) => {
            result_value(result).map(|response| response.changed)
        }
        _ => Err(HostCallError::UnexpectedResponse("state_set")),
    }
}

pub fn state_delete(key: impl Into<String>) -> Result<bool, HostCallError> {
    match call(PluginHostRequest::StateDelete(PluginStateDeleteRequest {
        key: key.into(),
    }))? {
        PluginHostResponse::StateDelete(result) => {
            result_value(result).map(|response| response.changed)
        }
        _ => Err(HostCallError::UnexpectedResponse("state_delete")),
    }
}

pub fn http(request: PluginHttpRequest) -> Result<PluginHttpResponse, HostCallError> {
    match call(PluginHostRequest::Http(request))? {
        PluginHostResponse::Http(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("http")),
    }
}

pub fn socket_open(request: SocketOpenRequest) -> Result<SocketOpenResponse, HostCallError> {
    match call(PluginHostRequest::SocketOpen(request))? {
        PluginHostResponse::SocketOpen(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("socket_open")),
    }
}

pub fn socket_read(request: SocketReadRequest) -> Result<SocketReadResponse, HostCallError> {
    match call(PluginHostRequest::SocketRead(request))? {
        PluginHostResponse::SocketRead(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("socket_read")),
    }
}

pub fn socket_write(request: SocketWriteRequest) -> Result<SocketWriteResponse, HostCallError> {
    match call(PluginHostRequest::SocketWrite(request))? {
        PluginHostResponse::SocketWrite(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("socket_write")),
    }
}

pub fn socket_starttls(
    request: SocketStartTlsRequest,
) -> Result<SocketStartTlsResponse, HostCallError> {
    match call(PluginHostRequest::SocketStartTls(request))? {
        PluginHostResponse::SocketStartTls(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("socket_starttls")),
    }
}

pub fn socket_close(request: SocketCloseRequest) -> Result<SocketCloseResponse, HostCallError> {
    match call(PluginHostRequest::SocketClose(request))? {
        PluginHostResponse::SocketClose(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("socket_close")),
    }
}

pub fn process_exec(
    request: PluginProcessExecRequest,
) -> Result<PluginProcessExecResponse, HostCallError> {
    match call(PluginHostRequest::ProcessExec(request))? {
        PluginHostResponse::ProcessExec(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("process_exec")),
    }
}

/// Open a bounded archive through the host's extraction service.
///
/// This is deliberately general rather than per-family: the host stages
/// `content` in a private workspace, delegates to the installed
/// archive-extractor plugin, and hands back the members. A subtitle provider
/// unpacking an `.xz` attachment and a download client reading a container use
/// the same call, and neither is granted filesystem access to either side of
/// that boundary.
///
/// A host with no archive extractor installed answers in-band, so the caller
/// sees [`HostCallError::Service`] carrying a `PluginErrorCode::Unsupported`
/// error rather than a transport failure.
#[cfg(feature = "archive-extract")]
pub fn archive_extract(
    request: PluginArchiveExtractRequest,
) -> Result<PluginArchiveExtractResponse, HostCallError> {
    match call(PluginHostRequest::ArchiveExtract(request))? {
        PluginHostResponse::ArchiveExtract(result) => result_value(result),
        _ => Err(HostCallError::UnexpectedResponse("archive_extract")),
    }
}

fn result_value<T>(result: PluginResult<T>) -> Result<T, HostCallError> {
    match result {
        PluginResult::Ok(value) => Ok(value),
        PluginResult::Err(error) => Err(HostCallError::Service(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::PluginErrorCode;
    use scryer_plugin_sdk::host::PluginConfigGetResponse;
    use std::sync::{Mutex, MutexGuard};

    /// The transport registry is process-wide, so tests that install one take
    /// this lock and clear it again. Guests never need it: a component
    /// instance runs exactly one invocation and is then dropped.
    static REGISTRY: Mutex<()> = Mutex::new(());

    fn lock_registry() -> MutexGuard<'static, ()> {
        REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn clear_host_call() {
        *HOST_CALL.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    #[test]
    fn guests_without_a_transport_report_host_unavailable() {
        let _guard = lock_registry();
        clear_host_call();

        let error = config_get("base_url").expect_err("no transport is installed");
        assert!(matches!(error, HostCallError::Unavailable));

        let error = http(scryer_plugin_sdk::host::PluginHttpRequest {
            url: "https://example.invalid".to_string(),
            method: None,
            headers: Default::default(),
            body: Vec::new(),
        })
        .expect_err("no transport is installed");
        assert!(matches!(error, HostCallError::Unavailable));
    }

    #[test]
    fn an_installed_transport_carries_postcard_requests_and_responses() {
        let _guard = lock_registry();

        fn transport(request: &[u8]) -> Result<Vec<u8>, HostTransportError> {
            let request: PluginHostRequest =
                postcard::from_bytes(request).map_err(|_| HostTransportError::InvalidRequest)?;
            let PluginHostRequest::ConfigGet(request) = request else {
                return Err(HostTransportError::InvalidRequest);
            };
            assert_eq!(request.key, "base_url");
            let response =
                PluginHostResponse::ConfigGet(PluginResult::Ok(PluginConfigGetResponse {
                    value: Some("https://example.invalid".to_string()),
                }));
            postcard::to_allocvec(&response).map_err(|_| HostTransportError::Failed)
        }

        install_host_call(transport);
        assert!(host_call_installed());
        assert_eq!(
            config_get("base_url").expect("the installed transport answers"),
            Some("https://example.invalid".to_string())
        );

        clear_host_call();
        assert!(!host_call_installed());
    }

    #[cfg(feature = "archive-extract")]
    #[test]
    fn an_unsupported_capability_arrives_as_a_typed_service_error() {
        let _guard = lock_registry();

        // Every optional field is populated on purpose. `PluginError` carries
        // `#[serde(skip_serializing_if = "Option::is_none")]` on
        // `debug_message` and `retry_after_seconds`, which is correct for the
        // JSON command envelope and wrong for postcard: a non-self-describing
        // format writes fewer fields than the derived deserializer reads, so a
        // `None` there makes the response undecodable
        // ("Hit the end of buffer"). That asymmetry is the *host's* to fix —
        // see `postcard_rejects_a_plugin_error_with_skipped_fields` below,
        // which pins the hazard so it is not rediscovered from a confusing
        // decode error in production.
        fn transport(_request: &[u8]) -> Result<Vec<u8>, HostTransportError> {
            let response = PluginHostResponse::ArchiveExtract(PluginResult::Err(PluginError {
                code: PluginErrorCode::Unsupported,
                public_message: "no archive extractor is installed".to_string(),
                debug_message: Some("no archive extractor is installed".to_string()),
                retry_after_seconds: Some(0),
                details: None,
            }));
            postcard::to_allocvec(&response).map_err(|_| HostTransportError::Failed)
        }

        install_host_call(transport);
        let error = archive_extract(PluginArchiveExtractRequest {
            content: Vec::new(),
            format: "xz".to_string(),
            filename: None,
            password: None,
        })
        .expect_err("the host has no extractor");
        assert!(
            matches!(
                &error,
                HostCallError::Service(error) if error.code == PluginErrorCode::Unsupported
            ),
            "capability availability must stay in-band, got {error}"
        );

        clear_host_call();
    }

    /// Pin the encoding hazard on the in-band error path.
    ///
    /// This is not PDK behaviour under test — it is a property of the SDK's
    /// `PluginError` that the whole "capability availability is in-band"
    /// contract depends on. A host answering `Unsupported` with the natural
    /// `debug_message: None, retry_after_seconds: None` produces bytes this
    /// guest cannot decode, and the plugin sees `Decode` instead of
    /// `Service(Unsupported)`. When the SDK drops `skip_serializing_if` from
    /// those fields (or the transport stops using postcard), this test starts
    /// failing and should simply be deleted.
    #[test]
    fn postcard_rejects_a_plugin_error_with_skipped_fields() {
        let error = PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "no archive extractor is installed".to_string(),
            debug_message: None,
            retry_after_seconds: None,
            details: None,
        };
        let encoded = postcard::to_allocvec(&error).expect("encode");
        assert!(
            postcard::from_bytes::<PluginError>(&encoded).is_err(),
            "PluginError now round-trips through postcard; delete this test and the \
             work-around in the test above"
        );
    }

    #[test]
    fn a_transport_failure_is_not_a_service_error() {
        let _guard = lock_registry();

        fn transport(_request: &[u8]) -> Result<Vec<u8>, HostTransportError> {
            Err(HostTransportError::Failed)
        }

        install_host_call(transport);
        let error = config_get("base_url").expect_err("the transport failed");
        assert!(matches!(
            error,
            HostCallError::Transport(HostTransportError::Failed)
        ));

        clear_host_call();
    }
}
