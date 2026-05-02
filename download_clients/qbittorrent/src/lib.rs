use std::collections::{BTreeSet, HashMap, HashSet};

use base64::{engine::general_purpose, Engine as _};
use extism_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadItem, PluginDownloadOutputKind, PluginError,
    PluginErrorCode, PluginResult, PluginTorrentContentLayout, PluginTorrentInitialState,
    PluginTorrentItem, ProviderDescriptor, SDK_VERSION,
};
use serde::Deserialize;
use sha1::{Digest, Sha1};

const COOKIE_VAR_KEY: &str = "qbittorrent.sid";
const IMPORTED_TAG_DEFAULT: &str = "scryer:imported";

fn plugin_error<T>(code: PluginErrorCode, public_message: impl Into<String>) -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
    })
}

fn plugin_error_response<T: serde::Serialize>(
    code: PluginErrorCode,
    public_message: impl Into<String>,
) -> FnResult<String> {
    Ok(serde_json::to_string(&plugin_error::<T>(
        code,
        public_message,
    ))?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutingMode {
    Category,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostImportAction {
    Retain,
    TagImported,
    Remove,
    RemoveWithData,
}

#[derive(Debug, Clone)]
struct QbittorrentConfig {
    webui_url: String,
    api_root: String,
    username: String,
    password: String,
    routing_mode: RoutingMode,
    static_tags: Vec<String>,
    auto_tmm: bool,
    start_paused: bool,
    force_start: bool,
    skip_checking: bool,
    imported_tag: String,
    post_import_action: PostImportAction,
}

#[derive(Debug, Default, Deserialize)]
struct QbTorrent {
    hash: String,
    name: String,
    state: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    content_path: Option<String>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    total_size: Option<i64>,
    #[serde(default)]
    amount_left: Option<i64>,
    #[serde(default)]
    eta: Option<i64>,
    #[serde(default)]
    progress: Option<f64>,
    #[serde(default)]
    completion_on: Option<i64>,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    uploaded: Option<i64>,
    #[serde(default)]
    downloaded: Option<i64>,
    #[serde(default)]
    upspeed: Option<i64>,
    #[serde(default)]
    dlspeed: Option<i64>,
    #[serde(default)]
    ratio: Option<f64>,
    #[serde(default)]
    seeding_time: Option<i64>,
    #[serde(default)]
    private: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct QbPreferences {
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    auto_tmm_enabled: Option<bool>,
    #[serde(default)]
    queueing_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct QbCategory {
    #[serde(default, rename = "savePath")]
    save_path: Option<String>,
}

#[plugin_fn]
pub fn scryer_describe(_input: String) -> FnResult<String> {
    Ok(build_descriptor_json()?)
}

fn build_descriptor_json() -> Result<String, Error> {
    let descriptor = PluginDescriptor {
        id: "qbittorrent".to_string(),
        name: "qBittorrent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "qbittorrent".to_string(),
            provider_aliases: vec!["qbit".to_string()],
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
                DownloadIsolationMode::Category,
                DownloadIsolationMode::Tag,
                DownloadIsolationMode::Directory,
            ],
            capabilities: DownloadClientCapabilities {
                pause: true,
                resume: true,
                remove: true,
                remove_with_data: true,
                mark_imported: true,
                prepare_for_import: false,
                client_status: true,
                queue_priority: false,
                seed_limits: true,
                start_paused: true,
                force_start: true,
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
                        DownloadIsolationMode::Category,
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Directory,
                    ],
                    post_import_isolation_modes: vec![
                        DownloadIsolationMode::Category,
                        DownloadIsolationMode::Tag,
                    ],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: true,
                    supports_start_paused: true,
                    supports_force_start: true,
                    supports_sequential_download: true,
                    supports_first_last_piece_priority: true,
                    supports_content_layout: true,
                    supports_skip_checking: true,
                    supports_auto_management: true,
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
    let config = match QbittorrentConfig::from_extism() {
        Ok(config) => config,
        Err(err) => {
            return plugin_error_response::<PluginDownloadClientAddResponse>(
                PluginErrorCode::InvalidConfig,
                err.to_string(),
            );
        }
    };
    let response = handle_download_add(config, request)?;
    Ok(serde_json::to_string(&response)?)
}

fn handle_download_add(
    config: QbittorrentConfig,
    request: PluginDownloadClientAddRequest,
) -> Result<PluginResult<PluginDownloadClientAddResponse>, Error> {
    let expected_hash = derive_expected_hash(&request);
    let tags = build_tags(&config, &request);
    let download_directory = normalize_non_empty(request.routing.download_directory.clone());
    let auto_tmm = if download_directory.is_some() {
        false
    } else {
        config.auto_tmm
    };

    let prepared_request =
        if let Some(torrent_bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
            let bytes = match general_purpose::STANDARD.decode(torrent_bytes_base64) {
                Ok(bytes) => bytes,
                Err(err) => {
                    return Ok(plugin_error(
                        PluginErrorCode::Permanent,
                        format!("invalid torrent_bytes_base64: {err}"),
                    ));
                }
            };
            let file_name = derive_torrent_filename(
                request.source.source_title.as_deref(),
                &request.title.title_name,
            );
            let body = build_add_multipart_body(
                &file_name,
                &bytes,
                AddOptions {
                    category: category_for_add(&config, &request),
                    tags: tags_to_csv(&tags),
                    savepath: download_directory.clone(),
                    ratio_limit: request
                        .torrent
                        .as_ref()
                        .and_then(|torrent| torrent.seed_goal_ratio)
                        .or(request.release.seed_goal_ratio),
                    seeding_time_limit_minutes: request
                        .torrent
                        .as_ref()
                        .and_then(|torrent| torrent.seed_goal_seconds)
                        .or(request.release.seed_goal_seconds)
                        .and_then(seconds_to_minutes),
                    auto_tmm: request_auto_tmm(&config, &request, auto_tmm),
                    paused: request_paused(&config, &request),
                    stop_condition: None,
                    content_layout: request_content_layout(&request),
                    skip_checking: request_skip_checking(&config, &request),
                    sequential_download: request_sequential_download(&request),
                    first_last_piece_prio: request_first_last_piece_prio(&request),
                    force_start: request_force_start(&config, &request),
                },
            );
            PreparedAddRequest::Multipart(body)
        } else {
            let Some(source_value) = (match request.source.kind {
                DownloadInputKind::MagnetUri => request
                    .source
                    .magnet_uri
                    .clone()
                    .or_else(|| request.source.download_url.clone()),
                DownloadInputKind::TorrentFile
                | DownloadInputKind::TorrentUrl
                | DownloadInputKind::TorrentBytes => request
                    .source
                    .torrent_url
                    .clone()
                    .or_else(|| request.source.download_url.clone())
                    .or_else(|| request.source.magnet_uri.clone()),
                DownloadInputKind::Nzb | DownloadInputKind::NzbUrl => request
                    .source
                    .magnet_uri
                    .clone()
                    .or_else(|| request.source.download_url.clone()),
            }) else {
                return Ok(plugin_error(
                    PluginErrorCode::Permanent,
                    "download source is missing",
                ));
            };

            let mut form_fields = vec![("urls".to_string(), source_value)];
            maybe_push_field(
                &mut form_fields,
                "category",
                category_for_add(&config, &request),
            );
            maybe_push_field(&mut form_fields, "tags", tags_to_csv(&tags));
            maybe_push_field(&mut form_fields, "savepath", download_directory);
            maybe_push_field(
                &mut form_fields,
                "ratioLimit",
                request
                    .torrent
                    .as_ref()
                    .and_then(|torrent| torrent.seed_goal_ratio)
                    .or(request.release.seed_goal_ratio)
                    .map(float_to_string),
            );
            maybe_push_field(
                &mut form_fields,
                "seedingTimeLimit",
                request
                    .torrent
                    .as_ref()
                    .and_then(|torrent| torrent.seed_goal_seconds)
                    .or(request.release.seed_goal_seconds)
                    .and_then(seconds_to_minutes)
                    .map(|value| value.to_string()),
            );
            if request_auto_tmm(&config, &request, auto_tmm) {
                form_fields.push(("autoTMM".to_string(), "true".to_string()));
            }
            if request_paused(&config, &request) {
                form_fields.push(("paused".to_string(), "true".to_string()));
            }
            if request_skip_checking(&config, &request) {
                form_fields.push(("skip_checking".to_string(), "true".to_string()));
            }
            if request_force_start(&config, &request) {
                form_fields.push(("forceStart".to_string(), "true".to_string()));
            }
            maybe_push_field(
                &mut form_fields,
                "contentLayout",
                request_content_layout(&request),
            );
            if request_sequential_download(&request) {
                form_fields.push(("sequentialDownload".to_string(), "true".to_string()));
            }
            if request_first_last_piece_prio(&request) {
                form_fields.push(("firstLastPiecePrio".to_string(), "true".to_string()));
            }
            PreparedAddRequest::Form(form_fields)
        };

    let before = list_torrents(&config, Some("all"))?;
    let before_hashes: HashSet<String> = before
        .iter()
        .map(|torrent| normalize_hash(&torrent.hash))
        .collect();

    match prepared_request {
        PreparedAddRequest::Multipart(body) => {
            post_multipart(&config, "/torrents/add", &body.content_type, body.body)?;
        }
        PreparedAddRequest::Form(form_fields) => {
            post_form(&config, "/torrents/add", &form_fields)?;
        }
    }

    let hash = resolve_added_hash(&config, &request, &before_hashes, expected_hash)?;
    let response = PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    };
    Ok(PluginResult::Ok(response))
}

#[plugin_fn]
pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_extism()?;
    let torrents = list_torrents(&config, Some("all"))?;
    let items = torrents
        .into_iter()
        .map(torrent_to_item)
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

#[plugin_fn]
pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_extism()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        completed_downloads(&config)?,
    ))?)
}

fn completed_downloads(config: &QbittorrentConfig) -> Result<Vec<PluginCompletedDownload>, Error> {
    let torrents = list_torrents(&config, Some("completed"))?;
    Ok(torrents
        .into_iter()
        .filter(|torrent| is_completed_state(&torrent.state))
        .filter_map(torrent_to_completed_download)
        .collect::<Vec<_>>())
}

fn completed_history_items(config: &QbittorrentConfig) -> Result<Vec<PluginDownloadItem>, Error> {
    let torrents = list_torrents(&config, Some("completed"))?;
    Ok(torrents
        .into_iter()
        .filter(|torrent| is_completed_state(&torrent.state))
        .map(torrent_to_item)
        .collect::<Vec<_>>())
}

#[plugin_fn]
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_extism()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        completed_history_items(&config)?,
    ))?)
}

#[plugin_fn]
pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&handle_download_control(request)?)?)
}

fn handle_download_control(
    request: PluginDownloadClientControlRequest,
) -> Result<PluginResult<()>, Error> {
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Ok(PluginResult::Err(PluginError {
            code: PluginErrorCode::Permanent,
            public_message: "client_item_id is required".to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }));
    }
    if matches!(request.action, DownloadControlAction::ForceStart) {
        return Ok(PluginResult::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "unsupported control action: force_start".to_string(),
            debug_message: None,
            retry_after_seconds: None,
        }));
    }

    let config = QbittorrentConfig::from_extism()?;

    match request.action {
        DownloadControlAction::Pause => {
            post_form(&config, "/torrents/pause", &[("hashes".to_string(), hash)])?
        }
        DownloadControlAction::Resume => {
            post_form(&config, "/torrents/resume", &[("hashes".to_string(), hash)])?
        }
        DownloadControlAction::Remove => post_form(
            &config,
            "/torrents/delete",
            &[
                ("hashes".to_string(), hash),
                (
                    "deleteFiles".to_string(),
                    if request.remove_data {
                        "true".to_string()
                    } else {
                        "false".to_string()
                    },
                ),
            ],
        )?,
        DownloadControlAction::ForceStart => unreachable!("handled before config lookup"),
    }

    Ok(PluginResult::Ok(()))
}

#[plugin_fn]
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    let hash = normalize_hash(
        &request
            .info_hash
            .clone()
            .unwrap_or_else(|| request.client_item_id.clone()),
    );
    if hash.is_empty() {
        return plugin_error_response::<()>(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        );
    }

    let config = QbittorrentConfig::from_extism()?;

    if !torrent_exists(&config, &hash)? {
        return Ok(serde_json::to_string(&PluginResult::Ok(()))?);
    }

    apply_post_import_isolation(&config, &hash, &request)?;

    match config.post_import_action {
        PostImportAction::Retain => {}
        PostImportAction::TagImported => {
            create_tag_if_missing(&config, &config.imported_tag)?;
            post_form(
                &config,
                "/torrents/addTags",
                &[
                    ("hashes".to_string(), hash.clone()),
                    ("tags".to_string(), config.imported_tag.clone()),
                ],
            )?;
        }
        PostImportAction::Remove => {
            post_form(
                &config,
                "/torrents/delete",
                &[
                    ("hashes".to_string(), hash.clone()),
                    ("deleteFiles".to_string(), "false".to_string()),
                ],
            )?;
        }
        PostImportAction::RemoveWithData => {
            post_form(
                &config,
                "/torrents/delete",
                &[
                    ("hashes".to_string(), hash.clone()),
                    ("deleteFiles".to_string(), "true".to_string()),
                ],
            )?;
        }
    }

    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

#[plugin_fn]
pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_extism()?;
    let version = get_text(&config, "/app/version")?;
    let preferences: QbPreferences = get_json(&config, "/app/preferences")?;
    let categories: HashMap<String, QbCategory> = get_json(&config, "/torrents/categories")?;

    let mut roots = BTreeSet::new();
    if let Some(root) = normalize_non_empty(preferences.save_path) {
        roots.insert(root);
    }
    for category in categories.values() {
        if let Some(root) = normalize_non_empty(category.save_path.clone()) {
            roots.insert(root);
        }
    }

    let mut warnings = Vec::new();
    if config.auto_tmm {
        warnings.push(
            "automatic torrent management is enabled for this plugin; explicit per-download paths may be ignored"
                .to_string(),
        );
    }
    if !is_localhost_url(&config.webui_url) && roots.is_empty() {
        warnings.push(
            "no remote output roots were discovered; remote import path resolution may require manual path mapping"
                .to_string(),
        );
    }
    if matches!(config.post_import_action, PostImportAction::RemoveWithData) {
        warnings.push(
            "post-import action removes torrent data from qBittorrent after import".to_string(),
        );
    }

    let sorting_mode = match (
        preferences.auto_tmm_enabled.unwrap_or(false),
        preferences.queueing_enabled.unwrap_or(false),
    ) {
        (true, true) => Some("auto_tmm+queueing".to_string()),
        (true, false) => Some("auto_tmm".to_string()),
        (false, true) => Some("queueing".to_string()),
        (false, false) => Some("manual".to_string()),
    };

    let status = PluginDownloadClientStatus {
        version: Some(version),
        is_localhost: Some(is_localhost_url(&config.webui_url)),
        remote_output_roots: roots.into_iter().collect(),
        removes_completed_downloads: Some(matches!(
            config.post_import_action,
            PostImportAction::Remove | PostImportAction::RemoveWithData
        )),
        sorting_mode,
        warnings,
    };

    Ok(serde_json::to_string(&PluginResult::Ok(status))?)
}

#[plugin_fn]
pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_extism()?;
    var::remove(COOKIE_VAR_KEY)?;
    let version = get_text(&config, "/app/version")?;
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

impl QbittorrentConfig {
    fn from_extism() -> Result<Self, Error> {
        let base_url = config::get("base_url")
            .map_err(|e| Error::msg(format!("missing config base_url: {e}")))?
            .unwrap_or_default();
        if base_url.trim().is_empty() {
            return Err(Error::msg("qBittorrent requires base_url"));
        }

        let username = config::get("username")
            .map_err(|e| Error::msg(format!("missing config username: {e}")))?
            .unwrap_or_default();
        let password = config::get("password")
            .map_err(|e| Error::msg(format!("missing config password: {e}")))?
            .unwrap_or_default();
        if username.trim().is_empty() || password.is_empty() {
            return Err(Error::msg("qBittorrent requires username and password"));
        }

        let webui_url = normalize_webui_url(&base_url);
        let api_root = format!("{}/api/v2", webui_url.trim_end_matches('/'));
        let routing_mode = match config::get("routing_mode")
            .ok()
            .flatten()
            .unwrap_or_else(|| "category".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "tag" => RoutingMode::Tag,
            _ => RoutingMode::Category,
        };
        let static_tags = parse_csv(
            &config::get("static_tags")
                .ok()
                .flatten()
                .unwrap_or_default(),
        );
        let auto_tmm = config_bool("auto_tmm", false);
        let start_paused = config_bool("start_paused", false);
        let force_start = config_bool("force_start", false);
        let skip_checking = config_bool("skip_checking", false);
        let imported_tag = config::get("imported_tag")
            .ok()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| IMPORTED_TAG_DEFAULT.to_string());
        let post_import_action = match config::get("post_import_action")
            .ok()
            .flatten()
            .unwrap_or_else(|| "tag_imported".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "retain" => PostImportAction::Retain,
            "remove" => PostImportAction::Remove,
            "remove_with_data" => PostImportAction::RemoveWithData,
            _ => PostImportAction::TagImported,
        };

        Ok(Self {
            webui_url,
            api_root,
            username,
            password,
            routing_mode,
            static_tags,
            auto_tmm,
            start_paused,
            force_start,
            skip_checking,
            imported_tag,
            post_import_action,
        })
    }
}

#[derive(Debug)]
struct MultipartBody {
    content_type: String,
    body: Vec<u8>,
}

#[derive(Debug)]
enum PreparedAddRequest {
    Multipart(MultipartBody),
    Form(Vec<(String, String)>),
}

#[derive(Debug)]
struct AddOptions {
    category: Option<String>,
    tags: Option<String>,
    savepath: Option<String>,
    ratio_limit: Option<f64>,
    seeding_time_limit_minutes: Option<i64>,
    auto_tmm: bool,
    paused: bool,
    stop_condition: Option<String>,
    content_layout: Option<String>,
    skip_checking: bool,
    sequential_download: bool,
    first_last_piece_prio: bool,
    force_start: bool,
}

fn config_fields() -> Vec<ConfigFieldDef> {
    vec![
        ConfigFieldDef {
            key: "username".to_string(),
            label: "Username".to_string(),
            field_type: ConfigFieldType::String,
            required: true,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("qBittorrent WebUI username".to_string()),
        },
        ConfigFieldDef {
            key: "password".to_string(),
            label: "Password".to_string(),
            field_type: ConfigFieldType::Password,
            required: true,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("qBittorrent WebUI password".to_string()),
        },
        ConfigFieldDef {
            key: "routing_mode".to_string(),
            label: "Isolation Routing".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("category".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![
                ConfigFieldOption {
                    value: "category".to_string(),
                    label: "Category".to_string(),
                },
                ConfigFieldOption {
                    value: "tag".to_string(),
                    label: "Tag".to_string(),
                },
            ],
            help_text: Some(
                "Apply Scryer isolation values as qBittorrent categories or tags".to_string(),
            ),
        },
        ConfigFieldDef {
            key: "static_tags".to_string(),
            label: "Static Tags".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Comma-separated tags added to every torrent".to_string()),
        },
        ConfigFieldDef {
            key: "auto_tmm".to_string(),
            label: "Automatic Torrent Management".to_string(),
            field_type: ConfigFieldType::Bool,
            required: false,
            default_value: Some("false".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Enable qBittorrent automatic torrent management unless Scryer provided an explicit download directory"
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "start_paused".to_string(),
            label: "Start Paused".to_string(),
            field_type: ConfigFieldType::Bool,
            required: false,
            default_value: Some("false".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Add torrents in a paused state".to_string()),
        },
        ConfigFieldDef {
            key: "force_start".to_string(),
            label: "Force Start".to_string(),
            field_type: ConfigFieldType::Bool,
            required: false,
            default_value: Some("false".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Force-start torrents after adding them".to_string()),
        },
        ConfigFieldDef {
            key: "skip_checking".to_string(),
            label: "Skip Recheck".to_string(),
            field_type: ConfigFieldType::Bool,
            required: false,
            default_value: Some("false".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some("Skip piece recheck when adding local torrent payloads".to_string()),
        },
        ConfigFieldDef {
            key: "post_import_action".to_string(),
            label: "Post-Import Action".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("tag_imported".to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![
                ConfigFieldOption {
                    value: "tag_imported".to_string(),
                    label: "Tag Imported".to_string(),
                },
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
            help_text: Some("What Scryer should do in qBittorrent after a successful import".to_string()),
        },
        ConfigFieldDef {
            key: "imported_tag".to_string(),
            label: "Imported Tag".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: Some(IMPORTED_TAG_DEFAULT.to_string()),
            value_source: Default::default(),
            host_binding: None,
            options: vec![],
            help_text: Some(
                "Tag applied after import when post-import action is set to Tag Imported"
                    .to_string(),
            ),
        },
    ]
}

fn config_bool(key: &str, default: bool) -> bool {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn normalize_webui_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = trimmed.strip_suffix("/api/v2") {
        stripped.trim_end_matches('/').to_string()
    } else {
        trimmed
    }
}

fn api_url(config: &QbittorrentConfig, path: &str) -> String {
    format!(
        "{}{}",
        config.api_root.trim_end_matches('/'),
        if path.starts_with('/') { path } else { "/" }
    )
}

fn webui_header_url(config: &QbittorrentConfig) -> &str {
    config.webui_url.as_str()
}

fn extract_cookie(response: &HttpResponse) -> Option<String> {
    response
        .headers()
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .and_then(|(_, value)| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn login(config: &QbittorrentConfig) -> Result<String, Error> {
    let body = form_encode(&[
        ("username".to_string(), config.username.clone()),
        ("password".to_string(), config.password.clone()),
    ]);
    let request = HttpRequest::new(api_url(config, "/auth/login"))
        .with_method("POST")
        .with_header("Content-Type", "application/x-www-form-urlencoded")
        .with_header("Referer", webui_header_url(config))
        .with_header("Origin", webui_header_url(config))
        .with_header("User-Agent", "scryer-qbittorrent-plugin/0.1");
    let response = http::request::<Vec<u8>>(&request, Some(body.into_bytes()))
        .map_err(|e| Error::msg(format!("qBittorrent login request failed: {e}")))?;
    if response.status_code() >= 400 {
        let body = String::from_utf8_lossy(&response.body()).to_string();
        return Err(Error::msg(format!(
            "qBittorrent login failed with HTTP {}: {}",
            response.status_code(),
            body
        )));
    }
    let cookie = extract_cookie(&response)
        .ok_or_else(|| Error::msg("qBittorrent login did not return a session cookie"))?;
    let body = String::from_utf8_lossy(&response.body()).trim().to_string();
    if !(body.eq_ignore_ascii_case("ok.") || body.eq_ignore_ascii_case("ok")) {
        return Err(Error::msg(format!(
            "qBittorrent login rejected credentials: {body}"
        )));
    }
    var::set(COOKIE_VAR_KEY, cookie.clone())?;
    Ok(cookie)
}

fn request_with_auth(
    config: &QbittorrentConfig,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<HttpResponse, Error> {
    let cookie = match var::get::<String>(COOKIE_VAR_KEY)? {
        Some(cookie) if !cookie.trim().is_empty() => cookie,
        _ => login(config)?,
    };

    let request = build_request(config, method, path, &cookie, content_type);
    let response = http::request::<Vec<u8>>(&request, body.clone())
        .map_err(|e| Error::msg(format!("qBittorrent request failed: {e}")))?;

    if response.status_code() == 403 {
        var::remove(COOKIE_VAR_KEY)?;
        let cookie = login(config)?;
        let retry = build_request(config, method, path, &cookie, content_type);
        return http::request::<Vec<u8>>(&retry, body)
            .map_err(|e| Error::msg(format!("qBittorrent retry failed: {e}")));
    }

    Ok(response)
}

fn build_request(
    config: &QbittorrentConfig,
    method: &str,
    path: &str,
    cookie: &str,
    content_type: Option<&str>,
) -> HttpRequest {
    let mut request = HttpRequest::new(api_url(config, path))
        .with_method(method)
        .with_header("Cookie", cookie)
        .with_header("Referer", webui_header_url(config))
        .with_header("Origin", webui_header_url(config))
        .with_header("User-Agent", "scryer-qbittorrent-plugin/0.1")
        .with_header("Accept", "application/json, text/plain;q=0.9, */*;q=0.8");
    if let Some(content_type) = content_type {
        request = request.with_header("Content-Type", content_type);
    }
    request
}

fn get_text(config: &QbittorrentConfig, path: &str) -> Result<String, Error> {
    let response = request_with_auth(config, "GET", path, None, None)?;
    ensure_success(path, &response)?;
    Ok(String::from_utf8_lossy(&response.body()).trim().to_string())
}

fn get_json<T: for<'de> Deserialize<'de>>(
    config: &QbittorrentConfig,
    path: &str,
) -> Result<T, Error> {
    let response = request_with_auth(config, "GET", path, None, None)?;
    ensure_success(path, &response)?;
    response
        .json()
        .map_err(|e| Error::msg(format!("invalid qBittorrent JSON from {path}: {e}")))
}

fn post_form(
    config: &QbittorrentConfig,
    path: &str,
    fields: &[(String, String)],
) -> Result<(), Error> {
    let response = request_with_auth(
        config,
        "POST",
        path,
        Some(form_encode(fields).into_bytes()),
        Some("application/x-www-form-urlencoded"),
    )?;
    ensure_success(path, &response)
}

fn post_multipart(
    config: &QbittorrentConfig,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<(), Error> {
    let response = request_with_auth(config, "POST", path, Some(body), Some(content_type))?;
    ensure_success(path, &response)
}

fn ensure_success(path: &str, response: &HttpResponse) -> Result<(), Error> {
    if response.status_code() >= 400 {
        let body = String::from_utf8_lossy(&response.body()).trim().to_string();
        return Err(Error::msg(format!(
            "qBittorrent {} failed with HTTP {}: {}",
            path,
            response.status_code(),
            body
        )));
    }
    Ok(())
}

fn list_torrents(
    config: &QbittorrentConfig,
    filter: Option<&str>,
) -> Result<Vec<QbTorrent>, Error> {
    let mut path = "/torrents/info?sort=added_on&reverse=true".to_string();
    if let Some(filter) = filter {
        path.push_str("&filter=");
        path.push_str(&url_encode(filter));
    }
    get_json(config, &path)
}

fn torrent_exists(config: &QbittorrentConfig, hash: &str) -> Result<bool, Error> {
    let path = format!("/torrents/info?hashes={}", url_encode(hash));
    let torrents: Vec<QbTorrent> = get_json(config, &path)?;
    Ok(!torrents.is_empty())
}

fn resolve_added_hash(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
    before_hashes: &HashSet<String>,
    expected_hash: Option<String>,
) -> Result<String, Error> {
    if let Some(hash) = expected_hash {
        return Ok(hash);
    }

    let expected_names = candidate_names(request);
    for _ in 0..4 {
        let after = list_torrents(config, Some("all"))?;
        if let Some(hash) = discover_hash_candidate(&after, before_hashes, &expected_names) {
            return Ok(hash);
        }
    }

    Err(Error::msg(
        "torrent was added to qBittorrent, but the plugin could not resolve its hash; provide an info-hash hint or magnet URI"
            .to_string(),
    ))
}

fn candidate_names(request: &PluginDownloadClientAddRequest) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(value) = normalize_non_empty(request.release.release_title.clone()) {
        values.push(value);
    }
    if let Some(value) = normalize_non_empty(request.source.source_title.clone()) {
        values.push(value);
    }
    if let Some(value) = normalize_non_empty(Some(request.title.title_name.clone())) {
        values.push(value);
    }
    values
}

fn discover_hash_candidate(
    torrents: &[QbTorrent],
    before_hashes: &HashSet<String>,
    expected_names: &[String],
) -> Option<String> {
    let expected = expected_names
        .iter()
        .map(|value| normalize_title_match(value))
        .collect::<Vec<_>>();

    for torrent in torrents {
        let hash = normalize_hash(&torrent.hash);
        if hash.is_empty() || before_hashes.contains(&hash) {
            continue;
        }
        let torrent_name = normalize_title_match(&torrent.name);
        if expected.contains(&torrent_name) {
            return Some(hash);
        }
    }

    torrents
        .iter()
        .map(|torrent| normalize_hash(&torrent.hash))
        .find(|hash| !hash.is_empty() && !before_hashes.contains(hash))
        .or_else(|| {
            torrents
                .iter()
                .find(|torrent| {
                    let torrent_name = normalize_title_match(&torrent.name);
                    expected.contains(&torrent_name)
                })
                .map(|torrent| normalize_hash(&torrent.hash))
        })
}

fn build_tags(config: &QbittorrentConfig, request: &PluginDownloadClientAddRequest) -> Vec<String> {
    let mut tags = config.static_tags.clone();
    tags.push("scryer-origin".to_string());
    if let Some(title_id) = request.title.title_id.as_deref() {
        tags.push(format!("scryer-title-{}", sanitize_tag_fragment(title_id)));
    }
    tags.push(format!(
        "scryer-facet-{}",
        sanitize_tag_fragment(&request.title.media_facet)
    ));
    for tag in &request.title.tags {
        if let Some(tag) = normalize_non_empty(Some(tag.clone())) {
            tags.push(format!("scryer-tag-{}", sanitize_tag_fragment(&tag)));
        }
    }
    if matches!(config.routing_mode, RoutingMode::Tag) {
        if let Some(isolation) = request.routing.isolation_value.as_deref() {
            tags.push(sanitize_tag_fragment(isolation));
        }
    }
    dedupe(tags)
}

fn request_paused(config: &QbittorrentConfig, request: &PluginDownloadClientAddRequest) -> bool {
    match request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.initial_state)
    {
        Some(PluginTorrentInitialState::Paused | PluginTorrentInitialState::Stopped) => true,
        Some(PluginTorrentInitialState::Started) => false,
        Some(PluginTorrentInitialState::Default) | None => config.start_paused,
    }
}

fn request_force_start(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.force_start)
        .unwrap_or(config.force_start)
}

fn request_skip_checking(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.skip_checking)
        .unwrap_or(config.skip_checking)
}

fn request_auto_tmm(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
    fallback: bool,
) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.auto_management)
        .unwrap_or_else(|| {
            if request.routing.download_directory.is_some() {
                false
            } else {
                fallback && config.auto_tmm
            }
        })
}

fn request_sequential_download(request: &PluginDownloadClientAddRequest) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.sequential_download)
        .unwrap_or(false)
}

fn request_first_last_piece_prio(request: &PluginDownloadClientAddRequest) -> bool {
    request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.first_last_piece_priority)
        .unwrap_or(false)
}

fn request_content_layout(request: &PluginDownloadClientAddRequest) -> Option<String> {
    match request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.content_layout)
    {
        Some(PluginTorrentContentLayout::Original) => Some("Original".to_string()),
        Some(PluginTorrentContentLayout::Subfolder) => Some("Subfolder".to_string()),
        Some(PluginTorrentContentLayout::NoSubfolder) => Some("NoSubfolder".to_string()),
        Some(PluginTorrentContentLayout::Default) | None => None,
    }
}

fn apply_post_import_isolation(
    config: &QbittorrentConfig,
    hash: &str,
    request: &PluginDownloadClientMarkImportedRequest,
) -> Result<(), Error> {
    let Some(target) = request
        .post_import_isolation
        .iter()
        .find(|entry| {
            matches!(
                (config.routing_mode, entry.mode),
                (RoutingMode::Category, DownloadIsolationMode::Category)
                    | (RoutingMode::Tag, DownloadIsolationMode::Tag)
                    | (RoutingMode::Tag, DownloadIsolationMode::Label)
            )
        })
        .map(|entry| entry.value.trim())
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    match config.routing_mode {
        RoutingMode::Category => post_form(
            config,
            "/torrents/setCategory",
            &[
                ("hashes".to_string(), hash.to_string()),
                ("category".to_string(), target.to_string()),
            ],
        ),
        RoutingMode::Tag => post_form(
            config,
            "/torrents/addTags",
            &[
                ("hashes".to_string(), hash.to_string()),
                ("tags".to_string(), target.to_string()),
            ],
        ),
    }
}

fn category_for_add(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
) -> Option<String> {
    if matches!(config.routing_mode, RoutingMode::Category) {
        return normalize_non_empty(request.routing.isolation_value.clone());
    }
    None
}

fn tags_to_csv(tags: &[String]) -> Option<String> {
    if tags.is_empty() {
        None
    } else {
        Some(tags.join(","))
    }
}

fn maybe_push_field(fields: &mut Vec<(String, String)>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        fields.push((key.to_string(), value));
    }
}

fn float_to_string(value: f64) -> String {
    let mut text = format!("{value:.4}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn seconds_to_minutes(seconds: i64) -> Option<i64> {
    if seconds <= 0 {
        None
    } else {
        Some((seconds + 59) / 60)
    }
}

fn build_add_multipart_body(file_name: &str, bytes: &[u8], options: AddOptions) -> MultipartBody {
    let boundary = "----scryer-qbittorrent-boundary";
    let mut body = Vec::new();
    let ratio_limit = options.ratio_limit.map(float_to_string);
    let seeding_time_limit_minutes = options
        .seeding_time_limit_minutes
        .map(|value| value.to_string());

    append_multipart_text(&mut body, boundary, "savepath", options.savepath.as_deref());
    append_multipart_text(&mut body, boundary, "category", options.category.as_deref());
    append_multipart_text(&mut body, boundary, "tags", options.tags.as_deref());
    append_multipart_text(&mut body, boundary, "ratioLimit", ratio_limit.as_deref());
    append_multipart_text(
        &mut body,
        boundary,
        "seedingTimeLimit",
        seeding_time_limit_minutes.as_deref(),
    );
    if options.auto_tmm {
        append_multipart_text(&mut body, boundary, "autoTMM", Some("true"));
    }
    if options.paused {
        append_multipart_text(&mut body, boundary, "paused", Some("true"));
    }
    if options.skip_checking {
        append_multipart_text(&mut body, boundary, "skip_checking", Some("true"));
    }
    if options.force_start {
        append_multipart_text(&mut body, boundary, "forceStart", Some("true"));
    }
    append_multipart_text(
        &mut body,
        boundary,
        "stopCondition",
        options.stop_condition.as_deref(),
    );
    append_multipart_text(
        &mut body,
        boundary,
        "contentLayout",
        options.content_layout.as_deref(),
    );
    if options.sequential_download {
        append_multipart_text(&mut body, boundary, "sequentialDownload", Some("true"));
    }
    if options.first_last_piece_prio {
        append_multipart_text(&mut body, boundary, "firstLastPiecePrio", Some("true"));
    }

    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"torrents\"; filename=\"{}\"\r\n",
            escape_quotes(file_name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/x-bittorrent\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    MultipartBody {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
    }
}

fn append_multipart_text(body: &mut Vec<u8>, boundary: &str, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", key).as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
}

fn escape_quotes(value: &str) -> String {
    value.replace('"', "")
}

fn form_encode(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn derive_expected_hash(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .release
        .info_hash_v1
        .clone()
        .or_else(|| request.release.info_hash_hint.clone())
        .map(|value| normalize_hash(&value))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            request
                .source
                .magnet_uri
                .as_deref()
                .and_then(parse_magnet_info_hash)
        })
        .or_else(|| {
            request
                .source
                .torrent_bytes_base64
                .as_deref()
                .and_then(|value| general_purpose::STANDARD.decode(value).ok())
                .and_then(|bytes| compute_torrent_info_hash(&bytes).ok())
        })
}

fn parse_magnet_info_hash(uri: &str) -> Option<String> {
    let query = uri.strip_prefix("magnet:?")?;
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("xt=") {
            if let Some(urn) = value.strip_prefix("urn:btih:") {
                return Some(normalize_hash(&percent_decode(urn)));
            }
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[idx + 1..idx + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    idx += 3;
                    continue;
                }
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn compute_torrent_info_hash(bytes: &[u8]) -> Result<String, Error> {
    let (start, end) = find_info_dict_range(bytes)?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes[start..end]);
    Ok(to_lower_hex(&hasher.finalize()))
}

fn find_info_dict_range(bytes: &[u8]) -> Result<(usize, usize), Error> {
    if bytes.first().copied() != Some(b'd') {
        return Err(Error::msg("torrent payload is not a bencoded dictionary"));
    }

    let mut idx = 1usize;
    while idx < bytes.len() {
        if bytes[idx] == b'e' {
            break;
        }
        let (key, next) = parse_bencoded_string(bytes, idx)?;
        let value_start = next;
        let value_end = parse_bencoded_value(bytes, value_start)?;
        if key == b"info" {
            return Ok((value_start, value_end));
        }
        idx = value_end;
    }

    Err(Error::msg(
        "torrent payload is missing top-level info dictionary",
    ))
}

fn parse_bencoded_string(bytes: &[u8], start: usize) -> Result<(&[u8], usize), Error> {
    let mut idx = start;
    while idx < bytes.len() && bytes[idx] != b':' {
        if !bytes[idx].is_ascii_digit() {
            return Err(Error::msg("invalid bencoded string length"));
        }
        idx += 1;
    }
    if idx >= bytes.len() {
        return Err(Error::msg("unterminated bencoded string length"));
    }
    let len = std::str::from_utf8(&bytes[start..idx])
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| Error::msg("invalid bencoded string length"))?;
    let data_start = idx + 1;
    let data_end = data_start + len;
    if data_end > bytes.len() {
        return Err(Error::msg("bencoded string exceeds torrent payload length"));
    }
    Ok((&bytes[data_start..data_end], data_end))
}

fn parse_bencoded_value(bytes: &[u8], start: usize) -> Result<usize, Error> {
    if start >= bytes.len() {
        return Err(Error::msg("unexpected end of torrent payload"));
    }

    match bytes[start] {
        b'i' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx += 1;
            }
            if idx >= bytes.len() {
                return Err(Error::msg("unterminated bencoded integer"));
            }
            Ok(idx + 1)
        }
        b'l' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                idx = parse_bencoded_value(bytes, idx)?;
            }
            if idx >= bytes.len() {
                return Err(Error::msg("unterminated bencoded list"));
            }
            Ok(idx + 1)
        }
        b'd' => {
            let mut idx = start + 1;
            while idx < bytes.len() && bytes[idx] != b'e' {
                let (_, next) = parse_bencoded_string(bytes, idx)?;
                idx = parse_bencoded_value(bytes, next)?;
            }
            if idx >= bytes.len() {
                return Err(Error::msg("unterminated bencoded dictionary"));
            }
            Ok(idx + 1)
        }
        b'0'..=b'9' => parse_bencoded_string(bytes, start).map(|(_, end)| end),
        _ => Err(Error::msg("unsupported bencoded token in torrent payload")),
    }
}

fn to_lower_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + (value - 10)) as char,
        _ => '0',
    }
}

fn torrent_to_item(torrent: QbTorrent) -> PluginDownloadItem {
    let state = map_state(&torrent.state);
    let category = normalize_non_empty(torrent.category.clone());
    let remote_output_path = preferred_content_path(&torrent);
    let content_paths = remote_output_path.clone().into_iter().collect::<Vec<_>>();
    let progress_percent = torrent
        .progress
        .map(|value| (value * 100.0).round().clamp(0.0, 100.0) as u8)
        .or_else(|| {
            if is_completed_state(&torrent.state) {
                Some(100)
            } else {
                None
            }
        });
    let raw_state = normalize_non_empty(Some(torrent.state.clone()));
    PluginDownloadItem {
        client_item_id: normalize_hash(&torrent.hash),
        info_hash: Some(normalize_hash(&torrent.hash)),
        title: torrent.name,
        state,
        message: state_message(&torrent.state),
        category,
        remote_output_path,
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(normalize_hash(&torrent.hash)),
            info_hash_v2: None,
            client_native_id: Some(torrent.hash.clone()),
            tags: parse_csv(torrent.tags.as_deref().unwrap_or_default()),
            labels: Vec::new(),
            categories: torrent.category.iter().cloned().collect(),
            views: Vec::new(),
            save_path: normalize_non_empty(torrent.save_path.clone()),
            content_paths,
            uploaded_bytes: positive_i64(torrent.uploaded),
            downloaded_bytes: positive_i64(torrent.downloaded),
            upload_rate_bytes_per_second: positive_i64(torrent.upspeed),
            download_rate_bytes_per_second: positive_i64(torrent.dlspeed),
            seed_ratio: torrent
                .ratio
                .filter(|value| value.is_finite() && *value >= 0.0),
            seed_time_seconds: positive_i64(torrent.seeding_time),
            metadata_only: Some(false),
            is_encrypted: None,
            is_private: torrent.private,
            raw_status: raw_state.clone(),
            status_reason: state_message(&torrent.state),
        }),
        total_size_bytes: torrent.total_size.or(torrent.size),
        remaining_size_bytes: torrent.amount_left,
        eta_seconds: positive_i64(torrent.eta),
        progress_percent,
        can_move_files: Some(is_completed_state(&torrent.state)),
        can_remove: Some(true),
        removed: Some(false),
        raw_state,
        completed_at: unix_to_rfc3339(torrent.completion_on),
    }
}

fn torrent_to_completed_download(torrent: QbTorrent) -> Option<PluginCompletedDownload> {
    let hash = normalize_hash(&torrent.hash);
    if hash.is_empty() {
        return None;
    }
    let dest_dir = derive_completed_dest_dir(&torrent)?;
    let content_paths = preferred_content_path(&torrent)
        .into_iter()
        .collect::<Vec<_>>();
    let output_kind = match content_paths.first() {
        Some(path) if path_looks_like_file(path) => PluginDownloadOutputKind::File,
        Some(_) => PluginDownloadOutputKind::Directory,
        None => PluginDownloadOutputKind::Unknown,
    };
    Some(PluginCompletedDownload {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
        name: torrent.name.clone(),
        dest_dir,
        category: normalize_non_empty(torrent.category.clone()),
        output_kind: Some(output_kind),
        content_paths,
        size_bytes: torrent.total_size.or(torrent.size),
        completed_at: unix_to_rfc3339(torrent.completion_on),
        parameters: parameters_from_tags(torrent.tags.as_deref()),
    })
}

fn parameters_from_tags(tags: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for tag in parse_csv(tags.unwrap_or_default()) {
        if tag == "scryer-origin" {
            continue;
        }
        if let Some(title_id) = tag.strip_prefix("scryer-title-") {
            out.push(("*scryer_title_id".to_string(), title_id.to_string()));
        } else if let Some(facet) = tag.strip_prefix("scryer-facet-") {
            out.push(("*scryer_facet".to_string(), facet.to_string()));
        }
    }
    out
}

fn parse_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let normalized = value.trim().to_ascii_lowercase();
        if !normalized.is_empty() && seen.insert(normalized) {
            out.push(value.trim().to_string());
        }
    }
    out
}

fn sanitize_tag_fragment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, ':' | '-' | '_' | '.') {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || matches!(ch, '/' | '\\') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn normalize_title_match(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn derive_torrent_filename(source_title: Option<&str>, title_name: &str) -> String {
    let candidate = source_title
        .and_then(|value| normalize_non_empty(Some(value.to_string())))
        .unwrap_or_else(|| title_name.to_string());
    if candidate.to_ascii_lowercase().ends_with(".torrent") {
        candidate
    } else {
        format!("{candidate}.torrent")
    }
}

fn derive_completed_dest_dir(torrent: &QbTorrent) -> Option<String> {
    let content_path = normalize_non_empty(torrent.content_path.clone());
    let save_path = normalize_non_empty(torrent.save_path.clone());
    match (content_path, save_path) {
        (Some(content_path), Some(save_path)) => {
            if path_looks_like_file(&content_path) {
                Some(save_path)
            } else {
                Some(content_path)
            }
        }
        (Some(content_path), None) => Some(content_path),
        (None, Some(save_path)) => Some(save_path),
        (None, None) => None,
    }
}

/// Detect whether a qBittorrent content_path points to a single file (as
/// opposed to a directory created for a multi-file torrent).
///
/// Scene release names like `Show.S01E02.2160p.WEB.h265-GROUP` are full of
/// dots but are directories, so we check for a *known media file extension*
/// rather than just "contains a dot".
fn path_looks_like_file(path: &str) -> bool {
    const FILE_EXTENSIONS: &[&str] = &[
        // video
        "mkv", "mp4", "avi", "wmv", "mov", "m4v", "ts", "m2ts", "webm", "flv", "ogv",
        // archive
        "rar", "zip", "7z", // audio (for music torrents)
        "flac", "mp3", "ogg", "wav", "aac", "m4a", // subtitle
        "srt", "ass", "ssa", "sub", "idx", "sup",
        // other single-file types qBittorrent may report
        "iso", "img", "nzb", "torrent",
    ];
    let trimmed = path.trim_end_matches('/');
    let last_segment = match trimmed.rsplit('/').next() {
        Some(s) => s,
        None => return false,
    };
    // Extract extension after the *last* dot
    let ext = match last_segment.rsplit('.').next() {
        Some(e) => e,
        None => return false,
    };
    // Must actually have a dot (rsplit returns the whole string if no dot)
    if ext == last_segment {
        return false;
    }
    FILE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

fn preferred_content_path(torrent: &QbTorrent) -> Option<String> {
    normalize_non_empty(torrent.content_path.clone())
        .or_else(|| normalize_non_empty(torrent.save_path.clone()))
}

fn positive_i64(value: Option<i64>) -> Option<i64> {
    value.filter(|value| *value >= 0)
}

fn unix_to_rfc3339(value: Option<i64>) -> Option<String> {
    let value = value?;
    if value <= 0 {
        return None;
    }
    Some(format_unix_timestamp(value))
}

fn format_unix_timestamp(value: i64) -> String {
    chrono_like_rfc3339(value)
}

fn chrono_like_rfc3339(timestamp: i64) -> String {
    // qBittorrent timestamps are unix seconds.
    // Keep this implementation dependency-free for the plugin crate.
    let secs = timestamp;
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d)
}

fn map_state(state: &str) -> DownloadItemState {
    match state.trim().to_ascii_lowercase().as_str() {
        "queueddl" => DownloadItemState::Queued,
        "pauseddl" => DownloadItemState::Paused,
        "metadl" | "forcedmetadl" | "stalleddl" | "forceddl" | "downloading" | "allocating" => {
            DownloadItemState::Downloading
        }
        "checkingup" | "checkingdl" | "checkingresumedata" => DownloadItemState::Verifying,
        "moving" => DownloadItemState::ImportPending,
        "pausedup" | "queuedup" | "stalledup" | "uploading" | "forcedup" => {
            DownloadItemState::Completed
        }
        "error" | "missingfiles" => DownloadItemState::Failed,
        "unknown" => DownloadItemState::Error,
        _ => DownloadItemState::Warning,
    }
}

fn state_message(state: &str) -> Option<String> {
    match state.trim().to_ascii_lowercase().as_str() {
        "missingfiles" => Some("qBittorrent reports missing files".to_string()),
        "error" => Some("qBittorrent reports a torrent error".to_string()),
        "moving" => Some("qBittorrent is moving torrent files".to_string()),
        _ => None,
    }
}

fn is_completed_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "pausedup" | "queuedup" | "stalledup" | "uploading" | "forcedup"
    )
}

fn create_tag_if_missing(config: &QbittorrentConfig, tag: &str) -> Result<(), Error> {
    if tag.trim().is_empty() {
        return Ok(());
    }
    post_form(
        config,
        "/torrents/createTags",
        &[("tags".to_string(), tag.to_string())],
    )
}

fn is_localhost_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1") || lower.contains("://[::1]")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_download_client() {
        let json = build_descriptor_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"]["kind"], "download_client");
        assert_eq!(value["provider"]["provider_type"], "qbittorrent");
        assert_eq!(value["provider"]["accepted_inputs"][0], "magnet_uri");
        assert_eq!(
            value["provider"]["capabilities"]["torrent"]["supported_sources"][1],
            "torrent_url"
        );
        assert_eq!(
            value["provider"]["capabilities"]["torrent"]["supports_post_import_isolation"],
            true
        );
    }

    #[test]
    fn v11_add_request_fields_deserialize() {
        let json = r#"{
            "source":{
                "kind":"torrent_bytes",
                "torrent_bytes_base64":"dG9ycmVudA==",
                "torrent_url":"https://tracker.example/release.torrent",
                "torrent_file_name":"release.torrent",
                "torrent_content_type":"application/x-bittorrent"
            },
            "release":{
                "release_title":"Example",
                "info_hash_hint":"abcdef0123456789abcdef0123456789abcdef01",
                "info_hash_v1":"abcdef0123456789abcdef0123456789abcdef01"
            },
            "title":{
                "title_name":"Example",
                "media_facet":"series",
                "tags":[]
            },
            "routing":{
                "isolation_value":"series",
                "isolation":[{"mode":"category","value":"series"}],
                "post_import_isolation":[{"mode":"tag","value":"imported"}]
            },
            "torrent":{
                "source_preference":["torrent_bytes","torrent_url"],
                "sequential_download":true,
                "first_last_piece_priority":true,
                "content_layout":"subfolder",
                "skip_checking":true
            }
        }"#;

        let request: PluginDownloadClientAddRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.source.kind, DownloadInputKind::TorrentBytes);
        assert_eq!(
            request
                .torrent
                .as_ref()
                .and_then(|torrent| torrent.content_layout),
            Some(PluginTorrentContentLayout::Subfolder)
        );
        assert_eq!(request.routing.post_import_isolation.len(), 1);
    }

    #[test]
    fn magnet_hash_is_extracted() {
        let hash = parse_magnet_info_hash(
            "magnet:?xt=urn:btih:ABCDEF1234567890ABCDEF1234567890ABCDEF12&dn=Example",
        )
        .unwrap();
        assert_eq!(hash, "abcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn percent_decoder_handles_hex_sequences() {
        assert_eq!(percent_decode("Hello%20World"), "Hello World");
    }

    #[test]
    fn torrent_info_hash_is_computed_from_info_dict() {
        let torrent = b"d8:announce14:http://tracker4:infod6:lengthi12345e4:name8:test.txt12:piece lengthi262144e6:pieces20:12345678901234567890ee";
        let hash = compute_torrent_info_hash(torrent).unwrap();
        assert_eq!(hash.len(), 40);
    }

    #[test]
    fn state_mapping_handles_completed_states() {
        assert_eq!(map_state("pausedUP"), DownloadItemState::Completed);
        assert_eq!(map_state("moving"), DownloadItemState::ImportPending);
        assert_eq!(map_state("missingFiles"), DownloadItemState::Failed);
    }

    #[test]
    fn completed_dest_dir_prefers_save_path_for_single_file() {
        let torrent = QbTorrent {
            name: "Movie".to_string(),
            save_path: Some("/downloads/movies".to_string()),
            content_path: Some("/downloads/movies/Movie.mkv".to_string()),
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some("/downloads/movies")
        );
    }

    #[test]
    fn completed_dest_dir_uses_content_path_for_directory_torrent() {
        let torrent = QbTorrent {
            name: "Series".to_string(),
            save_path: Some("/downloads/tv".to_string()),
            content_path: Some("/downloads/tv/Series Season 01".to_string()),
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some("/downloads/tv/Series Season 01")
        );
    }

    #[test]
    fn completed_download_reports_output_kind_for_single_file() {
        let torrent = QbTorrent {
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            save_path: Some("/downloads/movies".to_string()),
            content_path: Some("/downloads/movies/Movie.mkv".to_string()),
            ..QbTorrent::default()
        };
        let completed = torrent_to_completed_download(torrent).unwrap();
        assert_eq!(completed.output_kind, Some(PluginDownloadOutputKind::File));
        assert_eq!(
            completed.content_paths,
            vec!["/downloads/movies/Movie.mkv".to_string()]
        );
    }

    #[test]
    fn completed_history_item_uses_download_item_shape() {
        let torrent = QbTorrent {
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            state: "pausedUP".to_string(),
            save_path: Some("/downloads/movies".to_string()),
            content_path: Some("/downloads/movies/Movie.mkv".to_string()),
            ..QbTorrent::default()
        };
        let item = torrent_to_item(torrent);
        assert_eq!(item.title, "Movie");
        assert_eq!(item.state, DownloadItemState::Completed);
        assert_eq!(
            item.remote_output_path.as_deref(),
            Some("/downloads/movies/Movie.mkv")
        );
    }

    #[test]
    fn completed_dest_dir_uses_content_path_for_scene_release_with_dots() {
        // Scene release names like "Show.S01E02.2160p.WEB.h265-GROUP" contain
        // dots but are directories, not files.
        let torrent = QbTorrent {
            name: "Rooster.S01E02.DV.HDR.2160p.WEB.h265-ETHEL".to_string(),
            save_path: Some("/qbit-downloads/tv".to_string()),
            content_path: Some(
                "/qbit-downloads/tv/Rooster.S01E02.DV.HDR.2160p.WEB.h265-ETHEL".to_string(),
            ),
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some("/qbit-downloads/tv/Rooster.S01E02.DV.HDR.2160p.WEB.h265-ETHEL")
        );
    }

    #[test]
    fn path_looks_like_file_detects_video_extension() {
        assert!(path_looks_like_file("/downloads/Movie.mkv"));
        assert!(path_looks_like_file("/downloads/movie.MP4"));
        assert!(path_looks_like_file("/downloads/archive.rar"));
    }

    #[test]
    fn path_looks_like_file_rejects_scene_directory_names() {
        assert!(!path_looks_like_file(
            "/downloads/tv/Show.S01E02.2160p.WEB.h265-GROUP"
        ));
        assert!(!path_looks_like_file(
            "/downloads/Rooster.S01E02.DV.HDR.2160p.WEB.h265-ETHEL"
        ));
    }

    #[test]
    fn internal_tags_round_trip_to_parameters() {
        let parameters =
            parameters_from_tags(Some("scryer-origin,scryer-title-abc123,scryer-facet-anime"));
        assert!(parameters.contains(&("*scryer_title_id".to_string(), "abc123".to_string())));
        assert!(parameters.contains(&("*scryer_facet".to_string(), "anime".to_string())));
    }

    #[test]
    fn form_encoding_escapes_spaces() {
        let encoded = form_encode(&[("savepath".to_string(), "/downloads/Some Show".to_string())]);
        assert!(encoded.contains("savepath=%2Fdownloads%2FSome+Show"));
    }

    #[test]
    fn discovered_hash_prefers_new_name_match() {
        let before = HashSet::from(["aaaa".to_string()]);
        let torrents = vec![
            QbTorrent {
                hash: "aaaa".to_string(),
                name: "Old".to_string(),
                ..QbTorrent::default()
            },
            QbTorrent {
                hash: "bbbb".to_string(),
                name: "Example Release".to_string(),
                ..QbTorrent::default()
            },
        ];
        let hash = discover_hash_candidate(&torrents, &before, &["Example Release".to_string()]);
        assert_eq!(hash.as_deref(), Some("bbbb"));
    }

    #[test]
    fn multipart_body_contains_torrent_file_part() {
        let body = build_add_multipart_body(
            "test.torrent",
            b"abcd",
            AddOptions {
                category: Some("anime".to_string()),
                tags: Some("scryer-origin".to_string()),
                savepath: None,
                ratio_limit: None,
                seeding_time_limit_minutes: None,
                auto_tmm: false,
                paused: true,
                stop_condition: None,
                content_layout: None,
                skip_checking: false,
                sequential_download: false,
                first_last_piece_prio: false,
                force_start: false,
            },
        );
        let text = String::from_utf8_lossy(&body.body);
        assert!(text.contains("filename=\"test.torrent\""));
        assert!(text.contains("name=\"category\""));
        assert!(text.contains("name=\"paused\""));
    }

    #[test]
    fn control_missing_client_item_id_returns_structured_error() {
        match handle_download_control(PluginDownloadClientControlRequest {
            client_item_id: String::new(),
            action: DownloadControlAction::Pause,
            remove_data: false,
            is_history: false,
        })
        .unwrap()
        {
            PluginResult::Err(error) => {
                assert_eq!(error.code, PluginErrorCode::Permanent);
                assert_eq!(error.public_message, "client_item_id is required");
            }
            PluginResult::Ok(()) => panic!("expected structured error"),
        }
    }

    #[test]
    fn control_force_start_returns_structured_unsupported_error() {
        match handle_download_control(PluginDownloadClientControlRequest {
            client_item_id: "abc123".to_string(),
            action: DownloadControlAction::ForceStart,
            remove_data: false,
            is_history: false,
        })
        .unwrap()
        {
            PluginResult::Err(error) => {
                assert_eq!(error.code, PluginErrorCode::Unsupported);
                assert_eq!(
                    error.public_message,
                    "unsupported control action: force_start"
                );
            }
            PluginResult::Ok(()) => panic!("expected structured error"),
        }
    }

    fn test_add_request(kind: DownloadInputKind) -> PluginDownloadClientAddRequest {
        serde_json::from_value(serde_json::json!({
            "source": { "kind": kind },
            "release": { "release_title": "Example Release" },
            "title": {
                "title_name": "Example",
                "media_facet": "movie",
                "tags": []
            },
            "routing": {
                "isolation_value": "movie",
                "isolation": [],
                "post_import_isolation": []
            }
        }))
        .unwrap()
    }

    fn test_config() -> QbittorrentConfig {
        QbittorrentConfig {
            webui_url: "http://localhost:8080".to_string(),
            api_root: "http://localhost:8080/api/v2".to_string(),
            username: "user".to_string(),
            password: "pass".to_string(),
            routing_mode: RoutingMode::Category,
            static_tags: Vec::new(),
            auto_tmm: false,
            start_paused: false,
            force_start: false,
            skip_checking: false,
            imported_tag: IMPORTED_TAG_DEFAULT.to_string(),
            post_import_action: PostImportAction::TagImported,
        }
    }

    #[test]
    fn add_missing_source_returns_structured_error() {
        match handle_download_add(
            test_config(),
            test_add_request(DownloadInputKind::TorrentUrl),
        )
        .unwrap()
        {
            PluginResult::Err(error) => {
                assert_eq!(error.code, PluginErrorCode::Permanent);
                assert_eq!(error.public_message, "download source is missing");
            }
            PluginResult::Ok(_) => panic!("expected structured error"),
        }
    }

    #[test]
    fn add_invalid_torrent_bytes_returns_structured_error() {
        let mut request = test_add_request(DownloadInputKind::TorrentBytes);
        request.source.torrent_bytes_base64 = Some("not-base64".to_string());

        match handle_download_add(test_config(), request).unwrap() {
            PluginResult::Err(error) => {
                assert_eq!(error.code, PluginErrorCode::Permanent);
                assert!(error
                    .public_message
                    .contains("invalid torrent_bytes_base64"));
            }
            PluginResult::Ok(_) => panic!("expected structured error"),
        }
    }
}
