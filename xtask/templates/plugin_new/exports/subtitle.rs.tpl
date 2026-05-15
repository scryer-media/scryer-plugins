#[plugin_fn]
pub fn scryer_validate_config(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::Ok(SubtitlePluginValidateConfigResponse {
            status: SubtitleValidateConfigStatus::Valid,
            message: None,
            retry_after_seconds: None,
        }),
    )?)
}

#[plugin_fn]
pub fn scryer_subtitle_search(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::Ok(SubtitlePluginSearchResponse::default()),
    )?)
}

#[plugin_fn]
pub fn scryer_subtitle_download(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::<SubtitlePluginDownloadResponse>::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "subtitle download is not implemented".to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }),
    )?)
}
