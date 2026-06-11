use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use hmac::{Hmac, Mac};
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
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha1::Sha1;

const SESSION_VAR_KEY: &str = "freebox.session_token";

#[derive(Debug, Clone)]
struct FreeboxConfig {
    api_root: String,
    app_id: String,
    app_token: String,
    destination_directory: String,
    category: String,
    recent_priority_first: bool,
    older_priority_first: bool,
    add_paused: bool,
}

#[derive(Default, Deserialize)]
struct FreeboxResponse<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    result: Option<T>,
}

#[derive(Default, Deserialize)]
struct FreeboxLogin {
    #[serde(default)]
    challenge: String,
    #[serde(default)]
    session_token: String,
}

#[derive(Default, Deserialize)]
struct FreeboxDownloadConfiguration {
    #[serde(default, rename = "download_dir")]
    download_directory: String,
}

#[derive(Default, Deserialize, Clone)]
struct FreeboxDownloadTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "download_dir")]
    download_directory: String,
    #[serde(default, rename = "info_hash")]
    info_hash: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    eta: i64,
    #[serde(default)]
    error: String,
    #[serde(default, rename = "type")]
    task_type: String,
    #[serde(default, rename = "stop_ratio")]
    stop_ratio: i64,
    #[serde(default)]
    size: i64,
    #[serde(default, rename = "rx_pct")]
    received_percent: i64,
    #[serde(default, rename = "rx_bytes")]
    received_bytes: i64,
    #[serde(default, rename = "rx_rate")]
    received_rate: i64,
    #[serde(default, rename = "tx_bytes")]
    transmitted_bytes: i64,
    #[serde(default, rename = "tx_rate")]
    transmitted_rate: i64,
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
        id: "freebox".to_string(),
        name: "Freebox Download".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "freebox".to_string(),
            provider_aliases: vec!["freebox-download".to_string()],
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
                remove_with_data: true,
                mark_imported: false,
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
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: false,
                    supports_start_paused: true,
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
    let config = FreeboxConfig::from_extism()?;
    let directory = get_download_directory(&config, &request)?;
    let id = if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let decoded = STANDARD
            .decode(bytes)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?;
        let filename = request
            .source
            .torrent_file_name
            .clone()
            .unwrap_or_else(|| "download.torrent".to_string());
        add_file(&config, &filename, &decoded, &directory)?
    } else if let Some(source) = source_url(&request) {
        add_url(&config, &source, &directory)?
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    };
    set_torrent_settings(&config, &id, &request)?;
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty());
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientAddResponse {
            client_item_id: id,
            info_hash: hash,
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = FreeboxConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| matches_category_or_destination(&config, torrent))
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = FreeboxConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| matches_category_or_destination(&config, torrent))
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = FreeboxConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| matches_category_or_destination(&config, torrent))
        .filter(|torrent| torrent.status == "done")
        .map(|torrent| torrent_to_completed(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = FreeboxConfig::from_extism()?;
    match request.action {
        DownloadControlAction::Remove => {
            let path = if request.remove_data {
                format!("/downloads/{}/erase", request.client_item_id)
            } else {
                format!("/downloads/{}", request.client_item_id)
            };
            let _: serde_json::Value = api_json(&config, "DELETE", &path, None, true)?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Freebox control action is not implemented by Sonarr's Freebox client",
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
    let config = FreeboxConfig::from_extism()?;
    authenticate(&config, false)?;
    let root = get_download_directory_for_config(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: None,
            is_localhost: Some(is_localhost_url(&config.api_root)),
            remote_output_roots: if root.is_empty() {
                Vec::new()
            } else {
                vec![root]
            },
            removes_completed_downloads: Some(false),
            sorting_mode: Some("freebox-api".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = FreeboxConfig::from_extism()?;
    var::remove(SESSION_VAR_KEY)?;
    authenticate(&config, true)?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl FreeboxConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "mafreebox.freebox.fr".to_string());
        let port = config_value("port").unwrap_or_else(|| "443".to_string());
        let api_url = config_value("api_url").unwrap_or_else(|| "/api/v1/".to_string());
        let scheme = if config_bool("use_ssl", true) {
            "https"
        } else {
            "http"
        };
        Ok(Self {
            api_root: format!("{scheme}://{host}:{port}/{}", api_url.trim_matches('/')),
            app_id: config_value("app_id").unwrap_or_default(),
            app_token: config_value("app_token").unwrap_or_default(),
            destination_directory: config_value("destination_directory").unwrap_or_default(),
            category: config_value("category").unwrap_or_default(),
            recent_priority_first: config_value("recent_priority").as_deref() == Some("first"),
            older_priority_first: config_value("older_priority").as_deref() == Some("first"),
            add_paused: config_bool("add_paused", false),
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
            Some("mafreebox.freebox.fr"),
            None,
        ),
        field(
            "port",
            "Port",
            ConfigFieldType::Number,
            true,
            Some("443"),
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
        connection_field("api_url", "API URL", true, Some("/api/v1/"), None),
        field(
            "app_id",
            "App ID",
            ConfigFieldType::String,
            true,
            None,
            None,
        ),
        field(
            "app_token",
            "App Token",
            ConfigFieldType::Password,
            true,
            None,
            None,
        ),
        field(
            "destination_directory",
            "Destination",
            ConfigFieldType::String,
            false,
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
        priority_field("recent_priority", "Recent Priority"),
        priority_field("older_priority", "Older Priority"),
        field(
            "add_paused",
            "Add Paused",
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
        help_text: None,
    }
}

fn authenticate(config: &FreeboxConfig, force: bool) -> Result<String, Error> {
    if !force
        && let Some(token) = var::get(SESSION_VAR_KEY)?
            .map(|value: String| value)
            .filter(|value| !value.is_empty())
    {
        return Ok(token);
    }
    let challenge: FreeboxLogin = api_json(config, "GET", "/login", None, false)?;
    let mut mac = Hmac::<Sha1>::new_from_slice(config.app_token.as_bytes())
        .map_err(|error| Error::msg(format!("invalid Freebox app token: {error}")))?;
    mac.update(challenge.challenge.as_bytes());
    let password = hex_lower(&mac.finalize().into_bytes());
    let session: FreeboxLogin = api_json(
        config,
        "POST",
        "/login/session",
        Some(serde_json::json!({
            "app_id": config.app_id,
            "password": password,
        })),
        false,
    )?;
    if session.session_token.is_empty() {
        return Err(Error::msg("Freebox did not return a session token"));
    }
    var::set(SESSION_VAR_KEY, session.session_token.clone())?;
    Ok(session.session_token)
}

fn api_json<T: DeserializeOwned>(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    auth: bool,
) -> Result<T, Error> {
    let url = format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    );
    let mut request = HttpRequest::new(url)
        .with_method(method)
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "scryer-freebox-plugin/0.1");
    if auth {
        request = request.with_header("X-Fbx-App-Auth", authenticate(config, false)?);
    }
    let response = http::request::<Vec<u8>>(
        &request,
        body.map(|body| serde_json::to_vec(&body).unwrap_or_default()),
    )
    .map_err(|error| Error::msg(format!("Freebox request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        var::remove(SESSION_VAR_KEY)?;
        return Err(Error::msg("Freebox authentication failed"));
    }
    if status == 404 {
        return Err(Error::msg("Unable to reach Freebox API; verify API URL"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "Freebox returned HTTP {status}: {text}"
        )));
    }
    let response: FreeboxResponse<T> = serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("Freebox response parse failed: {error}")))?;
    if response.success {
        response
            .result
            .ok_or_else(|| Error::msg("Freebox response did not include result"))
    } else {
        Err(Error::msg(format!(
            "Freebox API returned error: {}",
            response
                .error_code
                .or(response.msg)
                .unwrap_or_else(|| "unknown".to_string())
        )))
    }
}

fn api_form<T: DeserializeOwned>(
    config: &FreeboxConfig,
    method: &str,
    path: &str,
    form: &[(String, String)],
) -> Result<T, Error> {
    let url = format!("{}{}", config.api_root.trim_end_matches('/'), path);
    let body = form
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let request = HttpRequest::new(url)
        .with_method(method)
        .with_header("Content-Type", "application/x-www-form-urlencoded")
        .with_header("X-Fbx-App-Auth", authenticate(config, false)?)
        .with_header("User-Agent", "scryer-freebox-plugin/0.1");
    parse_response(
        http::request::<Vec<u8>>(&request, Some(body.into_bytes())),
        "Freebox form",
    )
}

fn api_multipart<T: DeserializeOwned>(
    config: &FreeboxConfig,
    path: &str,
    file_name: &str,
    file_bytes: &[u8],
    form: &[(String, String)],
) -> Result<T, Error> {
    let boundary = "scryer-freebox-boundary";
    let mut body = Vec::new();
    for (key, value) in form {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"download_file\"; filename=\"{}\"\r\n",
            file_name.replace('"', "")
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = HttpRequest::new(format!("{}{}", config.api_root.trim_end_matches('/'), path))
        .with_method("POST")
        .with_header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .with_header("X-Fbx-App-Auth", authenticate(config, false)?)
        .with_header("User-Agent", "scryer-freebox-plugin/0.1");
    parse_response(
        http::request::<Vec<u8>>(&request, Some(body)),
        "Freebox multipart",
    )
}

fn parse_response<T: DeserializeOwned>(
    response: Result<HttpResponse, Error>,
    label: &str,
) -> Result<T, Error> {
    let response =
        response.map_err(|error| Error::msg(format!("{label} request failed: {error}")))?;
    let status = response.status_code();
    let text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "{label} returned HTTP {status}: {text}"
        )));
    }
    let response: FreeboxResponse<T> = serde_json::from_str(&text)
        .map_err(|error| Error::msg(format!("{label} response parse failed: {error}")))?;
    if response.success {
        response
            .result
            .ok_or_else(|| Error::msg(format!("{label} response did not include result")))
    } else {
        Err(Error::msg(format!(
            "{label} API returned error: {}",
            response
                .error_code
                .or(response.msg)
                .unwrap_or_else(|| "unknown".to_string())
        )))
    }
}

fn add_url(config: &FreeboxConfig, url: &str, directory: &str) -> Result<String, Error> {
    let mut form = vec![("download_url".to_string(), url.to_string())];
    if !directory.is_empty() {
        form.push(("download_dir".to_string(), STANDARD.encode(directory)));
    }
    let task: FreeboxDownloadTask = api_form(config, "POST", "/downloads/add", &form)?;
    Ok(task.id)
}

fn add_file(
    config: &FreeboxConfig,
    file_name: &str,
    file_bytes: &[u8],
    directory: &str,
) -> Result<String, Error> {
    let form = if directory.is_empty() {
        Vec::new()
    } else {
        vec![("download_dir".to_string(), STANDARD.encode(directory))]
    };
    let task: FreeboxDownloadTask =
        api_multipart(config, "/downloads/add", file_name, file_bytes, &form)?;
    Ok(task.id)
}

fn set_torrent_settings(
    config: &FreeboxConfig,
    id: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), Error> {
    let recent = request.release.is_recent.unwrap_or(false);
    let add_first =
        (recent && config.recent_priority_first) || (!recent && config.older_priority_first);
    let seed_ratio = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio)
        .map(|ratio| ratio * 100.0);
    if !config.add_paused && !add_first && seed_ratio.is_none() {
        return Ok(());
    }
    let mut body = serde_json::Map::new();
    if config.add_paused {
        body.insert(
            "status".to_string(),
            serde_json::Value::String("stopped".to_string()),
        );
    }
    if add_first {
        body.insert(
            "queue_pos".to_string(),
            serde_json::Value::String("1".to_string()),
        );
    }
    if let Some(seed_ratio) = seed_ratio {
        body.insert("stop_ratio".to_string(), serde_json::json!(seed_ratio));
    }
    let _: FreeboxDownloadTask = api_json(
        config,
        "PUT",
        &format!("/downloads/{id}"),
        Some(serde_json::Value::Object(body)),
        true,
    )?;
    Ok(())
}

fn list_torrents(config: &FreeboxConfig) -> Result<Vec<FreeboxDownloadTask>, Error> {
    let tasks: Vec<FreeboxDownloadTask> = api_json(config, "GET", "/downloads/", None, true)?;
    Ok(tasks
        .into_iter()
        .filter(|task| task.task_type.eq_ignore_ascii_case("bt"))
        .collect())
}

fn get_download_directory(
    config: &FreeboxConfig,
    request: &PluginDownloadClientAddRequest,
) -> Result<String, Error> {
    if let Some(directory) = request
        .routing
        .download_directory
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(directory.trim_end_matches('/').to_string());
    }
    get_download_directory_for_config(config)
}

fn get_download_directory_for_config(config: &FreeboxConfig) -> Result<String, Error> {
    if !config.destination_directory.is_empty() {
        return Ok(config
            .destination_directory
            .trim_end_matches('/')
            .to_string());
    }
    let download_config: FreeboxDownloadConfiguration =
        api_json(config, "GET", "/downloads/config/", None, true)?;
    let mut dest_dir = decode_base64(&download_config.download_directory)
        .trim_end_matches('/')
        .to_string();
    if !config.category.is_empty() {
        dest_dir = format!("{}/{}", dest_dir.trim_end_matches('/'), config.category);
    }
    Ok(dest_dir)
}

fn matches_category_or_destination(config: &FreeboxConfig, torrent: &FreeboxDownloadTask) -> bool {
    let output = decode_base64(&torrent.download_directory);
    if !config.destination_directory.is_empty()
        && !output.starts_with(config.destination_directory.trim_end_matches('/'))
    {
        return false;
    }
    if !config.category.is_empty()
        && !output
            .split(['/', '\\'])
            .any(|segment| segment == config.category)
    {
        return false;
    }
    true
}

fn torrent_to_item(config: &FreeboxConfig, torrent: FreeboxDownloadTask) -> PluginDownloadItem {
    let output = decode_base64(&torrent.download_directory);
    let remaining = ((torrent.size as f64) * (1.0 - (torrent.received_percent as f64 / 10000.0)))
        .round()
        .max(0.0) as i64;
    let state = map_state(&torrent);
    PluginDownloadItem {
        client_item_id: torrent.id.clone(),
        download_id: None,
        info_hash: non_empty(normalize_hash(&torrent.info_hash)),
        title: torrent.name.clone(),
        state,
        message: if torrent.status == "error" {
            Some(error_description(&torrent.error))
        } else {
            None
        },
        category: non_empty(config.category.clone()),
        remote_output_path: non_empty(output.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: non_empty(normalize_hash(&torrent.info_hash)),
            save_path: non_empty(output.clone()),
            content_paths: non_empty(output).into_iter().collect(),
            uploaded_bytes: Some(torrent.transmitted_bytes),
            downloaded_bytes: Some(torrent.received_bytes),
            upload_rate_bytes_per_second: Some(torrent.transmitted_rate),
            download_rate_bytes_per_second: Some(torrent.received_rate),
            seed_ratio: Some(if torrent.stop_ratio <= 0 {
                0.0
            } else {
                torrent.stop_ratio as f64 / 100.0
            }),
            raw_status: Some(torrent.status.clone()),
            status_reason: non_empty(torrent.error.clone()),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (torrent.eta > 0).then_some(torrent.eta),
        progress_percent: Some(
            ((torrent.received_percent as f64 / 100.0)
                .round()
                .clamp(0.0, 100.0)) as u8,
        ),
        can_move_files: Some(torrent.status == "done"),
        can_remove: Some(torrent.status == "done"),
        removed: Some(false),
        raw_state: Some(torrent.status),
        completed_at: None,
    }
}

fn torrent_to_completed(
    config: &FreeboxConfig,
    torrent: FreeboxDownloadTask,
) -> PluginCompletedDownload {
    let output = decode_base64(&torrent.download_directory);
    PluginCompletedDownload {
        client_item_id: torrent.id,
        download_id: None,
        info_hash: non_empty(normalize_hash(&torrent.info_hash)),
        name: torrent.name,
        dest_dir: output.clone(),
        category: non_empty(config.category.clone()),
        output_kind: Some(if path_looks_like_file(&output) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(output).into_iter().collect(),
        size_bytes: Some(torrent.size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn map_state(torrent: &FreeboxDownloadTask) -> DownloadItemState {
    match torrent.status.as_str() {
        "stopped" | "stopping" => DownloadItemState::Paused,
        "queued" => DownloadItemState::Queued,
        "starting" | "downloading" | "retry" | "checking" => DownloadItemState::Downloading,
        "error" => DownloadItemState::Warning,
        "done" | "seeding" => DownloadItemState::Completed,
        _ => DownloadItemState::Downloading,
    }
}

fn error_description(error: &str) -> String {
    match error {
        "internal" => "Internal error.".to_string(),
        "disk_full" => "The disk is full.".to_string(),
        "unknown" => "Unknown error.".to_string(),
        "parse_error" => "Parse error.".to_string(),
        "unknown_host" => "Unknown host.".to_string(),
        "timeout" => "Timeout.".to_string(),
        "bad_authentication" => "Invalid credentials.".to_string(),
        "connection_refused" => "Remote host refused connection.".to_string(),
        "bt_tracker_error" => "Unable to announce on tracker.".to_string(),
        "bt_missing_files" => "Missing torrent files.".to_string(),
        "bt_file_error" => "Error accessing torrent files.".to_string(),
        "missing_ctx_file" => "Error accessing task context file.".to_string(),
        "nzb_no_group" => "Cannot find the requested group on server.".to_string(),
        "nzb_not_found" => "Article not found on the server.".to_string(),
        "nzb_invalid_crc" => "Invalid article CRC.".to_string(),
        "nzb_invalid_size" => "Invalid article size.".to_string(),
        "nzb_invalid_filename" => "Invalid filename.".to_string(),
        "nzb_open_failed" => "Error opening.".to_string(),
        "nzb_write_failed" => "Error writing.".to_string(),
        "nzb_missing_size" => "Missing article size.".to_string(),
        "nzb_decode_error" => "Article decoding error.".to_string(),
        "nzb_missing_segments" => "Missing article segments.".to_string(),
        "nzb_error" => "Other nzb error.".to_string(),
        "nzb_authentication_required" => "Nzb server need authentication.".to_string(),
        "" => "Unknown error.".to_string(),
        value => format!("{value} - Unknown error"),
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

fn decode_base64(value: &str) -> String {
    STANDARD
        .decode(value)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
