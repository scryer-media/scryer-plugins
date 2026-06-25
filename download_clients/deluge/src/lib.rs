use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    DownloadClientCapabilities, DownloadClientDescriptor, DownloadControlAction, DownloadInputKind,
    DownloadIsolationMode, DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload,
    PluginDescriptor, PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentInitialState, PluginTorrentItem,
    PluginTorrentQueuePlacement, ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;

const COOKIE_VAR_KEY: &str = "deluge.cookie";
const REQUIRED_PROPERTIES: &[&str] = &[
    "hash",
    "name",
    "state",
    "progress",
    "eta",
    "message",
    "is_finished",
    "save_path",
    "total_size",
    "total_done",
    "time_added",
    "active_time",
    "ratio",
    "is_auto_managed",
    "stop_at_ratio",
    "remove_at_ratio",
    "stop_ratio",
];

#[derive(Debug, Clone)]
struct DelugeConfig {
    json_url: String,
    password: String,
    category: String,
    imported_category: String,
    recent_priority: PluginTorrentQueuePlacement,
    older_priority: PluginTorrentQueuePlacement,
    add_paused: bool,
    download_directory: String,
    completed_directory: String,
    post_import_action: PostImportAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostImportAction {
    Retain,
    Remove,
    RemoveWithData,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<DelugeError>,
}

#[derive(Debug, Deserialize)]
struct DelugeError {
    #[serde(default, alias = "Code")]
    code: i64,
    #[serde(default, alias = "Message")]
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct UpdateUiResult {
    #[serde(default)]
    torrents: HashMap<String, DelugeTorrent>,
}

#[derive(Debug, Default, Deserialize)]
struct DelugeTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    eta: f64,
    #[serde(default)]
    message: String,
    #[serde(default, rename = "is_finished")]
    is_finished: bool,
    #[serde(default, rename = "save_path")]
    download_path: String,
    #[serde(default, rename = "total_size")]
    size: i64,
    #[serde(default, rename = "total_done")]
    bytes_downloaded: i64,
    #[serde(default, rename = "active_time")]
    seconds_downloading: i64,
    #[serde(default)]
    ratio: f64,
    #[serde(default, rename = "is_auto_managed")]
    is_auto_managed: bool,
    #[serde(default, rename = "stop_at_ratio")]
    stop_at_ratio: bool,
    #[serde(default, rename = "stop_ratio")]
    stop_ratio: f64,
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
        id: "deluge".to_string(),
        name: "Deluge".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "deluge".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![
                DownloadIsolationMode::Tag,
                DownloadIsolationMode::Category,
                DownloadIsolationMode::Directory,
            ],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: true,
                mark_imported: true,
                prepare_for_import: false,
                client_status: true,
                queue_priority: true,
                seed_limits: true,
                start_paused: true,
                force_start: false,
                per_download_directory: true,
                host_fs_required: false,
                test_connection: true,
                torrent: Some(DownloadTorrentCapabilities {
                    supported_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    preferred_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    isolation_modes: vec![
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Category,
                        DownloadIsolationMode::Directory,
                    ],
                    post_import_isolation_modes: vec![DownloadIsolationMode::Tag],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: false,
                    supports_start_paused: true,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: true,
                    supports_post_import_isolation: true,
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
    let config = DelugeConfig::from_extism()?;
    let options = add_options(&config, &request);
    let hash = if let Some(bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        let file_name = torrent_file_name(&request);
        call_value(
            &config,
            "core.add_torrent_file",
            serde_json::json!([file_name, bytes_base64, options]),
        )?
    } else if let Some(source) = magnet_source(&request) {
        call_value(
            &config,
            "core.add_torrent_magnet",
            serde_json::json!([source, options]),
        )?
    } else if let Some(source) = torrent_file_url(&request) {
        let bytes = get_external_bytes(&source)?;
        call_value(
            &config,
            "core.add_torrent_file",
            serde_json::json!([torrent_file_name(&request), STANDARD.encode(bytes), options]),
        )?
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    }
    .as_str()
    .map(normalize_hash)
    .filter(|value| !value.is_empty())
    .ok_or_else(|| Error::msg("Deluge did not return an added torrent hash"))?;

    apply_seed_limits(&config, &hash, &request)?;
    if !config.category.is_empty() {
        set_label(&config, &hash, &config.category)?;
    }
    if should_move_to_top(&config, &request) {
        let _ = call_value(
            &config,
            "core.queue_top",
            serde_json::json!([[hash.clone()]]),
        );
    }

    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: hash.clone(),
            info_hash: Some(hash),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| !torrent.hash.trim().is_empty() && !torrent.name.trim().is_empty())
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = DelugeConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_valid_torrent)
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(is_valid_torrent)
        .filter(|torrent| torrent.is_finished && torrent.state != "Checking")
        .map(|torrent| torrent_to_completed(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = DelugeConfig::from_extism()?;
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ))?);
    }
    match request.action {
        DownloadControlAction::Remove => {
            call_value(
                &config,
                "core.remove_torrent",
                serde_json::json!([hash, request.remove_data]),
            )?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Deluge control action is not implemented by Sonarr's Deluge client",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = DelugeConfig::from_extism()?;
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if !hash.is_empty()
        && !config.imported_category.is_empty()
        && config.imported_category != config.category
    {
        let _ = set_label(&config, &hash, &config.imported_category);
    }
    match config.post_import_action {
        PostImportAction::Retain => {}
        PostImportAction::Remove => {
            let _ = call_value(
                &config,
                "core.remove_torrent",
                serde_json::json!([hash, false]),
            );
        }
        PostImportAction::RemoveWithData => {
            let _ = call_value(
                &config,
                "core.remove_torrent",
                serde_json::json!([hash, true]),
            );
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism()?;
    let version = get_version(&config)?;
    let roots = output_roots(&config);
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(version),
            is_localhost: Some(is_localhost_url(&config.json_url)),
            remote_output_roots: roots,
            removes_completed_downloads: Some(!matches!(
                config.post_import_action,
                PostImportAction::Retain
            )),
            sorting_mode: Some("deluge-jsonrpc".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism()?;
    var::remove(COOKIE_VAR_KEY)?;
    let version = get_version(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

impl DelugeConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "8112".to_string());
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
            json_url: format!("{}/json", base.trim_end_matches('/')),
            password: config_value("password").unwrap_or_else(|| "deluge".to_string()),
            category: config_value("category").unwrap_or_else(|| "tv-sonarr".to_string()),
            imported_category: config_value("post_import_category").unwrap_or_default(),
            recent_priority: queue_placement_config("recent_priority"),
            older_priority: queue_placement_config("older_priority"),
            add_paused: config_bool("add_paused", false),
            download_directory: config_value("download_directory").unwrap_or_default(),
            completed_directory: config_value("completed_directory").unwrap_or_default(),
            post_import_action: match config_value("post_import_action").as_deref() {
                Some("remove") => PostImportAction::Remove,
                Some("remove_with_data") => PostImportAction::RemoveWithData,
                _ => PostImportAction::Retain,
            },
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
            Some("8112"),
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
            "password",
            "Password",
            ConfigFieldType::Password,
            true,
            Some("deluge"),
            None,
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            Some("tv-sonarr"),
            None,
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        queue_placement_field(
            "recent_priority",
            "Recent Priority",
            "Queue placement for recent releases",
        ),
        queue_placement_field(
            "older_priority",
            "Older Priority",
            "Queue placement for older releases",
        ),
        field(
            "add_paused",
            "Add Paused",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "download_directory",
            "Download Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        field(
            "completed_directory",
            "Completed Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        ConfigFieldDef {
            key: "post_import_action".to_string(),
            label: "Post Import Action".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("retain".to_string()),
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![
                ConfigFieldOption {
                    value: "retain".to_string(),
                    label: "Retain".to_string(),
                },
                ConfigFieldOption {
                    value: "remove".to_string(),
                    label: "Remove Torrent".to_string(),
                },
                ConfigFieldOption {
                    value: "remove_with_data".to_string(),
                    label: "Remove With Data".to_string(),
                },
            ],
            help_text: Some(
                "What Scryer should do in Deluge after a successful import".to_string(),
            ),
        },
    ]
}

fn add_options(
    config: &DelugeConfig,
    request: &PluginDownloadClientAddRequest,
) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    options.insert(
        "add_paused".to_string(),
        serde_json::Value::Bool(
            request
                .torrent
                .as_ref()
                .and_then(|torrent| torrent.initial_state)
                .is_some_and(|state| state == PluginTorrentInitialState::Paused)
                || config.add_paused,
        ),
    );
    options.insert(
        "remove_at_ratio".to_string(),
        serde_json::Value::Bool(false),
    );
    if let Some(path) = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            (!config.download_directory.is_empty()).then_some(config.download_directory.clone())
        })
    {
        options.insert(
            "download_location".to_string(),
            serde_json::Value::String(path),
        );
    }
    if !config.completed_directory.is_empty() {
        options.insert(
            "move_completed_path".to_string(),
            serde_json::Value::String(config.completed_directory.clone()),
        );
        options.insert("move_completed".to_string(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(options)
}

fn should_move_to_top(config: &DelugeConfig, request: &PluginDownloadClientAddRequest) -> bool {
    match request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.queue_placement)
    {
        Some(PluginTorrentQueuePlacement::First) => true,
        Some(PluginTorrentQueuePlacement::Last) => false,
        Some(PluginTorrentQueuePlacement::Default) | None => {
            let placement = if request.release.is_recent.unwrap_or(false) {
                config.recent_priority
            } else {
                config.older_priority
            };
            placement == PluginTorrentQueuePlacement::First
        }
    }
}

fn authenticate(config: &DelugeConfig, force: bool) -> Result<String, Error> {
    if !force
        && let Some(cookie) = var::get(COOKIE_VAR_KEY)?
            .map(|value: String| value.trim().to_string())
            .filter(|value| !value.is_empty())
    {
        return Ok(cookie);
    }
    let response = raw_call(
        config,
        "auth.login",
        serde_json::json!([config.password]),
        None,
    )?;
    let parsed = parse_rpc(response.body_text.as_str())?;
    if !parsed.result.as_bool().unwrap_or(false) {
        return Err(Error::msg("Failed to authenticate with Deluge"));
    }
    let cookie = response
        .cookie
        .ok_or_else(|| Error::msg("Deluge auth did not return a cookie"))?;
    var::set(COOKIE_VAR_KEY, cookie.clone())?;
    connect_daemon(config, &cookie)?;
    Ok(cookie)
}

fn connect_daemon(config: &DelugeConfig, cookie: &str) -> Result<(), Error> {
    let connected = call_value_with_cookie(config, "web.connected", serde_json::json!([]), cookie)?;
    if connected.as_bool().unwrap_or(false) {
        return Ok(());
    }
    let hosts = call_value_with_cookie(config, "web.get_hosts", serde_json::json!([]), cookie)?;
    if let Some(hosts) = hosts.as_array() {
        for host in hosts {
            let Some(values) = host.as_array() else {
                continue;
            };
            if values.get(1).and_then(|value| value.as_str()) == Some("127.0.0.1")
                && let Some(id) = values.first()
            {
                call_value_with_cookie(config, "web.connect", serde_json::json!([id]), cookie)?;
                return Ok(());
            }
        }
    }
    Err(Error::msg("Failed to connect to Deluge daemon"))
}

fn call_value(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let cookie = authenticate(config, false)?;
    let response = raw_call(config, method, params.clone(), Some(cookie.clone()))?;
    let parsed = parse_rpc(&response.body_text)?;
    if let Some(error) = parsed.error {
        if matches!(error.code, 1 | 2) {
            let cookie = authenticate(config, true)?;
            return call_value_with_cookie(config, method, params, &cookie);
        }
        return Err(Error::msg(format!(
            "Deluge error {}: {}",
            error.code, error.message
        )));
    }
    Ok(parsed.result)
}

fn call_value_with_cookie(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
    cookie: &str,
) -> Result<serde_json::Value, Error> {
    let response = raw_call(config, method, params, Some(cookie.to_string()))?;
    let parsed = parse_rpc(&response.body_text)?;
    if let Some(error) = parsed.error {
        return Err(Error::msg(format!(
            "Deluge error {}: {}",
            error.code, error.message
        )));
    }
    Ok(parsed.result)
}

struct RawResponse {
    body_text: String,
    cookie: Option<String>,
}

fn raw_call(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
    cookie: Option<String>,
) -> Result<RawResponse, Error> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": "scryer"
    });
    let mut request = HttpRequest::new(&config.json_url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "scryer-deluge-plugin/0.1");
    if let Some(cookie) = cookie {
        request = request.with_header("Cookie", cookie);
    }
    let response = http::request::<Vec<u8>>(&request, Some(serde_json::to_vec(&body)?))
        .map_err(|error| Error::msg(format!("Deluge request failed: {error}")))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "Deluge returned HTTP {status}: {body_text}"
        )));
    }
    Ok(RawResponse {
        body_text,
        cookie: extract_cookie(&response),
    })
}

fn parse_rpc(body: &str) -> Result<RpcResponse, Error> {
    serde_json::from_str(body)
        .map_err(|error| Error::msg(format!("Deluge response parse failed: {error}")))
}

fn extract_cookie(response: &HttpResponse) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn list_torrents(config: &DelugeConfig) -> Result<Vec<DelugeTorrent>, Error> {
    let mut filter = serde_json::Map::new();
    if !config.category.is_empty() {
        filter.insert(
            "label".to_string(),
            serde_json::Value::String(config.category.clone()),
        );
    }
    let response = call_value(
        config,
        "web.update_ui",
        serde_json::json!([REQUIRED_PROPERTIES, filter]),
    )?;
    let update: UpdateUiResult = serde_json::from_value(response)
        .map_err(|error| Error::msg(format!("Deluge torrent response parse failed: {error}")))?;
    Ok(update.torrents.into_values().collect())
}

fn get_version(config: &DelugeConfig) -> Result<String, Error> {
    let methods = call_value(config, "system.listMethods", serde_json::json!([]))?;
    let method = if methods.as_array().is_some_and(|methods| {
        methods
            .iter()
            .any(|value| value.as_str() == Some("daemon.get_version"))
    }) {
        "daemon.get_version"
    } else {
        "daemon.info"
    };
    call_value(config, method, serde_json::json!([]))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::msg("Deluge version response was not a string"))
}

fn set_label(config: &DelugeConfig, hash: &str, label: &str) -> Result<(), Error> {
    let _ = call_value(
        config,
        "label.set_torrent",
        serde_json::json!([hash, label]),
    )?;
    Ok(())
}

fn apply_seed_limits(
    config: &DelugeConfig,
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), Error> {
    let Some(ratio) = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio)
    else {
        return Ok(());
    };
    let _ = call_value(
        config,
        "core.set_torrent_options",
        serde_json::json!([[hash], { "stop_ratio": ratio, "stop_at_ratio": 1 }]),
    )?;
    Ok(())
}

fn output_roots(config: &DelugeConfig) -> Vec<String> {
    let mut roots = Vec::new();
    if !config.completed_directory.is_empty() {
        roots.push(config.completed_directory.clone());
    }
    if !config.download_directory.is_empty()
        && !roots.iter().any(|root| root == &config.download_directory)
    {
        roots.push(config.download_directory.clone());
    }
    roots
}

fn torrent_to_item(config: &DelugeConfig, torrent: DelugeTorrent) -> PluginDownloadItem {
    let hash = normalize_hash(&torrent.hash);
    let remaining = (torrent.size - torrent.bytes_downloaded).max(0);
    let path = output_path(&torrent);
    let state = map_state(&torrent);
    let ratio_goal_met = !matches!(config.post_import_action, PostImportAction::Retain)
        && torrent.is_auto_managed
        && torrent.stop_at_ratio
        && torrent.ratio >= torrent.stop_ratio
        && torrent.state == "Paused";
    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: torrent.name.clone(),
        state,
        message: if torrent.message.trim().is_empty() {
            None
        } else {
            Some(torrent.message.clone())
        },
        category: (!config.category.is_empty()).then_some(config.category.clone()),
        remote_output_path: Some(path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            labels: (!config.category.is_empty())
                .then_some(config.category.clone())
                .into_iter()
                .collect(),
            save_path: Some(torrent.download_path.clone()),
            content_paths: vec![path],
            downloaded_bytes: Some(torrent.bytes_downloaded),
            seed_ratio: Some(torrent.ratio),
            seed_time_seconds: Some(torrent.seconds_downloading),
            raw_status: Some(torrent.state.clone()),
            status_reason: if torrent.message.trim().is_empty() {
                None
            } else {
                Some(torrent.message.clone())
            },
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (torrent.eta >= 0.0).then_some(torrent.eta as i64),
        progress_percent: Some(torrent.progress.round().clamp(0.0, 100.0) as u8),
        can_move_files: Some(ratio_goal_met),
        can_remove: Some(ratio_goal_met),
        removed: Some(false),
        raw_state: Some(torrent.state),
        completed_at: None,
    }
}

fn torrent_to_completed(config: &DelugeConfig, torrent: DelugeTorrent) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.hash);
    let path = output_path(&torrent);
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name,
        dest_dir: path.clone(),
        category: (!config.category.is_empty()).then_some(config.category.clone()),
        output_kind: Some(if path_looks_like_file(&path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: vec![path],
        size_bytes: Some(torrent.size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn output_path(torrent: &DelugeTorrent) -> String {
    format!(
        "{}/{}",
        torrent.download_path.trim_end_matches('/'),
        torrent.name
    )
}

fn map_state(torrent: &DelugeTorrent) -> DownloadItemState {
    if torrent.state == "Error" {
        DownloadItemState::Warning
    } else if torrent.is_finished && torrent.state != "Checking" {
        DownloadItemState::Completed
    } else if torrent.state == "Queued" {
        DownloadItemState::Queued
    } else if torrent.state == "Paused" {
        DownloadItemState::Paused
    } else {
        DownloadItemState::Downloading
    }
}

fn is_valid_torrent(torrent: &DelugeTorrent) -> bool {
    !torrent.hash.trim().is_empty() && !torrent.name.trim().is_empty()
}

fn magnet_source(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .source
        .magnet_uri
        .clone()
        .or_else(|| request.source.download_url.clone())
        .filter(|value| value.trim_start().starts_with("magnet:"))
}

fn torrent_file_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    if matches!(
        request.source.kind,
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl
    ) {
        return None;
    }

    request
        .source
        .torrent_url
        .clone()
        .or_else(|| request.source.download_url.clone())
        .filter(|value| !value.trim_start().starts_with("magnet:"))
}

fn torrent_file_name(request: &PluginDownloadClientAddRequest) -> String {
    request
        .source
        .torrent_file_name
        .clone()
        .or_else(|| request.source.source_title.clone())
        .unwrap_or_else(|| format!("{}.torrent", request.title.title_name))
}

fn get_external_bytes(url: &str) -> Result<Vec<u8>, Error> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-deluge-plugin/0.1");
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

fn queue_placement_config(key: &str) -> PluginTorrentQueuePlacement {
    match config_value(key).as_deref() {
        Some("first") => PluginTorrentQueuePlacement::First,
        Some("last") => PluginTorrentQueuePlacement::Last,
        _ => PluginTorrentQueuePlacement::Last,
    }
}

fn queue_placement_field(key: &str, label: &str, help_text: &str) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Select,
        required: false,
        default_value: Some("last".to_string()),
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![
            ConfigFieldOption {
                value: "last".to_string(),
                label: "Last".to_string(),
            },
            ConfigFieldOption {
                value: "first".to_string(),
                label: "First".to_string(),
            },
        ],
        help_text: Some(help_text.to_string()),
    }
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
