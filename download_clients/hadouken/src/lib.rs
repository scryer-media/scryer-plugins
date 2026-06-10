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
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone)]
struct HadoukenConfig {
    api_url: String,
    username: String,
    password: String,
    category: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HadoukenState {
    Unknown,
    QueuedForChecking,
    CheckingFiles,
    Downloading,
    Paused,
}

#[derive(Default, Deserialize)]
struct HadoukenSystemInfo {
    #[serde(default)]
    versions: std::collections::HashMap<String, String>,
}

#[derive(Default, Deserialize)]
struct HadoukenTorrentResponse {
    #[serde(default)]
    torrents: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone)]
struct HadoukenTorrent {
    info_hash: String,
    progress: f64,
    name: String,
    label: String,
    save_path: String,
    state: HadoukenState,
    is_finished: bool,
    total_size: i64,
    downloaded_bytes: i64,
    uploaded_bytes: i64,
    download_rate: i64,
    error: String,
}

#[derive(Default, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
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
        id: "hadouken".to_string(),
        name: "Hadouken".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "hadouken".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![DownloadIsolationMode::Tag],
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
                    isolation_modes: vec![DownloadIsolationMode::Tag],
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
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    let config = HadoukenConfig::from_extism()?;
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());

    let added_hash = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let returned_hash = call::<String>(
            &config,
            "webui.addTorrent",
            serde_json::json!(["file", bytes, { "label": config.category }]),
        )?;
        Some(normalize_hash(&returned_hash))
    } else if let Some(source) = source_url(&request) {
        let returned_hash: String = call(
            &config,
            "webui.addTorrent",
            serde_json::json!(["url", source, { "label": config.category }]),
        )?;
        Some(normalize_hash(&returned_hash))
            .filter(|value| !value.is_empty())
            .or(hash)
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    };

    let client_item_id = added_hash
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg("Hadouken did not return an info hash"))?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: client_item_id.clone(),
            info_hash: Some(client_item_id),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = HadoukenConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| config.category.is_empty() || torrent.label == config.category)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = HadoukenConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| config.category.is_empty() || torrent.label == config.category)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = HadoukenConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| config.category.is_empty() || torrent.label == config.category)
        .filter(|torrent| torrent.is_finished)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = HadoukenConfig::from_extism()?;
    match request.action {
        DownloadControlAction::Remove => {
            let method = if request.remove_data {
                "removedata"
            } else {
                "remove"
            };
            let _: bool = call(
                &config,
                "webui.perform",
                serde_json::json!([method, [request.client_item_id]]),
            )?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Hadouken control action is not implemented by Sonarr's Hadouken client",
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
    let config = HadoukenConfig::from_extism()?;
    let settings: std::collections::HashMap<String, serde_json::Value> =
        call(&config, "webui.getSettings", serde_json::json!([]))?;
    let root = settings
        .get("bittorrent.defaultSavePath")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: get_version(&config).ok(),
            is_localhost: Some(is_localhost_url(&config.api_url)),
            remote_output_roots: if root.is_empty() {
                Vec::new()
            } else {
                vec![root]
            },
            removes_completed_downloads: Some(false),
            sorting_mode: Some("hadouken-webui".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = HadoukenConfig::from_extism()?;
    let version = get_version(&config)?;
    if version_lt(&version, "5.1") {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!("Hadouken {version} is older than Sonarr's required 5.1"),
        ))?);
    }
    let _ = list_torrents(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

impl HadoukenConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "7070".to_string());
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
            api_url: format!("{}/api", base.trim_end_matches('/')),
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_else(|| "sonarr-tv".to_string()),
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
            Some("7070"),
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
        connection_field("url_base", "URL Base", false, None, None),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "password",
            "Password",
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
            Some("sonarr-tv"),
            None,
        ),
    ]
}

fn get_version(config: &HadoukenConfig) -> Result<String, Error> {
    let info: HadoukenSystemInfo = call(config, "core.getSystemInfo", serde_json::json!([]))?;
    Ok(info
        .versions
        .get("hadouken")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string()))
}

fn list_torrents(config: &HadoukenConfig) -> Result<Vec<HadoukenTorrent>, Error> {
    let response: HadoukenTorrentResponse = call(config, "webui.list", serde_json::json!([]))?;
    Ok(response
        .torrents
        .into_iter()
        .filter_map(map_torrent)
        .collect())
}

fn call<T: DeserializeOwned>(
    config: &HadoukenConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<T, Error> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let auth = STANDARD.encode(format!("{}:{}", config.username, config.password));
    let request = HttpRequest::new(&config.api_url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("Accept-Encoding", "gzip,deflate")
        .with_header("Authorization", format!("Basic {auth}"))
        .with_header("User-Agent", "scryer-hadouken-plugin/0.1");
    let response = http::request::<Vec<u8>>(
        &request,
        Some(serde_json::to_vec(&body).map_err(|error| Error::msg(error.to_string()))?),
    )
    .map_err(|error| Error::msg(format!("Hadouken request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        return Err(Error::msg("Failed to authenticate with Hadouken"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "Hadouken returned HTTP {status}: {text}"
        )));
    }
    let rpc: RpcResponse<T> = serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("Hadouken response parse failed: {error}")))?;
    if let Some(error) = rpc.error {
        return Err(Error::msg(format!("Hadouken returned error: {error}")));
    }
    rpc.result
        .ok_or_else(|| Error::msg("Hadouken response did not contain result"))
}

fn map_torrent(raw: Vec<serde_json::Value>) -> Option<HadoukenTorrent> {
    let info_hash = value_string(raw.first()?).to_ascii_uppercase();
    let state = raw
        .get(1)
        .and_then(value_i64)
        .map(parse_state)
        .unwrap_or(HadoukenState::Unknown);
    let progress = raw.get(4).and_then(value_f64).unwrap_or_default();
    Some(HadoukenTorrent {
        info_hash,
        state,
        name: raw.get(2).map(value_string).unwrap_or_default(),
        total_size: raw.get(3).and_then(value_i64).unwrap_or_default(),
        progress,
        downloaded_bytes: raw.get(5).and_then(value_i64).unwrap_or_default(),
        uploaded_bytes: raw.get(6).and_then(value_i64).unwrap_or_default(),
        download_rate: raw.get(9).and_then(value_i64).unwrap_or_default(),
        label: raw.get(11).map(value_string).unwrap_or_default(),
        error: raw.get(21).map(value_string).unwrap_or_default(),
        save_path: raw.get(26).map(value_string).unwrap_or_default(),
        is_finished: progress >= 1000.0,
    })
}

fn torrent_to_item(torrent: HadoukenTorrent) -> PluginDownloadItem {
    let remaining = (torrent.total_size - torrent.downloaded_bytes).max(0);
    let state = map_state(&torrent);
    let output_path = join_path(&torrent.save_path, &torrent.name);
    let seed_ratio = if torrent.downloaded_bytes > 0 {
        Some(torrent.uploaded_bytes as f64 / torrent.downloaded_bytes as f64)
    } else {
        Some(0.0)
    };
    PluginDownloadItem {
        client_item_id: normalize_hash(&torrent.info_hash),
        info_hash: Some(normalize_hash(&torrent.info_hash)),
        title: torrent.name.clone(),
        state,
        message: if torrent.error.is_empty() {
            None
        } else {
            Some(torrent.error.clone())
        },
        category: non_empty(torrent.label.clone()),
        remote_output_path: non_empty(output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(normalize_hash(&torrent.info_hash)),
            tags: non_empty(torrent.label.clone()).into_iter().collect(),
            save_path: non_empty(torrent.save_path.clone()),
            content_paths: non_empty(output_path).into_iter().collect(),
            uploaded_bytes: Some(torrent.uploaded_bytes),
            downloaded_bytes: Some(torrent.downloaded_bytes),
            download_rate_bytes_per_second: Some(torrent.download_rate),
            seed_ratio,
            raw_status: Some(format!("{:?}", torrent.state)),
            status_reason: non_empty(torrent.error.clone()),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (torrent.download_rate > 0)
            .then_some(torrent.total_size / torrent.download_rate),
        progress_percent: Some(((torrent.progress / 10.0).round().clamp(0.0, 100.0)) as u8),
        can_move_files: Some(torrent.is_finished && torrent.state == HadoukenState::Paused),
        can_remove: Some(torrent.is_finished && torrent.state == HadoukenState::Paused),
        removed: Some(false),
        raw_state: Some(format!("{:?}", torrent.state)),
        completed_at: None,
    }
}

fn torrent_to_completed(torrent: HadoukenTorrent) -> PluginCompletedDownload {
    let output_path = join_path(&torrent.save_path, &torrent.name);
    PluginCompletedDownload {
        client_item_id: normalize_hash(&torrent.info_hash),
        info_hash: Some(normalize_hash(&torrent.info_hash)),
        name: torrent.name,
        dest_dir: output_path.clone(),
        category: non_empty(torrent.label),
        output_kind: Some(if path_looks_like_file(&output_path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(output_path).into_iter().collect(),
        size_bytes: Some(torrent.total_size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn map_state(torrent: &HadoukenTorrent) -> DownloadItemState {
    if !torrent.error.is_empty() {
        DownloadItemState::Warning
    } else if torrent.is_finished && torrent.state != HadoukenState::CheckingFiles {
        DownloadItemState::Completed
    } else if torrent.state == HadoukenState::QueuedForChecking {
        DownloadItemState::Queued
    } else if torrent.state == HadoukenState::Paused {
        DownloadItemState::Paused
    } else {
        DownloadItemState::Downloading
    }
}

fn parse_state(state: i64) -> HadoukenState {
    if (state & 1) == 1 {
        HadoukenState::Downloading
    } else if (state & 2) == 2 {
        HadoukenState::CheckingFiles
    } else if (state & 32) == 32 {
        HadoukenState::Paused
    } else if (state & 64) == 64 {
        HadoukenState::QueuedForChecking
    } else {
        HadoukenState::Unknown
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

fn value_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string().trim_matches('"').to_string())
}

fn value_i64(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn value_f64(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), name)
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
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
    last.contains('.')
}

fn version_lt(left: &str, right: &str) -> bool {
    let parse = |value: &str| -> Vec<u32> {
        value
            .split('.')
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
