use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use roxmltree::{Document, Node};
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    DownloadClientCapabilities, DownloadClientDescriptor, DownloadControlAction, DownloadInputKind,
    DownloadIsolationMode, DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload,
    PluginDescriptor, PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const IMPORTED_VIEW: &str = "scryer_imported";
const SEED_CONFIG_VAR_PREFIX: &str = "rtorrent.seed_config.";

#[derive(Debug, Clone)]
struct RTorrentConfig {
    rpc_url: String,
    username: String,
    password: String,
    category: String,
    post_import_category: String,
    directory: String,
    recent_priority: i64,
    older_priority: i64,
    add_stopped: bool,
}

#[derive(Debug, Clone, Default)]
struct RTorrentTorrent {
    name: String,
    hash: String,
    path: String,
    category: String,
    total_size: i64,
    remaining_size: i64,
    down_rate: i64,
    ratio: i64,
    is_active: bool,
    is_finished: bool,
    finished_time: i64,
}

#[derive(Default, Deserialize, Serialize)]
struct RTorrentSeedConfig {
    ratio: Option<f64>,
    seed_time_seconds: Option<i64>,
}

#[derive(Debug, Clone)]
enum XmlValue {
    String(String),
    Base64(Vec<u8>),
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
        id: "rtorrent".to_string(),
        name: "rTorrent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "rtorrent".to_string(),
            provider_aliases: vec!["rTorrent".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![DownloadIsolationMode::Tag, DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: false,
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
                    isolation_modes: vec![
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Directory,
                    ],
                    post_import_isolation_modes: vec![DownloadIsolationMode::Tag],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
                    supports_start_paused: true,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: false,
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
    let config = RTorrentConfig::from_extism()?;
    let category = request
        .routing
        .isolation_value
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.category.clone());
    let directory = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.directory.clone());
    let priority = if request.release.is_recent.unwrap_or(false) {
        config.recent_priority
    } else {
        config.older_priority
    };
    let mut args = vec![XmlValue::String(String::new())];
    let method = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let decoded = STANDARD
            .decode(bytes)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?;
        args.push(XmlValue::Base64(decoded));
        if config.add_stopped {
            "load.raw"
        } else {
            "load.raw_start"
        }
    } else if let Some(source) = source_url(&request) {
        args.push(XmlValue::String(source));
        if config.add_stopped {
            "load.normal"
        } else {
            "load.start"
        }
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    };
    args.extend(
        command_list(&category, priority, &directory)
            .into_iter()
            .map(XmlValue::String),
    );
    let response = call_document(&config, method, &args)?;
    if int_response(&response)? != 0 {
        return Err(Error::msg("rTorrent did not accept the torrent").into());
    }
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg("rTorrent add requires an info hash from the release"))?;
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
    let config = RTorrentConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .filter(|torrent| torrent.is_finished)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = RTorrentConfig::from_extism()?;
    match request.action {
        DownloadControlAction::Remove => {
            if request.remove_data {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Unsupported,
                    "Scryer deletes rTorrent data through host filesystem access; this ABI only supports d.erase",
                ))?);
            }
            let response = call_document(
                &config,
                "d.erase",
                &[XmlValue::String(normalize_hash(&request.client_item_id))],
            )?;
            if int_response(&response)? != 0 {
                return Err(Error::msg("rTorrent did not remove the torrent").into());
            }
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "rTorrent control action is not implemented by Scryer's rTorrent client",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = RTorrentConfig::from_extism()?;
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if !config.post_import_category.is_empty()
        && config.post_import_category != config.category
        && let Ok(response) = call_document(
            &config,
            "d.custom1.set",
            &[
                XmlValue::String(hash.clone()),
                XmlValue::String(config.post_import_category.clone()),
            ],
        )
    {
        let _ = string_response(&response);
    }
    let _ = call_document(
        &config,
        "d.views.push_back_unique",
        &[
            XmlValue::String(hash),
            XmlValue::String(IMPORTED_VIEW.to_string()),
        ],
    );
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    let version = get_version(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(version),
            is_localhost: Some(is_localhost_url(&config.rpc_url)),
            remote_output_roots: if config.directory.is_empty() {
                Vec::new()
            } else {
                vec![config.directory]
            },
            removes_completed_downloads: Some(!config.post_import_category.is_empty()),
            sorting_mode: Some("rtorrent-xmlrpc".to_string()),
            warnings: vec![
                "Remove with data is unavailable because Scryer's rTorrent implementation deletes files through the host filesystem".to_string(),
                format!("Imported torrents are also pushed into the {IMPORTED_VIEW} view"),
            ],
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    let version = get_version(&config)?;
    if version_lt(&version, "0.9.0") {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!("rTorrent {version} is older than Scryer's required 0.9.0"),
        ))?);
    }
    let _ = list_torrents(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

impl RTorrentConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "8080".to_string());
        let url_base = config_value("url_base").unwrap_or_else(|| "RPC2".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Ok(Self {
            rpc_url: format!(
                "{scheme}://{host}:{port}/{}",
                url_base.trim_start_matches('/')
            ),
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_else(|| "scryer-tv".to_string()),
            post_import_category: config_value("post_import_category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
            recent_priority: config_value("recent_priority")
                .and_then(|value| value.parse().ok())
                .unwrap_or(2),
            older_priority: config_value("older_priority")
                .and_then(|value| value.parse().ok())
                .unwrap_or(2),
            add_stopped: config_bool("add_stopped", false),
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
            Some("8080"),
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
        connection_field("url_base", "URL Path", true, Some("RPC2"), None),
        field(
            "username",
            "Username",
            ConfigFieldType::String,
            false,
            None,
            None,
        ),
        field(
            "password",
            "Password",
            ConfigFieldType::Password,
            false,
            None,
            None,
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            true,
            Some("scryer-tv"),
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
        field(
            "directory",
            "Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        priority_field("recent_priority", "Recent Priority"),
        priority_field("older_priority", "Older Priority"),
        field(
            "add_stopped",
            "Add Stopped",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
    ]
}

fn priority_field(key: &str, label: &str) -> ConfigFieldDef {
    ConfigFieldDef {
        key: key.to_string(),
        label: label.to_string(),
        field_type: ConfigFieldType::Select,
        required: false,
        default_value: Some("2".to_string()),
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![
            ConfigFieldOption {
                value: "0".to_string(),
                label: "Very Low".to_string(),
            },
            ConfigFieldOption {
                value: "1".to_string(),
                label: "Low".to_string(),
            },
            ConfigFieldOption {
                value: "2".to_string(),
                label: "Normal".to_string(),
            },
            ConfigFieldOption {
                value: "3".to_string(),
                label: "High".to_string(),
            },
        ],
        help_text: None,
    }
}

fn get_version(config: &RTorrentConfig) -> Result<String, Error> {
    let response = call_document(config, "system.client_version", &[])?;
    Ok(string_response(&response)?.if_empty("0.0.0"))
}

fn list_torrents(config: &RTorrentConfig) -> Result<Vec<RTorrentTorrent>, Error> {
    let response = call_document(
        config,
        "d.multicall2",
        &[
            XmlValue::String(String::new()),
            XmlValue::String(String::new()),
            XmlValue::String("d.name=".to_string()),
            XmlValue::String("d.hash=".to_string()),
            XmlValue::String("d.base_path=".to_string()),
            XmlValue::String("d.custom1=".to_string()),
            XmlValue::String("d.size_bytes=".to_string()),
            XmlValue::String("d.left_bytes=".to_string()),
            XmlValue::String("d.down.rate=".to_string()),
            XmlValue::String("d.ratio=".to_string()),
            XmlValue::String("d.is_open=".to_string()),
            XmlValue::String("d.is_active=".to_string()),
            XmlValue::String("d.complete=".to_string()),
            XmlValue::String("d.timestamp.finished=".to_string()),
        ],
    )?;
    parse_torrents(&response)
}

fn call_document(
    config: &RTorrentConfig,
    method: &str,
    params: &[XmlValue],
) -> Result<String, Error> {
    let body = format!(
        r#"<?xml version="1.0"?><methodCall><methodName>{}</methodName><params>{}</params></methodCall>"#,
        xml_escape(method),
        params
            .iter()
            .map(|param| format!("<param><value>{}</value></param>", xml_value(param)))
            .collect::<Vec<_>>()
            .join("")
    );
    let mut request = HttpRequest::new(&config.rpc_url)
        .with_method("POST")
        .with_header("Content-Type", "text/xml")
        .with_header("User-Agent", "scryer-rtorrent-plugin/0.1");
    if !config.username.is_empty() || !config.password.is_empty() {
        let auth = STANDARD.encode(format!("{}:{}", config.username, config.password));
        request = request.with_header("Authorization", format!("Basic {auth}"));
    }
    let response = http::request::<Vec<u8>>(&request, Some(body.into_bytes()))
        .map_err(|error| Error::msg(format!("rTorrent XML-RPC request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "rTorrent XML-RPC returned HTTP {status}: {text}"
        )));
    }
    check_fault(&text)?;
    Ok(text)
}

fn parse_torrents(xml: &str) -> Result<Vec<RTorrentTorrent>, Error> {
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let response_value = first_response_value(&doc)
        .ok_or_else(|| Error::msg("rTorrent response missing torrent array"))?;
    let mut out = Vec::new();
    for row in array_values(response_value) {
        let values = array_values(row);
        if values.len() < 12 {
            continue;
        }
        out.push(RTorrentTorrent {
            name: node_text(values[0]).unwrap_or_default(),
            hash: normalize_hash(&node_text(values[1]).unwrap_or_default()),
            path: node_text(values[2]).unwrap_or_default(),
            category: decode_category(&node_text(values[3]).unwrap_or_default()),
            total_size: parse_i64(values[4]),
            remaining_size: parse_i64(values[5]),
            down_rate: parse_i64(values[6]),
            ratio: parse_i64(values[7]),
            is_active: parse_i64(values[9]) != 0,
            is_finished: parse_i64(values[10]) != 0,
            finished_time: parse_i64(values[11]),
        });
    }
    Ok(out)
}

fn torrent_to_item(config: &RTorrentConfig, torrent: RTorrentTorrent) -> PluginDownloadItem {
    let state = if torrent.is_finished {
        DownloadItemState::Completed
    } else if torrent.is_active {
        DownloadItemState::Downloading
    } else {
        DownloadItemState::Paused
    };
    let eta = if torrent.down_rate > 0 {
        Some(torrent.remaining_size / torrent.down_rate)
    } else {
        Some(0)
    };
    let can_remove = !config.post_import_category.is_empty() && can_remove(&torrent);
    PluginDownloadItem {
        client_item_id: torrent.hash.clone(),
        download_id: None,
        info_hash: Some(torrent.hash.clone()),
        title: torrent.name.clone(),
        state,
        message: None,
        category: non_empty(torrent.category.clone()),
        remote_output_path: non_empty(torrent.path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(torrent.hash.clone()),
            tags: non_empty(torrent.category.clone()).into_iter().collect(),
            save_path: non_empty(torrent.path.clone()),
            content_paths: non_empty(torrent.path.clone()).into_iter().collect(),
            download_rate_bytes_per_second: Some(torrent.down_rate),
            seed_ratio: Some(torrent.ratio as f64 / 1000.0),
            raw_status: Some(format!(
                "active={},finished={}",
                torrent.is_active, torrent.is_finished
            )),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_size),
        remaining_size_bytes: Some(torrent.remaining_size),
        eta_seconds: eta,
        progress_percent: if torrent.total_size > 0 {
            Some(
                (((torrent.total_size - torrent.remaining_size) as f64 / torrent.total_size as f64)
                    * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8,
            )
        } else {
            None
        },
        can_move_files: Some(can_remove),
        can_remove: Some(can_remove),
        removed: Some(false),
        raw_state: Some(format!(
            "active={},finished={}",
            torrent.is_active, torrent.is_finished
        )),
        completed_at: (torrent.finished_time > 0).then(|| torrent.finished_time.to_string()),
    }
}

fn torrent_matches_scope(config: &RTorrentConfig, torrent: &RTorrentTorrent) -> bool {
    (config.category.is_empty() || torrent.category == config.category)
        && !torrent.path.trim().is_empty()
        && !torrent.path.trim_start().starts_with('.')
}

fn torrent_to_completed(torrent: RTorrentTorrent) -> PluginCompletedDownload {
    PluginCompletedDownload {
        client_item_id: torrent.hash.clone(),
        download_id: None,
        info_hash: Some(torrent.hash),
        name: torrent.name,
        dest_dir: torrent.path.clone(),
        category: non_empty(torrent.category),
        output_kind: Some(if path_looks_like_file(&torrent.path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(torrent.path).into_iter().collect(),
        size_bytes: Some(torrent.total_size),
        completed_at: (torrent.finished_time > 0).then(|| torrent.finished_time.to_string()),
        parameters: Vec::new(),
    }
}

fn can_remove(torrent: &RTorrentTorrent) -> bool {
    if !torrent.is_finished {
        return false;
    }

    let Some(seed_config) = seed_config(&torrent.hash) else {
        return false;
    };

    let ratio = torrent.ratio as f64 / 1000.0;
    if seed_config.ratio.is_some_and(|limit| ratio >= limit) {
        return true;
    }

    if let Some(seed_time_seconds) = seed_config.seed_time_seconds
        && torrent.finished_time > 0
    {
        return now_unix_seconds().saturating_sub(torrent.finished_time) >= seed_time_seconds;
    }

    false
}

fn store_seed_config(hash: &str, request: &PluginDownloadClientAddRequest) -> Result<(), Error> {
    let seed_config = RTorrentSeedConfig {
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

    if seed_config.ratio.is_some() || seed_config.seed_time_seconds.is_some() {
        var::set(
            seed_config_var_key(hash),
            serde_json::to_string(&seed_config)?,
        )?;
    }

    Ok(())
}

fn seed_config(hash: &str) -> Option<RTorrentSeedConfig> {
    let key = seed_config_var_key(hash);
    var::get::<String>(&key)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn seed_config_var_key(hash: &str) -> String {
    format!("{SEED_CONFIG_VAR_PREFIX}{}", normalize_hash(hash))
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn command_list(label: &str, priority: i64, directory: &str) -> Vec<String> {
    let mut commands = Vec::new();
    if !label.trim().is_empty() {
        commands.push(format!("d.custom1.set={label}"));
    }
    if priority != 2 {
        commands.push(format!("d.priority.set={priority}"));
    }
    if !directory.trim().is_empty() {
        commands.push(format!("d.directory.set={directory}"));
    }
    commands
}

fn int_response(xml: &str) -> Result<i64, Error> {
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    Ok(first_response_value(&doc)
        .map(parse_i64)
        .unwrap_or_default())
}

fn string_response(xml: &str) -> Result<String, Error> {
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    Ok(first_response_value(&doc)
        .and_then(node_text)
        .unwrap_or_default())
}

fn check_fault(xml: &str) -> Result<(), Error> {
    if !xml.contains("<fault>") {
        return Ok(());
    }
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let fault = doc
        .descendants()
        .find(|node| node.has_tag_name("fault"))
        .ok_or_else(|| Error::msg("rTorrent returned an XML-RPC fault"))?;
    let code = member_value(fault, "faultCode")
        .and_then(node_text)
        .unwrap_or_default();
    let message = member_value(fault, "faultString")
        .and_then(node_text)
        .unwrap_or_default();
    Err(Error::msg(format!(
        "rTorrent returned error code {code}: {message}"
    )))
}

fn first_response_value<'a>(doc: &'a Document<'a>) -> Option<Node<'a, 'a>> {
    doc.descendants()
        .find(|node| node.has_tag_name("param"))?
        .children()
        .find(|node| node.has_tag_name("value"))
}

fn array_values<'a>(node: Node<'a, 'a>) -> Vec<Node<'a, 'a>> {
    node.children()
        .find(|child| child.has_tag_name("array"))
        .and_then(|array| array.children().find(|child| child.has_tag_name("data")))
        .map(|data| {
            data.children()
                .filter(|child| child.has_tag_name("value"))
                .collect()
        })
        .unwrap_or_default()
}

fn member_value<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.descendants()
        .filter(|child| child.has_tag_name("member"))
        .find(|member| {
            member
                .children()
                .find(|child| child.has_tag_name("name"))
                .and_then(|child| child.text())
                == Some(name)
        })?
        .children()
        .find(|child| child.has_tag_name("value"))
}

fn node_text(node: Node<'_, '_>) -> Option<String> {
    node.descendants()
        .find(|child| child.is_text() || child.text().is_some())
        .and_then(|child| child.text())
        .map(str::to_string)
}

fn parse_i64(node: Node<'_, '_>) -> i64 {
    node_text(node)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default()
}

fn xml_value(value: &XmlValue) -> String {
    match value {
        XmlValue::String(value) => format!("<string>{}</string>", xml_escape(value)),
        XmlValue::Base64(bytes) => format!("<base64>{}</base64>", STANDARD.encode(bytes)),
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

fn decode_category(value: &str) -> String {
    urlencoding::decode(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_string())
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

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
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
