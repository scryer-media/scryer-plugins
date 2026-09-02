use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, DownloadClientCapabilities, DownloadClientDescriptor,
    DownloadControlAction, DownloadInputKind, DownloadIsolationMode, DownloadItemState,
    DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const WATCH_FOLDER_GRACE_PERIOD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct BlackholeConfig {
    torrent_folder: String,
    watch_folder: String,
    save_magnet_files: bool,
    magnet_file_extension: String,
    read_only: bool,
}

fn plugin_error<T>(code: PluginErrorCode, public_message: impl Into<String>) -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    })
}

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "torrent-blackhole".to_string(),
        name: "Torrent Blackhole".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "torrent-blackhole".to_string(),
            provider_aliases: vec!["blackhole".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
            ],
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
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentFile,
                    ],
                    preferred_sources: vec![
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentFile,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::MagnetUri,
                    ],
                    isolation_modes: vec![DownloadIsolationMode::Directory],
                    supports_seed_ratio_limit: false,
                    supports_seed_time_limit: false,
                    supports_start_paused: false,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: false,
                    supports_post_import_isolation: false,
                    reports_content_paths: true,
                    ..DownloadTorrentCapabilities::default()
                }),
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // SDK 3.10 addition, and `false` is both the SDK's default and the
                // truth: this client's function table passes
                // `mark_imported_non_destructive: None`, so the bridge has nothing to
                // route a non-destructive handoff to.
                mark_imported_non_destructive: false,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    let config = BlackholeConfig::from_config()?;
    fs::create_dir_all(&config.torrent_folder)
        .map_err(|error| Error::msg(format!("failed to create torrent folder: {error}")))?;
    let title = clean_file_name(
        request
            .release
            .release_title
            .as_deref()
            .or(request.source.source_title.as_deref())
            .unwrap_or("download"),
    );
    let path = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let decoded = STANDARD
            .decode(bytes)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?;
        let path = Path::new(&config.torrent_folder).join(format!("{title}.torrent"));
        write_file(&path, &decoded)?;
        path
    } else if let Some(url) = torrent_file_url(&request) {
        let decoded = get_external_bytes(&url)?;
        let path = Path::new(&config.torrent_folder).join(format!("{title}.torrent"));
        write_file(&path, &decoded)?;
        path
    } else if let Some(magnet) = request
        .source
        .magnet_uri
        .as_deref()
        .or(request.source.download_url.as_deref())
        .filter(|value| value.starts_with("magnet:"))
    {
        if !config.save_magnet_files {
            return Ok(serde_json::to_string(&plugin_error::<
                PluginDownloadClientAddResponse,
            >(
                PluginErrorCode::Unsupported,
                "Torrent Blackhole does not support magnet links unless Save Magnet Files is enabled",
            ))?);
        }
        let path = Path::new(&config.torrent_folder).join(format!(
            "{}.{}",
            title,
            config.magnet_file_extension.trim_start_matches('.')
        ));
        write_file(&path, magnet.as_bytes())?;
        path
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "torrent bytes, torrent URL, or magnet link is required",
        ))?);
    };
    let id = path.to_string_lossy().to_string();
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: id,
            info_hash: hash,
        },
    ))?)
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_config()?;
    let items = scan_watch_folder(&config)
        .into_iter()
        .map(|entry| entry_to_item(&config, entry))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = BlackholeConfig::from_config()?;
    let items = scan_watch_folder(&config)
        .into_iter()
        .map(|entry| entry_to_item(&config, entry))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_config()?;
    let downloads = scan_watch_folder(&config)
        .into_iter()
        .filter(WatchFolderEntry::is_completed)
        .map(entry_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = BlackholeConfig::from_config()?;
    match request.action {
        DownloadControlAction::Remove => {
            if !request.remove_data {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Unsupported,
                    "Blackhole cannot remove a download item without deleting data",
                ))?);
            }
            if config.read_only {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Unsupported,
                    "Blackhole is configured read-only",
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
                "Torrent Blackhole does not support active control actions",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&_input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_config()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(true),
            remote_output_roots: if config.watch_folder.is_empty() {
                Vec::new()
            } else {
                vec![config.watch_folder]
            },
            removes_completed_downloads: Some(!config.read_only),
            sorting_mode: Some("watch-folder".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = BlackholeConfig::from_config()?;
    ensure_directory(&config.torrent_folder, "Torrent Folder")?;
    ensure_directory(&config.watch_folder, "Watch Folder")?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl BlackholeConfig {
    fn from_config() -> Result<Self, Error> {
        Ok(Self {
            torrent_folder: config_value("torrent_folder").unwrap_or_default(),
            watch_folder: config_value("watch_folder").unwrap_or_default(),
            save_magnet_files: config_bool("save_magnet_files", false),
            magnet_file_extension: config_value("magnet_file_extension")
                .unwrap_or_else(|| ".magnet".to_string()),
            read_only: config_bool("read_only", true),
        })
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "torrent_folder",
            "Torrent Folder",
            ConfigFieldType::Path,
            true,
            None,
            None,
        ),
        field(
            "watch_folder",
            "Watch Folder",
            ConfigFieldType::Path,
            true,
            None,
            None,
        ),
        field(
            "save_magnet_files",
            "Save Magnet Files",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "magnet_file_extension",
            "Magnet File Extension",
            ConfigFieldType::String,
            true,
            Some(".magnet"),
            None,
        ),
        field(
            "read_only",
            "Read Only",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            None,
        ),
    ]
}

fn write_file(path: &Path, content: &[u8]) -> Result<(), Error> {
    let mut file = fs::File::create(path)
        .map_err(|error| Error::msg(format!("failed to open file: {error}")))?;
    file.write_all(content)
        .map_err(|error| Error::msg(format!("failed to write file: {error}")))
}

fn torrent_file_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .source
        .torrent_url
        .clone()
        .or_else(|| request.source.download_url.clone())
        .filter(|value| !value.trim_start().starts_with("magnet:"))
}

fn get_external_bytes(url: &str) -> Result<Vec<u8>, Error> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-torrent-blackhole-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("torrent URL request failed: {error}")))?;
    let status = response.status_code();
    let body = response.body();
    if status >= 400 {
        return Err(Error::msg(format!(
            "torrent URL returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    if body.is_empty() {
        return Err(Error::msg("torrent URL returned an empty response body"));
    }
    Ok(body)
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

fn entry_to_item(config: &BlackholeConfig, entry: WatchFolderEntry) -> PluginDownloadItem {
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
        download_id: None,
        info_hash: None,
        title,
        state,
        message: None,
        category: Some("scryer".to_string()),
        remote_output_path: Some(id.clone()),
        torrent: Some(PluginTorrentItem {
            content_paths: vec![id.clone()],
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(size),
        remaining_size_bytes: remaining_grace_seconds.map(|_| size).or(Some(0)),
        eta_seconds: remaining_grace_seconds.or(Some(0)),
        progress_percent: if completed { Some(100) } else { None },
        // The watch folder is served by an external client this plugin cannot see, so it can
        // never prove seeding has finished. `Remove` here deletes the payload from disk, which
        // would pull data out from under a still-seeding client — hence `None` (unknowable)
        // rather than a guessed `true`. `can_move_files` additionally waits for the entry to
        // settle so a still-being-written download is never moved.
        can_move_files: Some(completed && !config.read_only),
        can_remove: None,
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
        download_id: None,
        info_hash: None,
        name,
        dest_dir: id.clone(),
        category: Some("scryer".to_string()),
        output_kind: Some(if path.is_file() {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: vec![id],
        size_bytes: Some(path_size(&path)),
        completed_at: None,
        parameters: Vec::new(),
        release_name: None,
    }
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

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn config_value(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_bool(key: &str, default: bool) -> bool {
    config_value(key)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(read_only: bool) -> BlackholeConfig {
        BlackholeConfig {
            torrent_folder: "/blackhole/torrents".to_string(),
            watch_folder: "/blackhole/done".to_string(),
            save_magnet_files: false,
            magnet_file_extension: ".magnet".to_string(),
            read_only,
        }
    }

    fn entry(remaining_grace_seconds: Option<i64>) -> WatchFolderEntry {
        WatchFolderEntry {
            path: PathBuf::from("/blackhole/done/Movie"),
            remaining_grace_seconds,
        }
    }

    #[test]
    fn can_remove_is_always_unknown_because_seeding_happens_outside_the_plugin() {
        assert_eq!(entry_to_item(&config(false), entry(None)).can_remove, None);
        assert_eq!(entry_to_item(&config(true), entry(None)).can_remove, None);
        assert_eq!(
            entry_to_item(&config(false), entry(Some(12))).can_remove,
            None
        );
    }

    #[test]
    fn can_move_files_waits_for_the_entry_to_settle() {
        assert_eq!(
            entry_to_item(&config(false), entry(Some(12))).can_move_files,
            Some(false)
        );
        assert_eq!(
            entry_to_item(&config(false), entry(None)).can_move_files,
            Some(true)
        );
    }

    #[test]
    fn read_only_blackholes_never_offer_to_move_files() {
        assert_eq!(
            entry_to_item(&config(true), entry(None)).can_move_files,
            Some(false)
        );
    }
}

// ---------------------------------------------------------------------------
// `scryer:download-client/download-client@1.0.0`
// ---------------------------------------------------------------------------
//
// Transport only. Every operation above is untouched — the same URLs, headers,
// status rules and plugin state. What changed is how the host reaches them: a
// `process` export carrying the very command envelope the Preview 1 runner
// already moved over stdin/stdout, instead of a `main` reading stdin.
//
// The function table is the single source of truth for both exports, so
// `describe` and `process` cannot drift apart, and the operation semantics —
// merged failed history, scoped listings, non-destructive mark-imported — stay
// in the PDK bridge where every client shares them.

wit_bindgen::generate!({
    // Fully qualified: `path` resolves two packages, so a bare world name is
    // ambiguous even though only one of them declares a world.
    world: "scryer:download-client/download-client@1.0.0",
    // The shared `scryer:host` package is listed first so the family package's
    // `import scryer:host/services@1.0.0` resolves against it.
    path: ["wit/host-v1.0.0", "wit/download-client-v1.0.0"],
    // The host package is its own WIT package, so wit-bindgen asks explicitly
    // whether to generate for it. Yes: the PDK holds only a `fn` pointer and
    // the entry macro binds it to this module's
    // `scryer::host::services::host-call`.
    generate_all,
});

fn functions() -> LegacyDownloadClientFunctions {
    LegacyDownloadClientFunctions {
        describe: scryer_describe,
        add: scryer_download_add,
        list_queue: scryer_download_list_queue,
        list_history: scryer_download_list_history,
        list_completed: scryer_download_list_completed,
        list_recent_completed: None,
        control: scryer_download_control,
        mark_imported: scryer_download_mark_imported,
        mark_imported_non_destructive: None,
        status: scryer_download_status,
        test_connection: scryer_download_test_connection,
    }
}

fn build_descriptor() -> PluginDescriptor {
    legacy_download_client_descriptor(&functions())
}

fn handle_download_client_command(
    command: PluginDownloadClientCommand,
) -> PluginDownloadClientCommandResult {
    bridge_download_client_command(&functions(), command)
}

scryer_plugin_pdk::scryer_download_client_component_main!(
    descriptor = build_descriptor,
    handler = handle_download_client_command,
);
