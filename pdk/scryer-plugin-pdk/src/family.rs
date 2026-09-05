//! WASI Preview 2 component glue for Scryer's command-envelope plugin
//! families.
//!
//! # What a family world is
//!
//! `scryer:subtitle/subtitle-provider@1.0.0`,
//! `scryer:download-client/download-client@1.0.0` and
//! `scryer:notification/notification@1.0.0` are the same world three
//! times over:
//!
//! ```wit
//! import scryer:host/services@1.0.0;
//! export describe: func() -> list<u8>;
//! export process: func(request: list<u8>) -> result<list<u8>, invocation-error>;
//! ```
//!
//! `scryer:subtitle@1.1.0` is the first revision to diverge: it adds
//! `import scryer:runtime/host@1.0.0` and lifts `process` to an `async func`,
//! so a provider can await an upstream request inside one invocation instead
//! of blocking a host thread on it. `describe` stays synchronous — it returns
//! a fixed document and awaits nothing — and the download-client and
//! notification worlds stay entirely synchronous on their 1.0.0 revisions.
//!
//! Both export payloads are UTF-8 JSON, and `process` carries the
//! [`PluginCommandRequest`]/[`PluginCommandResponse`] envelope defined by the
//! SDK's command ABI. **A guest owns its request and response types and its
//! dispatch `match`.** That is the whole reason this module is thin: it decodes the
//! envelope, checks the ABI version and the family tag, calls the plugin's
//! existing handler, and encodes the response.
//!
//! # Where the host-services import lives
//!
//! In the plugin crate, not here. `wit_bindgen` generates bindings per world,
//! so the PDK cannot own a `host-call` that serves three worlds at once
//! without picking one of them and hoping the others stay structurally
//! identical for ever. Instead each family entry macro installs a shim over
//! the plugin's own generated import via [`crate::host::install_host_call`],
//! and everything under [`crate::host`] then works unchanged. See that
//! module for the transport contract.
//!
//! # The whole of a migrated plugin's boilerplate
//!
//! ```ignore
//! wit_bindgen::generate!({ world: "subtitle-provider", path: "wit" });
//!
//! scryer_plugin_pdk::scryer_subtitle_component_main!(
//!     descriptor = build_descriptor,
//!     handler = handle_subtitle_command,
//! );
//! ```
//!
//! The macro must be invoked in the same module as `generate!`, because it
//! names that module's generated `Guest`, `InvocationError`, `export!`, and
//! `scryer::host::services`.

use std::future::Future;

use scryer_plugin_sdk::PluginDescriptor;
use scryer_plugin_sdk::command::{
    COMMAND_ABI_VERSION, PluginCommand, PluginCommandRequest, PluginCommandResponse,
    PluginCommandResult, PluginDownloadClientCommand, PluginDownloadClientCommandResult,
    PluginNotificationCommand, PluginNotificationCommandResult, PluginSubtitleCommand,
    PluginSubtitleCommandResult,
};

/// A world-level `invocation-error`, before it is mapped onto the plugin's own
/// generated enum.
///
/// This mirrors the three cases every family world declares. The PDK cannot
/// name the generated type — it belongs to the plugin crate — so the entry
/// macro converts this into it at the boundary.
///
/// It is reserved for payloads that cannot be parsed or produced at all.
/// Ordinary operational failures — a rejected configuration, an unreachable
/// provider, a rate limit — are a perfectly good [`PluginCommandResponse`]
/// carrying a typed `PluginResult::Err`, so the host keeps the plugin's own
/// diagnosis instead of a generic ABI failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationFailure {
    Failed,
    Cancelled,
    InvalidResponse,
}

/// Serialize the plugin descriptor for the world's `describe` export.
///
/// `describe` returns a bare `list<u8>`, so a serialization failure has no
/// channel of its own; an empty document is emitted and the host reports it as
/// an invalid descriptor. Descriptors are fixed literals in practice, so that
/// path is unreachable.
#[must_use]
pub fn descriptor_bytes(descriptor: PluginDescriptor) -> Vec<u8> {
    serde_json::to_vec(&descriptor).unwrap_or_default()
}

/// Decode one command envelope and check its ABI version.
///
/// Both dispatchers below share this so the envelope contract has exactly one
/// implementation, whatever shape the world's `process` export has.
fn decode_envelope(request: &[u8]) -> Result<PluginCommand, InvocationFailure> {
    let request: PluginCommandRequest =
        serde_json::from_slice(request).map_err(|_| InvocationFailure::InvalidResponse)?;
    if request.abi_version != COMMAND_ABI_VERSION {
        return Err(InvocationFailure::InvalidResponse);
    }
    Ok(request.command)
}

/// Encode one dispatch outcome as the world's response payload.
fn encode_response(result: PluginCommandResult) -> Result<Vec<u8>, InvocationFailure> {
    serde_json::to_vec(&PluginCommandResponse::new(result)).map_err(|_| InvocationFailure::Failed)
}

/// Decode one command envelope, dispatch it, and encode the response.
///
/// `handler` receives the whole [`PluginCommand`] and returns the matching
/// [`PluginCommandResult`]; the family helpers below wrap it so a plugin only
/// ever sees its own family's operations.
pub fn dispatch_command<H>(request: Vec<u8>, handler: H) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginCommand) -> Result<PluginCommandResult, InvocationFailure>,
{
    encode_response(handler(decode_envelope(&request)?)?)
}

/// The suspending form of [`dispatch_command`].
///
/// `scryer:subtitle@1.1.0` exports `process` as an `async func`, so its handler
/// may await host capabilities — an HTTP attempt, a rate gate — between the
/// envelope decode and the response encode. Nothing else about the contract
/// changes: the same ABI-version check runs first and the same JSON envelope
/// comes back.
pub async fn dispatch_command_async<H, F>(
    request: Vec<u8>,
    handler: H,
) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginCommand) -> F,
    F: Future<Output = Result<PluginCommandResult, InvocationFailure>>,
{
    encode_response(handler(decode_envelope(&request)?).await?)
}

/// Dispatch one `scryer:subtitle/subtitle-provider@1.1.0` invocation.
///
/// The handler sees the SDK's [`PluginSubtitleCommand`] — `ValidateConfig`,
/// `Search`, `Download`, `Generate` — and is `async`, because the world's
/// `process` export is. A subtitle provider talking to an upstream API awaits
/// [`crate::runtime::http`] in the middle of that handler instead of having
/// the host thread block on it.
pub async fn dispatch_subtitle<H, F>(
    request: Vec<u8>,
    handler: H,
) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginSubtitleCommand) -> F,
    F: Future<Output = PluginSubtitleCommandResult>,
{
    dispatch_command_async(request, |command| async move {
        match command {
            PluginCommand::Subtitle(command) => {
                Ok(PluginCommandResult::Subtitle(handler(command).await))
            }
            _ => Err(InvocationFailure::InvalidResponse),
        }
    })
    .await
}

/// Dispatch one `scryer:download-client/download-client` invocation.
pub fn dispatch_download_client<H>(
    request: Vec<u8>,
    handler: H,
) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginDownloadClientCommand) -> PluginDownloadClientCommandResult,
{
    dispatch_command(request, |command| match command {
        PluginCommand::DownloadClient(command) => {
            Ok(PluginCommandResult::DownloadClient(handler(command)))
        }
        _ => Err(InvocationFailure::InvalidResponse),
    })
}

/// Dispatch one `scryer:notification/notification` invocation.
pub fn dispatch_notification<H>(request: Vec<u8>, handler: H) -> Result<Vec<u8>, InvocationFailure>
where
    H: FnOnce(PluginNotificationCommand) -> PluginNotificationCommandResult,
{
    dispatch_command(request, |command| match command {
        PluginCommand::Notification(command) => {
            Ok(PluginCommandResult::Notification(handler(command)))
        }
        _ => Err(InvocationFailure::InvalidResponse),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::{
        PluginResult, SubtitlePluginSearchResponse, SubtitlePluginValidateConfigRequest,
        SubtitlePluginValidateConfigResponse, SubtitleValidateConfigStatus,
    };

    /// Drive a future that cannot suspend to completion.
    ///
    /// These cases exercise the envelope contract, not the host: no capability
    /// is installed, so every handler here resolves on its first poll. Polling
    /// once and refusing to loop keeps a future that *would* suspend from
    /// silently hanging the suite.
    fn now_or_never<F: Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        // SAFETY: every function in `VTABLE` ignores its data pointer.
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        match std::pin::pin!(future).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the dispatch future suspended without a host to wait on"),
        }
    }

    fn validate_config_request() -> Vec<u8> {
        serde_json::to_vec(&PluginCommandRequest::new(PluginCommand::Subtitle(
            PluginSubtitleCommand::ValidateConfig(SubtitlePluginValidateConfigRequest::default()),
        )))
        .expect("encode request")
    }

    async fn ok_validate(command: PluginSubtitleCommand) -> PluginSubtitleCommandResult {
        match command {
            PluginSubtitleCommand::ValidateConfig(_) => {
                PluginSubtitleCommandResult::ValidateConfig(PluginResult::Ok(
                    SubtitlePluginValidateConfigResponse {
                        status: SubtitleValidateConfigStatus::Valid,
                        message: None,
                        retry_after_seconds: None,
                    },
                ))
            }
            _ => PluginSubtitleCommandResult::Search(PluginResult::Ok(
                SubtitlePluginSearchResponse::default(),
            )),
        }
    }

    #[test]
    fn a_subtitle_envelope_round_trips_through_the_world_payload() {
        let encoded = now_or_never(dispatch_subtitle(validate_config_request(), ok_validate))
            .expect("dispatch succeeds");
        let response: PluginCommandResponse =
            serde_json::from_slice(&encoded).expect("decode response");
        assert_eq!(response.abi_version, COMMAND_ABI_VERSION);
        assert!(matches!(
            response.response,
            PluginCommandResult::Subtitle(PluginSubtitleCommandResult::ValidateConfig(
                PluginResult::Ok(_)
            ))
        ));
    }

    #[test]
    fn another_family_is_an_invocation_error_not_a_typed_result() {
        let request = serde_json::to_vec(&PluginCommandRequest::new(
            PluginCommand::DownloadClient(PluginDownloadClientCommand::GetCompleted(
                scryer_plugin_sdk::command::PluginDownloadGetCompletedRequest {
                    client_item_id: "opaque".to_string(),
                },
            )),
        ))
        .expect("encode request");
        assert_eq!(
            now_or_never(dispatch_subtitle(request, ok_validate)),
            Err(InvocationFailure::InvalidResponse)
        );
    }

    #[test]
    fn an_unsupported_abi_version_is_rejected_before_dispatch() {
        let mut request: serde_json::Value =
            serde_json::from_slice(&validate_config_request()).expect("decode request");
        request["abi_version"] = serde_json::json!(COMMAND_ABI_VERSION + 1);
        let request = serde_json::to_vec(&request).expect("encode request");
        assert_eq!(
            now_or_never(dispatch_subtitle(request, ok_validate)),
            Err(InvocationFailure::InvalidResponse)
        );
    }

    #[test]
    fn a_descriptor_is_emitted_as_utf8_json() {
        let descriptor = PluginDescriptor {
            id: "test".to_string(),
            name: "Test".to_string(),
            version: "0.1.0".to_string(),
            sdk_version: scryer_plugin_sdk::SDK_VERSION.to_string(),
            sdk_constraint: scryer_plugin_sdk::current_sdk_constraint(),
            socket_permissions: vec![],
            provider: scryer_plugin_sdk::ProviderDescriptor::Notification(
                scryer_plugin_sdk::NotificationDescriptor {
                    provider_type: "test".to_string(),
                    provider_aliases: vec![],
                    config_fields: vec![],
                    default_base_url: None,
                    allowed_hosts: vec![],
                    capabilities: scryer_plugin_sdk::NotificationCapabilities::default(),
                },
            ),
        };
        let bytes = descriptor_bytes(descriptor);
        let decoded: serde_json::Value =
            serde_json::from_slice(&bytes).expect("descriptor is JSON");
        assert_eq!(decoded["id"], "test");
    }
}
