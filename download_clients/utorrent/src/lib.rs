use base64::{Engine as _, engine::general_purpose::STANDARD};
use extism_pdk::*;
use roxmltree::Document;
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

const TOKEN_VAR_KEY: &str = "utorrent.token";
const COOKIE_VAR_KEY: &str = "utorrent.cookie";

const STATUS_STARTED: i64 = 1;
const STATUS_CHECKED: i64 = 8;
const STATUS_ERROR: i64 = 16;
const STATUS_PAUSED: i64 = 32;
const STATUS_QUEUED: i64 = 64;
const STATUS_LOADED: i64 = 128;

#[derive(Debug, Clone)]
struct UTorrentConfig {
    gui_url: String,
    username: String,
    password: String,
    category: String,
    post_import_category: String,
    recent_priority_first: bool,
    older_priority_first: bool,
    initial_state: String,
}

#[derive(Debug, Clone, Default)]
struct UTorrentTorrent {
    hash: String,
    status: i64,
    name: String,
    size: i64,
    progress: i64,
    downloaded: i64,
    uploaded: i64,
    ratio: i64,
    upload_speed: i64,
    download_speed: i64,
    eta: i64,
    label: String,
    remaining: i64,
    root_download_path: String,
    status_message: Option<String>,
}

#[derive(Default, Deserialize)]
struct UTorrentResponse {
    #[serde(default)]
    build: i64,
    #[serde(default)]
    torrents: Vec<Vec<serde_json::Value>>,
    #[serde(default)]
    settings: Vec<Vec<serde_json::Value>>,
}

struct RawResponse {
    body_text: String,
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
        id: "utorrent".to_string(),
        name: "uTorrent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "utorrent".to_string(),
            provider_aliases: vec!["microtorrent".to_string()],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![DownloadIsolationMode::Tag],
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
                force_start: true,
                per_download_directory: false,
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
                    isolation_modes: vec![DownloadIsolationMode::Tag],
                    post_import_isolation_modes: vec![DownloadIsolationMode::Tag],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
                    supports_start_paused: true,
                    supports_force_start: true,
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
    let config = UTorrentConfig::from_extism()?;
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg("uTorrent add requires an info hash from the release"))?;

    if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        let decoded = STANDARD
            .decode(bytes)
            .map_err(|error| Error::msg(format!("invalid torrent_bytes_base64: {error}")))?;
        let filename = request
            .source
            .torrent_file_name
            .clone()
            .unwrap_or_else(|| format!("{hash}.torrent"));
        let _ = post_multipart(
            &config,
            &[
                ("action".to_string(), "add-file".to_string()),
                ("path".to_string(), String::new()),
            ],
            "torrent_file",
            &filename,
            &decoded,
        )?;
    } else if let Some(source) = source_url(&request) {
        let _ = request_json(
            &config,
            &[
                ("action".to_string(), "add-url".to_string()),
                ("s".to_string(), source),
            ],
            "GET",
            None,
        )?;
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    }

    set_seed_config(&config, &hash, &request)?;
    if !config.category.is_empty() {
        set_label(&config, &hash, &config.category)?;
    }
    let recent = request.release.is_recent.unwrap_or(false);
    if (recent && config.recent_priority_first) || (!recent && config.older_priority_first) {
        let _ = request_json(
            &config,
            &[
                ("action".to_string(), "queuetop".to_string()),
                ("hash".to_string(), hash.clone()),
            ],
            "GET",
            None,
        )?;
    }
    if !config.initial_state.is_empty() {
        let _ = request_json(
            &config,
            &[
                ("action".to_string(), config.initial_state.clone()),
                ("hash".to_string(), hash.clone()),
            ],
            "GET",
            None,
        )?;
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
    let config = UTorrentConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent.label == config.category)
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = UTorrentConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent.label == config.category)
        .map(|torrent| torrent_to_item(&config, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = UTorrentConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|torrent| torrent.label == config.category)
        .filter(is_completed)
        .map(torrent_to_completed)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = UTorrentConfig::from_extism()?;
    let action = match request.action {
        DownloadControlAction::Remove => {
            if request.remove_data {
                "removedata"
            } else {
                "remove"
            }
        }
        DownloadControlAction::Pause => "pause",
        DownloadControlAction::Resume => "start",
        DownloadControlAction::ForceStart => "forcestart",
    };
    let _ = request_json(
        &config,
        &[
            ("action".to_string(), action.to_string()),
            ("hash".to_string(), normalize_hash(&request.client_item_id)),
        ],
        "GET",
        None,
    )?;
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = UTorrentConfig::from_extism()?;
    if config.post_import_category.is_empty() || config.post_import_category == config.category {
        return Ok(serde_json::to_string(&PluginResult::Ok(()))?);
    }
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    set_label(&config, &hash, &config.post_import_category)?;
    if !config.category.is_empty() {
        remove_label(&config, &hash, &config.category)?;
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = UTorrentConfig::from_extism()?;
    let response = get_settings(&config)?;
    let settings = settings_map(&response);
    let mut root = String::new();
    if settings
        .get("dir_active_download_flag")
        .is_some_and(|value| value == "true")
    {
        root = settings
            .get("dir_active_download")
            .cloned()
            .unwrap_or_default();
    }
    if settings
        .get("dir_completed_download_flag")
        .is_some_and(|value| value == "true")
    {
        root = settings
            .get("dir_completed_download")
            .cloned()
            .unwrap_or_default();
        if settings
            .get("dir_add_label")
            .is_some_and(|value| value == "true")
            && !config.category.is_empty()
        {
            root = join_path(&root, &config.category);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(
        PluginDownloadClientStatus {
            version: Some(response.build.to_string()),
            is_localhost: Some(is_localhost_url(&config.gui_url)),
            remote_output_roots: if root.is_empty() {
                Vec::new()
            } else {
                vec![root]
            },
            removes_completed_downloads: Some(!config.post_import_category.is_empty()),
            sorting_mode: Some("utorrent-webui".to_string()),
            warnings: vec!["Sonarr displays a provider warning for uTorrent".to_string()],
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = UTorrentConfig::from_extism()?;
    var::remove(TOKEN_VAR_KEY)?;
    var::remove(COOKIE_VAR_KEY)?;
    let response = get_settings(&config)?;
    if response.build < 25406 {
        return Ok(serde_json::to_string(&plugin_error::<String>(
            PluginErrorCode::Permanent,
            format!(
                "uTorrent build {} is older than Sonarr's required 25406",
                response.build
            ),
        ))?);
    }
    let _ = list_torrents(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        response.build.to_string(),
    ))?)
}

impl UTorrentConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "8080".to_string());
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
            gui_url: format!("{}/gui/", base.trim_end_matches('/')),
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            category: config_value("category").unwrap_or_else(|| "tv-sonarr".to_string()),
            post_import_category: config_value("post_import_category").unwrap_or_default(),
            recent_priority_first: config_value("recent_priority").as_deref() == Some("first"),
            older_priority_first: config_value("older_priority").as_deref() == Some("first"),
            initial_state: config_value("initial_state").unwrap_or_else(|| "start".to_string()),
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
        connection_field("url_base", "URL Base", false, None, None),
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
        priority_field("recent_priority", "Recent Priority"),
        priority_field("older_priority", "Older Priority"),
        ConfigFieldDef {
            key: "initial_state".to_string(),
            label: "Initial State".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("start".to_string()),
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![
                ConfigFieldOption {
                    value: "start".to_string(),
                    label: "Start".to_string(),
                },
                ConfigFieldOption {
                    value: "forcestart".to_string(),
                    label: "Force Start".to_string(),
                },
                ConfigFieldOption {
                    value: "pause".to_string(),
                    label: "Pause".to_string(),
                },
                ConfigFieldOption {
                    value: "stop".to_string(),
                    label: "Stop".to_string(),
                },
            ],
            help_text: None,
        },
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

fn get_settings(config: &UTorrentConfig) -> Result<UTorrentResponse, Error> {
    request_json(
        config,
        &[("action".to_string(), "getsettings".to_string())],
        "GET",
        None,
    )
}

fn list_torrents(config: &UTorrentConfig) -> Result<Vec<UTorrentTorrent>, Error> {
    let response = request_json(
        config,
        &[("list".to_string(), "1".to_string())],
        "GET",
        None,
    )?;
    Ok(response.torrents.into_iter().map(map_torrent).collect())
}

fn set_seed_config(
    config: &UTorrentConfig,
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> Result<(), Error> {
    let ratio = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio);
    let seconds = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_seconds)
        .or(request.release.seed_goal_seconds);
    if ratio.is_none() && seconds.is_none() {
        return Ok(());
    }
    let mut params = vec![
        ("action".to_string(), "setprops".to_string()),
        ("hash".to_string(), hash.to_string()),
        ("s".to_string(), "seed_override".to_string()),
        ("v".to_string(), "1".to_string()),
    ];
    if let Some(ratio) = ratio {
        params.push(("s".to_string(), "seed_ratio".to_string()));
        params.push((
            "v".to_string(),
            ((ratio * 1000.0).round() as i64).to_string(),
        ));
    }
    if let Some(seconds) = seconds {
        params.push(("s".to_string(), "seed_time".to_string()));
        params.push(("v".to_string(), seconds.to_string()));
    }
    let _ = request_json(config, &params, "GET", None)?;
    Ok(())
}

fn set_label(config: &UTorrentConfig, hash: &str, label: &str) -> Result<(), Error> {
    let _ = request_json(
        config,
        &[
            ("action".to_string(), "setprops".to_string()),
            ("hash".to_string(), hash.to_string()),
            ("s".to_string(), "label".to_string()),
            ("v".to_string(), label.to_string()),
        ],
        "GET",
        None,
    )?;
    Ok(())
}

fn remove_label(config: &UTorrentConfig, hash: &str, label: &str) -> Result<(), Error> {
    let _ = request_json(
        config,
        &[
            ("action".to_string(), "setprops".to_string()),
            ("hash".to_string(), hash.to_string()),
            ("s".to_string(), "label".to_string()),
            ("v".to_string(), label.to_string()),
            ("s".to_string(), "label".to_string()),
            ("v".to_string(), String::new()),
        ],
        "GET",
        None,
    )?;
    Ok(())
}

fn request_json(
    config: &UTorrentConfig,
    params: &[(String, String)],
    method: &str,
    body: Option<Vec<u8>>,
) -> Result<UTorrentResponse, Error> {
    let response = request_with_auth(config, params, method, body.clone(), false)?;
    if response.body_text.trim().is_empty() {
        return Ok(UTorrentResponse::default());
    }
    serde_json::from_str(&response.body_text)
        .map_err(|error| Error::msg(format!("uTorrent response parse failed: {error}")))
}

fn post_multipart(
    config: &UTorrentConfig,
    params: &[(String, String)],
    field_name: &str,
    filename: &str,
    file_bytes: &[u8],
) -> Result<UTorrentResponse, Error> {
    let boundary = "scryer-utorrent-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{}\"\r\n",
            filename.replace('"', "")
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    request_json(
        config,
        params,
        "POST",
        Some(with_content_type(body, boundary)),
    )
}

fn with_content_type(mut body: Vec<u8>, boundary: &str) -> Vec<u8> {
    let mut prefixed =
        format!("Content-Type: multipart/form-data; boundary={boundary}\n").into_bytes();
    prefixed.append(&mut body);
    prefixed
}

fn request_with_auth(
    config: &UTorrentConfig,
    params: &[(String, String)],
    method: &str,
    body: Option<Vec<u8>>,
    reauth: bool,
) -> Result<RawResponse, Error> {
    let auth = authenticate(config, reauth)?;
    let retry_body = body.clone();
    let mut query = vec![("token".to_string(), auth.0)];
    query.extend_from_slice(params);
    let url = format!("{}?{}", config.gui_url, encode_query(&query));
    let mut request = HttpRequest::new(url)
        .with_method(method)
        .with_header("Cache-Control", "no-cache")
        .with_header("Cookie", auth.1)
        .with_header("Authorization", basic_auth(config))
        .with_header("User-Agent", "scryer-utorrent-plugin/0.1");
    let mut actual_body = body;
    if let Some(bytes) = actual_body.as_mut() {
        if bytes.starts_with(b"Content-Type: ") {
            if let Some(pos) = bytes.iter().position(|byte| *byte == b'\n') {
                let header = String::from_utf8_lossy(&bytes[..pos]).to_string();
                let content_type = header
                    .trim_start_matches("Content-Type: ")
                    .trim()
                    .to_string();
                request = request.with_header("Content-Type", content_type);
                bytes.drain(..=pos);
            }
        }
    }
    let response = http::request::<Vec<u8>>(&request, actual_body)
        .map_err(|error| Error::msg(format!("uTorrent request failed: {error}")))?;
    let status = response.status_code();
    if (status == 400 || status == 401) && !reauth {
        var::remove(TOKEN_VAR_KEY)?;
        var::remove(COOKIE_VAR_KEY)?;
        return request_with_auth(config, params, method, retry_body, true);
    }
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status >= 400 {
        return Err(Error::msg(format!(
            "uTorrent returned HTTP {status}: {body_text}"
        )));
    }
    Ok(RawResponse { body_text })
}

fn authenticate(config: &UTorrentConfig, force: bool) -> Result<(String, String), Error> {
    if !force
        && let (Some(token), Some(cookie)) = (
            var::get(TOKEN_VAR_KEY)?.map(|value: String| value),
            var::get(COOKIE_VAR_KEY)?.map(|value: String| value),
        )
        && !token.is_empty()
        && !cookie.is_empty()
    {
        return Ok((token, cookie));
    }
    let request = HttpRequest::new(format!("{}token.html", config.gui_url))
        .with_method("GET")
        .with_header("Authorization", basic_auth(config))
        .with_header("User-Agent", "scryer-utorrent-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, None)
        .map_err(|error| Error::msg(format!("uTorrent token request failed: {error}")))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        return Err(Error::msg("Failed to authenticate with uTorrent"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "uTorrent token request returned HTTP {status}: {body_text}"
        )));
    }
    let token = parse_token(&body_text)?;
    let cookie = response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg("uTorrent token response did not include a cookie"))?
        .to_string();
    var::set(TOKEN_VAR_KEY, token.clone())?;
    var::set(COOKIE_VAR_KEY, cookie.clone())?;
    Ok((token, cookie))
}

fn parse_token(html: &str) -> Result<String, Error> {
    if let Ok(doc) = Document::parse(html)
        && let Some(text) = doc
            .descendants()
            .find(|node| node.attribute("id") == Some("token"))
            .and_then(|node| node.text())
    {
        return Ok(text.to_string());
    }
    html.split('>')
        .find_map(|part| part.split('<').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::msg("uTorrent token response did not contain a token"))
}

fn map_torrent(values: Vec<serde_json::Value>) -> UTorrentTorrent {
    UTorrentTorrent {
        hash: values
            .first()
            .map(value_string)
            .map(|value| normalize_hash(&value))
            .unwrap_or_default(),
        status: values.get(1).and_then(value_i64).unwrap_or_default(),
        name: values.get(2).map(value_string).unwrap_or_default(),
        size: values.get(3).and_then(value_i64).unwrap_or_default(),
        progress: values.get(4).and_then(value_i64).unwrap_or_default(),
        downloaded: values.get(5).and_then(value_i64).unwrap_or_default(),
        uploaded: values.get(6).and_then(value_i64).unwrap_or_default(),
        ratio: values.get(7).and_then(value_i64).unwrap_or_default(),
        upload_speed: values.get(8).and_then(value_i64).unwrap_or_default(),
        download_speed: values.get(9).and_then(value_i64).unwrap_or_default(),
        eta: values.get(10).and_then(value_i64).unwrap_or(-1),
        label: values.get(11).map(value_string).unwrap_or_default(),
        remaining: values.get(18).and_then(value_i64).unwrap_or_default(),
        status_message: values
            .get(21)
            .map(value_string)
            .filter(|value| !value.is_empty()),
        root_download_path: values.get(26).map(value_string).unwrap_or_default(),
    }
}

fn torrent_to_item(config: &UTorrentConfig, torrent: UTorrentTorrent) -> PluginDownloadItem {
    let state = map_state(&torrent);
    let output_path = if last_path_segment(&torrent.root_download_path) == torrent.name {
        torrent.root_download_path.clone()
    } else {
        join_path(&torrent.root_download_path, &torrent.name)
    };
    PluginDownloadItem {
        client_item_id: torrent.hash.clone(),
        info_hash: Some(torrent.hash.clone()),
        title: torrent.name.clone(),
        state,
        message: if status_has(torrent.status, STATUS_ERROR) {
            Some("uTorrent reports an error state".to_string())
        } else {
            torrent.status_message.clone()
        },
        category: non_empty(torrent.label.clone()),
        remote_output_path: non_empty(output_path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(torrent.hash.clone()),
            tags: non_empty(torrent.label.clone()).into_iter().collect(),
            save_path: non_empty(torrent.root_download_path.clone()),
            content_paths: non_empty(output_path.clone()).into_iter().collect(),
            uploaded_bytes: Some(torrent.uploaded),
            downloaded_bytes: Some(torrent.downloaded),
            upload_rate_bytes_per_second: Some(torrent.upload_speed),
            download_rate_bytes_per_second: Some(torrent.download_speed),
            seed_ratio: Some(torrent.ratio as f64 / 1000.0),
            raw_status: Some(torrent.status.to_string()),
            status_reason: torrent.status_message.clone(),
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size),
        remaining_size_bytes: Some(torrent.remaining),
        eta_seconds: (torrent.eta != -1).then_some(torrent.eta),
        progress_percent: Some(((torrent.progress as f64 / 10.0).round().clamp(0.0, 100.0)) as u8),
        can_move_files: Some(can_remove(config, &torrent)),
        can_remove: Some(can_remove(config, &torrent)),
        removed: Some(false),
        raw_state: Some(torrent.status.to_string()),
        completed_at: None,
    }
}

fn torrent_to_completed(torrent: UTorrentTorrent) -> PluginCompletedDownload {
    let output_path = if last_path_segment(&torrent.root_download_path) == torrent.name {
        torrent.root_download_path.clone()
    } else {
        join_path(&torrent.root_download_path, &torrent.name)
    };
    PluginCompletedDownload {
        client_item_id: torrent.hash.clone(),
        info_hash: Some(torrent.hash),
        name: torrent.name,
        dest_dir: output_path.clone(),
        category: non_empty(torrent.label),
        output_kind: Some(if path_looks_like_file(&output_path) {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: non_empty(output_path).into_iter().collect(),
        size_bytes: Some(torrent.size),
        completed_at: None,
        parameters: Vec::new(),
    }
}

fn map_state(torrent: &UTorrentTorrent) -> DownloadItemState {
    if status_has(torrent.status, STATUS_ERROR) {
        DownloadItemState::Warning
    } else if status_has(torrent.status, STATUS_LOADED)
        && status_has(torrent.status, STATUS_CHECKED)
        && torrent.remaining == 0
        && torrent.progress >= 1000
    {
        DownloadItemState::Completed
    } else if status_has(torrent.status, STATUS_PAUSED) {
        DownloadItemState::Paused
    } else if status_has(torrent.status, STATUS_STARTED) {
        DownloadItemState::Downloading
    } else {
        DownloadItemState::Queued
    }
}

fn is_completed(torrent: &UTorrentTorrent) -> bool {
    status_has(torrent.status, STATUS_LOADED)
        && status_has(torrent.status, STATUS_CHECKED)
        && torrent.remaining == 0
        && torrent.progress >= 1000
}

fn can_remove(config: &UTorrentConfig, torrent: &UTorrentTorrent) -> bool {
    !config.post_import_category.is_empty()
        && !status_has(torrent.status, STATUS_QUEUED)
        && !status_has(torrent.status, STATUS_STARTED)
}

fn status_has(status: i64, flag: i64) -> bool {
    (status & flag) == flag
}

fn settings_map(response: &UTorrentResponse) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for item in &response.settings {
        if let (Some(key), Some(value)) = (
            item.first().map(value_string),
            item.get(2).map(value_string),
        ) {
            out.insert(key, value);
        }
    }
    out
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

fn basic_auth(config: &UTorrentConfig) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{}:{}", config.username, config.password))
    )
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

fn value_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string().trim_matches('"').to_string())
}

fn value_i64(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), name)
    }
}

fn last_path_segment(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn path_looks_like_file(path: &str) -> bool {
    let Some(last) = path.trim_end_matches('/').rsplit('/').next() else {
        return false;
    };
    last.contains('.')
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
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
