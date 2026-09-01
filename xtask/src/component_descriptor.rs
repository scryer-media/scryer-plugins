//! Descriptor-only WASIp2 component runner used while packaging components.
//!
//! Every component family exposes the same synchronous `describe` operation, so
//! one runner serves them all: it registers every world's imports — the indexer
//! host interfaces and the archive extractor's crypto interface — with
//! deliberately inert implementations, so descriptor extraction cannot perform
//! network I/O, decrypt anything, read configuration, or mutate state.

use anyhow::{Result, anyhow, bail};
use futures::executor::block_on;
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
