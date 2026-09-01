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
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
struct TriblerConfig {
    api_root: String,
    api_key: String,
    category: String,
    directory: String,
    anonymity_level: i64,
    safe_seeding: bool,
}

#[derive(Default, Deserialize)]
struct DownloadsResponse {
    #[serde(default)]
    downloads: Vec<TriblerDownload>,
}

#[derive(Default, Deserialize, Clone)]
struct TriblerDownload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    progress: Option<f64>,
    #[serde(default)]
    infohash: String,
    #[serde(default)]
    eta: Option<f64>,
    #[serde(default, rename = "all_time_upload")]
    all_time_upload: Option<i64>,
    #[serde(default, rename = "all_time_download")]
    all_time_download: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "all_time_ratio")]
    all_time_ratio: Option<f64>,
    #[serde(default, rename = "time_added")]
    time_added: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "total_down")]
    total_down: Option<i64>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    destination: String,
    #[serde(default, rename = "speed_down")]
    speed_down: Option<f64>,
    #[serde(default, rename = "speed_up")]
    speed_up: Option<f64>,
}

#[derive(Default, Deserialize)]
struct FilesResponse {
    #[serde(default)]
    files: Vec<TriblerFile>,
}

#[derive(Default, Deserialize, Clone)]
struct TriblerFile {
    #[serde(default)]
    name: String,
}

#[derive(Default, Deserialize)]
struct AddDownloadResponse {
    #[serde(default)]
    infohash: String,
}

#[derive(Default, Deserialize)]
struct TriblerSettingsResponse {
    #[serde(default)]
    settings: TriblerSettings,
}

#[derive(Default, Deserialize)]
struct TriblerSettings {
    #[serde(default, rename = "libtorrent")]
    lib_torrent: LibTorrent,
}

#[derive(Default, Deserialize)]
struct LibTorrent {
    #[serde(default, rename = "download_defaults")]
    download_defaults: DownloadDefaults,
}

#[derive(Default, Deserialize, Clone)]
struct DownloadDefaults {
    #[serde(default, rename = "saveas")]
    save_as: String,
    #[serde(default, rename = "seeding_mode")]
    seeding_mode: Option<String>,
    #[serde(default, rename = "seeding_ratio")]
    seeding_ratio: Option<f64>,
    #[serde(default, rename = "seeding_time")]
    seeding_time: Option<f64>,
}

#[derive(Serialize)]
struct AddDownloadRequest {
    destination: Option<String>,
    uri: String,
    #[serde(rename = "safe_seeding")]
    safe_seeding: bool,
    #[serde(rename = "anon_hops")]
    anonymity_hops: i64,
}

#[derive(Serialize)]
struct RemoveDownloadRequest {
    #[serde(rename = "remove_data")]
    remove_data: bool,
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
        id: "tribler".to_string(),
        name: "Tribler".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "tribler".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![DownloadInputKind::MagnetUri],
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
                per_download_directory: true,
                host_fs_required: false,
                test_connection: true,
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![DownloadInputKind::MagnetUri],
                    preferred_sources: vec![DownloadInputKind::MagnetUri],
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
                    supports_anonymity_hops: true,
                    supports_safe_seeding: true,
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
    let config = TriblerConfig::from_extism()?;
    let Some(uri) = request
        .source
        .magnet_uri
        .clone()
        .or(request.source.download_url.clone())
    else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "Tribler only supports magnet links in Scryer",
        ))?);
    };
    let destination = get_download_directory(&config, &request)?;
    let response: AddDownloadResponse = request_json(
        &config,
        "PUT",
        "/downloads",
        Some(serde_json::to_value(AddDownloadRequest {
            destination,
            uri,
            safe_seeding: request
                .torrent
                .as_ref()
                .and_then(|torrent| torrent.safe_seeding)
                .unwrap_or(config.safe_seeding),
            anonymity_hops: request
                .torrent
                .as_ref()
                .and_then(|torrent| torrent.anonymity_hops)
                .map(i64::from)
                .unwrap_or(config.anonymity_level),
        })?),
    )?;
    let hash = normalize_hash(&response.infohash);
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: hash.clone(),
            info_hash: Some(hash),
        },
    ))?)
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = TriblerConfig::from_extism()?;
    let settings = get_settings(&config)?;
    let items = get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(|download| torrent_to_item(&config, &settings, download))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = TriblerConfig::from_extism()?;
    let settings = get_settings(&config)?;
    let items = get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(|download| torrent_to_item(&config, &settings, download))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = TriblerConfig::from_extism()?;
    let downloads = get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        // Completed downloads are those whose data is fully present; waiting for the seeding
        // goal here would keep finished payloads out of import indefinitely.
        .filter(|download| {
            matches!(download.status.as_deref(), Some("SEEDING" | "STOPPED"))
                && is_data_complete(download)
        })
        .map(|download| torrent_to_completed(&config, download))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = TriblerConfig::from_extism()?;
    match request.action {
        DownloadControlAction::Remove => {
            let _: serde_json::Value = request_json(
                &config,
                "DELETE",
                &format!("/downloads/{}", normalize_hash(&request.client_item_id)),
                Some(serde_json::to_value(RemoveDownloadRequest {
                    remove_data: request.remove_data,
                })?),
            )?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Tribler control action is not implemented by Scryer's Tribler download client",
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
    let config = TriblerConfig::from_extism()?;
    let settings = get_settings(&config)?;
    let mut root = settings
        .lib_torrent
        .download_defaults
        .save_as
        .trim_end_matches('/')
        .to_string();
    if !config.category.is_empty() {
        root = format!("{}/.{}", root, config.category);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(is_localhost_url(&config.api_root)),
            remote_output_roots: if root.is_empty() {
                Vec::new()
            } else {
                vec![root]
            },
            removes_completed_downloads: Some(false),
            sorting_mode: Some("tribler-api".to_string()),
            warnings: vec![
                "Scryer supports Tribler 8.0.7 and displays a provider warning for this client"
                    .to_string(),
            ],
        },
    ))?)
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = TriblerConfig::from_extism()?;
    let _ = get_settings(&config)?;
    let _ = get_downloads(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl TriblerConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "20100".to_string());
        let url_base = config_value("url_base").unwrap_or_default();
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        let base = if url_base.trim().is_empty() {
            format!("{scheme}://{host}:{port}")
        } else {
            format!("{scheme}://{host}:{port}/{}", url_base.trim_matches('/'))
        };
        Ok(Self {
            api_root: format!("{}/api", base.trim_end_matches('/')),
            api_key: config_value("api_key").unwrap_or_default(),
            category: config_value("category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
            anonymity_level: config_value("anonymity_level")
                .and_then(|value| value.parse().ok())
                .unwrap_or(1),
            safe_seeding: config_bool("safe_seeding", true),
        })
    }
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        field(
            "host",
            "Host",
            ConfigFieldType::String,
            true,
            Some("localhost"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("20100"),
            None,
        ),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "url_base",
            "URL Base",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        field(
            "anonymity_level",
            "Anonymity Level",
            ConfigFieldType::Number,
            false,
            Some("1"),
            None,
        ),
        field(
            "safe_seeding",
            "Safe Seeding",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            None,
        ),
    ]
}

fn get_settings(config: &TriblerConfig) -> Result<TriblerSettings, Error> {
    let response: TriblerSettingsResponse = request_json(config, "GET", "/settings", None)?;
    Ok(response.settings)
}

fn get_downloads(config: &TriblerConfig) -> Result<Vec<TriblerDownload>, Error> {
    let response: DownloadsResponse = request_json(config, "GET", "/downloads", None)?;
    Ok(response.downloads)
}

fn get_files(config: &TriblerConfig, hash: &str) -> Result<Vec<TriblerFile>, Error> {
    let response: FilesResponse =
        request_json(config, "GET", &format!("/downloads/{hash}/files"), None)?;
    Ok(response.files)
}

fn request_json<T: DeserializeOwned>(
    config: &TriblerConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T, Error> {
    let request = HttpRequest::new(format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    ))
    .with_method(method)
    .with_header("Content-Type", "application/json")
    .with_header("X-Api-Key", &config.api_key)
    .with_header("User-Agent", "scryer-tribler-plugin/0.1");
    let response = http::request::<Vec<u8>>(
        &request,
        body.map(|body| serde_json::to_vec(&body).unwrap_or_default()),
    )
    .map_err(|error| Error::msg(format!("Tribler request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        return Err(Error::msg("Tribler API key was rejected"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "Tribler returned HTTP {status}: {text}"
        )));
    }
    serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("Tribler response parse failed: {error}")))
}

fn get_download_directory(
    config: &TriblerConfig,
    request: &PluginDownloadClientAddRequest,
) -> Result<Option<String>, Error> {
    if let Some(directory) = request
        .routing
        .download_directory
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(directory.to_string()));
    }
    if !config.directory.is_empty() {
        return Ok(Some(config.directory.clone()));
    }
    if config.category.is_empty() {
        return Ok(None);
    }
    let settings = get_settings(config)?;
    Ok(Some(format!(
        "{}/{}",
        settings
            .lib_torrent
            .download_defaults
            .save_as
            .trim_end_matches('/'),
        config.category
    )))
}

fn torrent_to_item(
    config: &TriblerConfig,
    settings: &TriblerSettings,
    download: TriblerDownload,
) -> Result<PluginDownloadItem, Error> {
    let files = get_files(config, &download.infohash)?;
    let output_path = output_path(&download, &files);
    let size = download.size.unwrap_or_default();
    let progress = download.progress.unwrap_or_default().clamp(0.0, 1.0);
    let remaining = ((size as f64) * (1.0 - progress)).round().max(0.0) as i64;
    let state = map_state(&download);
    let hash = normalize_hash(&download.infohash);
    let can_remove = derive_can_remove(
        &download,
        &settings.lib_torrent.download_defaults,
        state,
        current_unix_seconds(),
    );
    let can_move_files = Some(is_data_complete(&download));
    Ok(PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: download.name.clone(),
        state,
        message: download.error.clone(),
        category: None,
        remote_output_path: non_empty(output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            save_path: non_empty(download.destination.clone()),
            content_paths: non_empty(output_path.clone()).into_iter().collect(),
            uploaded_bytes: download.all_time_upload,
            downloaded_bytes: download.all_time_download.or(download.total_down),
            upload_rate_bytes_per_second: download.speed_up.map(|value| value as i64),
            download_rate_bytes_per_second: download.speed_down.map(|value| value as i64),
            seed_ratio: download.all_time_ratio,
            is_encrypted: Some(false),
            raw_status: download.status.clone(),
            status_reason: download.error.clone(),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: download
            .eta
            .map(|value| value.clamp(0.0, 31_536_000.0) as i64),
        progress_percent: Some((progress * 100.0).round().clamp(0.0, 100.0) as u8),
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files,
        can_remove,
        removed: Some(false),
        raw_state: download.status,
        completed_at: None,
    })
}

fn torrent_to_completed(
    config: &TriblerConfig,
    download: TriblerDownload,
) -> Result<PluginCompletedDownload, Error> {
    let files = get_files(config, &download.infohash)?;
    let output_path = output_path(&download, &files);
    let hash = normalize_hash(&download.infohash);
    Ok(PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: download.name,
        dest_dir: output_path.clone(),
        category: None,
        output_kind: Some(if files.len() == 1 || path_looks_like_file(&output_path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(output_path).into_iter().collect(),
        size_bytes: download.size,
        completed_at: None,
        parameters: Vec::new(),
        release_name: None,
    })
}

fn output_path(download: &TriblerDownload, files: &[TriblerFile]) -> String {
    if files.len() == 1 {
        join_path(&download.destination, &files[0].name)
    } else {
        join_path(&download.destination, &download.name)
    }
}

fn is_visible_download(download: &TriblerDownload) -> bool {
    download.size.unwrap_or_default() > 0
}

fn map_state(download: &TriblerDownload) -> DownloadItemState {
    if download
        .error
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return DownloadItemState::Warning;
    }
    match download.status.as_deref() {
        Some("HASHCHECKING" | "WAITING4HASHCHECK" | "CIRCUITS" | "EXIT_NODES" | "DOWNLOADING") => {
            DownloadItemState::Downloading
        }
        Some("METADATA" | "ALLOCATING_DISKSPACE") => DownloadItemState::Queued,
        Some("SEEDING") => DownloadItemState::Completed,
        Some("STOPPED") if download.progress.unwrap_or_default() < 1.0 => DownloadItemState::Paused,
        Some("STOPPED") => DownloadItemState::Completed,
        Some("STOPPED_ON_ERROR") => DownloadItemState::Failed,
        _ => DownloadItemState::Downloading,
    }
}

/// Honest `can_remove` for Tribler.
///
/// Tribler has no per-download seeding limit; the only goal it enforces is the global
/// `seeding_mode` in its libtorrent download defaults. When that mode is missing or
/// unrecognised there is nothing to compare against and the plugin reports `None` so that
/// Scryer-side goal evaluation decides.
fn derive_can_remove(
    download: &TriblerDownload,
    defaults: &DownloadDefaults,
    state: DownloadItemState,
    now: i64,
) -> Option<bool> {
    if state != DownloadItemState::Completed {
        return Some(false);
    }
    // Tribler stops a download once the configured seeding goal is reached.
    let is_stopped = download.status.as_deref() == Some("STOPPED");
    match defaults.seeding_mode.as_deref() {
        // No seeding obligation at all.
        Some("never") => Some(true),
        // Seed indefinitely: the goal can never be reached.
        Some("forever") => Some(false),
        Some("ratio") => match download.all_time_ratio.zip(defaults.seeding_ratio) {
            Some((actual, target)) if actual >= target => is_stopped.then_some(true),
            Some(_) => Some(false),
            None => None,
        },
        Some("time") => match download.time_added.zip(defaults.seeding_time) {
            Some((started, seconds)) if started + (seconds as i64) < now => {
                is_stopped.then_some(true)
            }
            Some(_) => Some(false),
            None => None,
        },
        _ => None,
    }
}

/// Whether the payload is fully downloaded and therefore movable.
fn is_data_complete(download: &TriblerDownload) -> bool {
    download.status.as_deref() == Some("SEEDING") || download.progress.unwrap_or_default() >= 1.0
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), name)
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

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn path_looks_like_file(path: &str) -> bool {
    let Some(last) = path.trim_end_matches('/').rsplit('/').next() else {
        return false;
    };
    last.contains('.')
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

fn is_localhost_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn download(status: &str, progress: f64, ratio: f64) -> TriblerDownload {
        TriblerDownload {
            name: "Movie".to_string(),
            progress: Some(progress),
            infohash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            status: Some(status.to_string()),
            all_time_ratio: Some(ratio),
            time_added: Some(NOW - 3_600),
            size: Some(1_000),
            destination: "/downloads".to_string(),
            ..TriblerDownload::default()
        }
    }

    fn defaults(mode: Option<&str>, ratio: Option<f64>, time: Option<f64>) -> DownloadDefaults {
        DownloadDefaults {
            save_as: "/downloads".to_string(),
            seeding_mode: mode.map(str::to_string),
            seeding_ratio: ratio,
            seeding_time: time,
        }
    }

    #[test]
    fn can_remove_is_false_while_downloading() {
        assert_eq!(
            derive_can_remove(
                &download("DOWNLOADING", 0.4, 0.0),
                &defaults(Some("ratio"), Some(1.0), None),
                DownloadItemState::Downloading,
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_ratio() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 0.4),
                &defaults(Some("ratio"), Some(2.0), None),
                DownloadItemState::Completed,
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_tribler_stopped_a_download_at_its_ratio() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 2.5),
                &defaults(Some("ratio"), Some(2.0), None),
                DownloadItemState::Completed,
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_true_when_tribler_never_seeds() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 0.0),
                &defaults(Some("never"), None, None),
                DownloadItemState::Completed,
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_false_when_tribler_seeds_forever() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(Some("forever"), None, None),
                DownloadItemState::Completed,
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_unknown_without_a_recognised_seeding_mode() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(None, None, None),
                DownloadItemState::Completed,
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_the_ratio_goal_lacks_a_target() {
        assert_eq!(
            derive_can_remove(
                &download("STOPPED", 1.0, 9.0),
                &defaults(Some("ratio"), None, None),
                DownloadItemState::Completed,
                NOW
            ),
            None
        );
    }

    #[test]
    fn met_goal_that_tribler_has_not_stopped_yet_is_unknown() {
        assert_eq!(
            derive_can_remove(
                &download("SEEDING", 1.0, 9.0),
                &defaults(Some("ratio"), Some(2.0), None),
                DownloadItemState::Completed,
                NOW
            ),
            None
        );
    }

    #[test]
    fn data_completeness_covers_seeding_and_finished_downloads() {
        assert!(is_data_complete(&download("SEEDING", 0.999, 0.0)));
        assert!(is_data_complete(&download("STOPPED", 1.0, 0.0)));
        assert!(!is_data_complete(&download("DOWNLOADING", 0.5, 0.0)));
    }
}

#[cfg(test)]
mod extism_host_stubs {
    #[unsafe(no_mangle)]
    pub extern "C" fn alloc(_len: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn config_get(_ptr: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_headers() -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_request(_request: u64, _body: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn http_status_code() -> u64 {
        200
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn length(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn length_unsafe(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn load_u64(_offset: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn load_u8(_offset: u64) -> u8 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u64(_offset: u64, _value: u64) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn store_u8(_offset: u64, _value: u8) {}

    #[unsafe(no_mangle)]
    pub extern "C" fn var_get(_ptr: u64) -> u64 {
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn var_set(_ptr: u64, _value: u64) {}
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
