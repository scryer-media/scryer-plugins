use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use notify_common::*;

const SYNOINDEX: &str = "/usr/syno/bin/synoindex";

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let mut descriptor = build_notification_descriptor(
        "synology",
        "Synology Indexer",
        env!("CARGO_PKG_VERSION"),
        "synology",
        vec![NotificationDeliveryMode::MediaServerUpdate],
        vec![NotificationPayloadFormat::StructuredJson],
        config_fields(),
        false,
        false,
    );
    if let ProviderDescriptor::Notification(notification) = &mut descriptor.provider {
        notification.capabilities.requires_host_process = true;
    }
    Ok(serde_json::to_string(&descriptor)?)
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![field(
        "update_library",
        "Update Library",
        ConfigFieldType::Bool,
        false,
        Some("true"),
        Some("Run synoindex when Scryer imports, renames, or deletes media."),
    )]
}

#[plugin_fn]
pub fn scryer_notification_send(input: String) -> FnResult<String> {
    let req: PluginNotificationRequest = serde_json::from_str(&input)?;
    if !config_bool("update_library") {
        return Ok(serde_json::to_string(&PluginResult::Ok(ok_response()))?);
    }

    let mut responses = Vec::new();
    for args in synoindex_commands(&req) {
        responses.push(run_synoindex(args));
    }

    Ok(serde_json::to_string(&PluginResult::Ok(merge_responses(
        responses,
    )))?)
}

fn synoindex_commands(req: &PluginNotificationRequest) -> Vec<Vec<String>> {
    match req.event_type.as_str() {
        "download" | "upgrade" => {
            let mut commands = Vec::new();
            if let Some(import) = &req.import {
                commands.extend(
                    import
                        .deleted_paths
                        .iter()
                        .map(|path| vec!["-d".to_string(), path.clone()]),
                );
            }
            commands.extend(
                primary_paths(req)
                    .into_iter()
                    .map(|path| vec!["-a".to_string(), path]),
            );
            commands
        }
        "import_complete" | "rename" | "title_added" => title_path(req)
            .map(|path| vec![vec!["-R".to_string(), path]])
            .unwrap_or_default(),
        "file_deleted" | "file_deleted_for_upgrade" => primary_paths(req)
            .into_iter()
            .map(|path| vec!["-d".to_string(), path])
            .collect(),
        "title_deleted" => title_path(req)
            .map(|path| vec![vec!["-D".to_string(), path]])
            .unwrap_or_default(),
        "test" => vec![vec!["--help".to_string()]],
        _ => Vec::new(),
    }
}

fn primary_paths(req: &PluginNotificationRequest) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(file) = &req.file
        && let Some(path) = &file.primary_path
    {
        paths.push(path.clone());
    }
    for file in &req.media_files {
        paths.push(file.path.clone());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn title_path(req: &PluginNotificationRequest) -> Option<String> {
    req.title.as_ref().and_then(|title| title.path.clone())
}

fn run_synoindex(args: Vec<String>) -> PluginNotificationResponse {
    let allow_stdout = args.len() == 1 && args[0] == "--help";

    match process_exec(ProcessExecRequest {
        command: SYNOINDEX.to_string(),
        args,
        env: Default::default(),
        working_directory: None,
        stdin_base64: None,
        timeout_ms: Some(20000),
    }) {
        Ok(output) if output.timed_out => error_response("synoindex timed out", None),
        Ok(output) if output.status_code.unwrap_or(1) != 0 => error_response(
            format!(
                "synoindex exited with code {}{}",
                output
                    .status_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                process_output_suffix(&output)
            ),
            None,
        ),
        Ok(output) if has_output(&output.stderr_base64) => error_response(
            format!(
                "synoindex returned an error{}",
                process_output_suffix(&output)
            ),
            None,
        ),
        Ok(output) if has_output(&output.stdout_base64) && !allow_stdout => error_response(
            format!(
                "synoindex returned output{}",
                process_output_suffix(&output)
            ),
            None,
        ),
        Ok(_) => ok_response(),
        Err(error) => error_response(format!("synoindex failed: {}", error.message), None),
    }
}

fn has_output(encoded: &str) -> bool {
    STANDARD
        .decode(encoded.as_bytes())
        .map(|bytes| !String::from_utf8_lossy(&bytes).trim().is_empty())
        .unwrap_or(false)
}

fn process_output_suffix(output: &ProcessExecResponse) -> String {
    let stderr = decoded_trimmed(&output.stderr_base64);
    if !stderr.is_empty() {
        return format!(": {stderr}");
    }
    let stdout = decoded_trimmed(&output.stdout_base64);
    if !stdout.is_empty() {
        return format!(": {stdout}");
    }
    String::new()
}

fn decoded_trimmed(encoded: &str) -> String {
    STANDARD
        .decode(encoded.as_bytes())
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default()
}
