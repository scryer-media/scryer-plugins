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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostImportAction {
    Retain,
    Remove,
}

#[derive(Debug, Clone)]
struct Aria2Config {
    rpc_url: String,
    secret_token: String,
    directory: String,
    post_import_action: PostImportAction,
}

#[derive(Debug, Clone, Default)]
struct Aria2Status {
    bittorrent_name: Option<String>,
    info_hash: Option<String>,
    completed_length: i64,
    download_speed: i64,
    files: Vec<String>,
    gid: String,
    status: String,
    total_length: i64,
    upload_length: i64,
    error_message: Option<String>,
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
        id: "aria2".to_string(),
        name: "Aria2".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "aria2".to_string(),
            provider_aliases: vec!["aria2c".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: true,
                resume: true,
                remove: true,
                remove_with_data: false,
                mark_imported: true,
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
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    let config = Aria2Config::from_extism()?;
    let directory = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!config.directory.is_empty()).then_some(config.directory.clone()));
    let options = directory
        .map(|dir| vec![("dir".to_string(), dir)])
        .unwrap_or_default();

    let gid = if let Some(bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        let torrent_bytes = STANDARD
            .decode(bytes_base64)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?;
        call_string(
            &config,
            "aria2.addTorrent",
            &[
                xml_base64(&torrent_bytes),
                xml_array(Vec::new()),
                xml_struct(&options),
            ],
        )?
    } else if let Some(source) = source_url(&request) {
        call_string(
            &config,
            "aria2.addUri",
            &[xml_array(vec![xml_string(&source)]), xml_struct(&options)],
        )?
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    };

    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());
    let client_item_id = hash.clone().unwrap_or_else(|| gid.clone());
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id,
            info_hash: hash,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    scryer_download_list_queue_inner()
}

fn scryer_download_list_queue_inner() -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .filter(|torrent| torrent.status == "complete")
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = Aria2Config::from_extism()?;
    let Some(gid) = resolve_gid(&config, &request.client_item_id)? else {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "download item was not found",
        ))?);
    };

    match request.action {
        DownloadControlAction::Pause => {
            call_string(&config, "aria2.pause", &[xml_string(&gid)])?;
        }
        DownloadControlAction::Resume => {
            call_string(&config, "aria2.unpause", &[xml_string(&gid)])?;
        }
        DownloadControlAction::Remove => {
            let status = tell_status(&config, &gid)?;
            if matches!(status.status.as_str(), "complete" | "error" | "removed") {
                call_string(&config, "aria2.removeDownloadResult", &[xml_string(&gid)])?;
            } else {
                call_string(&config, "aria2.forceRemove", &[xml_string(&gid)])?;
            }
        }
        DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Aria2 does not support force_start through this plugin",
            ))?);
        }
    }

    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = Aria2Config::from_extism()?;
    if matches!(config.post_import_action, PostImportAction::Remove)
        && let Some(gid) = resolve_gid(&config, &request.client_item_id)?
    {
        let _ = call_string(&config, "aria2.removeDownloadResult", &[xml_string(&gid)]);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let version = get_version(&config)?;
    let globals = get_globals(&config)?;
    let mut roots = Vec::new();
    if let Some(dir) = globals.get("dir").filter(|value| !value.is_empty()) {
        roots.push(dir.clone());
    }
    if !config.directory.is_empty() && !roots.iter().any(|root| root == &config.directory) {
        roots.push(config.directory.clone());
    }
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(version),
            is_localhost: Some(is_localhost_url(&config.rpc_url)),
            remote_output_roots: roots,
            removes_completed_downloads: Some(false),
            sorting_mode: Some("aria2-xmlrpc".to_string()),
            warnings: vec![
                "Aria2 RPC cannot delete downloaded files; remove_with_data is not supported"
                    .to_string(),
            ],
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let version = get_version(&config)?;
    if version_lt(&version, "1.34.0") {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!("Aria2 {version} is older than Sonarr's required 1.34.0"),
        ))?);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

impl Aria2Config {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "6800".to_string());
        let rpc_path = config_value("rpc_path").unwrap_or_else(|| "/rpc".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Ok(Self {
            rpc_url: format!(
                "{scheme}://{host}:{port}/{}",
                rpc_path.trim_start_matches('/')
            ),
            secret_token: config_value("secret_token").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
            post_import_action: match config_value("post_import_action").as_deref() {
                Some("remove") => PostImportAction::Remove,
                _ => PostImportAction::Retain,
            },
        })
    }

    fn token_param(&self) -> String {
        xml_string(&format!("token:{}", self.secret_token))
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
            Some("6800"),
            None,
        ),
        connection_field("rpc_path", "XML-RPC Path", true, Some("/rpc"), None),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "secret_token",
            "Secret Token",
            ConfigFieldType::Password,
            false,
            Some("MySecretToken"),
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
                    label: "Remove Result".to_string(),
                },
            ],
            help_text: Some("What Scryer should do in Aria2 after a successful import".to_string()),
        },
    ]
}

fn call_document(config: &Aria2Config, method: &str, params: &[String]) -> Result<String, Error> {
    let mut all_params = vec![config.token_param()];
    all_params.extend_from_slice(params);
    let body = format!(
        r#"<?xml version="1.0"?><methodCall><methodName>{}</methodName><params>{}</params></methodCall>"#,
        xml_escape(method),
        all_params
            .iter()
            .map(|param| format!("<param><value>{param}</value></param>"))
            .collect::<Vec<_>>()
            .join("")
    );
    let request = HttpRequest::new(&config.rpc_url)
        .with_method("POST")
        .with_header("Content-Type", "text/xml")
        .with_header("User-Agent", "scryer-aria2-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, Some(body.into_bytes()))
        .map_err(|error| Error::msg(format!("Aria2 XML-RPC request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "Aria2 XML-RPC returned HTTP {status}: {text}"
        )));
    }
    check_fault(&text)?;
    Ok(text)
}

fn call_string(config: &Aria2Config, method: &str, params: &[String]) -> Result<String, Error> {
    let xml = call_document(config, method, params)?;
    let doc = Document::parse(&xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let value = first_response_value(&doc).ok_or_else(|| Error::msg("missing XML-RPC value"))?;
    Ok(node_text(value).unwrap_or_default())
}

fn get_version(config: &Aria2Config) -> Result<String, Error> {
    let xml = call_document(config, "aria2.getVersion", &[])?;
    let doc = Document::parse(&xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let value = first_response_value(&doc).ok_or_else(|| Error::msg("missing version value"))?;
    let version = member_value(value, "version")
        .and_then(node_text)
        .ok_or_else(|| Error::msg("Aria2 version response missing version"))?;
    Ok(version)
}

fn get_globals(config: &Aria2Config) -> Result<std::collections::HashMap<String, String>, Error> {
    let xml = call_document(config, "aria2.getGlobalOption", &[])?;
    let doc = Document::parse(&xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let value = first_response_value(&doc).ok_or_else(|| Error::msg("missing globals value"))?;
    Ok(struct_members(value))
}

fn list_torrents(config: &Aria2Config) -> Result<Vec<Aria2Status>, Error> {
    let mut out = Vec::new();
    for (method, args) in [
        ("aria2.tellActive", Vec::new()),
        ("aria2.tellWaiting", vec![xml_int(0), xml_int(10 * 1024)]),
        ("aria2.tellStopped", vec![xml_int(0), xml_int(10 * 1024)]),
    ] {
        let xml = call_document(config, method, &args)?;
        out.extend(parse_status_array(&xml)?);
    }
    Ok(out)
}

fn tell_status(config: &Aria2Config, gid: &str) -> Result<Aria2Status, Error> {
    let xml = call_document(config, "aria2.tellStatus", &[xml_string(gid)])?;
    let doc = Document::parse(&xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let value = first_response_value(&doc).ok_or_else(|| Error::msg("missing status value"))?;
    Ok(parse_status(value))
}

fn parse_status_array(xml: &str) -> Result<Vec<Aria2Status>, Error> {
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let value = first_response_value(&doc).ok_or_else(|| Error::msg("missing status array"))?;
    Ok(value
        .descendants()
        .filter(|node| node.has_tag_name("data"))
        .flat_map(|data| data.children().filter(|node| node.has_tag_name("value")))
        .map(parse_status)
        .collect())
}

fn parse_status(value: Node<'_, '_>) -> Aria2Status {
    let info_hash = member_value(value, "infoHash").and_then(node_text);
    let completed_length = member_value(value, "completedLength")
        .and_then(node_text)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let total_length = member_value(value, "totalLength")
        .and_then(node_text)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let upload_length = member_value(value, "uploadLength")
        .and_then(node_text)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let download_speed = member_value(value, "downloadSpeed")
        .and_then(node_text)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let files = member_value(value, "files")
        .map(parse_files)
        .unwrap_or_default();
    let bittorrent_name = member_value(value, "bittorrent")
        .and_then(|node| member_value(node, "name"))
        .and_then(node_text);

    Aria2Status {
        bittorrent_name,
        info_hash,
        completed_length,
        download_speed,
        files,
        gid: member_value(value, "gid")
            .and_then(node_text)
            .unwrap_or_default(),
        status: member_value(value, "status")
            .and_then(node_text)
            .unwrap_or_default(),
        total_length,
        upload_length,
        error_message: member_value(value, "errorMessage").and_then(node_text),
    }
}

fn parse_files(value: Node<'_, '_>) -> Vec<String> {
    value
        .descendants()
        .filter(|node| node.has_tag_name("struct"))
        .filter_map(|node| member_value(node, "path").and_then(node_text))
        .collect()
}

fn first_response_value<'a>(doc: &'a Document<'a>) -> Option<Node<'a, 'a>> {
    doc.descendants()
        .find(|node| node.has_tag_name("param"))?
        .children()
        .find(|node| node.has_tag_name("value"))
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

fn struct_members(node: Node<'_, '_>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for member in node
        .descendants()
        .filter(|node| node.has_tag_name("member"))
    {
        if let Some(name) = member
            .children()
            .find(|child| child.has_tag_name("name"))
            .and_then(|child| child.text())
            && let Some(value) = member
                .children()
                .find(|child| child.has_tag_name("value"))
                .and_then(node_text)
        {
            out.insert(name.to_string(), value);
        }
    }
    out
}

fn node_text(node: Node<'_, '_>) -> Option<String> {
    node.descendants()
        .find(|child| child.is_text() || child.text().is_some())
        .and_then(|child| child.text())
        .map(str::to_string)
}

fn check_fault(xml: &str) -> Result<(), Error> {
    if !xml.contains("<fault>") {
        return Ok(());
    }
    let doc = Document::parse(xml).map_err(|error| Error::msg(format!("invalid XML: {error}")))?;
    let fault = doc
        .descendants()
        .find(|node| node.has_tag_name("fault"))
        .ok_or_else(|| Error::msg("Aria2 returned an XML-RPC fault"))?;
    let code = member_value(fault, "faultCode")
        .and_then(node_text)
        .unwrap_or_default();
    let message = member_value(fault, "faultString")
        .and_then(node_text)
        .unwrap_or_default();
    Err(Error::msg(format!(
        "Aria2 returned error code {code}: {message}"
    )))
}

fn resolve_gid(config: &Aria2Config, client_item_id: &str) -> Result<Option<String>, Error> {
    let requested = normalize_hash(client_item_id);
    for torrent in list_torrents(config)? {
        if torrent.gid == client_item_id
            || torrent
                .info_hash
                .as_deref()
                .map(normalize_hash)
                .is_some_and(|hash| hash == requested)
        {
            return Ok(Some(torrent.gid));
        }
    }
    Ok(None)
}

fn is_visible_download(torrent: &Aria2Status) -> bool {
    !torrent
        .files
        .first()
        .is_some_and(|path| path.contains("[METADATA]"))
        && torrent.status != "removed"
}

fn torrent_to_item(torrent: Aria2Status) -> PluginDownloadItem {
    let title = torrent.bittorrent_name.clone().unwrap_or_default();
    let hash = torrent.info_hash.as_deref().map(normalize_hash);
    let id = hash.clone().unwrap_or_else(|| torrent.gid.clone());
    let remaining = (torrent.total_length - torrent.completed_length).max(0);
    let progress_percent = if torrent.total_length > 0 {
        Some(
            ((torrent.completed_length as f64 / torrent.total_length as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8,
        )
    } else {
        None
    };
    let eta = if torrent.download_speed > 0 {
        Some(remaining / torrent.download_speed)
    } else {
        None
    };
    let ratio = if torrent.total_length > 0 {
        Some(torrent.upload_length as f64 / torrent.total_length as f64)
    } else {
        None
    };
    let remote_output_path = get_output_path(&torrent);

    PluginDownloadItem {
        client_item_id: id.clone(),
        download_id: None,
        info_hash: hash.clone(),
        title,
        state: map_state(&torrent),
        message: torrent.error_message.clone(),
        category: None,
        remote_output_path: remote_output_path.clone(),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: hash,
            client_native_id: Some(torrent.gid.clone()),
            content_paths: remote_output_path.into_iter().collect(),
            uploaded_bytes: Some(torrent.upload_length),
            downloaded_bytes: Some(torrent.completed_length),
            download_rate_bytes_per_second: Some(torrent.download_speed),
            seed_ratio: ratio,
            metadata_only: Some(false),
            is_encrypted: Some(false),
            raw_status: Some(torrent.status.clone()),
            status_reason: torrent.error_message,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_length),
        remaining_size_bytes: Some(remaining),
        eta_seconds: eta,
        progress_percent,
        can_move_files: Some(false),
        can_remove: Some(torrent.status == "complete"),
        removed: Some(torrent.status == "removed"),
        raw_state: Some(torrent.status),
        completed_at: None,
    }
}

fn torrent_to_completed(torrent: Aria2Status) -> PluginCompletedDownload {
    let path = get_output_path(&torrent).unwrap_or_default();
    let hash = torrent.info_hash.as_deref().map(normalize_hash);
    PluginCompletedDownload {
        client_item_id: hash.clone().unwrap_or_else(|| torrent.gid.clone()),
        download_id: None,
        info_hash: hash,
        name: torrent.bittorrent_name.unwrap_or_default(),
        dest_dir: path.clone(),
        category: None,
        output_kind: Some(if path_looks_like_file(&path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: if path.is_empty() {
            Vec::new()
        } else {
            vec![path]
        },
        size_bytes: Some(torrent.total_length),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn get_output_path(torrent: &Aria2Status) -> Option<String> {
    longest_common_content_path(&torrent.files)
}

fn longest_common_content_path(paths: &[String]) -> Option<String> {
    let paths = paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    if paths.len() == 1 {
        return Some(paths[0].to_string());
    }

    let split_paths = paths
        .iter()
        .map(|path| path.split(['/', '\\']).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let max_common_len = split_paths
        .iter()
        .map(|parts| parts.len().saturating_sub(1))
        .min()
        .unwrap_or_default();
    let mut common_len = 0;
    for index in 0..max_common_len {
        let candidate = split_paths[0][index];
        if split_paths.iter().all(|parts| parts[index] == candidate) {
            common_len += 1;
        } else {
            break;
        }
    }

    if common_len == 0 {
        return None;
    }

    let separator = if paths[0].contains('\\') && !paths[0].contains('/') {
        "\\"
    } else {
        "/"
    };
    let mut common = split_paths[0][..common_len].join(separator);
    if common.is_empty() && (paths[0].starts_with('/') || paths[0].starts_with('\\')) {
        common = separator.to_string();
    }
    Some(common)
}

fn map_state(torrent: &Aria2Status) -> DownloadItemState {
    match torrent.status.as_str() {
        "active" if torrent.completed_length == torrent.total_length => {
            DownloadItemState::Completed
        }
        "active" => DownloadItemState::Downloading,
        "waiting" => DownloadItemState::Queued,
        "paused" => DownloadItemState::Paused,
        "error" => DownloadItemState::Failed,
        "complete" => DownloadItemState::Completed,
        "removed" => DownloadItemState::Completed,
        _ => DownloadItemState::Warning,
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

fn xml_string(value: &str) -> String {
    format!("<string>{}</string>", xml_escape(value))
}

fn xml_int(value: i64) -> String {
    format!("<int>{value}</int>")
}

fn xml_base64(bytes: &[u8]) -> String {
    format!("<base64>{}</base64>", STANDARD.encode(bytes))
}

fn xml_array(values: Vec<String>) -> String {
    format!(
        "<array><data>{}</data></array>",
        values
            .into_iter()
            .map(|value| format!("<value>{value}</value>"))
            .collect::<Vec<_>>()
            .join("")
    )
}

fn xml_struct(values: &[(String, String)]) -> String {
    format!(
        "<struct>{}</struct>",
        values
            .iter()
            .map(|(key, value)| format!(
                "<member><name>{}</name><value>{}</value></member>",
                xml_escape(key),
                xml_string(value)
            ))
            .collect::<Vec<_>>()
            .join("")
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
