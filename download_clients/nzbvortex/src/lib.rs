use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldRole, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, PluginCompletedDownload, PluginDescriptor, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientStatus, PluginDownloadItem,
    PluginDownloadOutputKind, PluginDownloadRelease, PluginError, PluginErrorCode, PluginResult,
    ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct NzbVortexConfig {
    base_url: String,
    api_key: String,
    category: String,
    recent_priority: i64,
    older_priority: i64,
}

#[derive(Default, Deserialize)]
struct AddRequest {
    #[serde(default)]
    source: AddSource,
    #[serde(default)]
    release: PluginDownloadRelease,
}

#[derive(Default, Deserialize)]
struct AddSource {
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    nzb_bytes_base64: Option<String>,
    #[serde(default)]
    nzb_file_name: Option<String>,
    #[serde(default)]
    source_title: Option<String>,
}

#[derive(Default, Deserialize)]
struct AddResponse {
    #[serde(default, rename = "add_uuid", alias = "addUuid", alias = "Id")]
    id: Option<String>,
}

#[derive(Default, Deserialize)]
struct VersionResponse {
    #[serde(default, rename = "Version", alias = "version")]
    version: Option<String>,
}

#[derive(Default, Deserialize)]
struct ApiVersionResponse {
    #[serde(default, rename = "ApiLevel", alias = "apiLevel", alias = "api_level")]
    api_level: Option<String>,
}

#[derive(Default, Deserialize)]
struct AuthNonceResponse {
    #[serde(
        default,
        rename = "AuthNonce",
        alias = "authNonce",
        alias = "auth_nonce"
    )]
    auth_nonce: Option<String>,
}

#[derive(Default, Deserialize)]
struct AuthResponse {
    #[serde(
        default,
        rename = "LoginResult",
        alias = "loginResult",
        alias = "login_result"
    )]
    login_result: Option<String>,
    #[serde(
        default,
        rename = "SessionId",
        alias = "sessionId",
        alias = "session_id"
    )]
    session_id: Option<String>,
}

#[derive(Default, Deserialize)]
struct GroupResponse {
    #[serde(default, rename = "Groups", alias = "groups")]
    groups: Vec<VortexGroup>,
}

#[derive(Default, Deserialize)]
struct VortexGroup {
    #[serde(
        default,
        rename = "GroupName",
        alias = "groupName",
        alias = "group_name"
    )]
    group_name: String,
}

#[derive(Default, Deserialize)]
struct QueueResponse {
    #[serde(default, rename = "nzbs", alias = "Nzbs")]
    items: Vec<VortexQueueItem>,
}

#[derive(Default, Deserialize)]
struct FilesResponse {
    #[serde(default, rename = "Files", alias = "files")]
    files: Vec<VortexFile>,
}

#[derive(Default, Deserialize)]
struct VortexFile {
    #[serde(default, rename = "FileName", alias = "fileName", alias = "filename")]
    file_name: String,
}

#[derive(Default, Deserialize)]
struct VortexQueueItem {
    #[serde(default, rename = "Id", alias = "id")]
    id: i64,
    #[serde(default, rename = "UiTitle", alias = "uiTitle", alias = "ui_title")]
    ui_title: String,
    #[serde(
        default,
        rename = "DestinationPath",
        alias = "destinationPath",
        alias = "destination_path"
    )]
    destination_path: String,
    #[serde(default, rename = "IsPaused", alias = "isPaused", alias = "is_paused")]
    is_paused: bool,
    #[serde(default, rename = "State", alias = "state")]
    state: i64,
    #[serde(
        default,
        rename = "StatusText",
        alias = "statusText",
        alias = "status_text"
    )]
    status_text: Option<String>,
    #[serde(
        default,
        rename = "TransferedSpeed",
        alias = "transferedSpeed",
        alias = "transferredSpeed"
    )]
    transferred_speed: i64,
    #[serde(default, rename = "Progress", alias = "progress")]
    progress: f64,
    #[serde(
        default,
        rename = "DownloadedSize",
        alias = "downloadedSize",
        alias = "downloaded_size"
    )]
    downloaded_size: i64,
    #[serde(
        default,
        rename = "TotalDownloadSize",
        alias = "totalDownloadSize",
        alias = "total_download_size"
    )]
    total_download_size: i64,
    #[serde(
        default,
        rename = "AddUUID",
        alias = "addUUID",
        alias = "addUuid",
        alias = "add_uuid"
    )]
    add_uuid: Option<String>,
    #[serde(
        default,
        rename = "GroupName",
        alias = "groupName",
        alias = "group_name"
    )]
    group_name: Option<String>,
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
        id: "nzbvortex".to_string(),
        name: "NZBVortex".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "nzbvortex".to_string(),
            provider_aliases: vec!["nzb-vortex".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![DownloadInputKind::Nzb, DownloadInputKind::NzbUrl],
            isolation_modes: vec![DownloadIsolationMode::Category],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
                remove: true,
                remove_with_data: true,
                mark_imported: true,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: false,
                start_paused: false,
                force_start: false,
                per_download_directory: false,
                host_fs_required: false,
                test_connection: true,
                torrent: None,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

#[plugin_fn]
pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: AddRequest = serde_json::from_str(&input)?;
    let config = NzbVortexConfig::from_extism()?;
    let (filename, bytes) = nzb_payload(&request)?;
    let priority = if request.release.is_recent.unwrap_or(false) {
        config.recent_priority
    } else {
        config.older_priority
    };
    let mut params = vec![("priority".to_string(), priority.to_string())];
    if !config.category.trim().is_empty() {
        params.push(("groupname".to_string(), config.category.clone()));
    }
    let (content_type, body) = multipart_nzb_body(&filename, &bytes)?;
    let response: AddResponse = post_json_authenticated(
        &config,
        "nzb/add",
        &params,
        body,
        &[("Content-Type", content_type.as_str())],
    )?;
    let id = response
        .id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg("NZBVortex did not return add_uuid"))?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: id,
            info_hash: None,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = NzbVortexConfig::from_extism()?;
    let items = list_queue(&config)?
        .into_iter()
        .filter(|item| item.state != 20)
        .map(|item| map_queue_item(&config, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = NzbVortexConfig::from_extism()?;
    let items = list_queue(&config)?
        .into_iter()
        .map(|item| map_queue_item(&config, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = NzbVortexConfig::from_extism()?;
    let downloads = list_queue(&config)?
        .into_iter()
        .filter(|item| item.state == 20)
        .map(|item| map_completed(&config, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = NzbVortexConfig::from_extism()?;
    match request.action {
        DownloadControlAction::Remove => {
            let Some(id) = resolve_numeric_id(&config, &request.client_item_id)? else {
                return Ok(serde_json::to_string(&plugin_error::<()>(
                    PluginErrorCode::Permanent,
                    format!("NZBVortex item '{}' was not found", request.client_item_id),
                ))?);
            };
            let action = if request.remove_data {
                "cancelDelete"
            } else {
                "cancel"
            };
            let _: Value = get_json_authenticated(&config, &format!("nzb/{id}/{action}"), &[])?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "NZBVortex does not expose this control action through Scryer",
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
    let config = NzbVortexConfig::from_extism()?;
    let version = get_version(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(version),
            is_localhost: Some(is_localhost_url(&config.base_url)),
            remote_output_roots: Vec::new(),
            removes_completed_downloads: Some(false),
            sorting_mode: Some("nzbvortex".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = NzbVortexConfig::from_extism()?;
    let version = get_version(&config)?;
    let api_version = get_api_version(&config)?;
    if !version_at_least(&api_version, 2, 3) {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!("NZBVortex API version {api_version} is below Scryer's required 2.3"),
        ))?);
    }
    authenticate(&config)?;
    if !config.category.trim().is_empty() {
        let groups: GroupResponse = get_json_authenticated(&config, "group", &[])?;
        let found = groups
            .groups
            .iter()
            .any(|group| group.group_name == config.category);
        if !found {
            return Ok(serde_json::to_string(&plugin_error::<String>(
                PluginErrorCode::Permanent,
                format!("NZBVortex group '{}' was not found", config.category),
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(format!(
        "{version} (API {api_version})"
    )))?)
}

impl NzbVortexConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_i64("port", 4321);
        let scheme = if config_bool("use_ssl", true) {
            "https"
        } else {
            "http"
        };
        let mut base_url = format!("{scheme}://{host}:{port}");
        let url_base = config_value("url_base").unwrap_or_default();
        let url_base = url_base.trim().trim_matches('/');
        if !url_base.is_empty() {
            base_url.push('/');
            base_url.push_str(url_base);
        }
        base_url.push_str("/api");

        Ok(Self {
            base_url,
            api_key: required_config("api_key")?,
            category: config_value("category").unwrap_or_else(|| "TV Shows".to_string()),
            recent_priority: config_i64("recent_priority", 0),
            older_priority: config_i64("older_priority", 0),
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
            Some("4321"),
            None,
        ),
        field(
            "use_ssl",
            "Use SSL",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            None,
        ),
        connection_field(
            "url_base",
            "URL Base",
            false,
            None,
            Some("Advanced path segment before /api."),
        ),
        field(
            "api_key",
            "API Key",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "category",
            "Group",
            ConfigFieldType::String,
            false,
            Some("TV Shows"),
            Some("NZBVortex group used by Scryer."),
        ),
        field(
            "recent_priority",
            "Recent Priority",
            ConfigFieldType::Number,
            false,
            Some("0"),
            Some("NZBVortex priority for recent releases: -1 low, 0 normal, 1 high."),
        ),
        field(
            "older_priority",
            "Older Priority",
            ConfigFieldType::Number,
            false,
            Some("0"),
            Some("NZBVortex priority for older releases: -1 low, 0 normal, 1 high."),
        ),
    ]
}

fn nzb_payload(request: &AddRequest) -> Result<(String, Vec<u8>), Error> {
    if let Some(raw) = request
        .source
        .nzb_bytes_base64
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let bytes = STANDARD
            .decode(raw)
            .map_err(|error| Error::msg(format!("invalid nzb_bytes_base64: {error}")))?;
        return Ok((nzb_file_name(request), bytes));
    }

    let Some(url) = request
        .source
        .download_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Err(Error::msg("NZBVortex requires an NZB payload or NZB URL"));
    };

    Ok((nzb_file_name(request), get_external_bytes(url)?))
}

fn nzb_file_name(request: &AddRequest) -> String {
    request
        .source
        .nzb_file_name
        .clone()
        .or_else(|| request.source.source_title.clone())
        .or_else(|| request.release.release_title.clone())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.to_ascii_lowercase().ends_with(".nzb") {
                value
            } else {
                format!("{value}.nzb")
            }
        })
        .unwrap_or_else(|| "download.nzb".to_string())
}

fn multipart_nzb_body(filename: &str, bytes: &[u8]) -> Result<(String, Vec<u8>), Error> {
    let boundary = format!("scryer-nzbvortex-{}", random_hex(16)?);
    let escaped_filename = filename.replace('"', "_").replace(['\r', '\n'], "_");
    let mut body = Vec::new();
    write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"; filename=\"{escaped_filename}\"\r\nContent-Type: application/x-nzb\r\n\r\n"
    )
    .map_err(|error| Error::msg(format!("failed to build multipart body: {error}")))?;
    body.extend_from_slice(bytes);
    write!(body, "\r\n--{boundary}--\r\n")
        .map_err(|error| Error::msg(format!("failed to finish multipart body: {error}")))?;
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

fn get_version(config: &NzbVortexConfig) -> Result<String, Error> {
    let response: VersionResponse = get_json(config, "app/appversion", &[])?;
    Ok(response.version.unwrap_or_default())
}

fn get_api_version(config: &NzbVortexConfig) -> Result<String, Error> {
    let response: ApiVersionResponse = get_json(config, "app/apilevel", &[])?;
    Ok(response.api_level.unwrap_or_default())
}

fn list_queue(config: &NzbVortexConfig) -> Result<Vec<VortexQueueItem>, Error> {
    let mut params = Vec::new();
    if !config.category.trim().is_empty() {
        params.push(("groupName".to_string(), config.category.clone()));
    }
    params.push(("limitDone".to_string(), "30".to_string()));
    let response: QueueResponse = get_json_authenticated(config, "nzb", &params)?;
    Ok(response.items)
}

fn resolve_numeric_id(
    config: &NzbVortexConfig,
    client_item_id: &str,
) -> Result<Option<i64>, Error> {
    if let Ok(id) = client_item_id.parse::<i64>() {
        return Ok(Some(id));
    }
    Ok(list_queue(config)?
        .into_iter()
        .find(|item| item.add_uuid.as_deref() == Some(client_item_id))
        .map(|item| item.id))
}

fn map_queue_item(
    config: &NzbVortexConfig,
    item: VortexQueueItem,
) -> Result<PluginDownloadItem, Error> {
    let client_item_id = item.add_uuid.clone().unwrap_or_else(|| item.id.to_string());
    let mut state = map_state(&item);
    let mut message = item.status_text.clone();
    let output_path = output_path(config, &item, &mut state, &mut message)?;
    let remaining = (item.total_download_size - item.downloaded_size).max(0);
    let progress = item.progress.round().clamp(0.0, 100.0) as u8;
    Ok(PluginDownloadItem {
        client_item_id,
        download_id: None,
        info_hash: None,
        title: item.ui_title,
        state,
        message,
        category: item.group_name,
        remote_output_path: output_path,
        torrent: None,
        total_size_bytes: Some(item.total_download_size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (item.transferred_speed > 0).then_some(remaining / item.transferred_speed),
        progress_percent: Some(progress),
        can_move_files: Some(true),
        can_remove: Some(true),
        removed: Some(false),
        raw_state: Some(item.state.to_string()),
        completed_at: None,
    })
}

fn map_completed(
    config: &NzbVortexConfig,
    item: VortexQueueItem,
) -> Result<PluginCompletedDownload, Error> {
    let mut state = DownloadItemState::Completed;
    let mut message = None;
    let output_path = output_path(config, &item, &mut state, &mut message)?
        .unwrap_or_else(|| item.destination_path.clone());
    Ok(PluginCompletedDownload {
        client_item_id: item.add_uuid.clone().unwrap_or_else(|| item.id.to_string()),
        download_id: None,
        info_hash: None,
        name: item.ui_title,
        dest_dir: output_path.clone(),
        category: item.group_name,
        output_kind: Some(PluginDownloadOutputKind::File),
        content_paths: vec![output_path],
        size_bytes: Some(item.total_download_size),
        completed_at: None,
        parameters: Vec::new(),
    })
}

fn output_path(
    config: &NzbVortexConfig,
    item: &VortexQueueItem,
    state: &mut DownloadItemState,
    message: &mut Option<String>,
) -> Result<Option<String>, Error> {
    let base = item.destination_path.trim();
    if base.is_empty() {
        return Ok(None);
    }
    if path_file_name(base) == item.ui_title {
        return Ok(Some(base.to_string()));
    }
    if item.state != 20 {
        return Ok(None);
    }
    let response: FilesResponse =
        get_json_authenticated(config, &format!("file/{}", item.id), &[])?;
    if response.files.len() > 1 {
        *state = DownloadItemState::Warning;
        *message = Some(format!("NZBVortex reported multiple files under {base}"));
    }
    Ok(response
        .files
        .first()
        .map(|file| join_path(base, &file.file_name))
        .or_else(|| Some(base.to_string())))
}

fn map_state(item: &VortexQueueItem) -> DownloadItemState {
    if item.is_paused {
        return DownloadItemState::Paused;
    }
    match item.state {
        0 => DownloadItemState::Queued,
        20 => DownloadItemState::Completed,
        21 | 22 | 24 => DownloadItemState::Failed,
        _ => DownloadItemState::Downloading,
    }
}

fn get_json<T: DeserializeOwned>(
    config: &NzbVortexConfig,
    path: &str,
    params: &[(String, String)],
) -> Result<T, Error> {
    let body = request_bytes(config, "GET", path, params, None, &[])?;
    parse_json(&body)
}

fn get_json_authenticated<T: DeserializeOwned>(
    config: &NzbVortexConfig,
    path: &str,
    params: &[(String, String)],
) -> Result<T, Error> {
    request_json_authenticated(config, "GET", path, params, None, &[])
}

fn post_json_authenticated<T: DeserializeOwned>(
    config: &NzbVortexConfig,
    path: &str,
    params: &[(String, String)],
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> Result<T, Error> {
    request_json_authenticated(config, "POST", path, params, Some(body), headers)
}

fn request_json_authenticated<T: DeserializeOwned>(
    config: &NzbVortexConfig,
    method: &str,
    path: &str,
    params: &[(String, String)],
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> Result<T, Error> {
    let mut authed_params = params.to_vec();
    authed_params.push(("sessionid".to_string(), authenticate(config)?));
    let bytes = request_bytes(config, method, path, &authed_params, body, headers)?;
    parse_json(&bytes)
}

fn authenticate(config: &NzbVortexConfig) -> Result<String, Error> {
    let nonce: AuthNonceResponse = get_json(config, "auth/nonce", &[])?;
    let nonce = nonce
        .auth_nonce
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg("NZBVortex did not return auth nonce"))?;
    let cnonce = random_hex(16)?;
    let hash = auth_hash(&nonce, &cnonce, &config.api_key);
    let response: AuthResponse = get_json(
        config,
        "auth/login",
        &[
            ("nonce".to_string(), nonce),
            ("cnonce".to_string(), cnonce),
            ("hash".to_string(), hash),
        ],
    )?;
    if normalize_token(response.login_result.as_deref().unwrap_or_default()) != "successful" {
        return Err(Error::msg("NZBVortex authentication failed"));
    }
    response
        .session_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::msg("NZBVortex did not return session id"))
}

fn auth_hash(nonce: &str, cnonce: &str, api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{nonce}:{cnonce}:{api_key}").as_bytes());
    STANDARD.encode(hasher.finalize())
}

fn request_bytes(
    config: &NzbVortexConfig,
    method: &str,
    path: &str,
    params: &[(String, String)],
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> Result<Vec<u8>, Error> {
    let mut request = HttpRequest::new(api_url(config, path, params))
        .with_method(method)
        .with_header("User-Agent", "scryer-nzbvortex-plugin/0.1");
    for (key, value) in headers {
        request = request.with_header(*key, *value);
    }
    let response = http::request::<Vec<u8>>(&request, body)
        .map_err(|error| Error::msg(format!("NZBVortex request failed: {error}")))?;
    let status = response.status_code();
    let response_body = response.body();
    if status >= 400 {
        return Err(Error::msg(format!(
            "NZBVortex returned HTTP {status}: {}",
            String::from_utf8_lossy(&response_body)
        )));
    }
    Ok(response_body)
}

fn get_external_bytes(url: &str) -> Result<Vec<u8>, Error> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", "scryer-nzbvortex-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("NZB URL request failed: {error}")))?;
    let status = response.status_code();
    let body = response.body();
    if status >= 400 {
        return Err(Error::msg(format!(
            "NZB URL returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    if body.is_empty() {
        return Err(Error::msg("NZB URL returned an empty response body"));
    }
    Ok(body)
}

fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| Error::msg(format!("NZBVortex JSON parse failed: {error}")))?;
    if let Some(result) = result_token(&value)
        && result != "ok"
    {
        return Err(Error::msg(format!("NZBVortex returned result {result}")));
    }
    serde_json::from_value(value)
        .map_err(|error| Error::msg(format!("NZBVortex response parse failed: {error}")))
}

fn result_token(value: &Value) -> Option<String> {
    value
        .get("result")
        .or_else(|| value.get("Result"))
        .and_then(Value::as_str)
        .map(normalize_token)
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !matches!(ch, '_' | '-' | ' '))
        .collect::<String>()
        .to_ascii_lowercase()
}

fn api_url(config: &NzbVortexConfig, path: &str, params: &[(String, String)]) -> String {
    let mut url = format!(
        "{}/{}",
        config.base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if !params.is_empty() {
        url.push('?');
        for (index, (key, value)) in params.iter().enumerate() {
            if index > 0 {
                url.push('&');
            }
            url.push_str(&urlencoding::encode(key));
            url.push('=');
            url.push_str(&urlencoding::encode(value));
        }
    }
    url
}

fn random_hex(byte_len: usize) -> Result<String, Error> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| Error::msg(format!("failed to generate NZBVortex cnonce: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn version_at_least(version: &str, major: i64, minor: i64) -> bool {
    let parts = version
        .split('.')
        .map(|part| part.parse::<i64>().unwrap_or(0))
        .collect::<Vec<_>>();
    let reported_major = *parts.first().unwrap_or(&0);
    let reported_minor = *parts.get(1).unwrap_or(&0);
    reported_major > major || (reported_major == major && reported_minor >= minor)
}

fn path_file_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn join_path(base: &str, child: &str) -> String {
    if base.ends_with('/') || base.ends_with('\\') {
        format!("{base}{child}")
    } else if base.contains('\\') && !base.contains('/') {
        format!("{base}\\{child}")
    } else {
        format!("{base}/{child}")
    }
}

fn config_i64(key: &str, default: i64) -> i64 {
    config_value(key)
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(default)
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

fn required_config(key: &str) -> Result<String, Error> {
    config_value(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg(format!("missing required config value '{key}'")))
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
