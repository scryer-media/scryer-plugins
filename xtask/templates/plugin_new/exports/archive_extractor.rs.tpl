#[plugin_fn]
pub fn scryer_archive_process(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(
        &PluginResult::<ArchivePluginProcessResponse>::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "archive extraction is not implemented".to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }),
    )?)
}
