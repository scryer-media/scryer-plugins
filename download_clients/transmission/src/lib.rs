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
    ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;

const SESSION_VAR_KEY: &str = "transmission.session_id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostImportAction {
    Retain,
    Remove,
    RemoveWithData,
}

#[derive(Debug, Clone)]
struct TransmissionConfig {
    rpc_url: String,
    username: String,
    password: String,
    category: String,
    imported_category: String,
    directory: String,
    add_paused: bool,
    post_import_action: PostImportAction,
}

#[derive(Debug, Default, Deserialize)]
struct RpcResponse {
    result: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct SessionConfig {
    #[serde(default, rename = "rpc-version")]
    rpc_version: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, rename = "download-dir")]
    download_dir: Option<String>,
    #[serde(default, rename = "seedRatioLimit")]
    seed_ratio_limit: Option<f64>,
    #[serde(default, rename = "seedRatioLimited")]
    seed_ratio_limited: Option<bool>,
    #[serde(default, rename = "idle-seeding-limit-enabled")]
    idle_seeding_limit_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct TransmissionTorrent {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default, rename = "hashString")]
    hash_string: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "downloadDir")]
    download_dir: String,
    #[serde(default, rename = "totalSize")]
    total_size: i64,
    #[serde(default, rename = "leftUntilDone")]
    left_until_done: i64,
    #[serde(default, rename = "isFinished")]
    is_finished: bool,
    #[serde(default)]
    eta: i64,
    #[serde(default)]
    status: i64,
    #[serde(default, rename = "secondsSeeding")]
    seconds_seeding: i64,
    #[serde(default, rename = "errorString")]
    error_string: String,
    #[serde(default, rename = "uploadedEver")]
    uploaded_ever: i64,
    #[serde(default, rename = "downloadedEver")]
    downloaded_ever: i64,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default, rename = "seedRatioLimit")]
    seed_ratio_limit: Option<f64>,
    #[serde(default, rename = "seedRatioMode")]
    seed_ratio_mode: Option<i64>,
    #[serde(default, rename = "seedIdleLimit")]
    seed_idle_limit: Option<i64>,
    #[serde(default, rename = "seedIdleMode")]
    seed_idle_mode: Option<i64>,
    #[serde(default, rename = "file-count")]
    file_count: Option<i64>,
    #[serde(default, rename = "fileCount")]
    vuze_file_count: Option<i64>,
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
        id: "transmission".to_string(),
        name: "Transmission".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "transmission".to_string(),
            provider_aliases: vec!["vuze".to_string(), "azureus".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![
                DownloadIsolationMode::Directory,
                DownloadIsolationMode::Tag,
                DownloadIsolationMode::Category,
            ],
            capabilities: DownloadClientCapabilities {
                pause: true,
                resume: true,
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
                        DownloadIsolationMode::Directory,
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Category,
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
    let config = TransmissionConfig::from_extism()?;
    let mut arguments = serde_json::Map::new();

    if let Some(torrent_bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        arguments.insert(
            "metainfo".to_string(),
            serde_json::Value::String(torrent_bytes_base64.to_string()),
        );
    } else if let Some(source) = source_url(&request) {
        arguments.insert("filename".to_string(), serde_json::Value::String(source));
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    }

    arguments.insert(
        "paused".to_string(),
        serde_json::Value::Bool(request_paused(&config, &request)),
    );
    if let Some(download_dir) = download_directory(&config, &request)? {
        arguments.insert(
            "download-dir".to_string(),
            serde_json::Value::String(download_dir),
        );
    }
    let labels = labels_for_request(&config, &request);
    if !labels.is_empty() {
        arguments.insert("labels".to_string(), serde_json::to_value(labels)?);
    }

    let response = rpc(
        &config,
        "torrent-add",
        Some(serde_json::Value::Object(arguments)),
    )?;
    let added = response
        .arguments
        .get("torrent-added")
        .or_else(|| response.arguments.get("torrent-duplicate"))
        .cloned()
        .unwrap_or_default();
    let hash = added
        .get("hashString")
        .and_then(|value| value.as_str())
        .map(normalize_hash)
        .filter(|value| !value.is_empty())
        .or_else(|| request.release.info_hash_v1.as_deref().map(normalize_hash))
        .or_else(|| {
            request
                .release
                .info_hash_hint
                .as_deref()
                .map(normalize_hash)
        })
        .ok_or_else(|| Error::msg("Transmission did not return an added torrent hash"))?;

    apply_seed_limits(&config, &hash, &request)?;
    if matches!(
        request
            .torrent
            .as_ref()
            .and_then(|torrent| torrent.queue_placement),
        Some(_)
    ) {
        let _ = rpc(
            &config,
            "queue-move-top",
            Some(serde_json::json!({ "ids": [hash.clone()] })),
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
    let config = TransmissionConfig::from_extism()?;
    let session = session_get(&config)?;
    let torrents = list_torrents(&config)?;
    let items = torrents
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&config, &session, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    let session = session_get(&config)?;
    let torrents = list_torrents(&config)?;
    let items = torrents
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&config, &session, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .filter(is_completed)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = TransmissionConfig::from_extism()?;
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ))?);
    }

    match request.action {
        DownloadControlAction::Pause => {
            rpc(
                &config,
                "torrent-stop",
                Some(serde_json::json!({ "ids": [hash] })),
            )?;
        }
        DownloadControlAction::Resume => {
            rpc(
                &config,
                "torrent-start",
                Some(serde_json::json!({ "ids": [hash] })),
            )?;
        }
        DownloadControlAction::Remove => {
            rpc(
                &config,
                "torrent-remove",
                Some(serde_json::json!({
                    "ids": [hash],
                    "delete-local-data": request.remove_data
                })),
            )?;
        }
        DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Transmission does not support force_start through this plugin",
            ))?);
        }
    }

    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = TransmissionConfig::from_extism()?;
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if hash.is_empty() {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ))?);
    }

    if !config.imported_category.is_empty() {
        let mut labels = torrent_labels(&config, &hash)?;
        labels.retain(|label| {
            config.category.is_empty() || !label.eq_ignore_ascii_case(&config.category)
        });
        labels.push(config.imported_category.clone());
        labels.sort_by_key(|label| label.to_ascii_lowercase());
        labels.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        rpc(
            &config,
            "torrent-set",
            Some(serde_json::json!({ "ids": [hash.clone()], "labels": labels })),
        )?;
    }

    match config.post_import_action {
        PostImportAction::Retain => {}
        PostImportAction::Remove => {
            rpc(
                &config,
                "torrent-remove",
                Some(serde_json::json!({ "ids": [hash], "delete-local-data": false })),
            )?;
        }
        PostImportAction::RemoveWithData => {
            rpc(
                &config,
                "torrent-remove",
                Some(serde_json::json!({ "ids": [hash], "delete-local-data": true })),
            )?;
        }
    }

    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    let session = session_get(&config)?;
    let roots = effective_output_root(&config, &session)
        .into_iter()
        .collect();

    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: session.version.or(session.rpc_version),
            is_localhost: Some(is_localhost_url(&config.rpc_url)),
            remote_output_roots: roots,
            removes_completed_downloads: Some(!matches!(
                config.post_import_action,
                PostImportAction::Retain
            )),
            sorting_mode: Some("transmission-rpc".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    var::remove(SESSION_VAR_KEY)?;
    let session = session_get(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        session.version.unwrap_or_else(|| "ok".to_string()),
    ))?)
}

impl TransmissionConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "9091".to_string());
        let use_ssl = config_bool("use_ssl", false);
        let url_base = config_value("url_base").unwrap_or_else(|| "/transmission/".to_string());
        let category = config_value("category").unwrap_or_else(|| "tv-sonarr".to_string());
        let scheme = if use_ssl { "https" } else { "http" };
        let rpc_url = format!(
            "{scheme}://{host}:{port}/{}/rpc",
            url_base.trim_matches('/')
        )
        .replace("//rpc", "/rpc");

        Ok(Self {
            rpc_url,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category,
            imported_category: config_value("post_import_category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
            add_paused: config_bool("add_paused", false),
            post_import_action: match config_value("post_import_action")
                .unwrap_or_else(|| "retain".to_string())
                .as_str()
            {
                "remove" => PostImportAction::Remove,
                "remove_with_data" => PostImportAction::RemoveWithData,
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
            Some("9091"),
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
        connection_field(
            "url_base",
            "URL Base",
            false,
            Some("/transmission/"),
            Some("Transmission RPC URL base"),
        ),
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
            false,
            Some("tv-sonarr"),
            Some("Transmission label/category used by Sonarr"),
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            Some("Label applied after Scryer imports the download"),
        ),
        field(
            "directory",
            "Directory",
            ConfigFieldType::String,
            false,
            None,
            Some("Optional download directory"),
        ),
        field(
            "add_paused",
            "Add Paused",
            ConfigFieldType::Bool,
            false,
            Some("false"),
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
                "What Scryer should do in Transmission after a successful import".to_string(),
            ),
        },
    ]
}

fn rpc(
    config: &TransmissionConfig,
    method: &str,
    arguments: Option<serde_json::Value>,
) -> Result<RpcResponse, Error> {
    let body = match arguments {
        Some(arguments) => serde_json::json!({ "method": method, "arguments": arguments }),
        None => serde_json::json!({ "method": method }),
    };
    let mut response = rpc_once(config, &body, cached_session_id()?);
    if response
        .as_ref()
        .is_ok_and(|response| response.status_code() == 409)
    {
        let session_id = response
            .as_ref()
            .ok()
            .and_then(extract_session_id)
            .ok_or_else(|| Error::msg("Transmission did not return a session id"))?;
        var::set(SESSION_VAR_KEY, session_id.clone())?;
        response = rpc_once(config, &body, Some(session_id));
    }
    let response = response?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 {
        return Err(Error::msg("Transmission user authentication failed"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "Transmission RPC returned HTTP {status}: {body_text}"
        )));
    }
    let parsed: RpcResponse = serde_json::from_str(&body_text)
        .map_err(|error| Error::msg(format!("Transmission response parse failed: {error}")))?;
    if parsed.result != "success" {
        return Err(Error::msg(format!(
            "Transmission RPC failed: {}",
            parsed.result
        )));
    }
    Ok(parsed)
}

fn rpc_once(
    config: &TransmissionConfig,
    body: &serde_json::Value,
    session_id: Option<String>,
) -> Result<HttpResponse, Error> {
    let mut request = HttpRequest::new(&config.rpc_url)
        .with_method("POST")
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "scryer-transmission-plugin/0.1");
    if !config.username.is_empty() || !config.password.is_empty() {
        request = request.with_header(
            "Authorization",
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", config.username, config.password))
            ),
        );
    }
    if let Some(session_id) = session_id {
        request = request.with_header("X-Transmission-Session-Id", session_id);
    }
    http::request::<Vec<u8>>(&request, Some(serde_json::to_vec(body)?))
        .map_err(|error| Error::msg(format!("Transmission RPC request failed: {error}")))
}

fn cached_session_id() -> Result<Option<String>, Error> {
    Ok(var::get(SESSION_VAR_KEY)?
        .map(|value: String| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn extract_session_id(response: &HttpResponse) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("X-Transmission-Session-Id"))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn session_get(config: &TransmissionConfig) -> Result<SessionConfig, Error> {
    let response = rpc(config, "session-get", None)?;
    serde_json::from_value(response.arguments)
        .map_err(|error| Error::msg(format!("Transmission session parse failed: {error}")))
}

fn list_torrents(config: &TransmissionConfig) -> Result<Vec<TransmissionTorrent>, Error> {
    let fields = vec![
        "id",
        "hashString",
        "name",
        "downloadDir",
        "totalSize",
        "leftUntilDone",
        "isFinished",
        "eta",
        "status",
        "secondsSeeding",
        "errorString",
        "uploadedEver",
        "downloadedEver",
        "seedRatioLimit",
        "seedRatioMode",
        "seedIdleLimit",
        "seedIdleMode",
        "fileCount",
        "file-count",
        "labels",
    ];
    let response = rpc(
        config,
        "torrent-get",
        Some(serde_json::json!({ "fields": fields })),
    )?;
    serde_json::from_value(
        response
            .arguments
            .get("torrents")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
    )
    .map_err(|error| Error::msg(format!("Transmission torrent parse failed: {error}")))
}

fn torrent_labels(config: &TransmissionConfig, hash: &str) -> Result<Vec<String>, Error> {
    let response = rpc(
        config,
        "torrent-get",
        Some(serde_json::json!({
            "fields": ["labels"],
            "ids": [hash],
        })),
    )?;
    let torrent = response
        .arguments
        .get("torrents")
        .and_then(|value| value.as_array())
        .and_then(|torrents| torrents.first())
        .cloned()
        .ok_or_else(|| Error::msg("Transmission did not return the imported torrent"))?;
    let torrent: TransmissionTorrent = serde_json::from_value(torrent)
        .map_err(|error| Error::msg(format!("Transmission torrent parse failed: {error}")))?;
    Ok(torrent.labels)
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

fn download_directory(
    config: &TransmissionConfig,
    request: &PluginDownloadClientAddRequest,
) -> Result<Option<String>, Error> {
    if let Some(directory) = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(directory));
    }

    if !config.directory.is_empty() {
        return Ok(Some(config.directory.clone()));
    }

    let session = session_get(config)?;
    Ok(effective_output_root(config, &session))
}

fn effective_output_root(config: &TransmissionConfig, session: &SessionConfig) -> Option<String> {
    if !config.directory.is_empty() {
        return Some(config.directory.clone());
    }

    let root = session
        .download_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    if config.category.is_empty() {
        Some(root.to_string())
    } else {
        Some(format!(
            "{}/{}",
            root.trim_end_matches('/'),
            config.category.trim_matches('/')
        ))
    }
}

fn labels_for_request(
    config: &TransmissionConfig,
    request: &PluginDownloadClientAddRequest,
) -> Vec<String> {
    let mut labels = Vec::new();
    if !config.category.is_empty() {
        labels.push(config.category.clone());
    }
    if let Some(value) = request.routing.isolation_value.as_deref() {
        if !value.trim().is_empty() {
            labels.push(value.trim().to_string());
        }
    }
    labels.sort();
    labels.dedup();
    labels
}

fn request_paused(config: &TransmissionConfig, request: &PluginDownloadClientAddRequest) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.initial_state)
        .is_some_and(|state| state == PluginTorrentInitialState::Paused)
        || config.add_paused
}

fn apply_seed_limits(
    config: &TransmissionConfig,
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), Error> {
    let ratio = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio);
    let seed_minutes = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_seconds)
        .or(request.release.seed_goal_seconds)
        .map(|seconds| (seconds / 60).max(0));

    let mut args = serde_json::Map::new();
    args.insert("ids".to_string(), serde_json::json!([hash]));
    if let Some(ratio) = ratio {
        args.insert("seedRatioLimit".to_string(), serde_json::json!(ratio));
        args.insert("seedRatioMode".to_string(), serde_json::json!(1));
    }
    if let Some(minutes) = seed_minutes {
        args.insert("seedIdleLimit".to_string(), serde_json::json!(minutes));
        args.insert("seedIdleMode".to_string(), serde_json::json!(1));
    }
    if args.len() > 1 {
        rpc(config, "torrent-set", Some(serde_json::Value::Object(args)))?;
    }
    Ok(())
}

fn torrent_to_item(
    config: &TransmissionConfig,
    session: &SessionConfig,
    torrent: TransmissionTorrent,
) -> PluginDownloadItem {
    let hash = normalize_hash(&torrent.hash_string);
    let state = map_state(&torrent);
    let remote_output_path = output_path(&torrent);
    let progress_percent = if torrent.total_size > 0 {
        Some(
            (((torrent.total_size - torrent.left_until_done).max(0) as f64
                / torrent.total_size as f64)
                * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8,
        )
    } else {
        None
    };
    let ratio = if torrent.downloaded_ever > 0 {
        Some(torrent.uploaded_ever as f64 / torrent.downloaded_ever as f64)
    } else {
        None
    };
    let can_remove = can_remove(config, session, &torrent, state, ratio);

    PluginDownloadItem {
        client_item_id: hash.clone(),
        info_hash: Some(hash.clone()),
        title: torrent.name.clone(),
        state,
        message: if torrent.error_string.trim().is_empty() {
            None
        } else {
            Some(torrent.error_string.clone())
        },
        category: torrent.labels.first().cloned(),
        remote_output_path: Some(remote_output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            client_native_id: torrent.id.map(|id| id.to_string()),
            labels: torrent.labels.clone(),
            save_path: Some(torrent.download_dir.clone()),
            content_paths: vec![remote_output_path],
            uploaded_bytes: Some(torrent.uploaded_ever),
            downloaded_bytes: Some(torrent.downloaded_ever),
            seed_ratio: ratio,
            seed_time_seconds: Some(torrent.seconds_seeding),
            raw_status: Some(torrent.status.to_string()),
            status_reason: if torrent.error_string.trim().is_empty() {
                None
            } else {
                Some(torrent.error_string.clone())
            },
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.total_size),
        remaining_size_bytes: Some(torrent.left_until_done),
        eta_seconds: (torrent.eta >= 0).then_some(torrent.eta),
        progress_percent,
        can_move_files: Some(can_remove && torrent.status == 0),
        can_remove: Some(can_remove),
        removed: Some(false),
        raw_state: Some(torrent.status.to_string()),
        completed_at: None,
    }
}

fn torrent_to_completed(torrent: TransmissionTorrent) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.hash_string);
    let path = output_path(&torrent);
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
        name: torrent.name.clone(),
        dest_dir: path.clone(),
        category: torrent.labels.first().cloned(),
        output_kind: Some(if torrent_file_count(&torrent) > 1 {
            PluginDownloadOutputKind::Directory
        } else {
            PluginDownloadOutputKind::Unknown
        }),
        content_paths: vec![path],
        size_bytes: Some(torrent.total_size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn output_path(torrent: &TransmissionTorrent) -> String {
    format!(
        "{}/{}",
        torrent.download_dir.trim_end_matches('/'),
        torrent.name.replace(':', "_")
    )
}

fn torrent_file_count(torrent: &TransmissionTorrent) -> i64 {
    torrent
        .file_count
        .or(torrent.vuze_file_count)
        .unwrap_or_default()
}

fn map_state(torrent: &TransmissionTorrent) -> DownloadItemState {
    if !torrent.error_string.trim().is_empty() {
        return DownloadItemState::Warning;
    }
    if is_completed(torrent) {
        return DownloadItemState::Completed;
    }
    match torrent.status {
        0 => DownloadItemState::Paused,
        1 | 2 => DownloadItemState::Verifying,
        3 | 5 => DownloadItemState::Queued,
        4 => DownloadItemState::Downloading,
        6 => DownloadItemState::Completed,
        _ => DownloadItemState::Warning,
    }
}

fn is_completed(torrent: &TransmissionTorrent) -> bool {
    torrent.left_until_done == 0 && matches!(torrent.status, 0 | 5 | 6)
        || torrent.is_finished && !matches!(torrent.status, 1 | 2)
}

fn can_remove(
    config: &TransmissionConfig,
    session: &SessionConfig,
    torrent: &TransmissionTorrent,
    state: DownloadItemState,
    ratio: Option<f64>,
) -> bool {
    if matches!(config.post_import_action, PostImportAction::Retain)
        || state != DownloadItemState::Completed
    {
        return false;
    }

    has_reached_seed_limit(session, torrent, ratio)
}

fn has_reached_seed_limit(
    session: &SessionConfig,
    torrent: &TransmissionTorrent,
    ratio: Option<f64>,
) -> bool {
    let is_stopped = torrent.status == 0;
    let is_seeding = torrent.status == 6;

    match torrent.seed_ratio_mode.unwrap_or_default() {
        1 => {
            if is_stopped
                && ratio.is_some_and(|ratio| {
                    torrent.seed_ratio_limit.is_some_and(|limit| ratio >= limit)
                })
            {
                return true;
            }
        }
        0 => {
            if is_stopped
                && session.seed_ratio_limited.unwrap_or(false)
                && ratio.is_some_and(|ratio| {
                    session.seed_ratio_limit.is_some_and(|limit| ratio >= limit)
                })
            {
                return true;
            }
        }
        _ => {}
    }

    match torrent.seed_idle_mode.unwrap_or_default() {
        1 => {
            if (is_stopped || is_seeding)
                && torrent
                    .seed_idle_limit
                    .is_some_and(|limit| torrent.seconds_seeding > limit * 60)
            {
                return true;
            }
        }
        0 => {
            if is_stopped && session.idle_seeding_limit_enabled.unwrap_or(false) {
                return true;
            }
        }
        _ => {}
    }

    false
}

fn torrent_matches_scope(config: &TransmissionConfig, torrent: &TransmissionTorrent) -> bool {
    if !config.category.is_empty() && !torrent.labels.is_empty() {
        return torrent
            .labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(&config.category));
    }

    if !config.directory.is_empty() {
        return path_is_or_under(&torrent.download_dir, &config.directory);
    }

    if !config.category.is_empty() {
        return torrent
            .download_dir
            .split(['/', '\\'])
            .any(|part| part.eq_ignore_ascii_case(&config.category));
    }

    true
}

fn path_is_or_under(path: &str, root: &str) -> bool {
    let path = path.trim_end_matches(|ch| ch == '/' || ch == '\\');
    let root = root.trim_end_matches(|ch| ch == '/' || ch == '\\');
    path.eq_ignore_ascii_case(root)
        || path
            .get(root.len()..)
            .is_some_and(|suffix| suffix.starts_with('/') || suffix.starts_with('\\'))
}

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
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
