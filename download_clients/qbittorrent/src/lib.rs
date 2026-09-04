use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldType, DownloadClientCapabilities,
    DownloadClientDescriptor, DownloadControlAction, DownloadInputKind, DownloadIsolationMode,
    DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload, PluginDescriptor,
    PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadFeedbackScope, PluginDownloadItem,
    PluginDownloadListRecentCompletedRequest, PluginDownloadOutputKind,
    PluginDownloadScopedListRequest, PluginDownloadScopedListResponse,
    PluginDownloadScopedRecentCompletedRequest, PluginError, PluginErrorCode, PluginResult,
    PluginTorrentContentLayout, PluginTorrentInitialState, PluginTorrentItem, ProviderDescriptor,
    SDK_VERSION,
};
use serde::Deserialize;
use sha1::{Digest, Sha1};

const COOKIE_VAR_KEY: &str = "qbittorrent.sid";
const IMPORTED_TAG_DEFAULT: &str = "scryer:imported";
const ROUTING_CATEGORY_TAG_PREFIX: &str = "scryer:routing-category:";

fn plugin_error<T>(code: PluginErrorCode, public_message: impl Into<String>) -> PluginResult<T> {
    PluginResult::Err(PluginError {
        code,
        public_message: public_message.into(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
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

#[derive(Debug, Clone)]
struct QbittorrentConfig {
    webui_url: String,
    api_root: String,
    api_key: String,
    username: String,
    password: String,
    routing_mode: RoutingMode,
    static_tags: Vec<String>,
    auto_tmm: bool,
    start_paused: bool,
    force_start: bool,
    skip_checking: bool,
    imported_tag: String,
    tag_after_import: bool,
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
    /// Per-torrent share ratio limit. `-2` defers to the global limit, `-1` means unlimited.
    /// Absent on qBittorrent builds that predate the field: treated as "defer to global".
    #[serde(default)]
    ratio_limit: Option<f64>,
    /// Per-torrent seeding time limit in minutes (`-2` global, `-1` unlimited).
    #[serde(default)]
    seeding_time_limit: Option<i64>,
    /// Per-torrent inactive seeding time limit in minutes (`-2` global, `-1` unlimited).
    #[serde(default)]
    inactive_seeding_time_limit: Option<i64>,
    /// Unix seconds of the last piece transfer, used for the inactive seeding limit.
    #[serde(default)]
    last_activity: Option<i64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct QbPreferences {
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    auto_tmm_enabled: Option<bool>,
    #[serde(default)]
    queueing_enabled: Option<bool>,
    #[serde(default)]
    max_ratio_enabled: Option<bool>,
    #[serde(default)]
    max_ratio: Option<f64>,
    #[serde(default)]
    max_seeding_time_enabled: Option<bool>,
    #[serde(default)]
    max_seeding_time: Option<i64>,
    #[serde(default)]
    max_inactive_seeding_time_enabled: Option<bool>,
    #[serde(default)]
    max_inactive_seeding_time: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct QbCategory {
    #[serde(default, rename = "savePath")]
    save_path: Option<String>,
}

pub fn scryer_describe(_input: String) -> FnResult<String> {
    build_descriptor_json()
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
                category_scoped_feedback: true,
                pause: true,
                resume: true,
                remove: true,
                remove_with_data: true,
                mark_imported: true,
                mark_imported_non_destructive: true,
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

pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    let config = match QbittorrentConfig::from_config() {
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

pub fn scryer_download_list_queue(input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
    let scope = scoped_feedback_request(&input).map(|(scope, _)| scope);
    let torrents = list_torrents(&config, Some("all"))?
        .into_iter()
        .filter(|torrent| {
            scope
                .as_ref()
                .is_none_or(|scope| torrent_matches_feedback_scope(&config, scope, torrent))
        })
        .collect::<Vec<_>>();
    let preferences = seed_preferences(&config, &torrents);
    let items = torrents
        .into_iter()
        .map(|torrent| torrent_to_item_with_preferences(torrent, preferences.as_ref()))
        .collect::<Vec<_>>();
    if scope.is_some() {
        return Ok(serde_json::to_string(&PluginResult::Ok(
            PluginDownloadScopedListResponse {
                items,
                failures: Vec::new(),
            },
        ))?);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(items))?)
}

/// Fetches `/app/preferences` once per listing, and only when at least one torrent actually
/// defers to a global seeding limit. Failures are non-fatal: without globals the affected
/// axes report `Unknown`, which maps to `can_remove: None` rather than a guess.
fn seed_preferences(config: &QbittorrentConfig, torrents: &[QbTorrent]) -> Option<QbPreferences> {
    let needs_globals = torrents
        .iter()
        .any(|torrent| is_completed_state(&torrent.state) && defers_to_global_limits(torrent));
    if !needs_globals {
        return None;
    }
    get_json::<QbPreferences>(config, "/app/preferences").ok()
}

/// Only `-2` (or an absent field) defers to the global limits; `-1` means unlimited and
/// never consults them.
fn defers_to_global_limits(torrent: &QbTorrent) -> bool {
    torrent.ratio_limit.is_none_or(|limit| limit == -2.0)
        || torrent.seeding_time_limit.is_none_or(|limit| limit == -2)
        || torrent
            .inactive_seeding_time_limit
            .is_none_or(|limit| limit == -2)
}

pub fn scryer_download_list_completed(input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
    if let Some((scope, _)) = scoped_feedback_request(&input) {
        return Ok(serde_json::to_string(&PluginResult::Ok(
            scoped_completed_downloads(&config, &scope, None)?,
        ))?);
    }
    Ok(serde_json::to_string(&PluginResult::Ok(
        completed_downloads(&config, None)?,
    ))?)
}

pub fn scryer_download_list_recent_completed(input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
    if let Some((scope, limit)) = scoped_feedback_request(&input) {
        return Ok(serde_json::to_string(&PluginResult::Ok(
            scoped_completed_downloads(&config, &scope, limit)?,
        ))?);
    }
    let request: PluginDownloadListRecentCompletedRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        completed_downloads(&config, Some(request.limit))?,
    ))?)
}

fn completed_downloads(
    config: &QbittorrentConfig,
    limit: Option<usize>,
) -> Result<Vec<PluginCompletedDownload>, Error> {
    let torrents = list_completed_torrents(config)?;
    let raw_count = torrents.len();
    let imported_tag_count = torrents
        .iter()
        .filter(|torrent| torrent_has_tag(torrent, &config.imported_tag))
        .count();
    let (downloads, converted_count) = convert_completed_torrents(torrents, limit);
    eprintln!(
        "event=qbittorrent_completed_feedback_poll client=qbittorrent scope=unfiltered \
         raw_count={raw_count} imported_tag_count={imported_tag_count} returned_count={} \
         limit={limit:?} saturated={}",
        downloads.len(),
        limit.is_some_and(|limit| converted_count >= limit)
    );
    Ok(downloads)
}

fn scoped_feedback_request(input: &str) -> Option<(PluginDownloadFeedbackScope, Option<usize>)> {
    let value = serde_json::from_str::<serde_json::Value>(input).ok()?;
    value.get("scope")?;
    if let Ok(request) =
        serde_json::from_value::<PluginDownloadScopedRecentCompletedRequest>(value.clone())
    {
        return Some((request.scope, Some(request.limit)));
    }
    serde_json::from_value::<PluginDownloadScopedListRequest>(value)
        .ok()
        .map(|request| (request.scope, None))
}

fn feedback_scope_allows(scope: &PluginDownloadFeedbackScope, category: Option<&str>) -> bool {
    let categories = scope
        .categories
        .iter()
        .map(|category| category.trim())
        .filter(|category| !category.is_empty())
        .collect::<Vec<_>>();
    categories.is_empty()
        || category.is_some_and(|actual| {
            categories
                .into_iter()
                .any(|category| category.eq_ignore_ascii_case(actual.trim()))
        })
}

fn feedback_scope_allows_tags(scope: &PluginDownloadFeedbackScope, tags: Option<&str>) -> bool {
    let categories = scope
        .categories
        .iter()
        .map(|category| sanitize_tag_fragment(category.trim()))
        .filter(|category| !category.is_empty())
        .collect::<Vec<_>>();
    categories.is_empty()
        || tags.is_some_and(|tags| {
            tags.split(',').map(sanitize_tag_fragment).any(|tag| {
                categories
                    .iter()
                    .any(|category| category.eq_ignore_ascii_case(&tag))
            })
        })
}

fn torrent_matches_feedback_scope(
    config: &QbittorrentConfig,
    scope: &PluginDownloadFeedbackScope,
    torrent: &QbTorrent,
) -> bool {
    match config.routing_mode {
        RoutingMode::Category => {
            feedback_scope_allows(scope, torrent.category.as_deref())
                || scope
                    .categories
                    .iter()
                    .filter_map(|category| routing_category_tag(category))
                    .any(|tag| torrent_has_tag(torrent, &tag))
        }
        RoutingMode::Tag => feedback_scope_allows_tags(scope, torrent.tags.as_deref()),
    }
}

fn scoped_completed_downloads(
    config: &QbittorrentConfig,
    scope: &PluginDownloadFeedbackScope,
    limit: Option<usize>,
) -> Result<PluginDownloadScopedListResponse<PluginCompletedDownload>, Error> {
    let torrents = list_completed_torrents(config)?
        .into_iter()
        .filter(|torrent| torrent_matches_feedback_scope(config, scope, torrent))
        .collect::<Vec<_>>();
    let (items, _) = convert_completed_torrents(torrents, limit);
    Ok(PluginDownloadScopedListResponse {
        items,
        failures: Vec::new(),
    })
}

fn convert_completed_torrents(
    torrents: Vec<QbTorrent>,
    limit: Option<usize>,
) -> (Vec<PluginCompletedDownload>, usize) {
    let mut downloads = torrents
        .into_iter()
        .filter_map(torrent_to_completed_download)
        .collect::<Vec<_>>();
    let converted_count = downloads.len();
    if let Some(limit) = limit {
        downloads.truncate(limit);
    }
    (downloads, converted_count)
}

fn completed_history_items(config: &QbittorrentConfig) -> Result<Vec<PluginDownloadItem>, Error> {
    let torrents = list_completed_torrents(config)?;
    let preferences = seed_preferences(config, &torrents);
    Ok(torrents
        .into_iter()
        .map(|torrent| torrent_to_item_with_preferences(torrent, preferences.as_ref()))
        .collect::<Vec<_>>())
}

fn list_completed_torrents(config: &QbittorrentConfig) -> Result<Vec<QbTorrent>, Error> {
    let mut torrents =
        collect_completed_torrents(|filter| list_completed_torrents_for_filter(config, filter))?;
    sort_and_dedupe_completed_torrents(&mut torrents, &config.imported_tag);
    Ok(torrents)
}

fn torrent_has_tag(torrent: &QbTorrent, tag: &str) -> bool {
    torrent.tags.as_deref().is_some_and(|tags| {
        tags.split(',')
            .any(|candidate| candidate.trim().eq_ignore_ascii_case(tag.trim()))
    })
}

fn sort_and_dedupe_completed_torrents(torrents: &mut Vec<QbTorrent>, imported_tag: &str) {
    torrents.sort_by(|left, right| {
        torrent_has_tag(left, imported_tag)
            .cmp(&torrent_has_tag(right, imported_tag))
            .then_with(|| right.completion_on.cmp(&left.completion_on))
            .then_with(|| left.hash.cmp(&right.hash))
    });
    let mut seen = HashSet::new();
    torrents.retain(|torrent| seen.insert(normalize_hash(&torrent.hash)));
}

fn collect_completed_torrents<F>(mut fetch: F) -> Result<Vec<QbTorrent>, Error>
where
    F: FnMut(Option<&str>) -> Result<Vec<QbTorrent>, Error>,
{
    let mut first_error: Option<Error> = None;
    for filter in [Some("completed"), Some("all"), None] {
        match fetch(filter) {
            Ok(torrents) => {
                let completed = torrents
                    .into_iter()
                    .filter(|torrent| is_completed_state(&torrent.state))
                    .collect::<Vec<_>>();
                if !completed.is_empty() || filter.is_none() {
                    return Ok(completed);
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    Err(first_error.unwrap_or_else(|| {
        Error::msg("qBittorrent completed torrent listing returned no usable response".to_string())
    }))
}

pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
    Ok(serde_json::to_string(&PluginResult::Ok(
        completed_history_items(&config)?,
    ))?)
}

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
            details: None,
        }));
    }
    if matches!(request.action, DownloadControlAction::ForceStart) {
        return Ok(PluginResult::Err(PluginError {
            code: PluginErrorCode::Unsupported,
            public_message: "unsupported control action: force_start".to_string(),
            debug_message: None,
            retry_after_seconds: None,
            details: None,
        }));
    }

    let config = QbittorrentConfig::from_config()?;

    match request.action {
        DownloadControlAction::Pause | DownloadControlAction::Resume => {
            let version = get_text(&config, "/app/version")?;
            let endpoint = control_endpoint(request.action, &version);
            post_form(&config, endpoint, &[("hashes".to_string(), hash)])?
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

fn control_endpoint(action: DownloadControlAction, version: &str) -> &'static str {
    let is_v5_or_newer = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .is_some_and(|major| major >= 5);

    match (action, is_v5_or_newer) {
        (DownloadControlAction::Pause, true) => "/torrents/stop",
        (DownloadControlAction::Resume, true) => "/torrents/start",
        (DownloadControlAction::Pause, false) => "/torrents/pause",
        (DownloadControlAction::Resume, false) => "/torrents/resume",
        _ => unreachable!("control endpoint requested for unsupported action"),
    }
}

pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    mark_imported_non_destructive(input)
}

pub fn scryer_download_mark_imported_non_destructive(input: String) -> FnResult<String> {
    mark_imported_non_destructive(input)
}

fn mark_imported_non_destructive(input: String) -> FnResult<String> {
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

    let config = QbittorrentConfig::from_config()?;

    let Some(torrent) = torrent_by_hash(&config, &hash)? else {
        return Ok(serde_json::to_string(&PluginResult::Ok(()))?);
    };

    preserve_routing_category_tag(&config, &hash, &torrent)?;
    apply_post_import_isolation(&config, &hash, &request)?;

    if config.tag_after_import {
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

    Ok(serde_json::to_string(&PluginResult::Ok(()))?)
}

fn preserve_routing_category_tag(
    config: &QbittorrentConfig,
    hash: &str,
    torrent: &QbTorrent,
) -> Result<(), Error> {
    if !matches!(config.routing_mode, RoutingMode::Category) {
        return Ok(());
    }
    let Some(tag) = torrent.category.as_deref().and_then(routing_category_tag) else {
        return Ok(());
    };
    if torrent_has_tag(torrent, &tag) {
        return Ok(());
    }
    create_tag_if_missing(config, &tag)?;
    post_form(
        config,
        "/torrents/addTags",
        &[
            ("hashes".to_string(), hash.to_string()),
            ("tags".to_string(), tag),
        ],
    )
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
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
        removes_completed_downloads: Some(false),
        sorting_mode,
        warnings,
    };

    Ok(serde_json::to_string(&PluginResult::Ok(status))?)
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = QbittorrentConfig::from_config()?;
    var::remove(COOKIE_VAR_KEY)?;
    let version = get_text(&config, "/app/version")?;
    Ok(serde_json::to_string(&PluginResult::Ok(version))?)
}

fn validate_auth_configuration(api_key: &str, username: &str, password: &str) -> Result<(), Error> {
    if !api_key.trim().is_empty() {
        return Ok(());
    }
    let has_username = !username.trim().is_empty();
    let has_password = !password.is_empty();
    if has_username != has_password {
        return Err(Error::msg(
            "qBittorrent requires both username and password, or leave both blank for unauthenticated access",
        ));
    }
    Ok(())
}

impl QbittorrentConfig {
    fn uses_api_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn uses_credentials(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.is_empty()
    }

    fn from_config() -> Result<Self, Error> {
        let base_url = config::get("base_url")
            .map_err(|e| Error::msg(format!("missing config base_url: {e}")))?
            .unwrap_or_default();
        if base_url.trim().is_empty() {
            return Err(Error::msg("qBittorrent requires base_url"));
        }

        let api_key = config::get("api_key")
            .map_err(|e| Error::msg(format!("missing config api_key: {e}")))?
            .unwrap_or_default();
        let username = config::get("username")
            .map_err(|e| Error::msg(format!("missing config username: {e}")))?
            .unwrap_or_default();
        let password = config::get("password")
            .map_err(|e| Error::msg(format!("missing config password: {e}")))?
            .unwrap_or_default();
        validate_auth_configuration(&api_key, &username, &password)?;

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
        let explicit_tag_after_import = config::get("tag_after_import").ok().flatten();
        let legacy_post_import_action = config::get("post_import_action").ok().flatten();
        let tag_after_import = resolve_tag_after_import(
            explicit_tag_after_import.as_deref(),
            legacy_post_import_action.as_deref(),
        );

        Ok(Self {
            webui_url,
            api_root,
            api_key,
            username,
            password,
            routing_mode,
            static_tags,
            auto_tmm,
            start_paused,
            force_start,
            skip_checking,
            imported_tag,
            tag_after_import,
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
            key: "api_key".to_string(),
            label: "API Key".to_string(),
            field_type: ConfigFieldType::Password,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![],
            help_text: Some(
                "Optional qBittorrent 5.2+ API key. When set, Scryer uses Bearer authentication instead of username and password; clear it to return to credential authentication."
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "username".to_string(),
            label: "Username".to_string(),
            field_type: ConfigFieldType::String,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![],
            help_text: Some(
                "Optional qBittorrent WebUI username used when no API key is configured; leave blank only when auth bypass is enabled"
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "password".to_string(),
            label: "Password".to_string(),
            field_type: ConfigFieldType::Password,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![],
            help_text: Some(
                "Optional qBittorrent WebUI password used when no API key is configured; leave blank only when auth bypass is enabled"
                    .to_string(),
            ),
        },
        ConfigFieldDef {
            key: "routing_mode".to_string(),
            label: "Isolation Routing".to_string(),
            field_type: ConfigFieldType::Select,
            required: false,
            default_value: Some("category".to_string()),
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![
                ConfigFieldOption {
                    value: "category".to_string(),
                    label: "Category".to_string(),
                    config_overrides: Default::default(),
                },
                ConfigFieldOption {
                    value: "tag".to_string(),
                    label: "Tag".to_string(),
                    config_overrides: Default::default(),
                },
            ],
            help_text: Some(
                "Apply Scryer isolation values as qBittorrent categories or tags".to_string(),
            ),
        },
        ConfigFieldDef {
            key: "static_tags".to_string(),
            label: "Static Tags".to_string(),
            field_type: ConfigFieldType::Tag,
            required: false,
            default_value: None,
            value_source: Default::default(),
            host_binding: None,
            role: None,
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
            role: None,
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
            role: None,
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
            role: None,
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
            role: None,
            options: vec![],
            help_text: Some("Skip piece recheck when adding local torrent payloads".to_string()),
        },
        ConfigFieldDef {
            key: "tag_after_import".to_string(),
            label: "Tag after import".to_string(),
            field_type: ConfigFieldType::Bool,
            required: false,
            default_value: Some("true".to_string()),
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![],
            help_text: Some(
                "Apply the imported tag after Scryer verifies a successful import".to_string(),
            ),
        },
        ConfigFieldDef {
            key: "imported_tag".to_string(),
            label: "Imported tag".to_string(),
            field_type: ConfigFieldType::Tag,
            required: false,
            default_value: Some(IMPORTED_TAG_DEFAULT.to_string()),
            value_source: Default::default(),
            host_binding: None,
            role: None,
            options: vec![],
            help_text: Some("Tag applied after a verified import".to_string()),
        },
    ]
}

fn config_bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn resolve_tag_after_import(explicit: Option<&str>, legacy_action: Option<&str>) -> bool {
    explicit.map(config_bool_value).unwrap_or_else(|| {
        !legacy_action.is_some_and(|action| action.trim().eq_ignore_ascii_case("retain"))
    })
}

fn config_bool(key: &str, default: bool) -> bool {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| config_bool_value(&value))
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

fn login_response_is_success(body: &str) -> bool {
    body.is_empty() || body.eq_ignore_ascii_case("ok.") || body.eq_ignore_ascii_case("ok")
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
    if !login_response_is_success(&body) {
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
    let cookie = if !config.uses_api_key() && config.uses_credentials() {
        Some(match var::get::<String>(COOKIE_VAR_KEY)? {
            Some(cookie) if !cookie.trim().is_empty() => cookie,
            _ => login(config)?,
        })
    } else {
        None
    };

    let request = build_request(config, method, path, cookie.as_deref(), content_type);
    let response = http::request::<Vec<u8>>(&request, body.clone())
        .map_err(|e| Error::msg(format!("qBittorrent request failed: {e}")))?;

    if should_retry_cookie_auth(config, response.status_code()) {
        var::remove(COOKIE_VAR_KEY)?;
        let cookie = login(config)?;
        let retry = build_request(config, method, path, Some(&cookie), content_type);
        return http::request::<Vec<u8>>(&retry, body)
            .map_err(|e| Error::msg(format!("qBittorrent retry failed: {e}")));
    }

    Ok(response)
}

fn should_retry_cookie_auth(config: &QbittorrentConfig, status_code: u16) -> bool {
    status_code == 403 && !config.uses_api_key() && config.uses_credentials()
}

fn build_request(
    config: &QbittorrentConfig,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    content_type: Option<&str>,
) -> HttpRequest {
    let mut request = HttpRequest::new(api_url(config, path))
        .with_method(method)
        .with_header("Referer", webui_header_url(config))
        .with_header("Origin", webui_header_url(config))
        .with_header("User-Agent", "scryer-qbittorrent-plugin/0.1")
        .with_header("Accept", "application/json, text/plain;q=0.9, */*;q=0.8");
    if config.uses_api_key() {
        request = request.with_header("Authorization", format!("Bearer {}", config.api_key.trim()));
    }
    if !config.uses_api_key()
        && let Some(cookie) = cookie
    {
        request = request.with_header("Cookie", cookie);
    }
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
    get_json(config, &torrents_info_path(filter, false))
}

fn list_completed_torrents_for_filter(
    config: &QbittorrentConfig,
    filter: Option<&str>,
) -> Result<Vec<QbTorrent>, Error> {
    get_json(config, &torrents_info_path(filter, true))
}

fn torrents_info_path(filter: Option<&str>, completed: bool) -> String {
    let sort = if completed {
        "completion_on"
    } else {
        "added_on"
    };
    let mut path = format!("/torrents/info?sort={sort}&reverse=true");
    if let Some(filter) = filter {
        path.push_str("&filter=");
        path.push_str(&url_encode(filter));
    }
    path
}

fn torrent_by_hash(config: &QbittorrentConfig, hash: &str) -> Result<Option<QbTorrent>, Error> {
    let path = format!("/torrents/info?hashes={}", url_encode(hash));
    let torrents: Vec<QbTorrent> = get_json(config, &path)?;
    Ok(torrents.into_iter().next())
}

fn resolve_added_hash(
    config: &QbittorrentConfig,
    request: &PluginDownloadClientAddRequest,
    before_hashes: &HashSet<String>,
    expected_hash: Option<String>,
) -> Result<String, Error> {
    let expected_hash = expected_hash
        .map(|hash| normalize_hash(&hash))
        .filter(|hash| !hash.is_empty());
    let expected_names = candidate_names(request);
    for attempt in 0..8 {
        let after = list_torrents(config, Some("all"))?;
        if let Some(expected_hash) = expected_hash.as_deref()
            && after
                .iter()
                .any(|torrent| normalize_hash(&torrent.hash) == expected_hash)
        {
            return Ok(expected_hash.to_string());
        }
        if let Some(hash) = discover_hash_candidate(&after, before_hashes, &expected_names) {
            return Ok(hash);
        }
        if attempt < 7 {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    resolve_unlisted_added_hash(expected_hash)
}

fn resolve_unlisted_added_hash(expected_hash: Option<String>) -> Result<String, Error> {
    if let Some(expected_hash) = expected_hash {
        eprintln!(
            "torrent add was accepted but expected info hash {expected_hash} did not appear in qBittorrent's list before the visibility probe timed out; returning the expected hash"
        );
        return Ok(expected_hash);
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
    if matches!(config.routing_mode, RoutingMode::Tag)
        && let Some(isolation) = request.routing.isolation_value.as_deref()
    {
        tags.push(sanitize_tag_fragment(isolation));
    }
    if matches!(config.routing_mode, RoutingMode::Category)
        && let Some(isolation) = request.routing.isolation_value.as_deref()
        && let Some(tag) = routing_category_tag(isolation)
    {
        tags.push(tag);
    }
    dedupe(tags)
}

fn routing_category_tag(category: &str) -> Option<String> {
    let category = sanitize_tag_fragment(category.trim());
    (!category.is_empty()).then(|| format!("{ROUTING_CATEGORY_TAG_PREFIX}{category}"))
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
        if let Some(value) = part.strip_prefix("xt=")
            && let Some(urn) = value.strip_prefix("urn:btih:")
        {
            return Some(normalize_hash(&percent_decode(urn)));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[idx + 1..idx + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            idx += 3;
            continue;
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

/// Whether the torrent has satisfied a seeding obligation the client itself enforces.
///
/// `Unknown` means qBittorrent exposes no usable limit for this torrent (no per-torrent
/// limit and no enabled global limit, or the value the limit is compared against is not
/// present in the API response). Callers must map `Unknown` to `can_remove: None` so that
/// Scryer-side (Tier B) goal evaluation decides instead of the plugin guessing.
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

/// Resolves a qBittorrent two-level limit: a per-torrent value `>= 0` wins, `-2` defers to
/// the global value when the matching global toggle is enabled, and `-1` (or a disabled
/// global) means "no limit".
fn resolve_effective_limit<T: PartialOrd + Copy>(
    per_torrent: Option<T>,
    defer_sentinel: T,
    zero: T,
    global_enabled: Option<bool>,
    global_value: Option<T>,
) -> Option<T> {
    match per_torrent {
        Some(value) if value >= zero => Some(value),
        Some(value) if value == defer_sentinel => {
            if global_enabled.unwrap_or(false) {
                global_value
            } else {
                None
            }
        }
        Some(_) => None,
        // Field absent from this qBittorrent build: qBittorrent's own default is "use global".
        None => {
            if global_enabled.unwrap_or(false) {
                global_value
            } else {
                None
            }
        }
    }
}

fn ratio_limit_state(torrent: &QbTorrent, preferences: Option<&QbPreferences>) -> SeedLimitState {
    let Some(limit) = resolve_effective_limit(
        torrent.ratio_limit,
        -2.0_f64,
        0.0_f64,
        preferences.and_then(|prefs| prefs.max_ratio_enabled),
        preferences.and_then(|prefs| prefs.max_ratio),
    ) else {
        return SeedLimitState::Unknown;
    };
    if !limit.is_finite() {
        return SeedLimitState::Unknown;
    }
    match torrent.ratio.filter(|value| value.is_finite()) {
        // qBittorrent's own tolerance (see Sonarr QBittorrent.HasReachedSeedLimit).
        Some(ratio) if limit - ratio <= 0.001 => SeedLimitState::Met,
        Some(_) => SeedLimitState::Unmet,
        None => SeedLimitState::Unknown,
    }
}

fn seeding_time_limit_state(
    torrent: &QbTorrent,
    preferences: Option<&QbPreferences>,
) -> SeedLimitState {
    let Some(limit_minutes) = resolve_effective_limit(
        torrent.seeding_time_limit,
        -2_i64,
        0_i64,
        preferences.and_then(|prefs| prefs.max_seeding_time_enabled),
        preferences.and_then(|prefs| prefs.max_seeding_time),
    ) else {
        return SeedLimitState::Unknown;
    };
    // `seeding_time` is present on the `torrents/info` payload since qBittorrent 4.4; on
    // older builds the axis is simply unknowable from the list call and we refuse to guess
    // rather than fanning out a per-torrent properties request on every poll.
    match torrent.seeding_time {
        Some(seconds) if seconds >= limit_minutes.saturating_mul(60) => SeedLimitState::Met,
        Some(_) => SeedLimitState::Unmet,
        None => SeedLimitState::Unknown,
    }
}

fn inactive_seeding_time_limit_state(
    torrent: &QbTorrent,
    preferences: Option<&QbPreferences>,
    now_unix_seconds: i64,
) -> SeedLimitState {
    let Some(limit_minutes) = resolve_effective_limit(
        torrent.inactive_seeding_time_limit,
        -2_i64,
        0_i64,
        preferences.and_then(|prefs| prefs.max_inactive_seeding_time_enabled),
        preferences.and_then(|prefs| prefs.max_inactive_seeding_time),
    ) else {
        return SeedLimitState::Unknown;
    };
    match torrent.last_activity.filter(|value| *value > 0) {
        Some(last_activity)
            if now_unix_seconds.saturating_sub(last_activity)
                > limit_minutes.saturating_mul(60) =>
        {
            SeedLimitState::Met
        }
        Some(_) => SeedLimitState::Unmet,
        None => SeedLimitState::Unknown,
    }
}

fn seed_limit_state(
    torrent: &QbTorrent,
    preferences: Option<&QbPreferences>,
    now_unix_seconds: i64,
) -> SeedLimitState {
    combine_seed_limit_states(&[
        ratio_limit_state(torrent, preferences),
        seeding_time_limit_state(torrent, preferences),
        inactive_seeding_time_limit_state(torrent, preferences, now_unix_seconds),
    ])
}

/// qBittorrent's "the client stopped this torrent after it finished downloading" states.
/// qBittorrent 5 renamed `pausedUP` to `stoppedUP`; both are accepted.
fn is_finished_seeding_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "pausedup" | "stoppedup"
    )
}

/// Honest `can_remove` for a qBittorrent torrent.
///
/// * `Some(true)` — qBittorrent stopped the torrent after download and one of its own
///   seeding limits is satisfied.
/// * `Some(false)` — the torrent is not finished, or a limit exists and is provably unmet.
/// * `None` — qBittorrent enforces no limit here (or a limit value is unavailable), so the
///   plugin cannot know; Scryer-side goal evaluation must decide.
fn derive_can_remove(
    torrent: &QbTorrent,
    preferences: Option<&QbPreferences>,
    now_unix_seconds: i64,
) -> Option<bool> {
    if !is_completed_state(&torrent.state) {
        return Some(false);
    }
    match seed_limit_state(torrent, preferences, now_unix_seconds) {
        SeedLimitState::Met if is_finished_seeding_state(&torrent.state) => Some(true),
        // Limit satisfied but qBittorrent has not stopped the torrent yet; do not pre-empt it.
        SeedLimitState::Met => None,
        SeedLimitState::Unmet => Some(false),
        SeedLimitState::Unknown => None,
    }
}

fn torrent_to_item_with_preferences(
    torrent: QbTorrent,
    preferences: Option<&QbPreferences>,
) -> PluginDownloadItem {
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
    let can_remove = derive_can_remove(&torrent, preferences, now_unix_seconds());
    let can_move_files = Some(is_completed_state(&torrent.state));
    PluginDownloadItem {
        client_item_id: normalize_hash(&torrent.hash),
        download_id: None,
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
        can_move_files,
        can_remove,
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
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name.clone(),
        release_name: None,
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
        (Some(content_path), Some(save_path)) if content_path == save_path => None,
        (Some(content_path), _) => Some(content_path),
        (None, save_path) => save_path,
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
        // qBittorrent 5 renamed `pausedDL` to `stoppedDL`.
        "pauseddl" | "stoppeddl" => DownloadItemState::Paused,
        "metadl" | "forcedmetadl" | "stalleddl" | "forceddl" | "downloading" | "allocating" => {
            DownloadItemState::Downloading
        }
        "checkingup" | "checkingdl" | "checkingresumedata" => DownloadItemState::Verifying,
        "moving" => DownloadItemState::ImportPending,
        // qBittorrent 5 renamed `pausedUP` to `stoppedUP`.
        "pausedup" | "stoppedup" | "queuedup" | "stalledup" | "uploading" | "forcedup" => {
            DownloadItemState::Completed
        }
        // qBittorrent uses these states for recoverable client-side conditions.
        // Keep the torrent visible for operator diagnosis instead of triggering
        // Scryer's failed-download cleanup flow.
        "error" | "missingfiles" => DownloadItemState::Warning,
        // `unknown` is qBittorrent's own "I could not determine this torrent's
        // state" answer, and anything unmatched is a state a newer qBittorrent
        // added. Neither is evidence of a failure, and neither should become a
        // queue row that is never cleaned up: keep polling, like Sonarr's
        // `default: // new status in API? default to downloading`
        // (Download/Clients/QBittorrent/QBittorrent.cs:350-355). `state_message`
        // carries the state string so the operator still sees what happened.
        _ => DownloadItemState::Downloading,
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn state_message(state: &str) -> Option<String> {
    let normalized = state.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "missingfiles" => Some("qBittorrent reports missing files".to_string()),
        "error" => Some("qBittorrent reports a torrent error".to_string()),
        "moving" => Some("qBittorrent is moving torrent files".to_string()),
        // Mirrors the state arms that `map_state` recognises; anything else is
        // an unknown or newly added qBittorrent state, reported as Downloading
        // with the raw state so the operator can see it.
        "queueddl" | "pauseddl" | "stoppeddl" | "metadl" | "forcedmetadl" | "stalleddl"
        | "forceddl" | "downloading" | "allocating" | "checkingup" | "checkingdl"
        | "checkingresumedata" | "pausedup" | "stoppedup" | "queuedup" | "stalledup"
        | "uploading" | "forcedup" => None,
        "" => Some("qBittorrent reported no torrent state".to_string()),
        other => Some(format!("Unknown qBittorrent download state: {other}")),
    }
}

fn is_completed_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "pausedup" | "stoppedup" | "queuedup" | "stalledup" | "uploading" | "forcedup"
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
        assert_eq!(
            value["provider"]["capabilities"]["category_scoped_feedback"],
            true
        );
        assert_eq!(
            value["provider"]["capabilities"]["mark_imported_non_destructive"],
            true
        );
    }

    #[test]
    fn scoped_feedback_request_preserves_categories_and_recent_limit() {
        let input = serde_json::json!({
            "scope": { "categories": ["Movies", "TV / Anime"] },
            "limit": 25,
        })
        .to_string();
        let (scope, limit) = scoped_feedback_request(&input).expect("scoped request");

        assert_eq!(scope.categories, vec!["Movies", "TV / Anime"]);
        assert_eq!(limit, Some(25));
        assert!(feedback_scope_allows(&scope, Some("movies")));
        assert!(!feedback_scope_allows(&scope, Some("music")));
    }

    #[test]
    fn tag_scoped_feedback_requires_a_matching_tag() {
        let scope = PluginDownloadFeedbackScope {
            categories: vec!["Movies".to_owned(), "TV / Anime".to_owned()],
        };

        assert!(feedback_scope_allows_tags(
            &scope,
            Some("scryer-origin,movies,other-tag")
        ));
        assert!(feedback_scope_allows_tags(&scope, Some("tv---anime")));
        assert!(!feedback_scope_allows_tags(&scope, Some("music,other-tag")));
        assert!(!feedback_scope_allows_tags(&scope, None));
        assert!(feedback_scope_allows_tags(
            &PluginDownloadFeedbackScope::default(),
            Some("music")
        ));
    }

    #[test]
    fn category_scoped_feedback_uses_original_category_after_post_import_move() {
        let config = test_config();
        let movies = PluginDownloadFeedbackScope {
            categories: vec!["Movies".to_owned()],
        };
        let television = PluginDownloadFeedbackScope {
            categories: vec!["Television".to_owned()],
        };
        let torrent = QbTorrent {
            category: Some("scryer-imported".to_owned()),
            tags: Some("scryer-origin,scryer:routing-category:movies".to_owned()),
            ..QbTorrent::default()
        };

        assert!(torrent_matches_feedback_scope(&config, &movies, &torrent));
        assert!(!torrent_matches_feedback_scope(
            &config,
            &television,
            &torrent
        ));
    }

    #[test]
    fn category_routing_add_records_the_feedback_category_in_a_tag() {
        let request = test_add_request(DownloadInputKind::MagnetUri);

        assert!(
            build_tags(&test_config(), &request)
                .iter()
                .any(|tag| tag == "scryer:routing-category:movie")
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
    fn state_mapping_handles_completed_and_warning_states() {
        assert_eq!(map_state("pausedUP"), DownloadItemState::Completed);
        assert_eq!(map_state("moving"), DownloadItemState::ImportPending);
        assert_eq!(map_state("missingFiles"), DownloadItemState::Warning);
    }

    #[test]
    fn completed_dest_dir_uses_content_path_for_single_file() {
        let torrent = QbTorrent {
            name: "Movie".to_string(),
            save_path: Some("/downloads/movies".to_string()),
            content_path: Some("/downloads/movies/Movie.mkv".to_string()),
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some("/downloads/movies/Movie.mkv")
        );
    }

    #[test]
    fn completed_dest_dir_falls_back_to_save_path_without_content_path() {
        let torrent = QbTorrent {
            name: "Movie".to_string(),
            save_path: Some("/downloads/movies".to_string()),
            content_path: None,
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some("/downloads/movies")
        );
    }

    #[test]
    fn completed_dest_dir_rejects_content_path_equal_to_shared_save_path() {
        let torrent = QbTorrent {
            hash: "shared-root-pack".to_string(),
            name: "Show Season 1".to_string(),
            save_path: Some("/downloads/series".to_string()),
            content_path: Some("/downloads/series".to_string()),
            ..QbTorrent::default()
        };

        assert_eq!(derive_completed_dest_dir(&torrent), None);
        assert!(torrent_to_completed_download(torrent).is_none());
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
    fn completed_dest_dir_uses_content_path_for_decypharr_release_folder() {
        let torrent = QbTorrent {
            name: "Harry.Potter.and.the.Prisoner.of.Azkaban.2004.BluRay.1080p.AV1.Opus-nAV1gator"
                .to_string(),
            save_path: Some("/mnt/symlinks/radarr".to_string()),
            content_path: Some(
                "/mnt/symlinks/radarr/Harry.Potter.and.the.Prisoner.of.Azkaban.2004.BluRay.1080p.AV1.Opus-nAV1gator"
                    .to_string(),
            ),
            ..QbTorrent::default()
        };
        assert_eq!(
            derive_completed_dest_dir(&torrent).as_deref(),
            Some(
                "/mnt/symlinks/radarr/Harry.Potter.and.the.Prisoner.of.Azkaban.2004.BluRay.1080p.AV1.Opus-nAV1gator"
            )
        );
    }

    fn feedback_torrent(hash: &str, completion_on: i64) -> QbTorrent {
        QbTorrent {
            hash: hash.to_string(),
            name: hash.to_string(),
            state: "pausedUP".to_string(),
            save_path: Some("/downloads".to_string()),
            content_path: Some(format!("/downloads/{hash}.mkv")),
            completion_on: Some(completion_on),
            ..QbTorrent::default()
        }
    }

    #[test]
    fn completed_queries_sort_by_completion_time() {
        assert_eq!(
            torrents_info_path(Some("completed"), true),
            "/torrents/info?sort=completion_on&reverse=true&filter=completed"
        );
        assert_eq!(
            torrents_info_path(Some("all"), false),
            "/torrents/info?sort=added_on&reverse=true&filter=all"
        );
    }

    #[test]
    fn completed_order_prioritizes_untagged_and_deduplicates_hashes() {
        let mut tagged = feedback_torrent("aaaa", 100);
        tagged.tags = Some(IMPORTED_TAG_DEFAULT.to_string());
        let mut torrents = vec![
            tagged,
            feedback_torrent("bbbb", 90),
            feedback_torrent("BBBB", 80),
            QbTorrent {
                hash: "cccc".to_string(),
                name: "cccc".to_string(),
                state: "pausedUP".to_string(),
                save_path: Some("/downloads".to_string()),
                content_path: Some("/downloads/cccc.mkv".to_string()),
                ..QbTorrent::default()
            },
        ];

        sort_and_dedupe_completed_torrents(&mut torrents, IMPORTED_TAG_DEFAULT);

        assert_eq!(
            torrents
                .iter()
                .map(|torrent| normalize_hash(&torrent.hash))
                .collect::<Vec<_>>(),
            vec!["bbbb", "cccc", "aaaa"]
        );
    }

    #[test]
    fn recent_limit_is_applied_after_completed_conversion() {
        let mut torrents = vec![
            feedback_torrent("bbbb", 9),
            QbTorrent {
                hash: "cccc".to_string(),
                state: "pausedUP".to_string(),
                completion_on: Some(10),
                ..QbTorrent::default()
            },
        ];
        sort_and_dedupe_completed_torrents(&mut torrents, IMPORTED_TAG_DEFAULT);

        let (downloads, converted_count) = convert_completed_torrents(torrents, Some(1));

        assert_eq!(converted_count, 1);
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].client_item_id, "bbbb");
    }

    #[test]
    fn imported_tag_priority_drains_a_500_torrent_backlog_in_batches() {
        let mut torrents = (0..500)
            .map(|index| feedback_torrent(&format!("{index:040x}"), 10_000 - index))
            .collect::<Vec<_>>();

        sort_and_dedupe_completed_torrents(&mut torrents, IMPORTED_TAG_DEFAULT);
        let first_batch = torrents
            .iter()
            .take(300)
            .map(|torrent| torrent.hash.clone())
            .collect::<HashSet<_>>();
        assert_eq!(first_batch.len(), 300);

        for torrent in &mut torrents {
            if first_batch.contains(&torrent.hash) {
                torrent.tags = Some(IMPORTED_TAG_DEFAULT.to_string());
            }
        }
        sort_and_dedupe_completed_torrents(&mut torrents, IMPORTED_TAG_DEFAULT);

        let next_unimported = torrents
            .iter()
            .take_while(|torrent| !torrent_has_tag(torrent, IMPORTED_TAG_DEFAULT))
            .collect::<Vec<_>>();
        assert_eq!(next_unimported.len(), 200);
        assert!(
            next_unimported
                .iter()
                .all(|torrent| !first_batch.contains(&torrent.hash))
        );
        assert!(
            torrents
                .iter()
                .skip(200)
                .all(|torrent| torrent_has_tag(torrent, IMPORTED_TAG_DEFAULT))
        );
    }

    #[test]
    fn completed_torrents_fall_back_to_all_filter_when_completed_filter_is_empty() {
        let mut requested_filters = Vec::new();
        let torrents = collect_completed_torrents(|filter| {
            requested_filters.push(filter.map(str::to_string));
            match filter {
                Some("completed") => Ok(Vec::new()),
                Some("all") => Ok(vec![QbTorrent {
                    hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
                    name: "Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR".to_string(),
                    state: "pausedUP".to_string(),
                    save_path: Some("/mnt/symlinks/radarr".to_string()),
                    content_path: Some(
                        "/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR"
                            .to_string(),
                    ),
                    ..QbTorrent::default()
                }]),
                _ => Ok(Vec::new()),
            }
        })
        .unwrap();

        assert_eq!(
            requested_filters,
            vec![Some("completed".to_string()), Some("all".to_string())]
        );
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].state, "pausedUP");
        assert_eq!(
            derive_completed_dest_dir(&torrents[0]).as_deref(),
            Some("/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR")
        );
        assert_eq!(
            preferred_content_path(&torrents[0]).as_deref(),
            Some("/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR")
        );
    }

    #[test]
    fn completed_torrents_fall_back_to_unfiltered_listing_when_filters_fail() {
        let mut requested_filters = Vec::new();
        let torrents = collect_completed_torrents(|filter| {
            requested_filters.push(filter.map(str::to_string));
            match filter {
                Some("completed") | Some("all") => {
                    Err(Error::msg("unsupported filter".to_string()))
                }
                Some(_) => Ok(Vec::new()),
                None => Ok(vec![QbTorrent {
                    hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
                    name: "Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR".to_string(),
                    state: "pausedUP".to_string(),
                    save_path: Some("/mnt/symlinks/radarr".to_string()),
                    content_path: Some(
                        "/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR"
                            .to_string(),
                    ),
                    ..QbTorrent::default()
                }]),
            }
        })
        .unwrap();

        assert_eq!(
            requested_filters,
            vec![Some("completed".to_string()), Some("all".to_string()), None,]
        );
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].state, "pausedUP");
        assert_eq!(
            derive_completed_dest_dir(&torrents[0]).as_deref(),
            Some("/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR")
        );
        assert_eq!(
            preferred_content_path(&torrents[0]).as_deref(),
            Some("/mnt/symlinks/radarr/Paperman.2012.720p.WEB-DL.AV1.AAC2.0-NTb.DECYPHARR")
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
        let item = torrent_to_item_with_preferences(torrent, None);
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
    fn login_response_accepts_cookie_only_success_body() {
        assert!(login_response_is_success(""));
        assert!(login_response_is_success("ok"));
        assert!(login_response_is_success("ok."));
        assert!(!login_response_is_success("unauthorized"));
    }

    #[test]
    fn incomplete_credentials_require_an_api_key() {
        assert!(validate_auth_configuration("", "user", "").is_err());
        assert!(validate_auth_configuration("", "", "pass").is_err());
        assert!(validate_auth_configuration("key", "user", "").is_ok());
    }

    #[test]
    fn blank_credentials_disable_auth() {
        let mut config = test_config();
        assert!(config.uses_credentials());

        config.username.clear();
        config.password.clear();

        assert!(!config.uses_credentials());
    }

    #[test]
    fn credential_fields_are_optional() {
        let fields = config_fields();
        let api_key = fields.iter().find(|field| field.key == "api_key").unwrap();
        let username = fields.iter().find(|field| field.key == "username").unwrap();
        let password = fields.iter().find(|field| field.key == "password").unwrap();

        assert!(!api_key.required);
        assert!(!username.required);
        assert!(!password.required);
    }

    #[test]
    fn post_import_configuration_is_non_destructive_and_migrates_legacy_values() {
        let fields = config_fields();
        assert!(fields.iter().all(|field| field.key != "post_import_action"));
        let tag_after_import = fields
            .iter()
            .find(|field| field.key == "tag_after_import")
            .expect("tag-after-import field");
        assert_eq!(tag_after_import.field_type, ConfigFieldType::Bool);
        assert_eq!(tag_after_import.default_value.as_deref(), Some("true"));
        assert!(
            fields
                .iter()
                .any(|field| field.key == "imported_tag" && field.label == "Imported tag")
        );

        assert!(!resolve_tag_after_import(None, Some("retain")));
        for legacy in ["tag_imported", "remove", "remove_with_data"] {
            assert!(resolve_tag_after_import(None, Some(legacy)));
        }
        assert!(!resolve_tag_after_import(
            Some("false"),
            Some("remove_with_data")
        ));
        assert!(resolve_tag_after_import(Some("true"), Some("retain")));
    }

    #[test]
    fn unauthenticated_request_omits_cookie_header() {
        let config = test_config();
        let request = build_request(&config, "GET", "/app/version", None, None);

        assert!(!request.headers.contains_key("Cookie"));
        assert!(!request.headers.contains_key("Authorization"));
    }

    #[test]
    fn api_key_request_uses_bearer_auth_without_cookie() {
        let mut config = test_config();
        config.api_key = "test-api-key".to_string();
        let request = build_request(&config, "GET", "/app/version", Some("SID=stale"), None);

        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some("Bearer test-api-key")
        );
        assert!(!request.headers.contains_key("Cookie"));
        assert!(config.uses_api_key());
    }

    #[test]
    fn api_key_rotation_uses_the_current_key() {
        let mut config = test_config();
        config.api_key = "rotated-key".to_string();

        let request = build_request(&config, "GET", "/app/version", None, None);

        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some("Bearer rotated-key")
        );
    }

    #[test]
    fn api_key_failures_never_use_cookie_retry() {
        let mut config = test_config();
        config.api_key = "test-api-key".to_string();

        assert!(!should_retry_cookie_auth(&config, 403));
    }

    #[test]
    fn clearing_api_key_returns_to_credential_authentication() {
        let mut config = test_config();
        config.api_key.clear();
        let request = build_request(&config, "GET", "/app/version", Some("SID=abc"), None);

        assert!(!request.headers.contains_key("Authorization"));
        assert_eq!(
            request.headers.get("Cookie").map(String::as_str),
            Some("SID=abc")
        );
        assert!(should_retry_cookie_auth(&config, 403));
    }

    #[test]
    fn authentication_bypass_never_uses_cookie_retry() {
        let mut config = test_config();
        config.api_key.clear();
        config.username.clear();
        config.password.clear();

        assert!(!should_retry_cookie_auth(&config, 403));
    }

    #[test]
    fn authenticated_request_includes_cookie_header() {
        let config = test_config();
        let request = build_request(&config, "GET", "/app/version", Some("SID=abc"), None);

        assert_eq!(
            request.headers.get("Cookie").map(String::as_str),
            Some("SID=abc")
        );
        assert_eq!(
            request.headers.get("Origin").map(String::as_str),
            Some(config.webui_url.as_str())
        );
        assert_eq!(
            request.headers.get("Referer").map(String::as_str),
            Some(config.webui_url.as_str())
        );
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
    fn unlisted_added_hash_returns_expected_hash_when_known() {
        let hash = resolve_unlisted_added_hash(Some(
            "92160d9a0e31d1e45b30b6b8101aa0c170e6bdbb".to_string(),
        ))
        .expect("expected hash should be accepted after visibility timeout");

        assert_eq!(hash, "92160d9a0e31d1e45b30b6b8101aa0c170e6bdbb");
    }

    #[test]
    fn unlisted_added_hash_without_expected_hash_still_errors() {
        let error = resolve_unlisted_added_hash(None).expect_err("missing hash should error");

        assert!(
            error
                .to_string()
                .contains("provide an info-hash hint or magnet URI")
        );
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

    #[test]
    fn control_endpoints_preserve_qbittorrent_4_behavior() {
        assert_eq!(
            control_endpoint(DownloadControlAction::Pause, "4.6.7"),
            "/torrents/pause"
        );
        assert_eq!(
            control_endpoint(DownloadControlAction::Resume, "v4.6.7"),
            "/torrents/resume"
        );
    }

    #[test]
    fn control_endpoints_use_qbittorrent_5_names() {
        assert_eq!(
            control_endpoint(DownloadControlAction::Pause, "5.0.0"),
            "/torrents/stop"
        );
        assert_eq!(
            control_endpoint(DownloadControlAction::Resume, "v5.1.2"),
            "/torrents/start"
        );
    }

    #[test]
    fn control_endpoints_fall_back_to_qbittorrent_4_names_for_unknown_versions() {
        assert_eq!(
            control_endpoint(DownloadControlAction::Pause, "development build"),
            "/torrents/pause"
        );
        assert_eq!(
            control_endpoint(DownloadControlAction::Resume, ""),
            "/torrents/resume"
        );
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
            api_key: String::new(),
            username: "user".to_string(),
            password: "pass".to_string(),
            routing_mode: RoutingMode::Category,
            static_tags: Vec::new(),
            auto_tmm: false,
            start_paused: false,
            force_start: false,
            skip_checking: false,
            imported_tag: IMPORTED_TAG_DEFAULT.to_string(),
            tag_after_import: true,
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
                assert!(
                    error
                        .public_message
                        .contains("invalid torrent_bytes_base64")
                );
            }
            PluginResult::Ok(_) => panic!("expected structured error"),
        }
    }

    fn seeding_torrent(state: &str) -> QbTorrent {
        QbTorrent {
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            state: state.to_string(),
            ..QbTorrent::default()
        }
    }

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn can_remove_is_false_while_still_downloading() {
        let torrent = QbTorrent {
            ratio_limit: Some(1.0),
            ratio: Some(0.0),
            ..seeding_torrent("downloading")
        };
        let item = torrent_to_item_with_preferences(torrent, None);
        assert_eq!(item.can_remove, Some(false));
        assert_eq!(item.can_move_files, Some(false));
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_per_torrent_ratio() {
        let torrent = QbTorrent {
            ratio_limit: Some(2.0),
            ratio: Some(0.4),
            ..seeding_torrent("uploading")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), Some(false));
    }

    #[test]
    fn can_remove_is_true_when_stopped_with_a_met_per_torrent_ratio() {
        let torrent = QbTorrent {
            ratio_limit: Some(2.0),
            ratio: Some(2.0),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), Some(true));
    }

    #[test]
    fn can_remove_is_true_for_legacy_paused_up_with_a_met_ratio() {
        let torrent = QbTorrent {
            ratio_limit: Some(1.5),
            ratio: Some(1.4995),
            ..seeding_torrent("pausedUP")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_when_the_torrent_has_no_seeding_obligation() {
        let torrent = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(-1),
            inactive_seeding_time_limit: Some(-1),
            ratio: Some(9.0),
            ..seeding_torrent("uploading")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), None);
    }

    #[test]
    fn can_remove_is_unknown_when_global_limits_are_disabled() {
        let preferences = QbPreferences {
            max_ratio_enabled: Some(false),
            max_seeding_time_enabled: Some(false),
            max_inactive_seeding_time_enabled: Some(false),
            ..QbPreferences::default()
        };
        let torrent = QbTorrent {
            ratio_limit: Some(-2.0),
            seeding_time_limit: Some(-2),
            inactive_seeding_time_limit: Some(-2),
            ratio: Some(0.2),
            seeding_time: Some(60),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(derive_can_remove(&torrent, Some(&preferences), NOW), None);
    }

    #[test]
    fn can_remove_falls_back_to_the_global_ratio_limit() {
        let preferences = QbPreferences {
            max_ratio_enabled: Some(true),
            max_ratio: Some(1.0),
            ..QbPreferences::default()
        };
        let met = QbTorrent {
            ratio_limit: Some(-2.0),
            ratio: Some(1.2),
            ..seeding_torrent("stoppedUP")
        };
        let unmet = QbTorrent {
            ratio_limit: Some(-2.0),
            ratio: Some(0.2),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(derive_can_remove(&met, Some(&preferences), NOW), Some(true));
        assert_eq!(
            derive_can_remove(&unmet, Some(&preferences), NOW),
            Some(false)
        );
    }

    #[test]
    fn per_torrent_ratio_limit_overrides_the_global_one() {
        let preferences = QbPreferences {
            max_ratio_enabled: Some(true),
            max_ratio: Some(0.1),
            ..QbPreferences::default()
        };
        let torrent = QbTorrent {
            ratio_limit: Some(3.0),
            ratio: Some(0.5),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(
            derive_can_remove(&torrent, Some(&preferences), NOW),
            Some(false)
        );
    }

    #[test]
    fn can_remove_uses_the_seeding_time_limit_from_the_list_payload() {
        let met = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(60),
            seeding_time: Some(3_600),
            ..seeding_torrent("stoppedUP")
        };
        let unmet = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(60),
            seeding_time: Some(120),
            ..seeding_torrent("uploading")
        };
        assert_eq!(derive_can_remove(&met, None, NOW), Some(true));
        assert_eq!(derive_can_remove(&unmet, None, NOW), Some(false));
    }

    #[test]
    fn seeding_time_limit_without_a_reported_seeding_time_is_unknown() {
        let torrent = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(60),
            seeding_time: None,
            inactive_seeding_time_limit: Some(-1),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(
            seeding_time_limit_state(&torrent, None),
            SeedLimitState::Unknown
        );
        assert_eq!(derive_can_remove(&torrent, None, NOW), None);
    }

    #[test]
    fn inactive_seeding_time_limit_is_honoured() {
        let torrent = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(-1),
            inactive_seeding_time_limit: Some(30),
            last_activity: Some(NOW - 3_600),
            ..seeding_torrent("stoppedUP")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), Some(true));
    }

    #[test]
    fn can_remove_is_unknown_when_the_limit_is_met_but_qbittorrent_is_still_seeding() {
        let torrent = QbTorrent {
            ratio_limit: Some(1.0),
            ratio: Some(3.0),
            ..seeding_torrent("uploading")
        };
        assert_eq!(derive_can_remove(&torrent, None, NOW), None);
    }

    #[test]
    fn queued_up_is_completed_and_never_an_error_state() {
        // qBittorrent 5.2 enables queueing by default, so finished torrents idle in queuedUP.
        assert_eq!(map_state("queuedUP"), DownloadItemState::Completed);
        assert!(is_completed_state("queuedUP"));
    }

    #[test]
    fn qbittorrent_5_stopped_states_map_like_their_paused_predecessors() {
        assert_eq!(map_state("stoppedUP"), DownloadItemState::Completed);
        assert_eq!(map_state("stoppedDL"), DownloadItemState::Paused);
        assert!(is_completed_state("stoppedUP"));
        assert!(!is_completed_state("stoppedDL"));
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let torrent = QbTorrent {
            ratio_limit: Some(5.0),
            ratio: Some(0.1),
            ..seeding_torrent("stoppedUP")
        };
        let item = torrent_to_item_with_preferences(torrent, None);
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, Some(false));
    }

    #[test]
    fn is_private_maps_present_true_present_false_and_absent() {
        let raw_private = r#"[{"hash":"a1","name":"n","state":"uploading","private":true}]"#;
        let raw_public = r#"[{"hash":"a1","name":"n","state":"uploading","private":false}]"#;
        let raw_absent = r#"[{"hash":"a1","name":"n","state":"uploading"}]"#;

        let private: Vec<QbTorrent> = serde_json::from_str(raw_private).unwrap();
        let public: Vec<QbTorrent> = serde_json::from_str(raw_public).unwrap();
        let absent: Vec<QbTorrent> = serde_json::from_str(raw_absent).unwrap();

        let map = |torrents: Vec<QbTorrent>| {
            torrent_to_item_with_preferences(torrents.into_iter().next().unwrap(), None)
                .torrent
                .unwrap()
                .is_private
        };

        assert_eq!(map(private), Some(true));
        assert_eq!(map(public), Some(false));
        // Pre-5.0 qBittorrent omits the field entirely; never claim a torrent is public.
        assert_eq!(map(absent), None);
    }

    #[test]
    fn observed_seed_state_is_taken_from_the_list_payload() {
        let raw =
            r#"[{"hash":"a1","name":"n","state":"uploading","ratio":1.75,"seeding_time":7200}]"#;
        let torrents: Vec<QbTorrent> = serde_json::from_str(raw).unwrap();
        let item = torrent_to_item_with_preferences(torrents.into_iter().next().unwrap(), None);
        let torrent = item.torrent.unwrap();
        assert_eq!(torrent.seed_ratio, Some(1.75));
        assert_eq!(torrent.seed_time_seconds, Some(7200));
    }

    #[test]
    fn only_defer_sentinel_or_absent_limits_consult_the_global_preferences() {
        let unlimited = QbTorrent {
            ratio_limit: Some(-1.0),
            seeding_time_limit: Some(-1),
            inactive_seeding_time_limit: Some(-1),
            ..seeding_torrent("stoppedUP")
        };
        assert!(!defers_to_global_limits(&unlimited));

        let deferring = QbTorrent {
            ratio_limit: Some(-2.0),
            seeding_time_limit: Some(-1),
            inactive_seeding_time_limit: Some(-1),
            ..seeding_torrent("stoppedUP")
        };
        assert!(defers_to_global_limits(&deferring));

        // Absent fields (older qBittorrent builds) default to "use global".
        assert!(defers_to_global_limits(&seeding_torrent("stoppedUP")));
    }

    #[test]
    fn share_limit_fields_deserialize_from_the_list_payload() {
        let raw = r#"[{"hash":"a1","name":"n","state":"stoppedUP","ratio":2.5,"ratio_limit":2.0,"seeding_time_limit":-2,"inactive_seeding_time_limit":-2,"last_activity":1699999000}]"#;
        let torrents: Vec<QbTorrent> = serde_json::from_str(raw).unwrap();
        let torrent = torrents.into_iter().next().unwrap();
        assert_eq!(torrent.ratio_limit, Some(2.0));
        assert_eq!(torrent.seeding_time_limit, Some(-2));
        assert_eq!(torrent.inactive_seeding_time_limit, Some(-2));
        assert_eq!(torrent.last_activity, Some(1_699_999_000));
        assert_eq!(derive_can_remove(&torrent, None, NOW), Some(true));
    }

    #[test]
    fn qbit_error_states_remain_warnings_and_do_not_trigger_failed_download_cleanup() {
        assert_eq!(map_state("error"), DownloadItemState::Warning);
        assert_eq!(map_state("downloading"), DownloadItemState::Downloading);
        assert_eq!(map_state("uploading"), DownloadItemState::Completed);
    }

    #[test]
    fn unknown_states_keep_polling_instead_of_warning_or_failing() {
        // qBittorrent's own "state could not be determined" answer, and any
        // state a newer qBittorrent adds, are not failures and must not park a
        // queue row in a state nothing clears.
        for state in ["unknown", "somethingNew", ""] {
            assert_eq!(
                map_state(state),
                DownloadItemState::Downloading,
                "state {state:?} should keep polling"
            );
            assert_ne!(map_state(state), DownloadItemState::Warning);
            assert_ne!(map_state(state), DownloadItemState::Error);
        }
    }

    #[test]
    fn unknown_states_still_carry_an_operator_message() {
        assert_eq!(
            state_message("somethingNew").as_deref(),
            Some("Unknown qBittorrent download state: somethingnew")
        );
        assert_eq!(
            state_message("unknown").as_deref(),
            Some("Unknown qBittorrent download state: unknown")
        );
        assert_eq!(
            state_message("").as_deref(),
            Some("qBittorrent reported no torrent state")
        );
        // Recognised states keep their existing messages (or none at all).
        assert_eq!(state_message("downloading"), None);
        assert_eq!(state_message("stoppedUP"), None);
        assert_eq!(
            state_message("missingFiles").as_deref(),
            Some("qBittorrent reports missing files")
        );
    }
}

// ---------------------------------------------------------------------------
// `scryer:download-client/download-client@1.0.0`
// ---------------------------------------------------------------------------
//
// Transport only. Every operation above is untouched: the same URLs, the same
// SID cookie in plugin state, the same 204-bodied login, the same category
// 409 handling. What changed is how the host reaches them — a `process`
// export carrying the very command envelope the Preview 1 runner already
// moved over stdin/stdout, instead of a `main` reading stdin.
//
// The function table is the single source of truth for both exports, so
// `describe` and `process` cannot drift apart, and the operation semantics —
// merged failed history, scoped listings, non-destructive mark-imported —
// stay in the PDK bridge where every client shares them.

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
        list_recent_completed: Some(scryer_download_list_recent_completed),
        control: scryer_download_control,
        mark_imported: scryer_download_mark_imported,
        mark_imported_non_destructive: Some(scryer_download_mark_imported_non_destructive),
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
