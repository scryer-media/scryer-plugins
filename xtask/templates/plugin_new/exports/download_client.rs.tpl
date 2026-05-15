#[plugin_fn]
pub fn scryer_download_add(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::<PluginDownloadClientAddResponse>::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "download add is not implemented".to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }),
    )?)
}

#[plugin_fn]
pub fn scryer_download_list_queue() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(Vec::<PluginDownloadItem>::new()))?)
}

#[plugin_fn]
pub fn scryer_download_list_history() -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::Ok(Vec::<PluginCompletedDownload>::new()),
    )?)
}

#[plugin_fn]
pub fn scryer_download_list_completed() -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::Ok(Vec::<PluginCompletedDownload>::new()),
    )?)
}

#[plugin_fn]
pub fn scryer_download_control(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status() -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::Ok(PluginDownloadClientStatus::default()),
    )?)
}

#[plugin_fn]
pub fn scryer_download_test_connection() -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}
