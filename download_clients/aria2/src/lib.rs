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
        details: None,
    })
}

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

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(is_visible_download)
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

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

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = Aria2Config::from_extism()?;
    let version = get_version(&config)?;
    if version_lt(&version, "1.34.0") {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!("Aria2 {version} is older than Scryer's required 1.34.0"),
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
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "remove".to_string(),
                    label: "Remove Result".to_string(),
                    config_overrides: Default::default(),
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
    let ratio = observed_ratio(&torrent);
    let can_remove = derive_can_remove(&torrent);
    let can_move_files = Some(is_data_complete(&torrent));
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
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files,
        can_remove,
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
        release_name: None,
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

/// Whether the payload is fully downloaded and therefore movable.
fn is_data_complete(torrent: &Aria2Status) -> bool {
    torrent.status == "complete"
        || (torrent.total_length > 0 && torrent.completed_length >= torrent.total_length)
}

/// Honest `can_remove` for aria2.
///
/// aria2 keeps a BitTorrent download `active` while it is seeding and only moves it to
/// `complete` once seeding has stopped (`--seed-ratio` / `--seed-time` reached, or seeding
/// disabled). Those option values are not part of `tellStatus`, so the seeding goal of a
/// still-active torrent is unknowable here and Scryer-side evaluation decides.
fn derive_can_remove(torrent: &Aria2Status) -> Option<bool> {
    match torrent.status.as_str() {
        "complete" => Some(true),
        _ if is_data_complete(torrent) => None,
        _ => Some(false),
    }
}

/// Observed share ratio: uploaded over what has actually been downloaded so far.
fn observed_ratio(torrent: &Aria2Status) -> Option<f64> {
    (torrent.completed_length > 0)
        .then(|| torrent.upload_length as f64 / torrent.completed_length as f64)
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
        // aria2 only ever reports the six statuses above, so this arm is for a
        // future aria2 that grows one. An unrecognised status is not evidence
        // of a failure, and a Warning here would be a queue row nothing ever
        // clears; keep polling instead. (Sonarr leaves its pre-initialised
        // `DownloadItemStatus.Failed` in place for the same case,
        // Download/Clients/Aria2/Aria2.cs:95 — the harsher choice, and the one
        // Scryer's failed-download cleanup makes destructive.)
        _ => DownloadItemState::Downloading,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: &str, completed: i64) -> Aria2Status {
        Aria2Status {
            bittorrent_name: Some("Movie".to_string()),
            info_hash: Some("abcdef0123456789abcdef0123456789abcdef01".to_string()),
            completed_length: completed,
            download_speed: 0,
            files: vec!["/downloads/Movie/Movie.mkv".to_string()],
            gid: "2089b05ecca3d829".to_string(),
            status: status.to_string(),
            total_length: 1_000,
            upload_length: 2_000,
            error_message: None,
        }
    }

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = status("active", 400);
        assert_eq!(derive_can_remove(&torrent), Some(false));
        assert!(!is_data_complete(&torrent));
    }

    #[test]
    fn can_remove_is_unknown_while_aria2_is_still_seeding() {
        // aria2 leaves a fully downloaded torrent `active` while it seeds, and the
        // seed-ratio/seed-time options are not part of tellStatus.
        let torrent = status("active", 1_000);
        assert_eq!(derive_can_remove(&torrent), None);
    }

    #[test]
    fn can_remove_is_true_once_aria2_reports_complete() {
        assert_eq!(derive_can_remove(&status("complete", 1_000)), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_for_a_paused_complete_download() {
        assert_eq!(derive_can_remove(&status("paused", 1_000)), None);
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let item = torrent_to_item(status("active", 1_000));
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, None);

        let downloading = torrent_to_item(status("active", 400));
        assert_eq!(downloading.can_move_files, Some(false));
    }

    #[test]
    fn observed_ratio_uses_completed_length() {
        assert_eq!(observed_ratio(&status("active", 1_000)), Some(2.0));
        assert_eq!(observed_ratio(&status("active", 0)), None);
    }

    #[test]
    fn is_private_is_never_claimed_because_aria2_does_not_report_it() {
        let item = torrent_to_item(status("complete", 1_000));
        assert_eq!(item.torrent.unwrap().is_private, None);
    }

    #[test]
    fn an_unrecognised_status_keeps_polling_instead_of_warning_or_failing() {
        // A status aria2 does not document today must not park the row in a
        // state nothing clears, nor trip Scryer's failed-download cleanup.
        assert_eq!(
            map_state(&status("somethingNew", 400)),
            DownloadItemState::Downloading
        );
        // The documented statuses keep their meaning.
        assert_eq!(map_state(&status("error", 400)), DownloadItemState::Failed);
        assert_eq!(map_state(&status("paused", 400)), DownloadItemState::Paused);
        assert_eq!(
            map_state(&status("complete", 1_000)),
            DownloadItemState::Completed
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
