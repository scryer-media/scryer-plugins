//! Descriptor-only WASIp2 component runner used while packaging components.
//!
//! Every component family exposes the same synchronous `describe` operation, so
//! one runner serves them all: it registers every world's imports — the indexer
//! host interfaces, the archive extractor's crypto interface, and the shared
//! `scryer:host/services` door every remaining family uses — with deliberately
//! inert implementations, so descriptor extraction cannot perform network I/O,
//! decrypt anything, read configuration, or mutate state.

use anyhow::{Result, anyhow, bail};
use futures::executor::block_on;
use scryer_plugin_sdk::{PluginError, PluginErrorCode};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::PluginDescriptor;

mod contract_v1_0 {
    wasmtime::component::bindgen!({
        world: "scryer:indexer/indexer-plugin@1.0.0",
        path: "wit",
    });
}

mod contract_v1_1 {
    wasmtime::component::bindgen!({
        world: "scryer:indexer/indexer-plugin@1.1.0",
        path: "wit/indexer-v1.1.0",
    });
}

/// The archive world. Only its `crypto` import matters here: `describe` has the
/// same `func() -> list<u8>` shape as the indexer worlds and is called through
/// the shared export lookup below.
mod archive_v1_0 {
    wasmtime::component::bindgen!({
        world: "scryer:archive/archive-extractor@1.0.0",
        path: "wit/archive-v1.0.0",
    });
}

/// The shared host-services door.
///
/// Subtitles, download clients and notifications all import
/// `scryer:host/services@1.0.0` and differ only in the `describe`/`process`
/// payloads they exchange, so this is bound **once, per interface** rather
/// than once per family world. The subtitle world is generated here purely
/// because a WIT world is what `bindgen!` takes; nothing below names it.
mod host_v1_0 {
    wasmtime::component::bindgen!({
        world: "scryer:subtitle/subtitle-provider@1.0.0",
        // Two packages, two paths — the shared host package first, so the
        // family package's `import scryer:host/services@1.0.0` resolves
        // against it.
        path: ["wit/host-v1.0.0", "wit/subtitle-v1.0.0"],
    });
}

use self::contract_v1_0::scryer::indexer::host::{
    Host, HostWithStore, HttpRequest, HttpResponse, LogLevel, TransportError,
};

struct DescriptorCtx {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for DescriptorCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl Host for DescriptorCtx {
    fn monotonic_now_ms(&mut self) -> u64 {
        0
    }

    fn operation_deadline_monotonic_ms(&mut self) -> u64 {
        0
    }

    fn wall_now_ms(&mut self) -> u64 {
        0
    }

    fn config_get(&mut self, _key: String) -> Option<String> {
        None
    }

    fn state_get(&mut self, _key: String) -> Option<Vec<u8>> {
        None
    }

    fn state_cas(
        &mut self,
        _key: String,
        _expected: Option<Vec<u8>>,
        _replacement: Option<Vec<u8>>,
    ) -> bool {
        false
    }

    fn log(&mut self, _level: LogLevel, _message: String) {}
}

impl HostWithStore<DescriptorCtx> for HasSelf<DescriptorCtx> {
    async fn http(
        _accessor: &wasmtime::component::Accessor<DescriptorCtx, Self>,
        _request: HttpRequest,
    ) -> Result<HttpResponse, TransportError> {
        Err(TransportError::ForbiddenOrigin)
    }

    async fn sleep(
        _accessor: &wasmtime::component::Accessor<DescriptorCtx, Self>,
        _duration_ms: u64,
    ) {
    }
}

impl contract_v1_1::scryer::indexer::host::Host for DescriptorCtx {
    fn monotonic_now_ms(&mut self) -> u64 {
        0
    }

    fn operation_deadline_monotonic_ms(&mut self) -> u64 {
        0
    }

    fn wall_now_ms(&mut self) -> u64 {
        0
    }

    fn config_get(&mut self, _key: String) -> Option<String> {
        None
    }

    fn provider_profile(&mut self) -> Option<Vec<u8>> {
        None
    }

    fn state_get(&mut self, _key: String) -> Option<Vec<u8>> {
        None
    }

    fn state_cas(
        &mut self,
        _key: String,
        _expected: Option<Vec<u8>>,
        _replacement: Option<Vec<u8>>,
    ) -> bool {
        false
    }

    fn log(&mut self, _level: contract_v1_1::scryer::indexer::host::LogLevel, _message: String) {}
}

/// Deliberately inert host services, exactly like the HTTP host above.
///
/// The distinction this implementation has to get right is the one the WIT
/// spells out: **`host-error` is transport failure, capability availability is
/// in-band.** A descriptor extraction runs against a host with no services
/// configured at all, which in Scryer is not an error condition — it is a
/// `PluginHostResponse` carrying `PluginResult::Err(Unsupported)`. Returning
/// `host-error` instead would make a guest that merely *asks* during
/// `describe` look like a broken artifact rather than one running on a bare
/// host, and would train guests to treat a recoverable answer as fatal.
///
/// A well-behaved guest makes no host calls during `describe` at all; this is
/// the floor under that, not a service.
impl host_v1_0::scryer::host::services::Host for DescriptorCtx {
    fn host_call(
        &mut self,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, host_v1_0::scryer::host::services::HostError> {
        inert_host_response(&request)
            .ok_or(host_v1_0::scryer::host::services::HostError::InvalidRequest)
    }
}

/// Encode the in-band `Unsupported` answer for whatever capability was asked
/// for, without decoding the request's payload.
///
/// This runner deliberately does not depend on the SDK's *capability set*.
/// `PluginHostRequest` and `PluginHostResponse` are parallel enums whose
/// variants are added in lockstep, and postcard frames an enum as a varint
/// discriminant followed by the payload — so echoing the discriminant and
/// appending `PluginResult::Err` (variant 1) plus the error produces the
/// correctly-typed response for a capability this xtask has never heard of.
/// A guest built against a newer SDK therefore still gets `Unsupported`
/// during descriptor extraction rather than a trap, which is the whole point
/// of keeping the capability set out of WIT in the first place.
fn inert_host_response(request: &[u8]) -> Option<Vec<u8>> {
    let (discriminant, _) = read_varint_u32(request)?;
    let error = PluginError {
        code: PluginErrorCode::Unsupported,
        public_message: "Scryer host services are not available during descriptor extraction"
            .to_string(),
        // Both optional fields are filled in on purpose: `PluginError` carries
        // `skip_serializing_if`, which postcard cannot round-trip — a `None`
        // here produces bytes the guest's decoder rejects outright.
        debug_message: Some(
            "the packaging descriptor runner registers every host import inert".to_string(),
        ),
        retry_after_seconds: Some(0),
        details: None,
    };

    let mut encoded = Vec::new();
    write_varint_u32(discriminant, &mut encoded);
    // `PluginResult::Err` is variant 1 for every payload type.
    write_varint_u32(1, &mut encoded);
    encoded.extend_from_slice(&postcard::to_allocvec(&error).ok()?);
    Some(encoded)
}

/// Postcard's unsigned varint: little-endian base-128, high bit continues.
fn read_varint_u32(bytes: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    for (index, byte) in bytes.iter().take(5).enumerate() {
        value |= u32::from(byte & 0x7f).checked_shl(7 * index as u32)?;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn write_varint_u32(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

/// Deliberately inert crypto, exactly like the HTTP host above.
///
/// `describe` is a pure function of the artifact and must never decrypt or
/// checksum anything; an archive component that reaches these during descriptor
/// extraction is misbehaving, and gets a rejection rather than a working core.
impl archive_v1_0::scryer::archive::crypto::Host for DescriptorCtx {
    fn aes_cbc_decrypt(
        &mut self,
        _key: Vec<u8>,
        _iv: Vec<u8>,
        _data: Vec<u8>,
    ) -> Result<Vec<u8>, archive_v1_0::scryer::archive::crypto::AesError> {
        Err(archive_v1_0::scryer::archive::crypto::AesError::BadKeyLength)
    }

    fn crc32(&mut self, seed: u32, _data: Vec<u8>) -> u32 {
        seed
    }
}

impl contract_v1_1::scryer::indexer::host::HostWithStore<DescriptorCtx> for HasSelf<DescriptorCtx> {
    async fn http(
        _accessor: &wasmtime::component::Accessor<DescriptorCtx, Self>,
        _request: contract_v1_1::scryer::indexer::host::HttpRequest,
    ) -> Result<
        contract_v1_1::scryer::indexer::host::HttpResponse,
        contract_v1_1::scryer::indexer::host::TransportError,
    > {
        Err(contract_v1_1::scryer::indexer::host::TransportError::ForbiddenOrigin)
    }

    async fn sleep(
        _accessor: &wasmtime::component::Accessor<DescriptorCtx, Self>,
        _duration_ms: u64,
    ) {
    }

    async fn emit_strategy_event(
        _accessor: &wasmtime::component::Accessor<DescriptorCtx, Self>,
        _event: Vec<u8>,
    ) -> Result<(), contract_v1_1::scryer::indexer::host::StrategyEventError> {
        Err(contract_v1_1::scryer::indexer::host::StrategyEventError::NoActivePlan)
    }
}

pub(crate) fn descriptor_from_component(wasm: &[u8]) -> Result<Option<PluginDescriptor>> {
    if !wasm.starts_with(b"\0asm\r\0\x01\0") {
        return Ok(None);
    }

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)
        .map_err(|error| anyhow!("create component descriptor engine: {error:#}"))?;
    let component = Component::from_binary(&engine, wasm)
        .map_err(|error| anyhow!("compile plugin component: {error:#}"))?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|error| anyhow!("register WASI Preview 2 for component descriptor: {error:#}"))?;
    contract_v1_0::IndexerPlugin::add_to_linker::<DescriptorCtx, HasSelf<DescriptorCtx>>(
        &mut linker,
        |ctx| ctx,
    )
    .map_err(|error| anyhow!("register indexer 1.0 component descriptor host: {error:#}"))?;
    contract_v1_1::IndexerPlugin::add_to_linker::<DescriptorCtx, HasSelf<DescriptorCtx>>(
        &mut linker,
        |ctx| ctx,
    )
    .map_err(|error| anyhow!("register indexer 1.1 component descriptor host: {error:#}"))?;
    // Archive extractors import `crypto` instead of the indexer host. Every
    // world here is registered on the same linker and an unused import costs
    // nothing, so one instantiate path serves all of them — what distinguishes
    // the families is the `describe` document they return, not how they are run.
    archive_v1_0::ArchiveExtractor::add_to_linker::<DescriptorCtx, HasSelf<DescriptorCtx>>(
        &mut linker,
        |ctx| ctx,
    )
    .map_err(|error| anyhow!("register archive component descriptor host: {error:#}"))?;
    // Bound per *interface*, not per world: subtitles, download clients and
    // notifications import the identical `scryer:host/services@1.0.0`, so
    // registering each family world in turn would be a duplicate definition
    // rather than three registrations.
    host_v1_0::scryer::host::services::add_to_linker::<DescriptorCtx, HasSelf<DescriptorCtx>>(
        &mut linker,
        |ctx| ctx,
    )
    .map_err(|error| anyhow!("register shared host services for descriptors: {error:#}"))?;
    let mut store = Store::new(
        &engine,
        DescriptorCtx {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
        },
    );
    let instance = block_on(linker.instantiate_async(&mut store, &component))
        .map_err(|error| anyhow!("instantiate plugin component descriptor: {error:#}"))?;
    let describe = instance
        .get_typed_func::<(), (Vec<u8>,)>(&mut store, "describe")
        .map_err(|error| {
            anyhow!("plugin component lacks a compatible describe export: {error:#}")
        })?;
    let (encoded,) = block_on(
        store.run_concurrent(async move |accessor| describe.call_concurrent(accessor, ()).await),
    )
    .map_err(|error| anyhow!("plugin component descriptor scheduling failed: {error:#}"))?
    .map_err(|error| anyhow!("plugin component describe failed: {error:#}"))?;
    let descriptor = serde_json::from_slice(&encoded).map_err(|error| {
        anyhow!("plugin component describe returned invalid UTF-8 JSON: {error}")
    })?;
    if encoded.is_empty() {
        bail!("plugin component describe returned an empty descriptor")
    }
    Ok(Some(descriptor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scryer_plugin_sdk::PluginResult;
    use scryer_plugin_sdk::host::{PluginConfigGetRequest, PluginHostRequest, PluginHostResponse};

    #[test]
    fn a_describe_time_host_call_answers_unsupported_in_band() {
        let request =
            postcard::to_allocvec(&PluginHostRequest::ConfigGet(PluginConfigGetRequest {
                key: "base_url".to_string(),
            }))
            .expect("encode request");

        let encoded = inert_host_response(&request).expect("the runner always answers");
        let response: PluginHostResponse =
            postcard::from_bytes(&encoded).expect("the answer is a well-formed host response");

        match response {
            PluginHostResponse::ConfigGet(PluginResult::Err(error)) => {
                assert_eq!(error.code, PluginErrorCode::Unsupported);
            }
            other => panic!("descriptor extraction answered the wrong operation: {other:?}"),
        }
    }

    /// The forward-compatibility property: a capability this xtask's SDK does
    /// not know still gets the correctly-typed in-band answer, because the
    /// discriminant is echoed rather than interpreted.
    #[test]
    fn an_unknown_capability_still_gets_its_own_response_variant() {
        // Discriminant 12 is `ArchiveExtract` in the SDK that ships with the
        // component family hosts, and does not exist in the published one.
        let mut request = vec![12u8];
        request.extend_from_slice(&[0, 2, b'x', b'z', 0, 0]);

        let encoded = inert_host_response(&request).expect("the runner always answers");
        assert_eq!(
            encoded[0], 12,
            "the response must match the request variant"
        );
        assert_eq!(encoded[1], 1, "PluginResult::Err is variant 1");
    }

    #[test]
    fn varints_round_trip_across_the_continuation_boundary() {
        for value in [0u32, 1, 12, 127, 128, 300, u32::MAX] {
            let mut encoded = Vec::new();
            write_varint_u32(value, &mut encoded);
            assert_eq!(read_varint_u32(&encoded), Some((value, encoded.len())));
        }
    }
}
