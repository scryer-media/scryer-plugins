use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientStatus, PluginDownloadItem,
    PluginDownloadOutputKind, PluginError, PluginErrorCode, PluginResult, PluginTorrentItem,
    ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const FINISHED_AT_VAR_PREFIX: &str = "rqbit.finished_at.";
const SEED_CONFIG_VAR_PREFIX: &str = "rqbit.seed_config.";

#[derive(Debug, Clone)]
struct RqbitConfig {
    base_url: String,
}

#[derive(Default, Deserialize)]
struct RootResponse {
    #[serde(default)]
    version: String,
}

#[derive(Default, Deserialize)]
struct PostTorrentResponse {
    #[serde(default)]
    details: Option<PostTorrentDetails>,
}

#[derive(Default, Deserialize)]
struct PostTorrentDetails {
    #[serde(default, rename = "info_hash")]
    info_hash: String,
}

#[derive(Default, Deserialize)]
struct ListTorrentsResponse {
    #[serde(default)]
    torrents: Vec<TorrentWithStats>,
}

#[derive(Default, Deserialize)]
struct TorrentWithStats {
    id: i64,
    #[serde(default, rename = "info_hash")]
    info_hash: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "output_folder")]
    output_folder: String,
    #[serde(default)]
    stats: TorrentStats,
}

#[derive(Default, Deserialize)]
struct TorrentStats {
    #[serde(default)]
    state: i64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "progress_bytes")]
    progress_bytes: i64,
    #[serde(default, rename = "uploaded_bytes")]
    uploaded_bytes: i64,
    #[serde(default, rename = "total_bytes")]
    total_bytes: i64,
    #[serde(default)]
    finished: bool,
    #[serde(
        default,
        rename = "finished_at",
        alias = "finished_time",
        alias = "finished_at_seconds"
    )]
    finished_at_seconds: Option<i64>,
    #[serde(default)]
    live: Option<TorrentLiveStats>,
}

#[derive(Default, Deserialize)]
struct TorrentLiveStats {
    #[serde(default, rename = "download_speed")]
    download_speed: Option<TorrentSpeed>,
}

#[derive(Default, Deserialize)]
struct TorrentSpeed {
    #[serde(default)]
    mbps: f64,
}

#[derive(Default, Deserialize, Serialize)]
struct RqbitSeedConfig {
    ratio: Option<f64>,
    seed_time_seconds: Option<i64>,
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
        id: "rqbit".to_string(),
        name: "RQBit".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "rqbit".to_string(),
            provider_aliases: vec!["rqbit-web".to_string()],
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
                mark_imported: true,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: true,
                start_paused: false,
                force_start: false,
                per_download_directory: false,
                host_fs_required: false,
                test_connection: true,
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentFile,
                    ],
                    preferred_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    isolation_modes: vec![DownloadIsolationMode::Directory],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
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
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    let config = RqbitConfig::from_extism()?;
    let body = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        STANDARD
            .decode(bytes)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?
    } else if let Some(source) = source_url(&request) {
        source.into_bytes()
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    };

    let response = post_bytes(&config, "/torrents?overwrite=true", body)?;
    let parsed: PostTorrentResponse = serde_json::from_str(&response)
        .map_err(|error| Error::msg(format!("RQBit add response parse failed: {error}")))?;
    let hash = parsed
        .details
        .map(|details| normalize_hash(&details.info_hash))
        .filter(|value| !value.is_empty())
        .or_else(|| request.release.info_hash_v1.as_deref().map(normalize_hash))
        .or_else(|| {
            request
                .release
                .info_hash_hint
                .as_deref()
                .map(normalize_hash)
        })
        .ok_or_else(|| Error::msg("RQBit did not return an info hash"))?;
    store_seed_config(&hash, &request)?;

    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: hash.clone(),
            info_hash: Some(hash),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_torrent)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_torrent)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_torrent)
        .filter(|torrent| torrent.stats.finished)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = RqbitConfig::from_extism()?;
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ))?);
    }
    match request.action {
        DownloadControlAction::Remove => {
            let endpoint = if request.remove_data {
                "delete"
            } else {
                "forget"
            };
            post_bytes(&config, &format!("/torrents/{hash}/{endpoint}"), Vec::new())?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "RQBit does not support this control action through Scryer's client",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let root: RootResponse = serde_json::from_str(&get_text(&config, "")?)
        .map_err(|error| Error::msg(format!("RQBit root response parse failed: {error}")))?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(root.version),
            is_localhost: Some(is_localhost_url(&config.base_url)),
            remote_output_roots: Vec::new(),
            removes_completed_downloads: Some(false),
            sorting_mode: Some("rqbit-rest".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let root: RootResponse = serde_json::from_str(&get_text(&config, "")?)
        .map_err(|error| Error::msg(format!("RQBit root response parse failed: {error}")))?;
    if version_lt(&root.version, "8.0.0") {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!(
                "RQBit {} is older than Scryer's required 8.0.0",
                root.version
            ),
        ))?);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(root.version))?)
}

impl RqbitConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "3030".to_string());
        let url_base = config_value("url_base").unwrap_or_else(|| "/".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Ok(Self {
            base_url: format!("{scheme}://{host}:{port}/{}", url_base.trim_matches('/'))
                .trim_end_matches('/')
                .to_string(),
        })
    }
}

impl TorrentWithStats {
    fn output_path(&self) -> String {
        format!("{}{}", self.output_folder, self.name)
    }
}

fn is_visible_torrent(torrent: &TorrentWithStats) -> bool {
    let path = torrent.output_path();
    !path.trim().is_empty() && !path.starts_with('.')
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
            Some("3030"),
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
        connection_field("url_base", "URL Base", false, Some("/"), None),
    ]
}

fn list_torrents(config: &RqbitConfig) -> Result<Vec<TorrentWithStats>, Error> {
    let response: ListTorrentsResponse =
        serde_json::from_str(&get_text(config, "/torrents?with_stats=true")?)
            .map_err(|error| Error::msg(format!("RQBit torrent list parse failed: {error}")))?;
    Ok(response.torrents)
}

fn get_text(config: &RqbitConfig, path: &str) -> Result<String, Error> {
    let request = HttpRequest::new(api_url(config, path))
        .with_header("User-Agent", "scryer-rqbit-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("RQBit request failed: {error}")))?;
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!("RQBit returned HTTP {status}: {body}")));
    }
    Ok(body)
}

fn post_bytes(config: &RqbitConfig, path: &str, body: Vec<u8>) -> Result<String, Error> {
    let request = HttpRequest::new(api_url(config, path))
        .with_method("POST")
        .with_header("User-Agent", "scryer-rqbit-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, Some(body))
        .map_err(|error| Error::msg(format!("RQBit request failed: {error}")))?;
    let status = response.status_code();
    let body = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!("RQBit returned HTTP {status}: {body}")));
    }
    Ok(body)
}

fn api_url(config: &RqbitConfig, path: &str) -> String {
    format!(
        "{}{}{}",
        config.base_url.trim_end_matches('/'),
        if path.starts_with('/') || path.is_empty() {
            ""
        } else {
            "/"
        },
        path
    )
}

fn torrent_to_item(torrent: TorrentWithStats) -> PluginDownloadItem {
    let hash = normalize_hash(&torrent.info_hash);
    let remaining = (torrent.stats.total_bytes - torrent.stats.progress_bytes).max(0);
    let down_rate = torrent
        .stats
        .live
        .as_ref()
        .and_then(|live| live.download_speed.as_ref())
        .map(|speed| (speed.mbps * 1_048_576.0) as i64)
        .unwrap_or_default();
    let progress_percent = if torrent.stats.total_bytes > 0 {
        Some(
            ((torrent.stats.progress_bytes as f64 / torrent.stats.total_bytes as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8,
        )
    } else {
        None
    };
    let ratio = if torrent.stats.progress_bytes > 0 {
        Some(torrent.stats.uploaded_bytes as f64 / torrent.stats.progress_bytes as f64)
    } else {
        Some(0.0)
    };
    let can_remove = can_remove(&hash, &torrent, ratio);
    let path = torrent.output_path();

    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: torrent.name,
        state: map_state(&torrent.stats),
        message: torrent.stats.error.clone(),
        category: None,
        remote_output_path: Some(path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            client_native_id: Some(torrent.id.to_string()),
            content_paths: vec![path],
            uploaded_bytes: Some(torrent.stats.uploaded_bytes),
            downloaded_bytes: Some(torrent.stats.progress_bytes),
            download_rate_bytes_per_second: Some(down_rate),
            seed_ratio: ratio,
            raw_status: Some(torrent.stats.state.to_string()),
            status_reason: torrent.stats.error,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.stats.total_bytes),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (down_rate > 0).then_some(remaining / down_rate),
        progress_percent,
        can_move_files: Some(can_remove),
        can_remove: Some(can_remove),
        removed: Some(false),
        raw_state: Some(torrent.stats.state.to_string()),
        completed_at: None,
    }
}

fn torrent_to_completed(torrent: TorrentWithStats) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.info_hash);
    let path = torrent.output_path();
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name,
        dest_dir: path.clone(),
        category: None,
        output_kind: Some(if path_looks_like_file(&path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: vec![path],
        size_bytes: Some(torrent.stats.total_bytes),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn can_remove(hash: &str, torrent: &TorrentWithStats, ratio: Option<f64>) -> bool {
    if !torrent.stats.finished {
        return false;
    }

    let Some(seed_config) = seed_config(hash) else {
        return false;
    };

    if let (Some(current), Some(limit)) = (ratio, seed_config.ratio)
        && current >= limit
    {
        return true;
    }

    if let Some(seed_time_seconds) = seed_config.seed_time_seconds
        && let Some(finished_at) = finished_at(hash, torrent)
    {
        return now_unix_seconds().saturating_sub(finished_at) >= seed_time_seconds;
    }

    false
}

fn store_seed_config(hash: &str, request: &PluginDownloadClientAddRequest) -> Result<(), Error> {
    let seed_config = RqbitSeedConfig {
        ratio: request
            .torrent
            .as_ref()
            .and_then(|torrent| torrent.seed_goal_ratio)
            .or(request.release.seed_goal_ratio),
        seed_time_seconds: request
            .torrent
            .as_ref()
            .and_then(|torrent| torrent.seed_goal_seconds)
            .or(request.release.seed_goal_seconds),
    };

    let _ = var::remove(finished_at_var_key(hash));
    if seed_config.ratio.is_some() || seed_config.seed_time_seconds.is_some() {
        var::set(
            seed_config_var_key(hash),
            serde_json::to_string(&seed_config)?,
        )?;
    }

    Ok(())
}

fn seed_config(hash: &str) -> Option<RqbitSeedConfig> {
    let key = seed_config_var_key(hash);
    var::get::<String>(&key)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn finished_at(hash: &str, torrent: &TorrentWithStats) -> Option<i64> {
    if let Some(value) = torrent.stats.finished_at_seconds.filter(|value| *value > 0) {
        return Some(value);
    }

    let key = finished_at_var_key(hash);
    if let Some(value) = var::get::<String>(&key)
        .ok()
        .flatten()
        .and_then(|raw| raw.parse::<i64>().ok())
    {
        return Some(value);
    }

    let now = now_unix_seconds();
    let _ = var::set(&key, now.to_string());
    Some(now)
}

fn seed_config_var_key(hash: &str) -> String {
    format!("{SEED_CONFIG_VAR_PREFIX}{}", normalize_hash(hash))
}

fn finished_at_var_key(hash: &str) -> String {
    format!("{FINISHED_AT_VAR_PREFIX}{}", normalize_hash(hash))
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn map_state(stats: &TorrentStats) -> DownloadItemState {
    if stats.finished {
        DownloadItemState::Completed
    } else {
        match stats.state {
            0 | 2 => DownloadItemState::Downloading,
            _ => DownloadItemState::Paused,
        }
    }
}

fn source_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    match request.source.kind {
        DownloadInputKind::MagnetUri => request
            .source
            .magnet_uri
            .clone()
            .or_else(|| request.source.download_url.clone()),
        DownloadInputKind::TorrentUrl
        | DownloadInputKind::TorrentFile
        | DownloadInputKind::TorrentBytes => request
            .source
            .torrent_url
            .clone()
            .or_else(|| request.source.download_url.clone())
            .or_else(|| request.source.magnet_uri.clone()),
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => None,
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

fn path_looks_like_file(path: &str) -> bool {
    let Some(last) = path.trim_end_matches('/').rsplit('/').next() else {
        return false;
    };
    let Some(ext) = last.rsplit('.').next() else {
        return false;
    };
    ext != last
}

fn version_lt(left: &str, right: &str) -> bool {
    let parse = |value: &str| -> Vec<u32> {
        value
            .split(|ch: char| !ch.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .take(3)
            .map(|part| part.parse::<u32>().unwrap_or_default())
            .collect()
    };
    let left = parse(left);
    let right = parse(right);
    for index in 0..left.len().max(right.len()) {
        let l = left.get(index).copied().unwrap_or_default();
        let r = right.get(index).copied().unwrap_or_default();
        if l != r {
            return l < r;
        }
    }
    false
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

fn connection_field(
    key: &str,
    label: &str,
    required: bool,
    default_value: Option<&str>,
    help_text: Option<&str>,
) -> ConfigFieldDef {
    ConfigFieldDef {
        role: Some(ConfigFieldRole::ConnectionUrl),
        ..field(
            key,
            label,
            ConfigFieldType::String,
            required,
            default_value,
            help_text,
        )
    }
}

fn is_localhost_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
}
