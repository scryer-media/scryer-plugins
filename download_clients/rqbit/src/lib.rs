use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const FINISHED_AT_VAR_PREFIX: &str = "rqbit.finished_at.";
const SEED_CONFIG_VAR_PREFIX: &str = "rqbit.seed_config.";

#[derive(Debug, Clone)]
struct RqbitConfig {
    base_url: String,
    directory: Option<String>,
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
    state: String,
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
        details: None,
    })
}

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
                mark_imported: false,
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

    let response = post_bytes(&config, &add_torrent_path(&config), body)?;
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

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| is_visible_torrent(&config, torrent))
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| is_visible_torrent(&config, torrent))
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| is_visible_torrent(&config, torrent))
        .filter(|torrent| torrent.stats.finished)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

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

pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let _: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&plugin_error::<()>(
        PluginErrorCode::Unsupported,
        "RQBit has no category, label, or imported view to mark after import",
    ))?)
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = RqbitConfig::from_extism()?;
    let root: RootResponse = serde_json::from_str(&get_text(&config, "")?)
        .map_err(|error| Error::msg(format!("RQBit root response parse failed: {error}")))?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(root.version),
            is_localhost: Some(is_localhost_url(&config.base_url)),
            remote_output_roots: output_roots(&config),
            removes_completed_downloads: Some(false),
            sorting_mode: Some("rqbit-rest".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

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
        let directory = config_value("directory")
            .map(|value| normalize_directory(&value))
            .filter(|value| !value.is_empty());
        Ok(Self {
            base_url: format!("{scheme}://{host}:{port}/{}", url_base.trim_matches('/'))
                .trim_end_matches('/')
                .to_string(),
            directory,
        })
    }
}

impl TorrentWithStats {
    fn output_path(&self) -> String {
        let folder = self.output_folder.trim_end_matches('/');
        let name = self.name.trim_start_matches('/');
        if name.is_empty() {
            return folder.to_string();
        }
        if folder.rsplit('/').next() == Some(name) {
            return folder.to_string();
        }
        if folder.is_empty() {
            return name.to_string();
        }
        format!("{folder}/{name}")
    }
}

fn is_visible_torrent(config: &RqbitConfig, torrent: &TorrentWithStats) -> bool {
    let path = torrent.output_path();
    !path.trim().is_empty()
        && !path.starts_with('.')
        && config
            .directory
            .as_deref()
            .is_none_or(|directory| path_is_within_directory(&path, directory))
}

fn normalize_directory(path: &str) -> String {
    let path = path.trim();
    if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

fn path_is_within_directory(path: &str, directory: &str) -> bool {
    let path = normalize_directory(path);
    let directory = normalize_directory(directory);
    directory == "/"
        || path == directory
        || path
            .strip_prefix(&directory)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn add_torrent_path(config: &RqbitConfig) -> String {
    let Some(directory) = config.directory.as_deref() else {
        return "/torrents?overwrite=true".to_string();
    };
    let encoded_directory =
        percent_encoding::utf8_percent_encode(directory, percent_encoding::NON_ALPHANUMERIC);
    format!("/torrents?overwrite=true&output_folder={encoded_directory}")
}

fn output_roots(config: &RqbitConfig) -> Vec<String> {
    config.directory.iter().cloned().collect()
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
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            Some(
                "Optional rqbit output root. When set, Scryer sends downloads there and only lists torrents in that directory.",
            ),
        ),
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
    let now = now_unix_seconds();
    let can_remove = derive_can_remove(&hash, &torrent, ratio);
    let seed_time_seconds = torrent
        .stats
        .finished
        .then(|| finished_at(&hash, &torrent))
        .flatten()
        .map(|finished_at| now.saturating_sub(finished_at).max(0));
    let path = torrent.output_path();
    let completed_at = reported_completed_at(&torrent);

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
            seed_time_seconds,
            raw_status: Some(torrent.stats.state.clone()),
            status_reason: torrent.stats.error,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.stats.total_bytes),
        remaining_size_bytes: Some(remaining),
        eta_seconds: eta_seconds(remaining, down_rate),
        progress_percent,
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(torrent.stats.finished),
        can_remove,
        removed: Some(false),
        raw_state: Some(torrent.stats.state.clone()),
        completed_at,
    }
}

fn torrent_to_completed(torrent: TorrentWithStats) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.info_hash);
    let path = torrent.output_path();
    let completed_at = reported_completed_at(&torrent);
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
        completed_at,
        parameters: Vec::new(),
        release_name: None,
    }
}

fn reported_completed_at(torrent: &TorrentWithStats) -> Option<String> {
    torrent
        .stats
        .finished_at_seconds
        .filter(|value| *value > 0)
        .map(|value| value.to_string())
}

/// Honest `can_remove` for rqbit.
///
/// rqbit exposes no seeding-limit API, so the only goal this plugin can measure is the one
/// Scryer handed it at add time. Without that stash the verdict is unknowable (`None`) and
/// Scryer-side goal evaluation decides.
fn derive_can_remove(hash: &str, torrent: &TorrentWithStats, ratio: Option<f64>) -> Option<bool> {
    derive_can_remove_with_config(
        torrent,
        seed_config(hash),
        ratio,
        finished_at(hash, torrent),
        now_unix_seconds(),
    )
}

fn derive_can_remove_with_config(
    torrent: &TorrentWithStats,
    seed_config: Option<RqbitSeedConfig>,
    ratio: Option<f64>,
    finished_at: Option<i64>,
    now: i64,
) -> Option<bool> {
    if !torrent.stats.finished {
        return Some(false);
    }

    let seed_config = seed_config?;

    if let (Some(current), Some(limit)) = (ratio, seed_config.ratio)
        && current >= limit
    {
        return Some(true);
    }

    if let Some(seed_time_seconds) = seed_config.seed_time_seconds
        && let Some(finished_at) = finished_at
    {
        return Some(now.saturating_sub(finished_at) >= seed_time_seconds);
    }

    if seed_config.ratio.is_some() && ratio.is_some() {
        Some(false)
    } else {
        None
    }
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

fn eta_seconds(remaining_bytes: i64, download_rate_bytes_per_second: i64) -> Option<i64> {
    (download_rate_bytes_per_second > 0).then(|| remaining_bytes / download_rate_bytes_per_second)
}

fn map_state(stats: &TorrentStats) -> DownloadItemState {
    if stats.finished {
        return DownloadItemState::Completed;
    }
    match stats.state.to_ascii_lowercase().as_str() {
        "live" | "initializing" => DownloadItemState::Downloading,
        "error" => DownloadItemState::Failed,
        _ => DownloadItemState::Paused,
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn config(directory: Option<&str>) -> RqbitConfig {
        RqbitConfig {
            base_url: "http://rqbit:3030".to_string(),
            directory: directory.map(normalize_directory),
        }
    }

    fn finished_torrent() -> TorrentWithStats {
        TorrentWithStats {
            id: 1,
            info_hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            output_folder: "/downloads/Movie".to_string(),
            stats: TorrentStats {
                state: "live".to_string(),
                progress_bytes: 1_000,
                uploaded_bytes: 2_500,
                total_bytes: 1_000,
                finished: true,
                ..TorrentStats::default()
            },
        }
    }

    #[test]
    fn can_remove_is_false_while_the_download_is_unfinished() {
        let mut torrent = finished_torrent();
        torrent.stats.finished = false;
        torrent.stats.progress_bytes = 400;
        assert_eq!(
            derive_can_remove_with_config(
                &torrent,
                Some(RqbitSeedConfig {
                    ratio: Some(1.0),
                    seed_time_seconds: None,
                }),
                Some(0.4),
                Some(NOW - 100),
                NOW
            ),
            Some(false)
        );
    }

    fn stats(state: &str, finished: bool) -> TorrentStats {
        TorrentStats {
            state: state.to_string(),
            finished,
            ..Default::default()
        }
    }

    fn torrent(output_folder: &str, name: &str) -> TorrentWithStats {
        TorrentWithStats {
            output_folder: output_folder.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn parse_torrent(json: &str) -> TorrentWithStats {
        serde_json::from_str(json).expect("rqbit torrent JSON should parse")
    }

    #[test]
    fn descriptor_does_not_advertise_post_import_marking() {
        let descriptor: PluginDescriptor =
            serde_json::from_str(&scryer_describe(String::new()).expect("describe rqbit"))
                .expect("parse rqbit descriptor");
        let ProviderDescriptor::DownloadClient(client) = descriptor.provider else {
            panic!("rqbit must be a download client");
        };

        assert!(!client.capabilities.mark_imported);
        assert!(
            !client
                .capabilities
                .torrent
                .expect("torrent capabilities")
                .supports_post_import_isolation
        );
    }

    #[test]
    fn mark_imported_reports_unsupported_after_validating_the_request() {
        let result: PluginResult<()> = serde_json::from_str(
            &scryer_download_mark_imported(
                r#"{"client_item_id":"abcdef0123456789abcdef0123456789abcdef01"}"#.to_string(),
            )
            .expect("mark imported response"),
        )
        .expect("parse mark imported response");

        let PluginResult::Err(error) = result else {
            panic!("rqbit mark imported must be unsupported");
        };
        assert_eq!(error.code, PluginErrorCode::Unsupported);
    }

    #[test]
    fn add_path_preserves_legacy_behavior_without_directory() {
        assert_eq!(add_torrent_path(&config(None)), "/torrents?overwrite=true");
    }

    #[test]
    fn add_path_encodes_configured_directory() {
        assert_eq!(
            add_torrent_path(&config(Some("/downloads/Scryer TV"))),
            "/torrents?overwrite=true&output_folder=%2Fdownloads%2FScryer%20TV"
        );
    }

    #[test]
    fn directory_scope_accepts_only_the_configured_root_or_descendants() {
        let scoped = config(Some("/downloads/scryer"));
        assert!(is_visible_torrent(
            &scoped,
            &torrent("/downloads/scryer", "Movie")
        ));
        assert!(is_visible_torrent(
            &scoped,
            &torrent("/downloads/scryer/Show", "Episode")
        ));
        assert!(!is_visible_torrent(
            &scoped,
            &torrent("/downloads/scryer-old", "Movie")
        ));
        assert!(!is_visible_torrent(
            &scoped,
            &torrent("./downloads/scryer", "Movie")
        ));
    }

    #[test]
    fn legacy_scope_keeps_non_hidden_torrents_visible() {
        assert!(is_visible_torrent(
            &config(None),
            &torrent("/downloads/other-client", "Movie")
        ));
        assert!(!is_visible_torrent(
            &config(None),
            &torrent("./downloads", "Movie")
        ));
    }

    #[test]
    fn configured_directory_is_the_only_remote_output_root() {
        assert_eq!(
            output_roots(&config(Some("/downloads/scryer"))),
            vec!["/downloads/scryer"]
        );
        assert!(output_roots(&config(None)).is_empty());
    }

    #[test]
    fn reported_finished_at_is_exported_without_a_local_fallback() {
        let mut queue_torrent = finished_torrent();
        queue_torrent.stats.finished_at_seconds = Some(1_699_999_000);
        assert_eq!(
            torrent_to_item(queue_torrent).completed_at.as_deref(),
            Some("1699999000")
        );

        let mut completed_torrent = finished_torrent();
        completed_torrent.stats.finished_at_seconds = Some(1_700_000_000);
        assert_eq!(
            torrent_to_completed(completed_torrent)
                .completed_at
                .as_deref(),
            Some("1700000000")
        );

        let mut without_reported_time = finished_torrent();
        without_reported_time.stats.finished_at_seconds = Some(0);
        assert_eq!(torrent_to_item(without_reported_time).completed_at, None);
    }

    #[test]
    fn mark_imported_rejects_an_invalid_request() {
        assert!(scryer_download_mark_imported("not json".to_string()).is_err());
    }

    #[test]
    fn maps_all_rqbit_string_states() {
        assert_eq!(
            map_state(&stats("live", false)),
            DownloadItemState::Downloading
        );
        assert_eq!(
            map_state(&stats("initializing", false)),
            DownloadItemState::Downloading
        );
        assert_eq!(
            map_state(&stats("paused", false)),
            DownloadItemState::Paused
        );
        assert_eq!(map_state(&stats("error", false)), DownloadItemState::Failed);
        assert_eq!(
            map_state(&stats("paused", true)),
            DownloadItemState::Completed
        );
        assert_eq!(
            map_state(&stats("LIVE", false)),
            DownloadItemState::Downloading
        );
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_ratio_goal() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(),
                Some(RqbitSeedConfig {
                    ratio: Some(4.0),
                    seed_time_seconds: None,
                }),
                Some(2.5),
                None,
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn parses_string_states_from_rqbit_list_json() {
        let parsed: ListTorrentsResponse = serde_json::from_str(
            r#"{
              "torrents": [
                { "id": 0, "info_hash": "aa", "name": "a", "output_folder": "/downloads/a", "stats": { "state": "paused", "finished": true } },
                { "id": 1, "info_hash": "bb", "name": "b", "output_folder": "/downloads", "stats": { "state": "live", "finished": false } },
                { "id": 2, "info_hash": "cc", "name": "c", "output_folder": "/downloads/c", "stats": { "state": "initializing", "finished": false } },
                { "id": 3, "info_hash": "dd", "name": "d", "output_folder": "/downloads/d", "stats": { "state": "error", "finished": false, "error": "disk full" } }
              ]
            }"#,
        )
        .expect("rqbit list JSON should parse");

        let states: Vec<_> = parsed
            .torrents
            .iter()
            .map(|torrent| (torrent.stats.state.as_str(), map_state(&torrent.stats)))
            .collect();
        assert_eq!(
            states,
            vec![
                ("paused", DownloadItemState::Completed),
                ("live", DownloadItemState::Downloading),
                ("initializing", DownloadItemState::Downloading),
                ("error", DownloadItemState::Failed),
            ]
        );
    }

    #[test]
    fn can_remove_is_true_once_the_ratio_goal_is_met() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(),
                Some(RqbitSeedConfig {
                    ratio: Some(2.0),
                    seed_time_seconds: None,
                }),
                Some(2.5),
                None,
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn rejects_numeric_torrent_state() {
        let error = match serde_json::from_str::<ListTorrentsResponse>(
            r#"{
              "torrents": [
                {
                  "id": 1,
                  "info_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                  "name": "legacy",
                  "output_folder": "/downloads",
                  "stats": { "state": 1, "finished": false }
                }
              ]
            }"#,
        ) {
            Ok(_) => panic!("rqbit 8/9 serializes state as a string"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("invalid type"));
    }

    #[test]
    fn zero_download_rate_does_not_compute_eta() {
        assert_eq!(eta_seconds(1_048_576, 0), None);
        assert_eq!(eta_seconds(0, 0), None);

        let item = torrent_to_item(parse_torrent(
            r#"{
              "id": 4,
              "info_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "name": "Show.S01E02.mkv",
              "output_folder": "/downloads",
              "stats": {
                "state": "paused",
                "progress_bytes": 100,
                "total_bytes": 1000,
                "finished": false,
                "live": null
              }
            }"#,
        ));
        assert_eq!(item.eta_seconds, None);
    }

    #[test]
    fn positive_download_rate_reports_eta() {
        assert_eq!(eta_seconds(2_097_152, 1_048_576), Some(2));

        let item = torrent_to_item(parse_torrent(
            r#"{
              "id": 4,
              "info_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "name": "Show.S01E02.mkv",
              "output_folder": "/downloads",
              "stats": {
                "state": "live",
                "progress_bytes": 0,
                "total_bytes": 2097152,
                "finished": false,
                "live": { "download_speed": { "mbps": 1.0 } }
              }
            }"#,
        ));
        assert_eq!(item.eta_seconds, Some(2));
    }

    #[test]
    fn output_path_joins_single_file_into_session_folder() {
        // rqbit writes a single-file torrent directly into the session folder.
        assert_eq!(
            torrent("/downloads", "Show.S01E02.mkv").output_path(),
            "/downloads/Show.S01E02.mkv"
        );
        assert_eq!(
            torrent("/downloads/", "Show.S01E02.mkv").output_path(),
            "/downloads/Show.S01E02.mkv"
        );
    }

    #[test]
    fn can_remove_is_true_once_the_seed_time_goal_elapsed() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(),
                Some(RqbitSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(3_600),
                }),
                Some(0.1),
                Some(NOW - 7_200),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_unknown_without_a_stored_goal() {
        assert_eq!(
            derive_can_remove_with_config(&finished_torrent(), None, Some(9.0), None, NOW),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_the_seed_time_goal_has_no_reference_timestamp() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(),
                Some(RqbitSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(3_600),
                }),
                Some(0.1),
                None,
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let item = torrent_to_item(finished_torrent());
        assert_eq!(item.can_move_files, Some(true));
        // No stored goal in the test host, so the seeding verdict is unknown.
        assert_eq!(item.can_remove, None);
    }

    #[test]
    fn is_private_is_never_claimed_because_rqbit_does_not_report_it() {
        let item = torrent_to_item(finished_torrent());
        assert_eq!(item.torrent.unwrap().is_private, None);
    }

    #[test]
    fn observed_ratio_is_uploaded_over_downloaded() {
        let item = torrent_to_item(finished_torrent());
        assert_eq!(item.torrent.unwrap().seed_ratio, Some(2.5));
    }

    #[test]
    fn output_path_uses_multi_file_subfolder_without_duplicating_name() {
        // Multi-file torrents already include the torrent name in output_folder.
        assert_eq!(
            torrent("/downloads/Show s02e01-02", "Show s02e01-02").output_path(),
            "/downloads/Show s02e01-02"
        );
        assert_eq!(
            torrent("/downloads/Show s02e01-02/", "Show s02e01-02").output_path(),
            "/downloads/Show s02e01-02"
        );
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
