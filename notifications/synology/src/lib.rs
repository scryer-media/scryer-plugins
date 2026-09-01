//! Synology media-indexer notifications, as a WASI Preview 2 component.
//!
//! The plugin implements `scryer:notification/notification@1.0.0`: two exports
//! carrying UTF-8 JSON (`describe` returns a `PluginDescriptor`, `process`
//! exchanges a `PluginCommandRequest` for a `PluginCommandResponse`), plus the
//! shared `scryer:host/services@1.0.0` import.
//!
//! ## The family's only process user
//!
//! This channel sends nothing over the network. It runs `synoindex` on the NAS
//! so the DSM media index learns about a file Scryer just imported, renamed or
//! deleted — the one notification channel whose delivery is a host process
//! rather than a request.
//!
//! WASI Preview 2 has no process-execution capability at all, so this cannot
//! be a `wasi:*` import the way a socket theoretically could be. It arrives as
//! `PluginHostRequest::ProcessExec` on the same single `host-call` import
//! every other service uses, through [`notify_common::process_exec`]. That is
//! the notification world's design note in practice: the SDK owns the
//! capability set, so a family needing authority beyond HTTP imports the same
//! one function as the families that do not.
//!
//! Authority is unchanged and stays host-side. `requires_host_process` on the
//! descriptor below is a *request*; the loader additionally gates process
//! execution to first-party plugins, so a community channel declaring the same
//! capability gets an empty allowlist and `permission_denied` per call. This
//! plugin's own allowlist entry is `/usr/syno/bin/synoindex` and nothing else.
//!
//! ## One behaviour change, in `notify-common`
//!
//! The Extism host function reported a timeout as a *successful* response with
//! a `timed_out` flag. `ProcessExec` has no such field: a host-enforced timeout
//! is a typed `PluginError`, so it now arrives on the `Err` arm of
//! [`notify_common::process_exec`] carrying the host's own message, and is
//! reported by [`run_synoindex`] there rather than in the `timed_out` arm.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use notify_common::*;

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:notification/notification@1.0.0",
    // Two packages, two paths, matching the host's own bindgen: the shared
    // `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/notification-v1.0.0"],
    // The shared host package lives in its own WIT package, so wit-bindgen
    // asks explicitly whether to generate for it. Yes: the PDK holds only a
    // `fn` pointer and the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

scryer_plugin_pdk::scryer_notification_component_main!(
    descriptor = build_descriptor,
    handler = handle_notification_command,
);

const PROVIDER_TYPE: &str = "synology";
const SYNOINDEX: &str = "/usr/syno/bin/synoindex";

fn build_descriptor() -> PluginDescriptor {
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
    descriptor
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

/// The world's single `process` entry, dispatching the SDK's notification
/// command enum.
///
/// One arm per Extism entry point this plugin used to export. `action` is not
/// one of them: the descriptor advertises no action, the host does not route
/// one here, and the arm answers **in-band** with `Unsupported` rather than
/// trapping — a trap under a component costs the whole instance and replaces
/// the plugin's own diagnosis with a generic ABI failure.
fn handle_notification_command(
    command: PluginNotificationCommand,
) -> PluginNotificationCommandResult {
    match command {
        PluginNotificationCommand::Send(request) => {
            PluginNotificationCommandResult::Send(PluginResult::Ok(send_notification(&request)))
        }
        PluginNotificationCommand::Action(_) => {
            PluginNotificationCommandResult::Action(unsupported_action(PROVIDER_TYPE))
        }
    }
}

fn send_notification(req: &PluginNotificationRequest) -> PluginNotificationResponse {
    if !config_bool("update_library") {
        return ok_response();
    }

    let mut responses = Vec::new();
    for args in synoindex_commands(req) {
        responses.push(run_synoindex(args));
    }

    merge_responses(responses)
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
