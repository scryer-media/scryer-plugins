//! Compile-only guard for the async indexer-component macro.

use scryer_plugin_pdk::{Error, sdk};

fn descriptor() -> sdk::PluginDescriptor {
    unreachable!("component smoke fixture is never invoked")
}

async fn search(
    _request: sdk::PluginSearchRequest,
) -> Result<sdk::PluginSearchResponse, Error> {
    Ok(sdk::PluginSearchResponse::default())
}

scryer_plugin_pdk::scryer_indexer_component_main!(
    descriptor = descriptor,
    search = search,
);
