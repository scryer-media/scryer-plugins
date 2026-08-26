//! Temporary source bridge for the DLC-first command migration.
//!
//! The first-party clients keep their operation implementations while this
//! bridge turns the former JSON export functions into the typed command
//! protocol. It is intentionally PDK-owned so every migrated client has the
//! same exact completed-download lookup and no one reintroduces a raw path API.

use std::collections::HashMap;

use crate::sdk;
use crate::{
    FnResult, PluginDownloadClientCommand, PluginDownloadClientCommandResult,
    PluginDownloadGetCompletedRequest, run_download_client_plugin_with_descriptor,
};

pub struct LegacyDownloadClientFunctions {
    pub describe: fn(String) -> FnResult<String>,
    pub add: fn(String) -> FnResult<String>,
    pub list_queue: fn(String) -> FnResult<String>,
    pub list_history: fn(String) -> FnResult<String>,
    pub list_completed: fn(String) -> FnResult<String>,
    pub list_recent_completed: Option<fn(String) -> FnResult<String>>,
    pub control: fn(String) -> FnResult<String>,
    pub mark_imported: fn(String) -> FnResult<String>,
    pub mark_imported_non_destructive: Option<fn(String) -> FnResult<String>>,
    pub status: fn(String) -> FnResult<String>,
    pub test_connection: fn(String) -> FnResult<String>,
}

pub fn legacy_download_client_descriptor(
    functions: &LegacyDownloadClientFunctions,
) -> sdk::PluginDescriptor {
    let raw = (functions.describe)(String::new())
        .expect("first-party command DLC descriptor must serialize successfully");
    serde_json::from_str(&raw).expect("first-party command DLC descriptor must be valid")
}

pub fn run_download_client_bridge_with_descriptor(functions: LegacyDownloadClientFunctions) -> ! {
    let descriptor = legacy_download_client_descriptor(&functions);
    run_download_client_plugin_with_descriptor(
        move || descriptor,
        move |command| bridge_download_client_command(&functions, command),
    )
}

fn bridge_download_client_command(
    functions: &LegacyDownloadClientFunctions,
    command: PluginDownloadClientCommand,
) -> PluginDownloadClientCommandResult {
    match command {
        PluginDownloadClientCommand::Add(request) => {
            PluginDownloadClientCommandResult::Add(call(functions.add, request))
        }
        PluginDownloadClientCommand::ListQueue => {
            PluginDownloadClientCommandResult::ListQueue(list_queue_with_failed_history(functions))
        }
        PluginDownloadClientCommand::ListQueueScoped(_) => {
            PluginDownloadClientCommandResult::ListQueueScoped(scoped_list_response(
                list_queue_with_failed_history(functions),
            ))
        }
        PluginDownloadClientCommand::ListHistory => {
            PluginDownloadClientCommandResult::ListHistory(call(functions.list_completed, ()))
        }
        PluginDownloadClientCommand::ListHistoryScoped(_) => {
            PluginDownloadClientCommandResult::ListHistoryScoped(scoped_list_response(call(
                functions.list_completed,
                (),
            )))
        }
        PluginDownloadClientCommand::ListCompleted => {
            PluginDownloadClientCommandResult::ListCompleted(call(functions.list_completed, ()))
        }
        PluginDownloadClientCommand::ListCompletedScoped(_) => {
            PluginDownloadClientCommandResult::ListCompletedScoped(scoped_list_response(call(
                functions.list_completed,
                (),
            )))
        }
        PluginDownloadClientCommand::ListRecentCompleted(request) => {
            PluginDownloadClientCommandResult::ListRecentCompleted(list_recent_completed(
                functions, request,
            ))
        }
        PluginDownloadClientCommand::ListRecentCompletedScoped(request) => {
            let request = sdk::PluginDownloadListRecentCompletedRequest {
                limit: request.limit,
            };
            PluginDownloadClientCommandResult::ListRecentCompletedScoped(scoped_list_response(
                list_recent_completed(functions, request),
            ))
        }
        PluginDownloadClientCommand::GetCompleted(PluginDownloadGetCompletedRequest {
            client_item_id,
        }) => {
            let result =
                match call::<_, Vec<sdk::PluginCompletedDownload>>(functions.list_completed, ()) {
                    sdk::PluginResult::Ok(downloads) => sdk::PluginResult::Ok(
                        downloads
                            .into_iter()
                            .find(|download| download.client_item_id == client_item_id),
                    ),
                    sdk::PluginResult::Err(error) => sdk::PluginResult::Err(error),
                };
            PluginDownloadClientCommandResult::GetCompleted(result)
        }
        PluginDownloadClientCommand::Control(request) => {
            PluginDownloadClientCommandResult::Control(call(functions.control, request))
        }
        PluginDownloadClientCommand::MarkImported(request) => {
            PluginDownloadClientCommandResult::MarkImported(call(functions.mark_imported, request))
        }
        PluginDownloadClientCommand::MarkImportedNonDestructive(request) => {
            let result = functions.mark_imported_non_destructive.map_or_else(
                || sdk::PluginResult::Ok(()),
                |mark_imported| call(mark_imported, request),
            );
            PluginDownloadClientCommandResult::MarkImportedNonDestructive(result)
        }
        PluginDownloadClientCommand::Status => {
            PluginDownloadClientCommandResult::Status(call(functions.status, ()))
        }
        PluginDownloadClientCommand::TestConnection => {
            PluginDownloadClientCommandResult::TestConnection(call(functions.test_connection, ()))
        }
    }
}

fn list_recent_completed(
    functions: &LegacyDownloadClientFunctions,
    request: sdk::PluginDownloadListRecentCompletedRequest,
) -> sdk::PluginResult<Vec<sdk::PluginCompletedDownload>> {
    if let Some(list_recent_completed) = functions.list_recent_completed {
        call(list_recent_completed, request)
    } else {
        // Existing first-party DLCs do not export a separate recent endpoint.
        // Their complete list is still downloader-owned, so preserve the
        // legacy adapter's conservative fallback.
        call(functions.list_completed, ())
    }
}

fn scoped_list_response<T>(
    result: sdk::PluginResult<Vec<T>>,
) -> sdk::PluginResult<sdk::PluginDownloadScopedListResponse<T>> {
    match result {
        sdk::PluginResult::Ok(items) => {
            sdk::PluginResult::Ok(sdk::PluginDownloadScopedListResponse {
                items,
                failures: Vec::new(),
            })
        }
        sdk::PluginResult::Err(error) => sdk::PluginResult::Err(error),
    }
}

fn list_queue_with_failed_history(
    functions: &LegacyDownloadClientFunctions,
) -> sdk::PluginResult<Vec<sdk::PluginDownloadItem>> {
    let queue: Vec<sdk::PluginDownloadItem> = match call(functions.list_queue, ()) {
        sdk::PluginResult::Ok(items) => items,
        sdk::PluginResult::Err(error) => return sdk::PluginResult::Err(error),
    };
    let failed_history: Vec<sdk::PluginDownloadItem> = match call(functions.list_history, ()) {
        sdk::PluginResult::Ok(items) => items,
        sdk::PluginResult::Err(_) => return sdk::PluginResult::Ok(queue),
    };

    let mut items = Vec::with_capacity(queue.len() + failed_history.len());
    let mut positions = HashMap::new();
    for item in queue {
        if let Some(position) = positions.get(&item.client_item_id).copied() {
            items[position] = item;
        } else {
            positions.insert(item.client_item_id.clone(), items.len());
            items.push(item);
        }
    }
    for item in failed_history.into_iter().filter(|item| {
        matches!(
            item.state,
            sdk::DownloadItemState::Failed | sdk::DownloadItemState::Error
        )
    }) {
        if let Some(position) = positions.get(&item.client_item_id).copied() {
            items[position] = item;
        } else {
            positions.insert(item.client_item_id.clone(), items.len());
            items.push(item);
        }
    }

    sdk::PluginResult::Ok(items)
}

fn call<Request, Response>(
    function: fn(String) -> FnResult<String>,
    request: Request,
) -> sdk::PluginResult<Response>
where
    Request: serde::Serialize,
    Response: serde::de::DeserializeOwned,
{
    let request = match serde_json::to_string(&request) {
        Ok(request) => request,
        Err(error) => return bridge_error(format!("failed to encode command request: {error}")),
    };
    let raw = match function(request) {
        Ok(raw) => raw,
        Err(error) => return bridge_error(error.to_string()),
    };
    match serde_json::from_str(&raw) {
        Ok(result) => result,
        Err(error) => bridge_error(format!("plugin returned malformed response: {error}")),
    }
}

fn bridge_error<T>(message: String) -> sdk::PluginResult<T> {
    sdk::PluginResult::Err(sdk::PluginError {
        code: sdk::PluginErrorCode::Temporary,
        public_message: "download client command failed".to_string(),
        debug_message: Some(message),
        retry_after_seconds: None,
        details: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unexpected_legacy_history_call(_input: String) -> FnResult<String> {
        panic!("command ListHistory must not call the legacy list_history export")
    }

    fn completed_downloads(_input: String) -> FnResult<String> {
        Ok(serde_json::to_string(&sdk::PluginResult::Ok(vec![
            sdk::PluginCompletedDownload {
                client_item_id: "completed-item".to_string(),
                download_id: None,
                info_hash: None,
                name: "completed item".to_string(),
                release_name: None,
                dest_dir: "/downloads".to_string(),
                category: None,
                output_kind: None,
                content_paths: Vec::new(),
                size_bytes: None,
                completed_at: None,
                parameters: Vec::new(),
            },
        ]))?)
    }

    fn unused(_input: String) -> FnResult<String> {
        unreachable!("unused bridge function")
    }

    fn download_item(
        client_item_id: &str,
        state: sdk::DownloadItemState,
        message: Option<&str>,
    ) -> sdk::PluginDownloadItem {
        sdk::PluginDownloadItem {
            client_item_id: client_item_id.to_string(),
            download_id: None,
            info_hash: None,
            title: client_item_id.to_string(),
            state,
            message: message.map(str::to_string),
            category: None,
            remote_output_path: None,
            torrent: None,
            total_size_bytes: None,
            remaining_size_bytes: None,
            eta_seconds: None,
            progress_percent: None,
            can_move_files: None,
            can_remove: None,
            removed: None,
            raw_state: None,
            completed_at: None,
        }
    }

    fn queue_items(_input: String) -> FnResult<String> {
        Ok(serde_json::to_string(&sdk::PluginResult::Ok(vec![
            download_item("failed-item", sdk::DownloadItemState::Downloading, None),
            download_item("active-item", sdk::DownloadItemState::Downloading, None),
        ]))?)
    }

    fn history_items(_input: String) -> FnResult<String> {
        Ok(serde_json::to_string(&sdk::PluginResult::Ok(vec![
            download_item(
                "failed-item",
                sdk::DownloadItemState::Failed,
                Some("terminal reason"),
            ),
            download_item(
                "error-item",
                sdk::DownloadItemState::Error,
                Some("error reason"),
            ),
            download_item("completed-item", sdk::DownloadItemState::Completed, None),
        ]))?)
    }

    #[test]
    fn list_queue_merges_failed_history_with_terminal_state_precedence() {
        let functions = LegacyDownloadClientFunctions {
            describe: unused,
            add: unused,
            list_queue: queue_items,
            list_history: history_items,
            list_completed: completed_downloads,
            list_recent_completed: None,
            control: unused,
            mark_imported: unused,
            mark_imported_non_destructive: None,
            status: unused,
            test_connection: unused,
        };

        let result =
            bridge_download_client_command(&functions, PluginDownloadClientCommand::ListQueue);
        let PluginDownloadClientCommandResult::ListQueue(sdk::PluginResult::Ok(items)) = result
        else {
            panic!("expected successful list queue result");
        };

        assert_eq!(items.len(), 3);
        assert!(items.iter().any(|item| {
            item.client_item_id == "failed-item"
                && item.state == sdk::DownloadItemState::Failed
                && item.message.as_deref() == Some("terminal reason")
        }));
        assert!(items.iter().any(|item| {
            item.client_item_id == "error-item" && item.state == sdk::DownloadItemState::Error
        }));
        assert!(items.iter().any(|item| {
            item.client_item_id == "active-item"
                && item.state == sdk::DownloadItemState::Downloading
        }));
        assert!(
            !items
                .iter()
                .any(|item| item.client_item_id == "completed-item")
        );
    }

    #[test]
    fn unsupported_non_destructive_mark_is_a_safe_no_op() {
        let functions = LegacyDownloadClientFunctions {
            describe: unused,
            add: unused,
            list_queue: unused,
            list_history: unused,
            list_completed: completed_downloads,
            list_recent_completed: None,
            control: unused,
            mark_imported: unused,
            mark_imported_non_destructive: None,
            status: unused,
            test_connection: unused,
        };
        let request = sdk::PluginDownloadClientMarkImportedRequest {
            client_item_id: "ABCDEF".to_string(),
            info_hash: Some("ABCDEF".to_string()),
            title_id: None,
            title_name: None,
            category: None,
            post_import_isolation: Vec::new(),
            imported_path: None,
            download_path: None,
        };

        let result = bridge_download_client_command(
            &functions,
            PluginDownloadClientCommand::MarkImportedNonDestructive(request),
        );

        assert!(matches!(
            result,
            PluginDownloadClientCommandResult::MarkImportedNonDestructive(
                sdk::PluginResult::Ok(())
            )
        ));
    }

    #[test]
    fn list_history_uses_completed_download_shape() {
        let functions = LegacyDownloadClientFunctions {
            describe: unused,
            add: unused,
            list_queue: unused,
            list_history: unexpected_legacy_history_call,
            list_completed: completed_downloads,
            list_recent_completed: None,
            control: unused,
            mark_imported: unused,
            mark_imported_non_destructive: None,
            status: unused,
            test_connection: unused,
        };

        let result =
            bridge_download_client_command(&functions, PluginDownloadClientCommand::ListHistory);

        assert!(matches!(
            result,
            PluginDownloadClientCommandResult::ListHistory(sdk::PluginResult::Ok(downloads))
                if downloads.len() == 1
                    && downloads[0].client_item_id == "completed-item"
        ));
    }

    #[test]
    fn exact_lookup_returns_only_the_requested_completed_download() {
        let complete = sdk::PluginCompletedDownload {
            client_item_id: "retained-item".to_string(),
            download_id: None,
            info_hash: None,
            name: "retained item".to_string(),
            release_name: None,
            dest_dir: "/downloads".to_string(),
            category: None,
            output_kind: None,
            content_paths: Vec::new(),
            size_bytes: None,
            completed_at: None,
            parameters: Vec::new(),
        };
        let output = serde_json::to_string(&sdk::PluginResult::Ok(vec![complete])).unwrap();
        let result: sdk::PluginResult<Vec<sdk::PluginCompletedDownload>> =
            serde_json::from_str(&output).unwrap();
        assert!(matches!(result, sdk::PluginResult::Ok(downloads) if downloads.len() == 1));
    }
}
