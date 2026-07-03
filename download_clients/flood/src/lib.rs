use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use extism_pdk::*;
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

const COOKIE_VAR_KEY: &str = "flood.cookie";
const SEED_CONFIG_VAR_PREFIX: &str = "flood.seed_config.";

#[derive(Debug, Clone)]
struct FloodConfig {
    api_root: String,
    username: String,
    password: String,
    destination: String,
    tags: Vec<String>,
    post_import_tags: Vec<String>,
    additional_tags: Vec<String>,
    start_on_add: bool,
}

#[derive(Default, Deserialize)]
struct TorrentListSummary {
    #[serde(default)]
    torrents: HashMap<String, FloodTorrent>,
}

#[derive(Default, Deserialize, Clone)]
struct FloodTorrent {
    #[serde(default, rename = "bytesDone")]
    bytes_done: i64,
    #[serde(default)]
    directory: String,
    #[serde(default)]
    eta: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ratio: f64,
    #[serde(default, rename = "sizeBytes")]
    size_bytes: i64,
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, rename = "dateFinished")]
    date_finished: Option<i64>,
}

#[derive(Default, Deserialize)]
struct TorrentContent {
    #[serde(default)]
    path: String,
}

#[derive(Default, Deserialize)]
struct FloodClientSettings {
    #[serde(default, rename = "directoryDefault")]
    directory_default: String,
}

#[derive(Default, Deserialize, Serialize)]
struct FloodSeedConfig {
    ratio: Option<f64>,
    seed_time_seconds: Option<i64>,
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
        id: "flood".to_string(),
        name: "Flood".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "flood".to_string(),
            provider_aliases: vec![],
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
                mark_imported: true,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: true,
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
                    post_import_isolation_modes: vec![DownloadIsolationMode::Tag],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
                    supports_start_paused: false,
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
    let config = FloodConfig::from_extism()?;
    let hash = request
        .release
        .info_hash_v1
        .as_deref()
        .or(request.release.info_hash_hint.as_deref())
        .map(normalize_hash)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::msg("Flood add requires an info hash from the release"))?;
    let mut body = serde_json::Map::new();
    body.insert(
        "tags".to_string(),
        serde_json::to_value(tags_for_request(&config, &request))?,
    );
    if let Some(destination) = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!config.destination.is_empty()).then_some(config.destination.clone()))
    {
        body.insert(
            "destination".to_string(),
            serde_json::Value::String(destination),
        );
    }
    if config.start_on_add {
        body.insert("start".to_string(), serde_json::Value::Bool(true));
    }

    if let Some(bytes) = request.source.torrent_bytes_base64.as_deref() {
        body.insert("files".to_string(), serde_json::json!([bytes]));
        post_json(
            &config,
            "/torrents/add-files",
            serde_json::Value::Object(body),
        )?;
    } else if let Some(source) = source_url(&request) {
        body.insert("urls".to_string(), serde_json::json!([source]));
        post_json(
            &config,
            "/torrents/add-urls",
            serde_json::Value::Object(body),
        )?;
    } else {
        return Ok(serde_json::to_string(&plugin_error::<
            PluginDownloadClientAddResponse,
        >(
            PluginErrorCode::Permanent,
            "download source is missing",
        ))?);
    }

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
    let config = FloodConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|(_, torrent)| matches_scope(&config, torrent))
        .map(|(hash, torrent)| torrent_to_item(&config, hash, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = FloodConfig::from_extism()?;
    let items = list_torrents(&config)?
        .into_iter()
        .filter(|(_, torrent)| matches_scope(&config, torrent))
        .map(|(hash, torrent)| torrent_to_item(&config, hash, torrent))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = FloodConfig::from_extism()?;
    let downloads = list_torrents(&config)?
        .into_iter()
        .filter(|(_, torrent)| matches_scope(&config, torrent))
        .filter(|(_, torrent)| is_completed(torrent))
        .map(|(hash, torrent)| torrent_to_completed(&config, hash, torrent))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&PluginResult::Ok(downloads))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    let config = FloodConfig::from_extism()?;
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Ok(serde_json::to_string(&plugin_error::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ))?);
    }
    match request.action {
        DownloadControlAction::Remove => {
            post_json(
                &config,
                "/torrents/delete",
                serde_json::json!({ "hashes": [hash], "deleteData": request.remove_data }),
            )?;
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => {
            return Ok(serde_json::to_string(&plugin_error::<()>(
                PluginErrorCode::Unsupported,
                "Flood control action is not implemented by Scryer's Flood client",
            ))?);
        }
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let config = FloodConfig::from_extism()?;
    if config.post_import_tags.is_empty() {
        return Ok(serde_json::to_string(&PluginResult::Ok(()))?);
    }
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if let Some(current) = list_torrents(&config)?.get(&hash).cloned() {
        let mut tags = current.tags;
        for tag in &config.post_import_tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        patch_json(
            &config,
            "/torrents/tags",
            serde_json::json!({ "hashes": [hash], "tags": tags }),
        )?;
    }
    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = FloodConfig::from_extism()?;
    auth_verify(&config)?;
    let settings: FloodClientSettings =
        serde_json::from_str(&get_text(&config, "/client/settings")?)
            .map_err(|error| Error::msg(format!("Flood settings parse failed: {error}")))?;
    let root = if config.destination.is_empty() {
        settings.directory_default
    } else {
        config.destination.clone()
    };
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
            sorting_mode: Some("flood-api".to_string()),
            warnings: Vec::new(),
        },
    ))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = FloodConfig::from_extism()?;
    var::remove(COOKIE_VAR_KEY)?;
    auth_verify(&config)?;
    Ok(serde_json::to_string(&PluginResult::Ok("ok".to_string()))?)
}

impl FloodConfig {
    fn from_extism() -> Result<Self, Error> {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "3000".to_string());
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
            api_root: format!("{}/api", base.trim_end_matches('/')),
            username: config_value("username").unwrap_or_default(),
            password: config_value("password").unwrap_or_default(),
            destination: config_value("destination").unwrap_or_default(),
            tags: config_list("tags", &["scryer"]),
            post_import_tags: config_list("post_import_tags", &[]),
            additional_tags: config_list("additional_tags", &[]),
            start_on_add: config_bool("start_on_add", true),
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
            Some("3000"),
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
            "destination",
            "Destination",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        field(
            "tags",
            "Tags",
            ConfigFieldType::Tag,
            false,
            Some("scryer"),
            None,
        ),
        field(
            "post_import_tags",
            "Post Import Tags",
            ConfigFieldType::Tag,
            false,
            None,
            None,
        ),
        additional_tags_field(),
        field(
            "start_on_add",
            "Start On Add",
            ConfigFieldType::Bool,
            false,
            Some("true"),
            None,
        ),
    ]
}

fn authenticate(config: &FloodConfig, force: bool) -> Result<String, Error> {
    if !force
        && let Some(cookie) = var::get(COOKIE_VAR_KEY)?
            .map(|value: String| value.trim().to_string())
            .filter(|value| !value.is_empty())
    {
        return Ok(cookie);
    }
    let response = raw_request(
        config,
        "POST",
        "/auth/authenticate",
        None,
        Some(serde_json::json!({
            "username": config.username,
            "password": config.password
        })),
    )?;
    let cookie = extract_cookie(&response)
        .ok_or_else(|| Error::msg("Flood auth did not return a cookie"))?;
    var::set(COOKIE_VAR_KEY, cookie.clone())?;
    Ok(cookie)
}

fn auth_verify(config: &FloodConfig) -> Result<(), Error> {
    get_text(config, "/auth/verify").map(|_| ())
}

fn list_torrents(config: &FloodConfig) -> Result<HashMap<String, FloodTorrent>, Error> {
    let summary: TorrentListSummary = serde_json::from_str(&get_text(config, "/torrents")?)
        .map_err(|error| Error::msg(format!("Flood torrent list parse failed: {error}")))?;
    Ok(summary.torrents)
}

fn get_contents(config: &FloodConfig, hash: &str) -> Result<Vec<String>, Error> {
    let contents: Vec<TorrentContent> =
        serde_json::from_str(&get_text(config, &format!("/torrents/{hash}/contents"))?)
            .map_err(|error| Error::msg(format!("Flood torrent contents parse failed: {error}")))?;
    Ok(contents.into_iter().map(|content| content.path).collect())
}

fn get_text(config: &FloodConfig, path: &str) -> Result<String, Error> {
    let response = raw_request(
        config,
        "GET",
        path,
        Some(authenticate(config, false)?),
        None,
    )?;
    Ok(response.body_text)
}

fn post_json(config: &FloodConfig, path: &str, body: serde_json::Value) -> Result<String, Error> {
    let response = raw_request(
        config,
        "POST",
        path,
        Some(authenticate(config, false)?),
        Some(body),
    )?;
    Ok(response.body_text)
}

fn patch_json(config: &FloodConfig, path: &str, body: serde_json::Value) -> Result<String, Error> {
    let response = raw_request(
        config,
        "PATCH",
        path,
        Some(authenticate(config, false)?),
        Some(body),
    )?;
    Ok(response.body_text)
}

struct RawResponse {
    body_text: String,
    headers: Vec<(String, String)>,
}

fn raw_request(
    config: &FloodConfig,
    method: &str,
    path: &str,
    cookie: Option<String>,
    body: Option<serde_json::Value>,
) -> Result<RawResponse, Error> {
    let mut request = HttpRequest::new(api_url(config, path))
        .with_method(method)
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", "scryer-flood-plugin/0.1");
    if let Some(cookie) = cookie {
        request = request.with_header("Cookie", cookie);
    }
    let response = http::request::<Vec<u8>>(
        &request,
        body.map(|body| serde_json::to_vec(&body).unwrap_or_default()),
    )
    .map_err(|error| Error::msg(format!("Flood request failed: {error}")))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();
    if status == 401 || status == 403 {
        var::remove(COOKIE_VAR_KEY)?;
        return Err(Error::msg("Failed to authenticate with Flood"));
    }
    if status >= 400 {
        return Err(Error::msg(format!(
            "Flood returned HTTP {status}: {body_text}"
        )));
    }
    Ok(RawResponse {
        body_text,
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    })
}

fn api_url(config: &FloodConfig, path: &str) -> String {
    format!(
        "{}{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" },
        path
    )
}

fn extract_cookie(response: &RawResponse) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn tags_for_request(config: &FloodConfig, request: &PluginDownloadClientAddRequest) -> Vec<String> {
    let mut tags = config.tags.clone();
    tags.extend(additional_tags_for_request(config, request));
    if let Some(isolation) = request.routing.isolation_value.as_deref()
        && !isolation.trim().is_empty()
    {
        tags.push(isolation.trim().to_string());
    }
    dedupe(tags)
}

fn additional_tags_for_request(
    config: &FloodConfig,
    request: &PluginDownloadClientAddRequest,
) -> Vec<String> {
    let mut tags = Vec::new();
    for tag in &config.additional_tags {
        match tag.as_str() {
            "title_slug" => push_optional_tag(&mut tags, title_slug_for_request(request)),
            "title_tags" => tags.extend(request.title.tags.iter().cloned()),
            "year" => push_optional_tag(&mut tags, request.title.year.map(|year| year.to_string())),
            "indexer" => push_optional_tag(&mut tags, request.release.indexer_name.clone()),
            "languages" => push_optional_tag(&mut tags, request.title.language.clone()),
            "network" => push_optional_tag(&mut tags, request.title.network.clone()),
            _ => {}
        }
    }
    tags
}

fn push_optional_tag(tags: &mut Vec<String>, value: Option<String>) {
    if let Some(value) = value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        tags.push(value);
    }
}

fn title_slug_for_request(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request.title.title_slug.clone().or_else(|| {
        let fallback = slug_tag(&request.title.title_name);
        (!fallback.is_empty()).then_some(fallback)
    })
}

fn slug_tag(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_separator = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator && !out.is_empty() {
            out.push('-');
            last_was_separator = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn torrent_to_item(
    config: &FloodConfig,
    hash: String,
    torrent: FloodTorrent,
) -> PluginDownloadItem {
    let remaining = (torrent.size_bytes - torrent.bytes_done).max(0);
    let state = map_state(&torrent);
    let removable = can_remove(config, &hash, &torrent, state);
    PluginDownloadItem {
        client_item_id: normalize_hash(&hash),
        download_id: None,
        info_hash: Some(normalize_hash(&hash)),
        title: torrent.name.clone(),
        state,
        message: if torrent.message.trim().is_empty() {
            None
        } else {
            Some(torrent.message.clone())
        },
        category: torrent.tags.first().cloned(),
        remote_output_path: Some(torrent.directory.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(normalize_hash(&hash)),
            tags: torrent.tags.clone(),
            save_path: Some(torrent.directory.clone()),
            content_paths: vec![torrent.directory.clone()],
            downloaded_bytes: Some(torrent.bytes_done),
            seed_ratio: Some(torrent.ratio),
            raw_status: Some(torrent.status.join(",")),
            status_reason: if torrent.message.trim().is_empty() {
                None
            } else {
                Some(torrent.message.clone())
            },
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size_bytes),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (torrent.eta > 0).then_some(torrent.eta),
        progress_percent: if torrent.size_bytes > 0 {
            Some(
                ((torrent.bytes_done as f64 / torrent.size_bytes as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8,
            )
        } else {
            None
        },
        can_move_files: Some(removable),
        can_remove: Some(removable),
        removed: Some(false),
        raw_state: Some(torrent.status.join(",")),
        completed_at: torrent.date_finished.map(|value| value.to_string()),
    }
}

fn torrent_to_completed(
    config: &FloodConfig,
    hash: String,
    torrent: FloodTorrent,
) -> Result<PluginCompletedDownload, Error> {
    let content_paths = get_contents(config, &hash)?;
    let dest_dir = derive_import_path(&torrent, &content_paths);
    Ok(PluginCompletedDownload {
        client_item_id: normalize_hash(&hash),
        download_id: None,
        info_hash: Some(normalize_hash(&hash)),
        name: torrent.name,
        dest_dir: dest_dir.clone(),
        category: torrent.tags.first().cloned(),
        output_kind: Some(if content_paths.len() == 1 {
            PluginDownloadOutputKind::File
        } else {
            PluginDownloadOutputKind::Directory
        }),
        content_paths: if content_paths.is_empty() {
            vec![dest_dir]
        } else {
            content_paths
                .into_iter()
                .map(|path| format!("{}/{}", torrent.directory.trim_end_matches('/'), path))
                .collect()
        },
        size_bytes: Some(torrent.size_bytes),
        completed_at: torrent.date_finished.map(|value| value.to_string()),
        parameters: Vec::new(),
    })
}

fn derive_import_path(torrent: &FloodTorrent, content_paths: &[String]) -> String {
    if content_paths.len() == 1 {
        format!(
            "{}/{}",
            torrent.directory.trim_end_matches('/'),
            content_paths[0].trim_start_matches('/')
        )
    } else if let Some(first_segment) = content_paths
        .iter()
        .find_map(|path| path.split(['\\', '/']).find(|segment| !segment.is_empty()))
    {
        if content_paths.iter().all(|path| {
            path.split(['\\', '/']).find(|segment| !segment.is_empty()) == Some(first_segment)
        }) {
            format!(
                "{}/{}",
                torrent.directory.trim_end_matches('/'),
                first_segment
            )
        } else {
            torrent.directory.clone()
        }
    } else {
        torrent.directory.clone()
    }
}

fn map_state(torrent: &FloodTorrent) -> DownloadItemState {
    let status = torrent.status.join(",");
    if status.contains("seeding") || status.contains("complete") {
        DownloadItemState::Completed
    } else if status.contains("stopped") {
        DownloadItemState::Paused
    } else if status.contains("error") {
        DownloadItemState::Warning
    } else if status.contains("downloading") {
        DownloadItemState::Downloading
    } else {
        DownloadItemState::Queued
    }
}

fn is_completed(torrent: &FloodTorrent) -> bool {
    let status = torrent.status.join(",");
    status.contains("seeding") || status.contains("complete")
}

fn can_remove(
    config: &FloodConfig,
    hash: &str,
    torrent: &FloodTorrent,
    state: DownloadItemState,
) -> bool {
    if state != DownloadItemState::Completed || config.post_import_tags.is_empty() {
        return false;
    }

    let Some(seed_config) = seed_config(hash) else {
        return false;
    };

    if seed_config
        .ratio
        .is_some_and(|ratio| torrent.ratio >= ratio)
    {
        return true;
    }

    if let (Some(finished), Some(seed_time)) =
        (torrent.date_finished, seed_config.seed_time_seconds)
        && now_unix_seconds().saturating_sub(finished) >= seed_time
    {
        return true;
    }

    false
}

fn matches_scope(config: &FloodConfig, torrent: &FloodTorrent) -> bool {
    config.tags.iter().all(|tag| torrent.tags.contains(tag))
        && !config
            .post_import_tags
            .iter()
            .all(|tag| torrent.tags.contains(tag))
}

fn store_seed_config(hash: &str, request: &PluginDownloadClientAddRequest) -> Result<(), Error> {
    let seed_config = FloodSeedConfig {
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

fn seed_config(hash: &str) -> Option<FloodSeedConfig> {
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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

fn config_list(key: &str, default: &[&str]) -> Vec<String> {
    config_value(key)
        .map(|value| {
            value
                .split([',', ';', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| default.iter().map(|value| (*value).to_string()).collect())
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let normalized = value.to_ascii_lowercase();
        if seen.insert(normalized) {
            out.push(value);
        }
    }
    out
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

fn additional_tags_field() -> ConfigFieldDef {
    ConfigFieldDef {
        key: "additional_tags".to_string(),
        label: "Additional Tags".to_string(),
        field_type: ConfigFieldType::Tag,
        required: false,
        default_value: None,
        value_source: Default::default(),
        host_binding: None,
        role: None,
        options: vec![
            ConfigFieldOption {
                value: "title_slug".to_string(),
                label: "Title Slug".to_string(),
            },
            ConfigFieldOption {
                value: "title_tags".to_string(),
                label: "Title Tags".to_string(),
            },
            ConfigFieldOption {
                value: "year".to_string(),
                label: "Year".to_string(),
            },
            ConfigFieldOption {
                value: "indexer".to_string(),
                label: "Indexer".to_string(),
            },
            ConfigFieldOption {
                value: "languages".to_string(),
                label: "Language".to_string(),
            },
            ConfigFieldOption {
                value: "network".to_string(),
                label: "Network".to_string(),
            },
        ],
        help_text: Some("Metadata-derived tags added to new torrents".to_string()),
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
