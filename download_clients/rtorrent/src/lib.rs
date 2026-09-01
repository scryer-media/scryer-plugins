use base64::{Engine as _, engine::general_purpose::STANDARD};
use roxmltree::{Document, Node};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    DownloadClientCapabilities, DownloadClientDescriptor, DownloadControlAction, DownloadInputKind,
    DownloadIsolationMode, DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload,
    PluginDescriptor, PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadFeedbackScope, PluginDownloadItem,
    PluginDownloadListRecentCompletedRequest, PluginDownloadOutputKind,
    PluginDownloadScopedListRequest, PluginDownloadScopedListResponse,
    PluginDownloadScopedRecentCompletedRequest, PluginError, PluginErrorCode, PluginResult,
    PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::{Deserialize, Serialize};

const IMPORTED_VIEW: &str = "scryer_imported";
const ROUTING_CATEGORY_CUSTOM_KEY: &str = "scryer.routing_category";
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
    routing_category: String,
    total_size: i64,
    remaining_size: i64,
    down_rate: i64,
    ratio: i64,
    is_active: bool,
    is_finished: bool,
    finished_time: i64,
    /// `d.is_private=`; `None` when the rTorrent build did not return the column.
    is_private: Option<bool>,
}

impl RTorrentTorrent {
    /// Keep feedback bound to the routing category after `d.custom1` is changed on import.
    fn feedback_category(&self) -> &str {
        if self.routing_category.trim().is_empty() {
            &self.category
        } else {
            &self.routing_category
        }
    }
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
        details: None,
    })
}

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
                category_scoped_feedback: true,
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: false,
                mark_imported: true,
                mark_imported_non_destructive: true,
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
        return Err(Error::msg("rTorrent did not accept the torrent"));
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

pub fn scryer_download_list_queue(input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    if let Some(scope) = scoped_feedback_scope(&input) {
        let items = feedback_torrents(&config, Some(&scope))?
            .into_iter()
            .map(torrent_to_item)
            .collect::<Vec<_>>();
        return Ok(serde_json::to_string(&PluginResult::Ok(
            PluginDownloadScopedListResponse {
                items,
                failures: Vec::new(),
            },
        ))?);
    }
    let items = feedback_torrents(&config, None)?
        .into_iter()
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_history(input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    if let Some(scope) = scoped_feedback_scope(&input) {
        let mut torrents = feedback_torrents(&config, Some(&scope))?;
        sort_torrents_by_completion(&mut torrents);
        let items = torrents
            .into_iter()
            .map(torrent_to_item)
            .collect::<Vec<_>>();
        return Ok(serde_json::to_string(&PluginResult::Ok(
            PluginDownloadScopedListResponse {
                items,
                failures: Vec::new(),
            },
        ))?);
    }
    let mut torrents = feedback_torrents(&config, None)?;
    sort_torrents_by_completion(&mut torrents);
    let items = torrents
        .into_iter()
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_completed(input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    if let Some(scope) = scoped_feedback_scope(&input) {
        let downloads = completed_feedback_torrents(&config, Some(&scope))?
            .into_iter()
            .map(torrent_to_completed)
            .collect::<Vec<_>>();
        return Ok(serde_json::to_string(&PluginResult::Ok(
            PluginDownloadScopedListResponse {
                items: downloads,
                failures: Vec::new(),
            },
        ))?);
    }
    let downloads = completed_feedback_torrents(&config, None)?
        .into_iter()
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

pub fn scryer_download_list_recent_completed(input: String) -> FnResult<String> {
    let config = RTorrentConfig::from_extism()?;
    let value: serde_json::Value = serde_json::from_str(&input)?;
    if value.get("scope").is_some() {
        let request: PluginDownloadScopedRecentCompletedRequest = serde_json::from_value(value)?;
        let mut items = completed_feedback_torrents(&config, Some(&request.scope))?
            .into_iter()
            .map(torrent_to_completed)
            .collect::<Vec<_>>();
        items.truncate(request.limit);
        return Ok(serde_json::to_string(&PluginResult::Ok(
            PluginDownloadScopedListResponse {
                items,
                failures: Vec::new(),
            },
        ))?);
    }
    let request: PluginDownloadListRecentCompletedRequest = serde_json::from_value(value)?;
    let mut items = completed_feedback_torrents(&config, None)?
        .into_iter()
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    items.truncate(request.limit);
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

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
                return Err(Error::msg("rTorrent did not remove the torrent"));
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

pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = RTorrentConfig::from_extism()?;
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if !config.post_import_category.is_empty() && config.post_import_category != config.category {
        let response = call_document(&config, "d.custom1", &[XmlValue::String(hash.clone())])?;
        let routing_category = decode_category(&string_response(&response)?);
        if !routing_category.trim().is_empty()
            && !routing_category.eq_ignore_ascii_case(&config.post_import_category)
        {
            let response = call_document(
                &config,
                "d.custom.set",
                &[
                    XmlValue::String(hash.clone()),
                    XmlValue::String(ROUTING_CATEGORY_CUSTOM_KEY.to_string()),
                    XmlValue::String(routing_category),
                ],
            )?;
            if int_response(&response)? != 0 {
                return Err(Error::msg(
                    "rTorrent did not preserve the routing category before moving the imported torrent",
                ));
            }
        }
        let response = call_document(
            &config,
            "d.custom1.set",
            &[
                XmlValue::String(hash.clone()),
                XmlValue::String(config.post_import_category.clone()),
            ],
        )?;
        if int_response(&response)? != 0 {
            return Err(Error::msg(
                "rTorrent did not update the imported torrent category",
            ));
        }
    }
    let response = call_document(
        &config,
        "d.views.push_back_unique",
        &[
            XmlValue::String(hash),
            XmlValue::String(IMPORTED_VIEW.to_string()),
        ],
    )?;
    if int_response(&response)? != 0 {
        return Err(Error::msg(
            "rTorrent did not add the imported torrent to the imported view",
        ));
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

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
            // Moving an imported torrent into another category retains it in rTorrent. Reporting
            // otherwise prevents the host from calling `mark_imported` to perform that move.
            removes_completed_downloads: Some(false),
            sorting_mode: Some("rtorrent-xmlrpc".to_string()),
            warnings: vec![
                "Remove with data is unavailable because Scryer's rTorrent implementation deletes files through the host filesystem".to_string(),
                format!("Imported torrents are also pushed into the {IMPORTED_VIEW} view"),
            ],
        },
    ))?)
}

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
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "1".to_string(),
                label: "Low".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "2".to_string(),
                label: "Normal".to_string(),
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "3".to_string(),
                label: "High".to_string(),
                config_overrides: Default::default(),
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
            XmlValue::String(format!("d.custom={ROUTING_CATEGORY_CUSTOM_KEY}")),
            XmlValue::String("d.size_bytes=".to_string()),
            XmlValue::String("d.left_bytes=".to_string()),
            XmlValue::String("d.down.rate=".to_string()),
            XmlValue::String("d.ratio=".to_string()),
            XmlValue::String("d.is_open=".to_string()),
            XmlValue::String("d.is_active=".to_string()),
            XmlValue::String("d.complete=".to_string()),
            XmlValue::String("d.timestamp.finished=".to_string()),
            // Appended last so older rTorrent builds that omit it keep parsing.
            XmlValue::String("d.is_private=".to_string()),
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
        if values.len() < 13 {
            continue;
        }
        out.push(RTorrentTorrent {
            name: node_text(values[0]).unwrap_or_default(),
            hash: normalize_hash(&node_text(values[1]).unwrap_or_default()),
            path: node_text(values[2]).unwrap_or_default(),
            category: decode_category(&node_text(values[3]).unwrap_or_default()),
            routing_category: decode_category(&node_text(values[4]).unwrap_or_default()),
            total_size: parse_i64(values[5]),
            remaining_size: parse_i64(values[6]),
            down_rate: parse_i64(values[7]),
            ratio: parse_i64(values[8]),
            is_active: parse_i64(values[10]) != 0,
            is_finished: parse_i64(values[11]) != 0,
            finished_time: parse_i64(values[12]),
            is_private: values.get(13).map(|value| parse_i64(*value) != 0),
        });
    }
    Ok(out)
}

fn torrent_to_item(torrent: RTorrentTorrent) -> PluginDownloadItem {
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
    let now = now_unix_seconds();
    let can_remove = derive_can_remove(&torrent, now);
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
            seed_time_seconds: seed_time_seconds(&torrent, now),
            is_private: torrent.is_private,
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
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(torrent.is_finished),
        can_remove,
        removed: Some(false),
        raw_state: Some(format!(
            "active={},finished={}",
            torrent.is_active, torrent.is_finished
        )),
        completed_at: (torrent.finished_time > 0).then(|| torrent.finished_time.to_string()),
    }
}

fn scoped_feedback_scope(input: &str) -> Option<PluginDownloadFeedbackScope> {
    let value = serde_json::from_str::<serde_json::Value>(input).ok()?;
    value.get("scope")?;
    if let Ok(request) =
        serde_json::from_value::<PluginDownloadScopedRecentCompletedRequest>(value.clone())
    {
        return Some(request.scope);
    }
    serde_json::from_value::<PluginDownloadScopedListRequest>(value)
        .ok()
        .map(|request| request.scope)
}

fn feedback_scope_allows(scope: &PluginDownloadFeedbackScope, actual: &str) -> bool {
    let actual = actual.trim();
    let configured = scope
        .categories
        .iter()
        .map(|category| category.trim())
        .filter(|category| !category.is_empty())
        .collect::<Vec<_>>();
    configured.is_empty()
        || configured
            .into_iter()
            .any(|category| category.eq_ignore_ascii_case(actual))
}

fn feedback_torrents(
    config: &RTorrentConfig,
    scope: Option<&PluginDownloadFeedbackScope>,
) -> Result<Vec<RTorrentTorrent>, Error> {
    Ok(list_torrents(config)?
        .into_iter()
        .filter(|torrent| torrent_matches_feedback_scope(config, scope, torrent))
        .collect())
}

fn torrent_matches_feedback_scope(
    config: &RTorrentConfig,
    scope: Option<&PluginDownloadFeedbackScope>,
    torrent: &RTorrentTorrent,
) -> bool {
    torrent_matches_scope(config, torrent)
        && scope.is_none_or(|scope| feedback_scope_allows(scope, torrent.feedback_category()))
}

fn sort_torrents_by_completion(torrents: &mut [RTorrentTorrent]) {
    torrents.sort_by(|left, right| {
        right
            .finished_time
            .cmp(&left.finished_time)
            .then_with(|| left.hash.cmp(&right.hash))
    });
}

fn completed_feedback_torrents(
    config: &RTorrentConfig,
    scope: Option<&PluginDownloadFeedbackScope>,
) -> Result<Vec<RTorrentTorrent>, Error> {
    let mut torrents = feedback_torrents(config, scope)?
        .into_iter()
        .filter(|torrent| torrent.is_finished)
        .collect::<Vec<_>>();
    sort_torrents_by_completion(&mut torrents);
    Ok(torrents)
}

fn torrent_matches_scope(config: &RTorrentConfig, torrent: &RTorrentTorrent) -> bool {
    category_allowed(&config.category, torrent.feedback_category())
        && !torrent.path.trim().is_empty()
        && !torrent.path.trim_start().starts_with('.')
}

/// `category` may list several labels, comma or newline separated.
///
/// The host tags each download with its per-scope routing category, so a single
/// client can legitimately hold `movies`, `tv` and `anime` at once. Matching the
/// queue against one configured value made every other facet's downloads
/// invisible: they were never listed, never tracked, and so never imported,
/// while `scryer_download_add` had happily tagged them. An empty setting still
/// matches everything, and a single value behaves exactly as before.
fn category_allowed(configured: &str, actual: &str) -> bool {
    let actual = actual.trim();
    let mut configured_any = false;
    for want in configured.split([',', '\n']) {
        let want = want.trim();
        if want.is_empty() {
            continue;
        }
        configured_any = true;
        if want.eq_ignore_ascii_case(actual) {
            return true;
        }
    }
    !configured_any
}

fn torrent_to_completed(torrent: RTorrentTorrent) -> PluginCompletedDownload {
    PluginCompletedDownload {
        client_item_id: torrent.hash.clone(),
        download_id: None,
        info_hash: Some(torrent.hash),
        name: torrent.name,
        release_name: None,
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

/// Honest `can_remove` for rTorrent.
///
/// rTorrent exposes no per-torrent seeding limit, so the only goal this plugin can measure is
/// the one Scryer handed it at add time (stashed in a plugin variable). When that stash is
/// missing — a fresh plugin instance, or a torrent added outside Scryer — the plugin cannot
/// know whether seeding is finished and reports `None` so Scryer-side evaluation decides.
fn derive_can_remove(torrent: &RTorrentTorrent, now: i64) -> Option<bool> {
    derive_can_remove_with_config(torrent, seed_config(&torrent.hash), now)
}

fn derive_can_remove_with_config(
    torrent: &RTorrentTorrent,
    seed_config: Option<RTorrentSeedConfig>,
    now: i64,
) -> Option<bool> {
    if !torrent.is_finished {
        return Some(false);
    }

    let seed_config = seed_config?;

    let ratio = torrent.ratio as f64 / 1000.0;
    if seed_config.ratio.is_some_and(|limit| ratio >= limit) {
        return Some(true);
    }

    if let Some(seed_time_seconds) = seed_config.seed_time_seconds
        && torrent.finished_time > 0
    {
        return Some(now.saturating_sub(torrent.finished_time) >= seed_time_seconds);
    }

    // A ratio goal exists but is not met yet; a seed-time goal without a finished timestamp
    // is unmeasurable, so only the ratio verdict is reportable.
    if seed_config.ratio.is_some() {
        Some(false)
    } else {
        None
    }
}

/// Seconds the torrent has been seeding, derived from rTorrent's finished timestamp.
fn seed_time_seconds(torrent: &RTorrentTorrent, now: i64) -> Option<i64> {
    (torrent.is_finished && torrent.finished_time > 0)
        .then(|| now.saturating_sub(torrent.finished_time).max(0))
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
        commands.push(format!(
            "d.custom.set={ROUTING_CATEGORY_CUSTOM_KEY},{label}"
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn descriptor_advertises_non_destructive_import_marking() {
        let output = scryer_describe(String::new()).expect("descriptor");
        let descriptor: PluginDescriptor = serde_json::from_str(&output).expect("descriptor JSON");
        let ProviderDescriptor::DownloadClient(download_client) = descriptor.provider else {
            panic!("rTorrent descriptor must be a download client");
        };

        assert!(
            download_client.capabilities.mark_imported_non_destructive,
            "post-import category and view updates do not delete the rTorrent payload"
        );
    }

    fn finished_torrent(ratio_per_mille: i64) -> RTorrentTorrent {
        RTorrentTorrent {
            name: "Movie".to_string(),
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            path: "/downloads/Movie".to_string(),
            total_size: 1_000,
            remaining_size: 0,
            ratio: ratio_per_mille,
            is_active: true,
            is_finished: true,
            finished_time: NOW - 600,
            ..RTorrentTorrent::default()
        }
    }

    fn completed_torrent(hash: &str, category: &str, finished_time: i64) -> RTorrentTorrent {
        RTorrentTorrent {
            hash: hash.to_string(),
            category: category.to_string(),
            finished_time,
            ..finished_torrent(0)
        }
    }

    fn config_with_categories(category: &str) -> RTorrentConfig {
        RTorrentConfig {
            rpc_url: "http://rtorrent/RPC2".to_string(),
            username: String::new(),
            password: String::new(),
            category: category.to_string(),
            post_import_category: String::new(),
            directory: "/downloads".to_string(),
            recent_priority: 2,
            older_priority: 1,
            add_stopped: false,
        }
    }

    #[test]
    fn feedback_scope_intersects_the_configured_category_allowlist() {
        let movies_and_anime = PluginDownloadFeedbackScope {
            categories: vec!["movies".to_string(), "anime".to_string()],
        };
        let anime_only = PluginDownloadFeedbackScope {
            categories: vec!["anime".to_string()],
        };
        let all_categories = PluginDownloadFeedbackScope::default();
        let config = config_with_categories("movies, anime");
        let movies = completed_torrent("movies", "movies", NOW);
        let anime = completed_torrent("anime", "anime", NOW);
        let series = completed_torrent("series", "series", NOW);

        assert!(category_allowed("movies, anime", "movies"));
        assert!(category_allowed("movies, anime", "anime"));
        assert!(!category_allowed("movies, anime", "series"));
        assert!(torrent_matches_feedback_scope(
            &config,
            Some(&movies_and_anime),
            &movies
        ));
        assert!(torrent_matches_feedback_scope(
            &config,
            Some(&anime_only),
            &anime
        ));
        assert!(!torrent_matches_feedback_scope(
            &config,
            Some(&anime_only),
            &movies
        ));
        assert!(!torrent_matches_feedback_scope(
            &config,
            Some(&movies_and_anime),
            &series
        ));
        assert!(torrent_matches_feedback_scope(
            &config,
            Some(&all_categories),
            &anime
        ));
    }

    #[test]
    fn feedback_scope_uses_the_original_category_after_post_import_move() {
        let config = config_with_categories("movies, anime");
        let anime_only = PluginDownloadFeedbackScope {
            categories: vec!["anime".to_string()],
        };
        let movies_only = PluginDownloadFeedbackScope {
            categories: vec!["movies".to_string()],
        };
        let imported_anime = RTorrentTorrent {
            category: "scryer-imported".to_string(),
            routing_category: "anime".to_string(),
            ..completed_torrent("anime", "anime", NOW)
        };

        assert!(torrent_matches_feedback_scope(
            &config,
            Some(&anime_only),
            &imported_anime
        ));
        assert!(!torrent_matches_feedback_scope(
            &config,
            Some(&movies_only),
            &imported_anime
        ));
        assert!(torrent_matches_feedback_scope(
            &config,
            None,
            &imported_anime
        ));
        assert_eq!(
            torrent_to_item(imported_anime).category.as_deref(),
            Some("scryer-imported")
        );
    }

    #[test]
    fn add_commands_store_the_routing_category_in_a_named_custom_field() {
        let commands = command_list("anime", 2, "");
        assert!(commands.contains(&"d.custom1.set=anime".to_string()));
        assert!(commands.contains(&format!("d.custom.set={ROUTING_CATEGORY_CUSTOM_KEY},anime")));
    }

    #[test]
    fn completed_feedback_is_newest_first_stable_and_bounded() {
        let mut torrents = vec![
            completed_torrent("z-new", "movies", NOW - 10),
            completed_torrent("a-new", "movies", NOW - 10),
            completed_torrent("old", "movies", NOW - 20),
            RTorrentTorrent {
                hash: "unfinished".to_string(),
                is_finished: false,
                finished_time: 0,
                ..finished_torrent(0)
            },
        ];
        torrents.retain(|torrent| torrent.is_finished);
        sort_torrents_by_completion(&mut torrents);
        let mut downloads = torrents
            .into_iter()
            .map(torrent_to_completed)
            .collect::<Vec<_>>();
        downloads.truncate(2);

        assert_eq!(
            downloads
                .iter()
                .map(|download| download.client_item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-new", "z-new"]
        );
        assert_eq!(downloads[0].completed_at.as_deref(), Some("1699999990"));
    }

    #[test]
    fn can_remove_is_false_while_the_download_is_unfinished() {
        let torrent = RTorrentTorrent {
            is_finished: false,
            remaining_size: 500,
            ..finished_torrent(0)
        };
        assert_eq!(
            derive_can_remove_with_config(
                &torrent,
                Some(RTorrentSeedConfig {
                    ratio: Some(1.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_ratio_goal() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(400),
                Some(RTorrentSeedConfig {
                    ratio: Some(2.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_the_ratio_goal_is_met() {
        assert_eq!(
            derive_can_remove_with_config(
                &finished_torrent(2_100),
                Some(RTorrentSeedConfig {
                    ratio: Some(2.0),
                    seed_time_seconds: None,
                }),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_true_once_the_seed_time_goal_elapsed() {
        let torrent = RTorrentTorrent {
            finished_time: NOW - 7_200,
            ..finished_torrent(10)
        };
        assert_eq!(
            derive_can_remove_with_config(
                &torrent,
                Some(RTorrentSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(3_600),
                }),
                NOW
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_unknown_without_a_stored_goal() {
        // rTorrent exposes no per-torrent limit, so a torrent Scryer did not add (or whose
        // stashed goal was lost) is unknowable here.
        assert_eq!(
            derive_can_remove_with_config(&finished_torrent(5_000), None, NOW),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_only_a_seed_time_goal_exists_without_a_finish_timestamp() {
        let torrent = RTorrentTorrent {
            finished_time: 0,
            ..finished_torrent(10)
        };
        assert_eq!(
            derive_can_remove_with_config(
                &torrent,
                Some(RTorrentSeedConfig {
                    ratio: None,
                    seed_time_seconds: Some(3_600),
                }),
                NOW
            ),
            None
        );
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let item = torrent_to_item(finished_torrent(10));
        assert_eq!(item.can_move_files, Some(true));
        // No stored goal in the test host, so the seeding verdict is unknown.
        assert_eq!(item.can_remove, None);
    }

    #[test]
    fn seed_time_is_derived_from_the_finished_timestamp() {
        let torrent = RTorrentTorrent {
            finished_time: NOW - 900,
            ..finished_torrent(10)
        };
        assert_eq!(seed_time_seconds(&torrent, NOW), Some(900));
        let unfinished = RTorrentTorrent {
            is_finished: false,
            ..torrent
        };
        assert_eq!(seed_time_seconds(&unfinished, NOW), None);
    }

    #[test]
    fn is_private_maps_present_true_present_false_and_absent() {
        let map = |is_private: Option<bool>| {
            let torrent = RTorrentTorrent {
                is_private,
                ..finished_torrent(10)
            };
            torrent_to_item(torrent).torrent.unwrap().is_private
        };
        assert_eq!(map(Some(true)), Some(true));
        assert_eq!(map(Some(false)), Some(false));
        assert_eq!(map(None), None);
    }

    #[test]
    fn is_private_column_is_optional_in_the_multicall_response() {
        let without_private = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data>
            <value><array><data>
                <value><string>Movie</string></value>
                <value><string>ABCDEF0123456789ABCDEF0123456789ABCDEF01</string></value>
                <value><string>/downloads/Movie</string></value>
                <value><string></string></value>
                <value><string></string></value>
                <value><i8>1000</i8></value>
                <value><i8>0</i8></value>
                <value><i8>0</i8></value>
                <value><i8>1500</i8></value>
                <value><i8>1</i8></value>
                <value><i8>1</i8></value>
                <value><i8>1</i8></value>
                <value><i8>1699999000</i8></value>
            </data></array></value>
        </data></array></value></param></params></methodResponse>"#;
        let torrents = parse_torrents(without_private).unwrap();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].is_private, None);
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

scryer_plugin_pdk::scryer_download_client_bridge_main!(
    describe = scryer_describe,
    add = scryer_download_add,
    list_queue = scryer_download_list_queue,
    list_history = scryer_download_list_history,
    list_completed = scryer_download_list_completed,
    list_recent_completed = Some(scryer_download_list_recent_completed),
    control = scryer_download_control,
    mark_imported = scryer_download_mark_imported,
    mark_imported_non_destructive = Some(scryer_download_mark_imported),
    status = scryer_download_status,
    test_connection = scryer_download_test_connection,
);
