use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, DownloadClientCapabilities, DownloadClientDescriptor,
    DownloadControlAction, DownloadInputKind, DownloadIsolationMode, DownloadItemState,
    PluginCompletedDownload, PluginDescriptor, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;

const WATCH_FOLDER_GRACE_PERIOD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct BlackholeConfig {
    nzb_folder: String,
    watch_folder: String,
}

#[derive(Default, Deserialize)]
struct AddRequest {
    #[serde(default)]
    source: AddSource,
    #[serde(default)]
    release: AddRelease,
}

#[derive(Default, Deserialize)]
struct AddSource {
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    nzb_bytes_base64: Option<String>,
    #[serde(default)]
    nzb_file_name: Option<String>,
    #[serde(default)]
    source_title: Option<String>,
}

#[derive(Default, Deserialize)]
struct AddRelease {
    #[serde(default)]
    release_title: Option<String>,
}

fn plugin_error<T>(code: PluginErrorCode, public_message: impl Into<String>) -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
    })
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "usenet-blackhole".to_string(),
        name: "Usenet Blackhole".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "usenet-blackhole".to_string(),
            provider_aliases: vec!["blackhole-usenet".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![DownloadInputKind::Nzb, DownloadInputKind::NzbUrl],
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: true,
                mark_imported: false,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: false,
                start_paused: false,
                force_start: false,
                per_download_directory: false,
                host_fs_required: true,
                test_connection: true,
                torrent: None,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: AddRequest = serde_json::from_str(&input)?;
    let config = BlackholeConfig::from_extism()?;
    fs::create_dir_all(&config.nzb_folder)
        .map_err(|error| Error::msg(format!("failed to create NZB folder: {error}")))?;
    let title = clean_file_name(
        request
            .source
            .nzb_file_name
            .as_deref()
            .map(|value| value.trim_end_matches(".nzb"))
            .or(request.release.release_title.as_deref())
            .or(request.source.source_title.as_deref())
            .unwrap_or("download"),
    );
    let bytes = nzb_payload(&request)?;
    let path = Path::new(&config.nzb_folder).join(format!("{title}.nzb"));
    write_file(&path, &bytes)?;
    let id = path.to_string_lossy().to_string();
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: id,
            info_hash: None,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_extism()?;
    let items = scan_watch_folder(&config)
        .into_iter()
        .map(|entry| entry_to_item(&config, entry))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = BlackholeConfig::from_extism()?;
    let items = scan_watch_folder(&config)
        .into_iter()
        .map(|entry| entry_to_item(&config, entry))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_extism()?;
    let downloads = scan_watch_folder(&config)
        .into_iter()
        .filter(WatchFolderEntry::is_completed)
        .map(entry_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    match request.action {
        DownloadControlAction::Remove => {
            if !request.remove_data {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Unsupported,
                    "Blackhole cannot remove a download item without deleting data",
                ))?);
            }
            let path = Path::new(&request.client_item_id);
            if path.is_dir() {
                fs::remove_dir_all(path)
                    .map_err(|error| Error::msg(format!("failed to remove directory: {error}")))?;
            } else if path.exists() {
                fs::remove_file(path)
                    .map_err(|error| Error::msg(format!("failed to remove file: {error}")))?;
            }
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Usenet Blackhole does not support active control actions",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&_input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_extism()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(true),
            remote_output_roots: if config.watch_folder.is_empty() {
                Vec::new()
            } else {
                vec![config.watch_folder]
            },
            removes_completed_downloads: Some(true),
            sorting_mode: Some("watch-folder".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_extism()?;
    ensure_directory(&config.nzb_folder, "NZB Folder")?;
    ensure_directory(&config.watch_folder, "Watch Folder")?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl BlackholeConfig {
    fn from_extism() -> Result<Self, Error> {
        Ok(Self {
            nzb_folder: config_value("nzb_folder").unwrap_or_default(),
            watch_folder: config_value("watch_folder").unwrap_or_default(),
        })
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "nzb_folder",
            "NZB Folder",
            ConfigFieldType::String,
            true,
            None,
            Some("Folder where .nzb files are written."),
        ),
        field(
            "watch_folder",
            "Watch Folder",
            ConfigFieldType::String,
            true,
            None,
            Some("Folder scanned for completed downloads."),
        ),
    ]
}

fn nzb_payload(request: &AddRequest) -> Result<Vec<u8>, Error> {
    if let Some(raw) = request
        .source
        .nzb_bytes_base64
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return STANDARD
            .decode(raw)
            .map_err(|error| Error::msg(format!("invalid nzb_bytes_base64: {error}")));
    }

    let Some(url) = request
        .source
        .download_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Err(Error::msg(
            "Usenet Blackhole requires an NZB payload or NZB URL",
        ));
    };
    get_external_bytes(url)
}

fn get_external_bytes(url: &str) -> Result<Vec<u8>, Error> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-usenet-blackhole-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("NZB URL request failed: {error}")))?;
    let status = response.status_code();
    let body = response.body();
    if status >= 400 {
        return Err(Error::msg(format!(
            "NZB URL returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    if body.is_empty() {
        return Err(Error::msg("NZB URL returned an empty response body"));
    }
    Ok(body)
}

fn write_file(path: &Path, content: &[u8]) -> Result<(), Error> {
    let mut file = fs::File::create(path)
        .map_err(|error| Error::msg(format!("failed to open file: {error}")))?;
    file.write_all(content)
        .map_err(|error| Error::msg(format!("failed to write file: {error}")))
}

#[derive(Clone)]
struct WatchFolderEntry {
    path: PathBuf,
    remaining_grace_seconds: Option<i64>,
}

impl WatchFolderEntry {
    fn is_completed(&self) -> bool {
        self.remaining_grace_seconds.is_none()
    }
}

fn scan_watch_folder(config: &BlackholeConfig) -> Vec<WatchFolderEntry> {
    let Ok(entries) = fs::read_dir(&config.watch_folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_watch_folder_download_path(path))
        .map(|path| WatchFolderEntry {
            remaining_grace_seconds: remaining_grace_seconds(&path),
            path,
        })
        .collect()
}

fn is_watch_folder_download_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if is_special_folder_name(name) {
        return false;
    }

    if path.is_dir() {
        return true;
    }

    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(is_video_extension)
}

fn is_special_folder_name(name: &str) -> bool {
    matches!(name, ".AppleDouble" | "@eaDir" | "lost+found")
        || name.starts_with('.')
        || name.ends_with(".partial")
}

fn is_video_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "avi"
            | "divx"
            | "m2ts"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "ts"
            | "vob"
            | "webm"
            | "wmv"
    )
}

fn entry_to_item(_config: &BlackholeConfig, entry: WatchFolderEntry) -> PluginDownloadItem {
    let completed = entry.is_completed();
    let remaining_grace_seconds = entry.remaining_grace_seconds;
    let path = entry.path;
    let id = path.to_string_lossy().to_string();
    let title = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let size = path_size(&path);
    let state = if completed {
        DownloadItemState::Completed
    } else {
        DownloadItemState::Downloading
    };
    PluginDownloadItem {
        client_item_id: id.clone(),
        info_hash: None,
        title,
        state,
        message: None,
        category: Some("sonarr".to_string()),
        remote_output_path: Some(id.clone()),
        torrent: None,
        total_size_bytes: Some(size),
        remaining_size_bytes: remaining_grace_seconds.map(|_| size).or(Some(0)),
        eta_seconds: remaining_grace_seconds.or(Some(0)),
        progress_percent: if completed { Some(100) } else { None },
        can_move_files: Some(true),
        can_remove: Some(true),
        removed: Some(false),
        raw_state: Some(if completed {
            "completed".to_string()
        } else {
            "waiting-for-stable-watch-folder-entry".to_string()
        }),
        completed_at: None,
    }
}

fn entry_to_completed(entry: WatchFolderEntry) -> PluginCompletedDownload {
    let path = entry.path;
    let id = path.to_string_lossy().to_string();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    PluginCompletedDownload {
        client_item_id: id.clone(),
        info_hash: None,
        name,
        dest_dir: id.clone(),
        category: Some("sonarr".to_string()),
        output_kind: Some(if path.is_file() {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: vec![id],
        size_bytes: Some(path_size(&path)),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn path_size(path: &Path) -> i64 {
    if path.is_file() {
        return path
            .metadata()
            .map(|meta| meta.len() as i64)
            .unwrap_or_default();
    }

    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| path_size(&entry.path()))
        .sum()
}

fn remaining_grace_seconds(path: &Path) -> Option<i64> {
    let modified = latest_modified(path)?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    let remaining = WATCH_FOLDER_GRACE_PERIOD.saturating_sub(age);
    (remaining > Duration::ZERO).then(|| remaining.as_secs().max(1) as i64)
}

fn latest_modified(path: &Path) -> Option<SystemTime> {
    let metadata = fs::metadata(path).ok()?;
    let mut latest = metadata.modified().ok();
    if metadata.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            if let Some(child_modified) = latest_modified(&entry.path()) {
                latest = match latest {
                    Some(current) if current >= child_modified => Some(current),
                    _ => Some(child_modified),
                };
            }
        }
    }
    latest
}

fn ensure_directory(path: &str, label: &str) -> Result<(), Error> {
    if path.trim().is_empty() {
        return Err(Error::msg(format!("{label} is required")));
    }
    let metadata = fs::metadata(path)
        .map_err(|error| Error::msg(format!("{label} is not accessible: {error}")))?;
    if !metadata.is_dir() {
        return Err(Error::msg(format!("{label} is not a directory")));
    }
    Ok(())
}

fn clean_file_name(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn field(
    key: &str,
    label: &str,
    field_type: ConfigFieldType,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type,
        required,
        default_value: default_value.map(str::to_string),
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![],
        help_text: help_text.map(str::to_string),
    }
}
