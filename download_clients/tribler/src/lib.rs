use extism_pdk::*;
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
    })
}

#[plugin_fn]
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
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
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
            "Tribler only supports magnet links in Sonarr",
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

#[plugin_fn]
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

#[plugin_fn]
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

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = TriblerConfig::from_extism()?;
    let settings = get_settings(&config)?;
    let downloads = get_downloads(&config)?
        .into_iter()
        .filter(is_visible_download)
        .filter(|download| {
            matches!(download.status.as_deref(), Some("SEEDING" | "STOPPED"))
                && has_reached_seed_limit(download, &settings.lib_torrent.download_defaults)
        })
        .map(|download| torrent_to_completed(&config, download))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
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
                "Tribler control action is not implemented by Sonarr's Tribler download client",
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
                "Sonarr supports Tribler 8.0.7 and displays a provider warning for this client"
                    .to_string(),
            ],
        },
    ))?)
}

#[plugin_fn]
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
            ConfigFieldType::String,
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
    let can_remove = has_reached_seed_limit(&download, &settings.lib_torrent.download_defaults);
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
        can_move_files: Some(can_remove),
        can_remove: Some(can_remove),
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

fn has_reached_seed_limit(download: &TriblerDownload, defaults: &DownloadDefaults) -> bool {
    if download.status.as_deref() != Some("STOPPED") {
        return false;
    }
    match defaults.seeding_mode.as_deref() {
        Some("ratio") => download
            .all_time_ratio
            .zip(defaults.seeding_ratio)
            .is_some_and(|(actual, target)| actual >= target),
        Some("time") => download
            .time_added
            .zip(defaults.seeding_time)
            .is_some_and(|(started, seconds)| started + (seconds as i64) < current_unix_seconds()),
        Some("never") => true,
        _ => false,
    }
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
