#[plugin_fn]
pub fn scryer_indexer_search(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(PluginSearchResponse::default()))?)
}
