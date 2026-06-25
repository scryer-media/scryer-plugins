use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, DownloadClientCapabilities, DownloadClientDescriptor,
    DownloadControlAction, DownloadInputKind, DownloadIsolationMode, DownloadItemState,
    DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddResponse, PluginDownloadClientControlRequest,
    PluginDownloadClientMarkImportedRequest, PluginDownloadClientStatus, PluginDownloadItem,
    PluginDownloadOutputKind, PluginError, PluginErrorCode, PluginResult, PluginTorrentItem,
    ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha1::{Digest, Sha1};
use std::collections::HashMap;

const SID_VAR_KEY: &str = "downloadstation.sid";

#[derive(Debug, Clone)]
struct DsConfig {
    base_url: String,
    host: String,
    username: String,
    password: String,
    category: String,
    directory: String,
}

#[derive(Default, Deserialize)]
struct DownloadStationAddRequest {
    #[serde(default)]
    source: DownloadStationSource,
    #[serde(default)]
    release: DownloadStationRelease,
}

#[derive(Default, Deserialize)]
struct DownloadStationSource {
    #[serde(default)]
    kind: Option<DownloadInputKind>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    magnet_uri: Option<String>,
    #[serde(default)]
    torrent_bytes_base64: Option<String>,
    #[serde(default)]
    torrent_url: Option<String>,
    #[serde(default)]
    torrent_file_name: Option<String>,
    #[serde(default)]
    nzb_bytes_base64: Option<String>,
    #[serde(default)]
    nzb_url: Option<String>,
    #[serde(default)]
    nzb_file_name: Option<String>,
}

#[derive(Default, Deserialize)]
struct DownloadStationRelease {
    #[serde(default)]
    info_hash_hint: Option<String>,
    #[serde(default)]
    info_hash_v1: Option<String>,
}

#[derive(Clone)]
struct ApiSelection {
    auth: ApiInfo,
    task: ApiInfo,
    task_v2: bool,
    info: Option<ApiInfo>,
    dsm_info: Option<ApiInfo>,
}

#[derive(Default, Clone, Deserialize)]
struct ApiInfo {
    #[serde(default, alias = "maxVersion")]
    max_version: i64,
    #[serde(default, alias = "minVersion")]
    min_version: i64,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct DiskResponse<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    error: Option<DiskError>,
    data: T,
}

#[derive(Default, Deserialize)]
struct DiskError {
    #[serde(default)]
    code: i64,
}

#[derive(Default, Deserialize)]
struct AuthData {
    #[serde(default, rename = "sid", alias = "SId")]
    sid: String,
}

#[derive(Default, Deserialize)]
struct TaskListV1 {
    #[serde(default)]
    tasks: Vec<DsTask>,
}

#[derive(Default, Deserialize)]
struct TaskListV2 {
    #[serde(default)]
    task: Vec<DsTaskV2>,
    #[serde(default)]
    total: i64,
}

#[derive(Default, Deserialize, Clone)]
struct DsTaskV2 {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    size: i64,
    #[serde(default, rename = "type")]
    task_type: String,
    #[serde(default)]
    status: i64,
    #[serde(default)]
    additional: DsAdditional,
}

#[derive(Default, Deserialize, Clone)]
struct DsTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    size: i64,
    #[serde(default, rename = "type")]
    task_type: String,
    #[serde(default)]
    status: DsStatus,
    #[serde(default, rename = "status_extra")]
    status_extra: HashMap<String, String>,
    #[serde(default)]
    additional: DsAdditional,
}

#[derive(Default, Deserialize, Clone)]
struct DsAdditional {
    #[serde(default)]
    detail: HashMap<String, String>,
    #[serde(default)]
    transfer: HashMap<String, String>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DsStatus {
    #[default]
    Unknown,
    Waiting,
    Downloading,
    Paused,
    Finishing,
    Finished,
    HashChecking,
    Seeding,
    FilehostingWaiting,
    Extracting,
    Error,
    CaptchaNeeded,
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
        id: "downloadstation".to_string(),
        name: "Download Station".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "downloadstation".to_string(),
            provider_aliases: vec!["synology-download-station".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
                DownloadInputKind::Nzb,
                DownloadInputKind::NzbUrl,
            ],
            isolation_modes: vec![DownloadIsolationMode::Tag, DownloadIsolationMode::Directory],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: false,
                mark_imported: false,
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
    let request: DownloadStationAddRequest = serde_json::from_str(&input)?;
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    let destination = get_download_directory(&config, &apis)?;
    if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let file_name = request
            .source
            .torrent_file_name
            .clone()
            .unwrap_or_else(|| "download.torrent".to_string());
        add_file(
            &config,
            &apis,
            &file_name,
            bytes,
            &destination,
            "application/x-bittorrent",
            "torrent_bytes_base64",
        )?;
    } else if let Some(bytes) = request.source.nzb_bytes_base64.as_deref() {
        let file_name = request
            .source
            .nzb_file_name
            .clone()
            .unwrap_or_else(|| "download.nzb".to_string());
        add_file(
            &config,
            &apis,
            &file_name,
            bytes,
            &destination,
            "application/x-nzb",
            "nzb_bytes_base64",
        )?;
    } else if let Some(source) = source_url(&request) {
        add_url(&config, &apis, &source, &destination)?;
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    }
    let tasks = list_tasks(&config, &apis)?;
    let id = tasks
        .iter()
        .find(|task| source_matches(task, &request))
        .map(|task| task.id.clone())
        .ok_or_else(|| Error::msg("Download Station did not return the added task"))?;
    let serial = serial_hash(&config, &apis).unwrap_or_else(|_| "downloadstation".to_string());
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: format!("{serial}:{id}"),
            info_hash: hash,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    let serial = serial_hash(&config, &apis).unwrap_or_else(|_| "downloadstation".to_string());
    let items = list_tasks(&config, &apis)?
        .into_iter()
        .filter(|task| matches_scope(&config, task))
        .map(|task| task_to_item(&config, &serial, task))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    let serial = serial_hash(&config, &apis).unwrap_or_else(|_| "downloadstation".to_string());
    let items = list_tasks(&config, &apis)?
        .into_iter()
        .filter(|task| matches_scope(&config, task))
        .map(|task| task_to_item(&config, &serial, task))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    let serial = serial_hash(&config, &apis).unwrap_or_else(|_| "downloadstation".to_string());
    let downloads = list_tasks(&config, &apis)?
        .into_iter()
        .filter(|task| matches_scope(&config, task))
        .filter(|task| task.status == DsStatus::Finished)
        .map(|task| task_to_completed(&config, &serial, task))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    match request.action {
        DownloadControlAction::Remove => {
            if request.remove_data {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Unsupported,
                    "Sonarr deletes Download Station data through host filesystem access before removing the task",
                ))?);
            }
            let id = parse_download_id(&request.client_item_id);
            let _: serde_json::Value = api_get(
                &config,
                &apis.task,
                if apis.task_v2 {
                    "SYNO.DownloadStation2.Task"
                } else {
                    "SYNO.DownloadStation.Task"
                },
                "delete",
                if apis.task_v2 { 2 } else { 1 },
                &[
                    ("id".to_string(), id),
                    ("force_complete".to_string(), "false".to_string()),
                ],
                true,
            )?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Download Station control action is not implemented by Sonarr's torrent client",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(_input: String) -> FnResult<String> {
    let _request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&_input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = DsConfig::from_extism()?;
    let apis = select_apis(&config)?;
    let root = get_download_directory(&config, &apis)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(matches!(config.host.as_str(), "127.0.0.1" | "localhost")),
            remote_output_roots: if root.is_empty() { Vec::new() } else { vec![format!("/{root}")] },
            removes_completed_downloads: Some(false),
            sorting_mode: Some(if apis.task_v2 { "downloadstation-v2" } else { "downloadstation-v1" }.to_string()),
            warnings: vec![
                "Sonarr displays a provider warning for Download Station".to_string(),
                "Remove with data is unavailable because Sonarr deletes files through the host filesystem".to_string(),
            ],
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = DsConfig::from_extism()?;
    var::remove(SID_VAR_KEY)?;
    let apis = select_apis(&config)?;
    let _sid = authenticate(&config, &apis.auth, true)?;
    if apis.task.min_version > 2 || apis.task.max_version < 2 {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            "Download Station Task API v2 is required by Sonarr",
        ))?);
    }
    let _ = list_tasks(&config, &apis)?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl DsConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = config_value("port").unwrap_or_else(|| "5000".to_string());
        let scheme = if config_bool("use_ssl", false) {
            "https"
        } else {
            "http"
        };
        Ok(Self {
            base_url: format!("{scheme}://{host}:{port}"),
            host,
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_default(),
            directory: config_value("directory").unwrap_or_default(),
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
            Some("127.0.0.1"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("5000"),
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
    ]
}

fn select_apis(config: &DsConfig) -> Result<ApiSelection, Error> {
    let data: HashMap<String, ApiInfo> = api_info_query(
        config,
        "SYNO.API.Auth,SYNO.DownloadStation2.Task,SYNO.DownloadStation.Task,SYNO.DownloadStation.Info,SYNO.DSM.Info",
    )?;
    let auth = data
        .get("SYNO.API.Auth")
        .cloned()
        .ok_or_else(|| Error::msg("SYNO.API.Auth was not advertised"))?;
    let info = data.get("SYNO.DownloadStation.Info").cloned();
    let dsm_info = data.get("SYNO.DSM.Info").cloned();
    if let Some(task) = data.get("SYNO.DownloadStation2.Task").cloned() {
        return Ok(ApiSelection {
            auth,
            task,
            task_v2: true,
            info,
            dsm_info,
        });
    }
    let task = data
        .get("SYNO.DownloadStation.Task")
        .cloned()
        .ok_or_else(|| Error::msg("Download Station Task API was not advertised"))?;
    Ok(ApiSelection {
        auth,
        task,
        task_v2: false,
        info,
        dsm_info,
    })
}

fn api_info_query(config: &DsConfig, query: &str) -> Result<HashMap<String, ApiInfo>, Error> {
    let url = format!(
        "{}/webapi/query.cgi?api=SYNO.API.Info&version=1&method=query&query={}",
        config.base_url,
        urlencoding::encode(query)
    );
    request_disk_json(config, "GET", &url, None, false)
}

fn authenticate(config: &DsConfig, auth: &ApiInfo, force: bool) -> Result<String, Error> {
    if !force
        && let Some(sid) = var::get(SID_VAR_KEY)?
            .map(|value: String| value)
            .filter(|value| !value.is_empty())
    {
        return Ok(sid);
    }
    let version = if auth.max_version >= 7 { 6 } else { 2 };
    let data: AuthData = api_get(
        config,
        auth,
        "SYNO.API.Auth",
        "login",
        version,
        &[
            ("account".to_string(), config.username.clone()),
            ("passwd".to_string(), config.password.clone()),
            ("format".to_string(), "sid".to_string()),
            ("session".to_string(), "DownloadStation".to_string()),
        ],
        false,
    )?;
    if data.sid.is_empty() {
        return Err(Error::msg("Download Station did not return a session id"));
    }
    var::set(SID_VAR_KEY, data.sid.clone())?;
    Ok(data.sid)
}

fn api_get<T: DeserializeOwned>(
    config: &DsConfig,
    info: &ApiInfo,
    api_name: &str,
    method: &str,
    version: i64,
    params: &[(String, String)],
    auth: bool,
) -> Result<T, Error> {
    let mut query = vec![
        ("api".to_string(), api_name.to_string()),
        ("version".to_string(), version.to_string()),
        ("method".to_string(), method.to_string()),
    ];
    if auth {
        let apis = select_apis(config)?;
        query.push(("_sid".to_string(), authenticate(config, &apis.auth, false)?));
    }
    query.extend_from_slice(params);
    let url = format!(
        "{}/webapi/{}?{}",
        config.base_url,
        info.path.trim_start_matches('/'),
        encode_query(&query)
    );
    request_disk_json(config, "GET", &url, None, auth)
}

fn request_disk_json<T: DeserializeOwned>(
    _config: &DsConfig,
    method: &str,
    url: &str,
    body: Option<Vec<u8>>,
    _auth: bool,
) -> Result<T, Error> {
    let request = HttpRequest::new(url)
        .with_method(method)
        .with_header("User-Agent", "scryer-downloadstation-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, body)
        .map_err(|error| Error::msg(format!("Download Station request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "Download Station returned HTTP {status}: {text}"
        )));
    }
    let parsed: DiskResponse<T> = serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("Download Station response parse failed: {error}")))?;
    if parsed.success {
        Ok(parsed.data)
    } else {
        let code = parsed.error.map(|error| error.code).unwrap_or_default();
        if matches!(code, 105 | 106 | 107 | 119) {
            var::remove(SID_VAR_KEY)?;
        }
        Err(Error::msg(format!(
            "Download Station API returned error code {code}"
        )))
    }
}

fn add_url(
    config: &DsConfig,
    apis: &ApiSelection,
    url: &str,
    destination: &str,
) -> Result<(), Error> {
    if apis.task_v2 {
        let mut params = vec![
            ("type".to_string(), "url".to_string()),
            ("url".to_string(), url.to_string()),
            ("create_list".to_string(), "false".to_string()),
        ];
        if !destination.is_empty() {
            params.push(("destination".to_string(), destination.to_string()));
        }
        let _: serde_json::Value = api_get(
            config,
            &apis.task,
            "SYNO.DownloadStation2.Task",
            "create",
            2,
            &params,
            true,
        )?;
    } else {
        let mut params = vec![("uri".to_string(), url.to_string())];
        if !destination.is_empty() {
            params.push(("destination".to_string(), destination.to_string()));
        }
        let _: serde_json::Value = api_get(
            config,
            &apis.task,
            "SYNO.DownloadStation.Task",
            "create",
            3,
            &params,
            true,
        )?;
    }
    Ok(())
}

fn add_file(
    config: &DsConfig,
    apis: &ApiSelection,
    file_name: &str,
    bytes_base64: &str,
    destination: &str,
    content_type: &str,
    payload_label: &str,
) -> Result<(), Error> {
    let bytes = STANDARD
        .decode(bytes_base64)
        .map_err(|error| Error::msg(format!("invalid {payload_label}: {error}")))?;
    if apis.task_v2 {
        let mut fields = vec![
            ("api".to_string(), "SYNO.DownloadStation2.Task".to_string()),
            ("version".to_string(), "2".to_string()),
            ("method".to_string(), "create".to_string()),
            ("type".to_string(), "\"file\"".to_string()),
            ("file".to_string(), "[\"fileData\"]".to_string()),
            ("create_list".to_string(), "false".to_string()),
        ];
        if !destination.is_empty() {
            fields.push(("destination".to_string(), format!("\"{destination}\"")));
        }
        api_multipart(
            config,
            apis,
            MultipartUpload {
                info: &apis.task,
                sid_in_query: true,
                fields: &fields,
                file_field: "fileData",
                file_name,
                content_type,
                file_bytes: &bytes,
            },
        )?;
    } else {
        let mut fields = vec![
            ("api".to_string(), "SYNO.DownloadStation.Task".to_string()),
            ("version".to_string(), "2".to_string()),
            ("method".to_string(), "create".to_string()),
        ];
        if !destination.is_empty() {
            fields.push(("destination".to_string(), destination.to_string()));
        }
        api_multipart(
            config,
            apis,
            MultipartUpload {
                info: &apis.task,
                sid_in_query: false,
                fields: &fields,
                file_field: "file",
                file_name,
                content_type,
                file_bytes: &bytes,
            },
        )?;
    }
    Ok(())
}

struct MultipartUpload<'a> {
    info: &'a ApiInfo,
    sid_in_query: bool,
    fields: &'a [(String, String)],
    file_field: &'a str,
    file_name: &'a str,
    content_type: &'a str,
    file_bytes: &'a [u8],
}

fn api_multipart(
    config: &DsConfig,
    apis: &ApiSelection,
    upload: MultipartUpload<'_>,
) -> Result<serde_json::Value, Error> {
    let boundary = "scryer-downloadstation-boundary";
    let sid = authenticate(config, &apis.auth, false)?;
    let url = if upload.sid_in_query {
        format!(
            "{}/webapi/{}?_sid={}",
            config.base_url,
            upload.info.path.trim_start_matches('/'),
            urlencoding::encode(&sid)
        )
    } else {
        format!(
            "{}/webapi/{}",
            config.base_url,
            upload.info.path.trim_start_matches('/')
        )
    };
    let mut body = Vec::new();
    if !upload.sid_in_query {
        write_form_field(&mut body, boundary, "_sid", &sid);
    }
    for (key, value) in upload.fields {
        write_form_field(&mut body, boundary, key, value);
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
            upload.file_field,
            upload.file_name.replace('"', "")
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", upload.content_type).as_bytes());
    body.extend_from_slice(upload.file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = HttpRequest::new(url)
        .with_method("POST")
        .with_header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .with_header("User-Agent", "scryer-downloadstation-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, Some(body)).map_err(|error| {
        Error::msg(format!(
            "Download Station multipart request failed: {error}"
        ))
    })?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "Download Station returned HTTP {status}: {text}"
        )));
    }
    let parsed: DiskResponse<serde_json::Value> = serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("Download Station response parse failed: {error}")))?;
    if parsed.success {
        Ok(parsed.data)
    } else {
        let code = parsed.error.map(|error| error.code).unwrap_or_default();
        Err(Error::msg(format!(
            "Download Station API returned error code {code}"
        )))
    }
}

fn write_form_field(body: &mut Vec<u8>, boundary: &str, key: &str, value: &str) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

fn list_tasks(config: &DsConfig, apis: &ApiSelection) -> Result<Vec<DsTask>, Error> {
    if apis.task_v2 {
        let detail: TaskListV2 = api_get(
            config,
            &apis.task,
            "SYNO.DownloadStation2.Task",
            "list",
            1,
            &[("additional".to_string(), "detail".to_string())],
            true,
        )?;
        if detail.total <= 0 {
            return Ok(Vec::new());
        }
        let transfer: TaskListV2 = api_get(
            config,
            &apis.task,
            "SYNO.DownloadStation2.Task",
            "list",
            1,
            &[("additional".to_string(), "transfer".to_string())],
            true,
        )?;
        let transfer_by_id = transfer
            .task
            .into_iter()
            .map(|task| (task.id.clone(), task.additional.transfer))
            .collect::<HashMap<_, _>>();
        return Ok(detail
            .task
            .into_iter()
            .map(|task| DsTask {
                id: task.id.clone(),
                title: task.title,
                size: task.size,
                task_type: task.task_type,
                status: status_from_int(task.status),
                status_extra: HashMap::new(),
                additional: DsAdditional {
                    detail: task.additional.detail,
                    transfer: transfer_by_id.get(&task.id).cloned().unwrap_or_default(),
                },
            })
            .filter(is_supported_task_type)
            .collect());
    }
    let list: TaskListV1 = api_get(
        config,
        &apis.task,
        "SYNO.DownloadStation.Task",
        "list",
        1,
        &[("additional".to_string(), "detail,transfer".to_string())],
        true,
    )?;
    Ok(list
        .tasks
        .into_iter()
        .filter(is_supported_task_type)
        .collect())
}

fn get_download_directory(config: &DsConfig, apis: &ApiSelection) -> Result<String, Error> {
    if !config.directory.is_empty() {
        return Ok(config.directory.trim_start_matches('/').to_string());
    }
    let Some(info) = apis.info.as_ref() else {
        return Ok(String::new());
    };
    let data: HashMap<String, serde_json::Value> = api_get(
        config,
        info,
        "SYNO.DownloadStation.Info",
        "getConfig",
        1,
        &[],
        true,
    )?;
    let mut dir = data
        .get("default_destination")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    if !dir.is_empty() && !config.category.is_empty() {
        dir = format!("{dir}/{}", config.category);
    }
    Ok(dir)
}

fn serial_hash(config: &DsConfig, apis: &ApiSelection) -> Result<String, Error> {
    let Some(info) = apis.dsm_info.as_ref() else {
        return Ok("downloadstation".to_string());
    };
    let data: HashMap<String, serde_json::Value> = api_get(
        config,
        info,
        "SYNO.DSM.Info",
        "getinfo",
        info.min_version,
        &[],
        true,
    )?;
    let serial = data
        .get("serial")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if serial.is_empty() {
        return Ok("downloadstation".to_string());
    }
    let digest = Sha1::digest(serial.as_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn source_matches(task: &DsTask, request: &DownloadStationAddRequest) -> bool {
    let Some(uri) = task.additional.detail.get("uri") else {
        return false;
    };
    source_match_candidates(request)
        .into_iter()
        .any(|candidate| candidate == *uri)
}

fn source_match_candidates(request: &DownloadStationAddRequest) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(value) = request
        .source
        .magnet_uri
        .as_ref()
        .or(request.source.torrent_url.as_ref())
        .or(request.source.nzb_url.as_ref())
        .or(request.source.download_url.as_ref())
        .filter(|value| !value.trim().is_empty())
    {
        values.push(value.clone());
    }
    if let Some(value) = request
        .source
        .torrent_file_name
        .as_ref()
        .or(request.source.nzb_file_name.as_ref())
        .filter(|value| !value.trim().is_empty())
    {
        values.push(value.clone());
        values.push(
            value
                .trim_end_matches(".torrent")
                .trim_end_matches(".nzb")
                .to_string(),
        );
    }
    values
}

fn task_to_item(config: &DsConfig, serial: &str, task: DsTask) -> PluginDownloadItem {
    let output_dir = format!(
        "/{}",
        task.additional
            .detail
            .get("destination")
            .cloned()
            .unwrap_or_default()
    );
    let remaining = remaining_size(&task);
    let status = map_status(&task);
    PluginDownloadItem {
        client_item_id: format!("{serial}:{}", task.id),
        download_id: None,
        info_hash: None,
        title: task.title.clone(),
        state: status,
        message: message(&task),
        category: non_empty(config.category.clone()),
        remote_output_path: if matches!(
            status,
            DownloadItemState::Completed | DownloadItemState::Failed
        ) {
            Some(format!(
                "{}/{}",
                output_dir.trim_end_matches('/'),
                task.title
            ))
        } else {
            None
        },
        torrent: task
            .task_type
            .eq_ignore_ascii_case("bt")
            .then(|| PluginTorrentItem {
                save_path: Some(output_dir.clone()),
                content_paths: vec![format!(
                    "{}/{}",
                    output_dir.trim_end_matches('/'),
                    task.title
                )],
                uploaded_bytes: transfer_i64(&task, "size_uploaded"),
                downloaded_bytes: transfer_i64(&task, "size_downloaded"),
                upload_rate_bytes_per_second: transfer_i64(&task, "speed_upload"),
                download_rate_bytes_per_second: transfer_i64(&task, "speed_download"),
                seed_ratio: seed_ratio(&task),
                raw_status: Some(status_name(task.status).to_string()),
                ..PluginTorrentItem::default()
            }),
        total_size_bytes: Some(task.size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: eta_seconds(&task, remaining),
        progress_percent: if task.size > 0 {
            Some(
                (((task.size - remaining) as f64 / task.size as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8,
            )
        } else {
            None
        },
        can_move_files: Some(task.status == DsStatus::Finished),
        can_remove: Some(task.status == DsStatus::Finished),
        removed: Some(false),
        raw_state: Some(status_name(task.status).to_string()),
        completed_at: None,
    }
}

fn task_to_completed(config: &DsConfig, serial: &str, task: DsTask) -> PluginCompletedDownload {
    let output_dir = format!(
        "/{}",
        task.additional
            .detail
            .get("destination")
            .cloned()
            .unwrap_or_default()
    );
    let path = format!("{}/{}", output_dir.trim_end_matches('/'), task.title);
    PluginCompletedDownload {
        client_item_id: format!("{serial}:{}", task.id),
        download_id: None,
        info_hash: None,
        name: task.title,
        dest_dir: path.clone(),
        category: non_empty(config.category.clone()),
        output_kind: Some(if path_looks_like_file(&path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: vec![path],
        size_bytes: Some(task.size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn is_supported_task_type(task: &DsTask) -> bool {
    task.task_type.eq_ignore_ascii_case("bt") || task.task_type.eq_ignore_ascii_case("nzb")
}

fn matches_scope(config: &DsConfig, task: &DsTask) -> bool {
    let output = task
        .additional
        .detail
        .get("destination")
        .cloned()
        .unwrap_or_default();
    if !config.directory.is_empty()
        && !format!("/{output}")
            .starts_with(&format!("/{}", config.directory.trim_start_matches('/')))
    {
        return false;
    }
    if config.directory.is_empty()
        && !config.category.is_empty()
        && !output
            .split(['/', '\\'])
            .any(|part| part == config.category)
    {
        return false;
    }
    true
}

fn parse_download_id(id: &str) -> String {
    id.split(':').next_back().unwrap_or(id).to_string()
}

fn status_from_int(value: i64) -> DsStatus {
    match value {
        1 => DsStatus::Waiting,
        2 => DsStatus::Downloading,
        3 => DsStatus::Paused,
        4 => DsStatus::Finishing,
        5 => DsStatus::Finished,
        6 => DsStatus::HashChecking,
        7 => DsStatus::Seeding,
        8 => DsStatus::FilehostingWaiting,
        9 => DsStatus::Extracting,
        10 => DsStatus::Error,
        11 => DsStatus::CaptchaNeeded,
        _ => DsStatus::Unknown,
    }
}

fn map_status(task: &DsTask) -> DownloadItemState {
    match task.status {
        DsStatus::Unknown | DsStatus::Waiting | DsStatus::FilehostingWaiting => {
            if task.size == 0 || remaining_size(task) > 0 {
                DownloadItemState::Queued
            } else {
                DownloadItemState::Completed
            }
        }
        DsStatus::Paused => DownloadItemState::Paused,
        DsStatus::Finished | DsStatus::Seeding => DownloadItemState::Completed,
        DsStatus::Error => DownloadItemState::Failed,
        _ => DownloadItemState::Downloading,
    }
}

fn message(task: &DsTask) -> Option<String> {
    if task.status == DsStatus::Extracting {
        return task
            .status_extra
            .get("unzip_progress")
            .map(|value| format!("Extracting: {value}%"));
    }
    if task.status == DsStatus::Error {
        return task.status_extra.get("error_detail").cloned();
    }
    None
}

fn remaining_size(task: &DsTask) -> i64 {
    task.size
        - transfer_i64(task, "size_downloaded")
            .unwrap_or_default()
            .max(0)
}

fn eta_seconds(task: &DsTask, remaining: i64) -> Option<i64> {
    let speed = transfer_i64(task, "speed_download").unwrap_or_default();
    (speed > 0).then_some(remaining / speed)
}

fn seed_ratio(task: &DsTask) -> Option<f64> {
    let downloaded = transfer_i64(task, "size_downloaded")?;
    let uploaded = transfer_i64(task, "size_uploaded")?;
    Some(if downloaded <= 0 {
        0.0
    } else {
        uploaded as f64 / downloaded as f64
    })
}

fn transfer_i64(task: &DsTask, key: &str) -> Option<i64> {
    task.additional.transfer.get(key)?.parse().ok()
}

fn status_name(status: DsStatus) -> &'static str {
    match status {
        DsStatus::Unknown => "unknown",
        DsStatus::Waiting => "waiting",
        DsStatus::Downloading => "downloading",
        DsStatus::Paused => "paused",
        DsStatus::Finishing => "finishing",
        DsStatus::Finished => "finished",
        DsStatus::HashChecking => "hash_checking",
        DsStatus::Seeding => "seeding",
        DsStatus::FilehostingWaiting => "filehosting_waiting",
        DsStatus::Extracting => "extracting",
        DsStatus::Error => "error",
        DsStatus::CaptchaNeeded => "captcha_needed",
    }
}

fn encode_query(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn source_url(request: &DownloadStationAddRequest) -> Option<String> {
    match source_kind(request) {
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
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => request
            .source
            .nzb_url
            .clone()
            .or_else(|| request.source.download_url.clone()),
    }
}

fn source_kind(request: &DownloadStationAddRequest) -> DownloadInputKind {
    request.source.kind.unwrap_or_else(|| {
        if request.source.magnet_uri.is_some() {
            DownloadInputKind::MagnetUri
        } else if request.source.torrent_url.is_some() {
            DownloadInputKind::TorrentUrl
        } else if request.source.torrent_bytes_base64.is_some() {
            DownloadInputKind::TorrentBytes
        } else {
            DownloadInputKind::NzbUrl
        }
    })
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
