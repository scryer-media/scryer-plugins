//! Descriptor-only WASIp2 component runner used while packaging indexers.
//!
//! Components expose a synchronous descriptor operation. The runner wires the
//! full host interface with deliberately inert implementations so descriptor
//! extraction cannot perform network I/O, read configuration, or mutate state.

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
        .map_err(|error| anyhow!("compile indexer component: {error:#}"))?;
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
    let mut store = Store::new(
        &engine,
        DescriptorCtx {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
        },
    );
    let instance = block_on(linker.instantiate_async(&mut store, &component))
        .map_err(|error| anyhow!("instantiate indexer component descriptor: {error:#}"))?;
    let describe = instance
        .get_typed_func::<(), (Vec<u8>,)>(&mut store, "describe")
        .map_err(|error| {
            anyhow!("indexer component lacks a compatible describe export: {error:#}")
        })?;
    let (encoded,) = block_on(
        store.run_concurrent(async move |accessor| describe.call_concurrent(accessor, ()).await),
    )
    .map_err(|error| anyhow!("indexer component descriptor scheduling failed: {error:#}"))?
    .map_err(|error| anyhow!("indexer component describe failed: {error:#}"))?;
    let descriptor = serde_json::from_slice(&encoded).map_err(|error| {
        anyhow!("indexer component describe returned invalid UTF-8 JSON: {error}")
    })?;
    if encoded.is_empty() {
        bail!("indexer component describe returned an empty descriptor")
    }
    Ok(Some(descriptor))
}
