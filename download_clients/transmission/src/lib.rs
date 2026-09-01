use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
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
    recent_priority: PluginTorrentQueuePlacement,
    older_priority: PluginTorrentQueuePlacement,
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
    #[serde(default, rename = "idle-seeding-limit")]
    idle_seeding_limit: Option<i64>,
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
    /// `torrent-get` reports `isPrivate` on every supported Transmission release; absent only
    /// when the field was not requested, in which case it must stay `None`.
    #[serde(default, rename = "isPrivate")]
    is_private: Option<bool>,
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
    if should_move_to_top(&config, &request) {
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

fn should_move_to_top(
    config: &TransmissionConfig,
    request: &PluginDownloadClientAddRequest,
) -> bool {
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

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    let session = session_get(&config)?;
    let torrents = list_torrents(&config)?;
    let items = torrents
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&session, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = TransmissionConfig::from_extism()?;
    let session = session_get(&config)?;
    let torrents = list_torrents(&config)?;
    let items = torrents
        .into_iter()
        .filter(|torrent| torrent_matches_scope(&config, torrent))
        .map(|torrent| torrent_to_item(&session, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

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
        let category = config_value("category").unwrap_or_else(|| "scryer-tv".to_string());
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
            recent_priority: queue_placement_config("recent_priority"),
            older_priority: queue_placement_config("older_priority"),
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
            Some("scryer-tv"),
            Some("Transmission label/category used by Scryer"),
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            Some("Label applied after Scryer imports the download"),
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
            "directory",
            "Directory",
            ConfigFieldType::Path,
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
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "remove".to_string(),
                    label: "Remove Torrent".to_string(),
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "remove_with_data".to_string(),
                    label: "Remove With Data".to_string(),
                    config_overrides: Default::default(),
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
        "isPrivate",
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
    if let Some(value) = request.routing.isolation_value.as_deref()
        && !value.trim().is_empty()
    {
        labels.push(value.trim().to_string());
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

fn torrent_to_item(session: &SessionConfig, torrent: TransmissionTorrent) -> PluginDownloadItem {
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
    let can_remove = derive_can_remove(session, &torrent, state, ratio);

    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
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
            is_private: torrent.is_private,
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
        // Data completeness only: whether moving is *safe* while seeding is a Scryer-side
        // policy decision that combines this with the resolved seeding goal.
        can_move_files: Some(state == DownloadItemState::Completed),
        can_remove,
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
        download_id: None,
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
        release_name: None,
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
    if torrent.total_size == 0 {
        return DownloadItemState::Queued;
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
        // A status code outside the documented 0..=6 range is a newer
        // Transmission, not a fault: Transmission reports real faults through
        // `errorString`, which is handled above. Keep polling rather than
        // parking the row in a state nothing ever clears, matching Sonarr's
        // final `else { Downloading }`
        // (Download/Clients/Transmission/TransmissionBase.cs:130-133).
        _ => DownloadItemState::Downloading,
    }
}

fn is_completed(torrent: &TransmissionTorrent) -> bool {
    if torrent.total_size == 0 {
        return false;
    }
    torrent.left_until_done == 0 && matches!(torrent.status, 0 | 5 | 6)
        || torrent.is_finished && !matches!(torrent.status, 1 | 2)
}

/// Whether Transmission's own seeding limits are satisfied for a torrent.
///
/// `Unknown` means Transmission enforces no limit for this torrent (mode 2 "unlimited", or
/// mode 0 with the matching global toggle off), so the plugin must report `can_remove: None`
/// and let Scryer-side goal evaluation decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeedLimitState {
    Met,
    Unmet,
    Unknown,
}

fn combine_seed_limit_states(states: &[SeedLimitState]) -> SeedLimitState {
    if states.contains(&SeedLimitState::Met) {
        SeedLimitState::Met
    } else if states.contains(&SeedLimitState::Unmet) {
        SeedLimitState::Unmet
    } else {
        SeedLimitState::Unknown
    }
}

fn compare_limit(current: Option<f64>, limit: Option<f64>) -> SeedLimitState {
    match (current, limit) {
        (Some(current), Some(limit)) if current >= limit => SeedLimitState::Met,
        (Some(_), Some(_)) => SeedLimitState::Unmet,
        _ => SeedLimitState::Unknown,
    }
}

fn ratio_limit_state(
    session: &SessionConfig,
    torrent: &TransmissionTorrent,
    ratio: Option<f64>,
) -> SeedLimitState {
    match torrent.seed_ratio_mode.unwrap_or_default() {
        // 1 = honour the per-torrent limit.
        1 => compare_limit(ratio, torrent.seed_ratio_limit),
        // 0 = follow the session default, which may be switched off entirely.
        0 if session.seed_ratio_limited.unwrap_or(false) => {
            compare_limit(ratio, session.seed_ratio_limit)
        }
        // 2 = seed regardless of ratio, or the global limit is disabled.
        _ => SeedLimitState::Unknown,
    }
}

/// Transmission has no total-seed-time limit; Sonarr (and this plugin's add path) map the
/// seed-time goal onto the *idle* limit, so the same field is what we read back.
fn idle_limit_state(session: &SessionConfig, torrent: &TransmissionTorrent) -> SeedLimitState {
    let is_stopped = torrent.status == 0;
    let is_seeding = torrent.status == 6;
    match torrent.seed_idle_mode.unwrap_or_default() {
        1 => match torrent.seed_idle_limit {
            Some(limit) if (is_stopped || is_seeding) && torrent.seconds_seeding > limit * 60 => {
                SeedLimitState::Met
            }
            Some(_) => SeedLimitState::Unmet,
            None => SeedLimitState::Unknown,
        },
        // Follow the session default. A stopped torrent alone is NOT proof the limit was
        // reached (the user may have stopped it), so compare against the session's idle
        // value the same way the per-torrent mode does; without the value the verdict is
        // unverifiable.
        0 if session.idle_seeding_limit_enabled.unwrap_or(false) => {
            match session.idle_seeding_limit {
                Some(limit)
                    if (is_stopped || is_seeding) && torrent.seconds_seeding > limit * 60 =>
                {
                    SeedLimitState::Met
                }
                Some(_) => SeedLimitState::Unmet,
                None => SeedLimitState::Unknown,
            }
        }
        _ => SeedLimitState::Unknown,
    }
}

fn seed_limit_state(
    session: &SessionConfig,
    torrent: &TransmissionTorrent,
    ratio: Option<f64>,
) -> SeedLimitState {
    combine_seed_limit_states(&[
        ratio_limit_state(session, torrent, ratio),
        idle_limit_state(session, torrent),
    ])
}

/// Honest `can_remove`: `Some(true)` only when Transmission stopped the torrent *and* one of
/// its own limits is satisfied, `Some(false)` when a limit exists and is unmet (or the data
/// is not complete), `None` when Transmission enforces nothing here.
fn derive_can_remove(
    session: &SessionConfig,
    torrent: &TransmissionTorrent,
    state: DownloadItemState,
    ratio: Option<f64>,
) -> Option<bool> {
    if state != DownloadItemState::Completed {
        return Some(false);
    }
    match seed_limit_state(session, torrent, ratio) {
        SeedLimitState::Met if torrent.status == 0 => Some(true),
        // Limit satisfied but Transmission has not stopped the torrent yet.
        SeedLimitState::Met => None,
        SeedLimitState::Unmet => Some(false),
        SeedLimitState::Unknown => None,
    }
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
    let path = path.trim_end_matches(['/', '\\']);
    let root = root.trim_end_matches(['/', '\\']);
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
                config_overrides: Default::default(),
            },
            ConfigFieldOption {
                value: "first".to_string(),
                label: "First".to_string(),
                config_overrides: Default::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS_STOPPED: i64 = 0;
    const STATUS_SEEDING: i64 = 6;
    const STATUS_DOWNLOADING: i64 = 4;

    fn complete_torrent(status: i64) -> TransmissionTorrent {
        TransmissionTorrent {
            hash_string: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            total_size: 1_000,
            left_until_done: 0,
            is_finished: true,
            status,
            downloaded_ever: 1_000,
            uploaded_ever: 500,
            ..TransmissionTorrent::default()
        }
    }

    fn item(session: &SessionConfig, torrent: TransmissionTorrent) -> PluginDownloadItem {
        torrent_to_item(session, torrent)
    }

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = TransmissionTorrent {
            total_size: 1_000,
            left_until_done: 400,
            is_finished: false,
            status: STATUS_DOWNLOADING,
            ..TransmissionTorrent::default()
        };
        let item = item(&SessionConfig::default(), torrent);
        assert_eq!(item.can_remove, Some(false));
        assert_eq!(item.can_move_files, Some(false));
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_per_torrent_ratio() {
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(1),
            seed_ratio_limit: Some(2.0),
            uploaded_ever: 500,
            ..complete_torrent(STATUS_SEEDING)
        };
        assert_eq!(
            derive_can_remove(
                &SessionConfig::default(),
                &torrent,
                DownloadItemState::Completed,
                Some(0.5)
            ),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_when_stopped_with_a_met_per_torrent_ratio() {
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(1),
            seed_ratio_limit: Some(1.0),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(
                &SessionConfig::default(),
                &torrent,
                DownloadItemState::Completed,
                Some(1.5)
            ),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_unknown_when_the_torrent_seeds_without_a_limit() {
        // seedRatioMode 2 = seed regardless of ratio; the global idle limit is off.
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(2),
            seed_idle_mode: Some(2),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(
                &SessionConfig::default(),
                &torrent,
                DownloadItemState::Completed,
                Some(9.0)
            ),
            None
        );
    }

    #[test]
    fn can_remove_is_unknown_when_the_global_ratio_limit_is_disabled() {
        let session = SessionConfig {
            seed_ratio_limited: Some(false),
            seed_ratio_limit: Some(2.0),
            ..SessionConfig::default()
        };
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(0),
            seed_idle_mode: Some(0),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(&session, &torrent, DownloadItemState::Completed, Some(9.0)),
            None
        );
    }

    #[test]
    fn can_remove_follows_the_session_ratio_limit_in_global_mode() {
        let session = SessionConfig {
            seed_ratio_limited: Some(true),
            seed_ratio_limit: Some(1.0),
            ..SessionConfig::default()
        };
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(0),
            seed_idle_mode: Some(2),
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(&session, &torrent, DownloadItemState::Completed, Some(1.2)),
            Some(true)
        );
        assert_eq!(
            derive_can_remove(&session, &torrent, DownloadItemState::Completed, Some(0.2)),
            Some(false)
        );
    }

    #[test]
    fn per_torrent_idle_limit_doubles_as_the_seed_time_limit() {
        let met = TransmissionTorrent {
            seed_ratio_mode: Some(2),
            seed_idle_mode: Some(1),
            seed_idle_limit: Some(30),
            seconds_seeding: 3_600,
            ..complete_torrent(STATUS_SEEDING)
        };
        let unmet = TransmissionTorrent {
            seconds_seeding: 60,
            ..complete_torrent(STATUS_SEEDING)
        };
        let unmet = TransmissionTorrent {
            seed_ratio_mode: Some(2),
            seed_idle_mode: Some(1),
            seed_idle_limit: Some(30),
            ..unmet
        };
        assert_eq!(
            idle_limit_state(&SessionConfig::default(), &met),
            SeedLimitState::Met
        );
        assert_eq!(
            idle_limit_state(&SessionConfig::default(), &unmet),
            SeedLimitState::Unmet
        );
    }

    #[test]
    fn met_limit_without_transmission_stopping_the_torrent_is_unknown() {
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(1),
            seed_ratio_limit: Some(1.0),
            ..complete_torrent(STATUS_SEEDING)
        };
        assert_eq!(
            derive_can_remove(
                &SessionConfig::default(),
                &torrent,
                DownloadItemState::Completed,
                Some(4.0)
            ),
            None
        );
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(1),
            seed_ratio_limit: Some(9.0),
            ..complete_torrent(STATUS_SEEDING)
        };
        let item = item(&SessionConfig::default(), torrent);
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, Some(false));
    }

    #[test]
    fn is_private_maps_present_true_present_false_and_absent() {
        let map = |raw: &str| {
            let torrent: TransmissionTorrent = serde_json::from_str(raw).unwrap();
            item(&SessionConfig::default(), torrent)
                .torrent
                .unwrap()
                .is_private
        };
        assert_eq!(
            map(r#"{"hashString":"a1","name":"n","isPrivate":true}"#),
            Some(true)
        );
        assert_eq!(
            map(r#"{"hashString":"a1","name":"n","isPrivate":false}"#),
            Some(false)
        );
        assert_eq!(map(r#"{"hashString":"a1","name":"n"}"#), None);
    }

    #[test]
    fn an_undocumented_status_code_keeps_polling_instead_of_warning() {
        // Transmission reports real faults through `errorString`; a status code
        // outside 0..=6 is a newer Transmission, not a failure, and must not
        // park the row in a state nothing clears.
        let torrent = TransmissionTorrent {
            total_size: 1_000,
            left_until_done: 400,
            is_finished: false,
            status: 42,
            ..TransmissionTorrent::default()
        };
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);

        // An error string still wins, whatever the status code.
        let errored = TransmissionTorrent {
            error_string: "No data found!".to_string(),
            ..torrent
        };
        assert_eq!(map_state(&errored), DownloadItemState::Warning);
    }

    #[test]
    fn observed_seed_state_comes_from_the_torrent_get_payload() {
        let torrent: TransmissionTorrent = serde_json::from_str(
            r#"{"hashString":"a1","name":"n","secondsSeeding":7200,"uploadedEver":300,"downloadedEver":200}"#,
        )
        .unwrap();
        let torrent = item(&SessionConfig::default(), torrent).torrent.unwrap();
        assert_eq!(torrent.seed_time_seconds, Some(7_200));
        assert_eq!(torrent.seed_ratio, Some(1.5));
    }

    #[test]
    fn global_idle_mode_compares_against_the_session_idle_value() {
        let session = SessionConfig {
            idle_seeding_limit_enabled: Some(true),
            idle_seeding_limit: Some(30),
            ..SessionConfig::default()
        };
        // A user-stopped torrent below the session idle limit must not read as limit-met.
        let unmet = TransmissionTorrent {
            seed_ratio_mode: Some(2),
            seed_idle_mode: Some(0),
            seconds_seeding: 60,
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(&session, &unmet, DownloadItemState::Completed, Some(9.0)),
            Some(false)
        );
        let met = TransmissionTorrent {
            seconds_seeding: 3_600,
            ..unmet
        };
        assert_eq!(
            derive_can_remove(&session, &met, DownloadItemState::Completed, Some(9.0)),
            Some(true)
        );
    }

    #[test]
    fn global_idle_mode_without_a_session_value_is_unknown() {
        let session = SessionConfig {
            idle_seeding_limit_enabled: Some(true),
            ..SessionConfig::default()
        };
        let torrent = TransmissionTorrent {
            seed_ratio_mode: Some(2),
            seed_idle_mode: Some(0),
            seconds_seeding: 3_600,
            ..complete_torrent(STATUS_STOPPED)
        };
        assert_eq!(
            derive_can_remove(&session, &torrent, DownloadItemState::Completed, Some(9.0)),
            None
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
