//! Deluge Web JSON-RPC download client.
//!
//! Reconciled against Sonarr's `NzbDrone.Core/Download/Clients/Deluge` (Deluge.cs,
//! DelugeProxy.cs, DelugeSettings.cs) and its `DelugeFixture`. Sonarr is the floor for
//! client knowledge — the status table, the label-plugin validation, the output-root
//! fallback chain, the JSON-RPC error codes that mean "re-login" — while the shape of
//! every answer is Scryer's: tri-state `can_remove`, data-only `can_move_files`, typed
//! `PluginErrorCode`s, and a post-import handoff that only ever relabels.
//!
//! ## Deluge version assumed
//!
//! Deluge **2.x** (`deluge/ui/web/json_api.py` + `deluge/core/core.py`), with the 1.3
//! fallbacks Sonarr still carries: `daemon.info` when `daemon.get_version` is absent,
//! and status keys that a 1.3 daemon simply omits (`completed_time`, `private`) being
//! optional here rather than required.

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use scryer_plugin_pdk::*;
use scryer_plugin_sdk::current_sdk_constraint;
use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldOption, ConfigFieldRole, ConfigFieldType,
    DownloadClientCapabilities, DownloadClientDescriptor, DownloadControlAction, DownloadInputKind,
    DownloadIsolationMode, DownloadItemState, DownloadTorrentCapabilities, PluginCompletedDownload,
    PluginDescriptor, PluginDownloadClientAddRequest, PluginDownloadClientAddResponse,
    PluginDownloadClientControlRequest, PluginDownloadClientMarkImportedRequest,
    PluginDownloadClientStatus, PluginDownloadIsolation, PluginDownloadItem,
    PluginDownloadOutputKind, PluginError, PluginErrorCode, PluginResult,
    PluginTorrentInitialState, PluginTorrentItem, PluginTorrentQueuePlacement, ProviderDescriptor,
    SDK_VERSION,
};
use serde::Deserialize;

const COOKIE_VAR_KEY: &str = "deluge.cookie";
/// Labels this instance has already proven exist, so the add path pays for
/// `label.get_labels` once rather than on every grab.
const KNOWN_LABELS_VAR_KEY: &str = "deluge.known_labels";
/// Set once the plugin has reconnected the Web UI to its daemon after seeing
/// hashless torrents; mirrors Sonarr's `_hasAttemptedReconnecting` instance flag.
const RECONNECT_VAR_KEY: &str = "deluge.reconnect_attempted";
/// How many torrents the last queue poll had to skip, surfaced in `status.warnings`.
const INVALID_TORRENTS_VAR_KEY: &str = "deluge.invalid_torrents";

/// Deluge label names are validated by the Label plugin itself
/// (`DelugeSettingsValidator`, DelugeSettings.cs:14-15 mirrors the same rule).
const LABEL_ALLOWED_CHARS: &str = "Allowed characters a-z, 0-9 and -";

const REQUIRED_PROPERTIES: &[&str] = &[
    "hash",
    "name",
    "state",
    "progress",
    "eta",
    "message",
    "is_finished",
    "save_path",
    "total_size",
    "total_done",
    "total_uploaded",
    "download_payload_rate",
    "upload_payload_rate",
    "time_added",
    // Deluge 2 only; a 1.3 daemon omits it and `completed_at` stays `None`.
    "completed_time",
    "seeding_time",
    "private",
    "ratio",
    "is_auto_managed",
    "stop_at_ratio",
    "remove_at_ratio",
    "stop_ratio",
    // Registered by the Label plugin, so Scryer can report the label in Deluge's own
    // casing rather than echoing back the configured string.
    "label",
    // A single-file torrent's `name` is the file; anything else is the directory
    // Deluge created under `save_path`. Cheap, unlike the `files` list Sonarr's own
    // comment warns times out on large season packs (DelugeProxy.cs:75-76).
    "num_files",
];

#[derive(Debug, Clone)]
struct DelugeConfig {
    json_url: String,
    password: String,
    category: String,
    imported_category: String,
    recent_priority: PluginTorrentQueuePlacement,
    older_priority: PluginTorrentQueuePlacement,
    add_paused: bool,
    download_directory: String,
    completed_directory: String,
}

/// A failure with an honest [`PluginErrorCode`], instead of the `anyhow` string the
/// bridge would otherwise flatten into `Temporary` (common rule 4). The `field` names
/// the config key Sonarr's `NzbDroneValidationFailure` would have attached.
#[derive(Debug, Clone)]
struct DelugeFailure {
    code: PluginErrorCode,
    field: Option<&'static str>,
    message: String,
    detail: Option<String>,
}

type DelugeResult<T> = Result<T, DelugeFailure>;

impl DelugeFailure {
    fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            field: None,
            message: message.into(),
            detail: None,
        }
    }

    fn for_field(code: PluginErrorCode, field: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            field: Some(field),
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn public_message(&self) -> String {
        match self.field {
            Some(field) => format!("{field}: {}", self.message),
            None => self.message.clone(),
        }
    }

    fn into_result<T>(self) -> PluginResult<T> {
        PluginResult::Err(PluginError {
            code: self.code,
            public_message: self.public_message(),
            debug_message: self.detail,
            retry_after_seconds: None,
            details: None,
        })
    }
}

fn respond<T: serde::Serialize>(result: DelugeResult<T>) -> FnResult<String> {
    Ok(serde_json::to_string(&match result {
        Ok(value) => PluginResult::Ok(value),
        Err(failure) => failure.into_result(),
    })?)
}

fn warn(message: impl AsRef<str>) {
    log::log(log::LogLevel::Warn, message.as_ref());
}

fn debug(message: impl AsRef<str>) {
    log::log(log::LogLevel::Debug, message.as_ref());
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<DelugeError>,
}

#[derive(Debug, Deserialize)]
struct DelugeError {
    #[serde(default, alias = "Code")]
    code: i64,
    #[serde(default, alias = "Message")]
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct UpdateUiResult {
    #[serde(default)]
    torrents: HashMap<String, DelugeTorrent>,
}

/// The Label plugin's per-label options (`label.get_options`), used by the
/// output-root fallback chain (`Deluge.cs:237-241`).
#[derive(Debug, Default, Clone, Deserialize)]
struct DelugeLabel {
    #[serde(default)]
    apply_move_completed: bool,
    #[serde(default)]
    move_completed: bool,
    #[serde(default)]
    move_completed_path: String,
}

#[derive(Debug, Default, Deserialize)]
struct DelugeTorrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    eta: f64,
    #[serde(default)]
    message: String,
    #[serde(default, rename = "is_finished")]
    is_finished: bool,
    #[serde(default, rename = "save_path")]
    download_path: String,
    #[serde(default, rename = "total_size")]
    size: i64,
    #[serde(default, rename = "total_done")]
    bytes_downloaded: i64,
    #[serde(default, rename = "total_uploaded")]
    bytes_uploaded: Option<i64>,
    #[serde(default, rename = "download_payload_rate")]
    download_rate: Option<f64>,
    #[serde(default, rename = "upload_payload_rate")]
    upload_rate: Option<f64>,
    /// Deluge 2's unix completion time; `0` on a torrent that has never finished, and
    /// absent entirely on Deluge 1.3.
    #[serde(default, rename = "completed_time")]
    completed_time: Option<f64>,
    /// Deluge counts seeding time separately from `active_time` (which also covers the
    /// download phase); only this value is a truthful `seed_time_seconds`.
    #[serde(default, rename = "seeding_time")]
    seeding_time: Option<i64>,
    #[serde(default)]
    ratio: f64,
    #[serde(default, rename = "is_auto_managed")]
    is_auto_managed: bool,
    #[serde(default, rename = "stop_at_ratio")]
    stop_at_ratio: bool,
    #[serde(default, rename = "stop_ratio")]
    stop_ratio: f64,
    #[serde(default)]
    private: Option<bool>,
    /// Reported by the Label plugin in Deluge's own casing
    /// (`memory: download-category-case-sensitivity`).
    #[serde(default)]
    label: Option<String>,
    #[serde(default, rename = "num_files")]
    num_files: Option<i64>,
}

pub fn scryer_describe(_input: String) -> FnResult<String> {
    let descriptor = PluginDescriptor {
        id: "deluge".to_string(),
        name: "Deluge".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: vec![],
        provider: ProviderDescriptor::DownloadClient(DownloadClientDescriptor {
            provider_type: "deluge".to_string(),
            provider_aliases: vec![],
            config_fields: config_fields(),
            default_base_url: None,
            allowed_hosts: vec![],
            accepted_inputs: vec![
                DownloadInputKind::MagnetUri,
                DownloadInputKind::TorrentBytes,
                DownloadInputKind::TorrentUrl,
                DownloadInputKind::TorrentFile,
            ],
            isolation_modes: vec![
                DownloadIsolationMode::Tag,
                DownloadIsolationMode::Category,
                DownloadIsolationMode::Directory,
            ],
            capabilities: DownloadClientCapabilities {
                pause: false,
                resume: false,
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
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentUrl,
                        DownloadInputKind::TorrentFile,
                    ],
                    // Magnet first, matching Sonarr's `PreferTorrentFile == false`
                    // (TorrentClientBase.cs:49, :107-127). `TorrentUrl` is last on
                    // purpose: the core fetches `.torrent` bytes with the indexer's
                    // own credentials, while a plugin-side GET has no indexer
                    // cookies and would 403 on most private trackers.
                    preferred_sources: vec![
                        DownloadInputKind::MagnetUri,
                        DownloadInputKind::TorrentBytes,
                        DownloadInputKind::TorrentFile,
                        DownloadInputKind::TorrentUrl,
                    ],
                    isolation_modes: vec![
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Category,
                        DownloadIsolationMode::Directory,
                    ],
                    // Deluge has exactly one label per torrent, so whichever of these
                    // the core routes lands on the same `label.set_torrent` call and
                    // implicitly drops the grab-time label.
                    post_import_isolation_modes: vec![
                        DownloadIsolationMode::Tag,
                        DownloadIsolationMode::Category,
                    ],
                    supports_seed_ratio_limit: true,
                    supports_seed_time_limit: false,
                    supports_start_paused: true,
                    supports_force_start: false,
                    supports_sequential_download: false,
                    supports_first_last_piece_priority: false,
                    supports_content_layout: false,
                    supports_skip_checking: false,
                    supports_auto_management: true,
                    supports_post_import_isolation: true,
                    reports_content_paths: true,
                    ..DownloadTorrentCapabilities::default()
                }),
                // SDK 3.10 addition. `false` is the SDK's own default and therefore exactly
                // what this client's pre-3.10 descriptor already meant to a 3.10 host;
                // advertising category-scoped feedback would be a behaviour change, not a
                // transport one, so it stays off across the component migration.
                category_scoped_feedback: false,
                // The core's post-import handoff
                // (`result_state.rs::schedule_non_destructive_import_mark`) routes here,
                // and this client answers it with a label swap and nothing else.
                mark_imported_non_destructive: true,
            },
        }),
    };
    Ok(serde_json::to_string(&descriptor)?)
}

pub fn scryer_download_add(input: String) -> FnResult<String> {
    let request: PluginDownloadClientAddRequest = serde_json::from_str(&input)?;
    respond(add_torrent(&DelugeConfig::from_extism(), &request))
}

fn add_torrent(
    config: &DelugeConfig,
    request: &PluginDownloadClientAddRequest,
) -> DelugeResult<PluginDownloadClientAddResponse> {
    let label = add_label(config, request);
    // The label has to exist before the torrent references it: `label.set_torrent`
    // against an unknown label raises, and a torrent that never gets labelled is
    // invisible to the label-filtered `web.update_ui` this client polls with.
    if let Some(label) = label.as_deref() {
        ensure_label(config, label)?;
    }

    let options = add_options(config, request);
    // Sonarr's non-`PreferTorrentFile` order (TorrentClientBase.cs:107-127): magnet,
    // then the `.torrent`. The core hands us bytes it fetched with indexer auth; the
    // plain GET below is only a last resort for a public, cookieless URL.
    let added = if let Some(source) = magnet_source(request) {
        call_value(
            config,
            "core.add_torrent_magnet",
            serde_json::json!([source, options]),
        )?
    } else if let Some(bytes_base64) = request.source.torrent_bytes_base64.as_deref() {
        call_value(
            config,
            "core.add_torrent_file",
            serde_json::json!([torrent_file_name(request), bytes_base64, options]),
        )?
    } else if let Some(source) = torrent_file_url(request) {
        let bytes = get_external_bytes(&source)?;
        call_value(
            config,
            "core.add_torrent_file",
            serde_json::json!([torrent_file_name(request), STANDARD.encode(bytes), options]),
        )?
    } else {
        return Err(DelugeFailure::new(
            PluginErrorCode::Permanent,
            "download source is missing",
        ));
    };

    let hash = added
        .as_str()
        .map(normalize_hash)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DelugeFailure::new(
                PluginErrorCode::Temporary,
                "Deluge did not return an added torrent hash",
            )
            .with_detail(added.to_string())
        })?;

    apply_seed_limits(config, &hash, request)?;
    if let Some(label) = label.as_deref() {
        set_label(config, &hash, label)?;
    }
    if should_move_to_top(config, request) {
        // Queue placement is a preference, not a precondition for the grab.
        if let Err(failure) = call_value(
            config,
            "core.queue_top",
            serde_json::json!([[hash.clone()]]),
        ) {
            warn(format!(
                "Deluge refused to move {hash} to the top of the queue: {}",
                failure.public_message()
            ));
        }
    }

    Ok(PluginDownloadClientAddResponse {
        client_item_id: hash.clone(),
        info_hash: Some(hash),
    })
}

pub fn scryer_download_list_queue(_input: String) -> FnResult<String> {
    respond(list_queue_items(&DelugeConfig::from_extism()))
}

fn list_queue_items(config: &DelugeConfig) -> DelugeResult<Vec<PluginDownloadItem>> {
    let torrents = list_torrents(config)?;
    let total = torrents.len();
    let items = torrents
        .into_iter()
        .filter(is_valid_torrent)
        .map(|torrent| torrent_to_item(config, torrent))
        .collect::<Vec<_>>();
    note_invalid_torrents(config, total - items.len());
    Ok(items)
}

/// Deluge has no failed-download history: a torrent that errors stays in the same
/// `web.update_ui` listing with `state == "Error"`, and the queue poll already reports
/// it as [`DownloadItemState::Warning`]. Returning nothing here keeps the PDK bridge's
/// queue+history merge (`download_client_bridge.rs:166-203`) to a single RPC per poll
/// instead of two identical ones.
pub fn scryer_download_list_history(_input: String) -> FnResult<String> {
    Ok(serde_json::to_string(&PluginResult::Ok(Vec::<
        PluginDownloadItem,
    >::new()))?)
}

pub fn scryer_download_list_completed(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism();
    respond(list_completed_downloads(&config))
}

fn list_completed_downloads(config: &DelugeConfig) -> DelugeResult<Vec<PluginCompletedDownload>> {
    Ok(list_torrents(config)?
        .into_iter()
        .filter(is_valid_torrent)
        .filter(|torrent| map_state(torrent) == DownloadItemState::Completed)
        .map(|torrent| torrent_to_completed(config, torrent))
        .collect())
}

pub fn scryer_download_control(input: String) -> FnResult<String> {
    let request: PluginDownloadClientControlRequest = serde_json::from_str(&input)?;
    respond(control(&DelugeConfig::from_extism(), &request))
}

fn control(
    config: &DelugeConfig,
    request: &PluginDownloadClientControlRequest,
) -> DelugeResult<()> {
    let hash = normalize_hash(&request.client_item_id);
    if hash.is_empty() {
        return Err(DelugeFailure::new(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
    }
    match request.action {
        DownloadControlAction::Remove => {
            call_value(
                config,
                "core.remove_torrent",
                serde_json::json!([hash, request.remove_data]),
            )?;
            Ok(())
        }
        DownloadControlAction::Pause
        | DownloadControlAction::Resume
        | DownloadControlAction::ForceStart => Err(DelugeFailure::new(
            PluginErrorCode::Unsupported,
            "Deluge control action is not implemented by Scryer's Deluge client",
        )),
    }
}

/// The destructive export has no core caller
/// (`result_state.rs::schedule_non_destructive_import_mark` is the only path), and it
/// runs the same body so a host that still reaches for it cannot get a hit and run.
pub fn scryer_download_mark_imported(input: String) -> FnResult<String> {
    mark_imported_non_destructive(input)
}

pub fn scryer_download_mark_imported_non_destructive(input: String) -> FnResult<String> {
    mark_imported_non_destructive(input)
}

fn mark_imported_non_destructive(input: String) -> FnResult<String> {
    let request: PluginDownloadClientMarkImportedRequest = serde_json::from_str(&input)?;
    respond(apply_post_import_label(
        &DelugeConfig::from_extism(),
        &request,
    ))
}

/// Sonarr's `MarkItemAsImported` (Deluge.cs:39-56): relabel, and *warn* rather than
/// fail when Deluge refuses ("Does the label exist?"). Scryer adds two things — the
/// core's routed `post_import_isolation` takes precedence over the configured label,
/// and the label is created first so the handoff cannot fail merely because nobody had
/// made it yet. Removal is never part of this: the core's seeding gate owns that.
fn apply_post_import_label(
    config: &DelugeConfig,
    request: &PluginDownloadClientMarkImportedRequest,
) -> DelugeResult<()> {
    let hash = normalize_hash(
        request
            .info_hash
            .as_deref()
            .unwrap_or(request.client_item_id.as_str()),
    );
    if hash.is_empty() {
        return Err(DelugeFailure::new(
            PluginErrorCode::Permanent,
            "client_item_id is required",
        ));
    }
    let Some(label) = post_import_label(config, &request.post_import_isolation) else {
        return Ok(());
    };

    if let Err(failure) =
        ensure_label(config, &label).and_then(|()| set_label(config, &hash, &label))
    {
        warn(format!(
            "Failed to set torrent post-import label \"{label}\" for {} in Deluge. Does the label exist? ({})",
            request.title_name.as_deref().unwrap_or(hash.as_str()),
            failure.public_message()
        ));
    }
    Ok(())
}

pub fn scryer_download_status(_input: String) -> FnResult<String> {
    respond(client_status(&DelugeConfig::from_extism()))
}

fn client_status(config: &DelugeConfig) -> DelugeResult<PluginDownloadClientStatus> {
    // One `system.listMethods` serves both the version probe and the label-plugin gate
    // on the output-root chain.
    let methods = get_methods(config)?;
    let version = version_from_methods(config, &methods)?;
    Ok(PluginDownloadClientStatus {
        version: Some(version),
        is_localhost: Some(is_localhost_url(&config.json_url)),
        remote_output_roots: output_roots(config, &methods)?,
        // Removal of a finished torrent is the core's decision through the seeding
        // gate; this client never removes at import time.
        removes_completed_downloads: Some(false),
        sorting_mode: Some("deluge-jsonrpc".to_string()),
        warnings: status_warnings(),
    })
}

pub fn scryer_download_test_connection(_input: String) -> FnResult<String> {
    let config = DelugeConfig::from_extism();
    let _ = var::remove(COOKIE_VAR_KEY);
    let _ = var::remove(KNOWN_LABELS_VAR_KEY);
    respond(test_connection(&config))
}

/// Sonarr's `Test(failures)` (Deluge.cs:264-274): connection, then category, then a
/// real listing. Each of its `NzbDroneValidationFailure`s becomes a typed
/// [`PluginErrorCode`] naming the same config field.
fn test_connection(config: &DelugeConfig) -> DelugeResult<String> {
    validate_label_name("category", &config.category)?;
    validate_label_name("post_import_category", &config.imported_category)?;

    let methods = get_methods(config)?;
    let version = version_from_methods(config, &methods)?;
    test_category(config, &methods)?;
    // `TestGetTorrents` (Deluge.cs:375-388): a version handshake proves the web UI is
    // up, not that the daemon behind it will answer a listing.
    list_torrents(config)?;
    Ok(version)
}

/// `TestCategory` (Deluge.cs:325-373).
fn test_category(config: &DelugeConfig, methods: &[String]) -> DelugeResult<()> {
    if config.category.is_empty() && config.imported_category.is_empty() {
        return Ok(());
    }
    if !label_plugin_available(methods) {
        return Err(DelugeFailure::for_field(
            PluginErrorCode::InvalidConfig,
            "category",
            "Deluge's Label plugin is not activated, so Scryer cannot label its torrents",
        )
        .with_detail(
            "Enable the Label plugin in Deluge (Preferences > Plugins) or clear the category fields."
                .to_string(),
        ));
    }

    let mut labels = available_labels(config)?;
    for (field, label) in [
        ("category", &config.category),
        ("post_import_category", &config.imported_category),
    ] {
        if label.is_empty() {
            continue;
        }
        let wanted = label.to_ascii_lowercase();
        if labels.iter().any(|known| known == &wanted) {
            continue;
        }
        create_label(config, &wanted)?;
        labels = available_labels(config)?;
        if !labels.iter().any(|known| known == &wanted) {
            return Err(DelugeFailure::for_field(
                PluginErrorCode::InvalidConfig,
                field,
                format!("Deluge's Label plugin would not create the label \"{wanted}\""),
            ));
        }
    }
    // The add path can now trust these for the rest of this instance's life.
    let _ = var::set(KNOWN_LABELS_VAR_KEY, labels);
    Ok(())
}

/// `DelugeSettingsValidator` (DelugeSettings.cs:14-15). Deluge's Label plugin
/// lower-cases and character-checks label ids itself, so anything outside this set is
/// a config error the operator has to fix, not a runtime failure to retry.
fn validate_label_name(field: &'static str, value: &str) -> DelugeResult<()> {
    if value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Ok(());
    }
    Err(DelugeFailure::for_field(
        PluginErrorCode::InvalidConfig,
        field,
        LABEL_ALLOWED_CHARS,
    ))
}

impl DelugeConfig {
    fn from_extism() -> Self {
        let host = config_value("host").unwrap_or_else(|| "localhost".to_string());
        let port = config_value("port").unwrap_or_else(|| "8112".to_string());
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
        Self {
            json_url: format!("{}/json", base.trim_end_matches('/')),
            password: config_value("password").unwrap_or_else(|| "deluge".to_string()),
            category: config_value("category").unwrap_or_else(|| "scryer-tv".to_string()),
            imported_category: config_value("post_import_category").unwrap_or_default(),
            recent_priority: queue_placement_config("recent_priority"),
            older_priority: queue_placement_config("older_priority"),
            add_paused: config_bool("add_paused", false),
            download_directory: config_value("download_directory").unwrap_or_default(),
            completed_directory: config_value("completed_directory").unwrap_or_default(),
            // NOTE: the retired `post_import_action` key is deliberately not read.
            // Its `remove`/`remove_with_data` values are gone (the core's seeding gate
            // owns removal) and its `retain` value is now the only behaviour, so an
            // existing config carrying any of the three keeps working unchanged: the
            // post-import label is applied when `post_import_category` asks for one,
            // exactly as Sonarr does, and nothing is ever removed.
        }
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
            Some("8112"),
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
            "password",
            "Password",
            ConfigFieldType::Password,
            true,
            Some("deluge"),
            None,
        ),
        field(
            "category",
            "Category",
            ConfigFieldType::String,
            false,
            Some("scryer-tv"),
            Some(&format!(
                "Deluge label applied to Scryer's torrents, and the filter it polls with. {LABEL_ALLOWED_CHARS}."
            )),
        ),
        field(
            "post_import_category",
            "Post Import Category",
            ConfigFieldType::String,
            false,
            None,
            Some(&format!(
                "Label moved onto a torrent once Scryer has imported it. Deluge allows one label per torrent, so this replaces the category above. {LABEL_ALLOWED_CHARS}."
            )),
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
            "add_paused",
            "Add Paused",
            ConfigFieldType::Bool,
            false,
            Some("false"),
            None,
        ),
        field(
            "download_directory",
            "Download Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
        field(
            "completed_directory",
            "Completed Directory",
            ConfigFieldType::Path,
            false,
            None,
            None,
        ),
    ]
}

fn add_options(
    config: &DelugeConfig,
    request: &PluginDownloadClientAddRequest,
) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    options.insert(
        "add_paused".to_string(),
        serde_json::Value::Bool(
            request
                .torrent
                .as_ref()
                .and_then(|torrent| torrent.initial_state)
                .is_some_and(|state| state == PluginTorrentInitialState::Paused)
                || config.add_paused,
        ),
    );
    options.insert(
        "remove_at_ratio".to_string(),
        serde_json::Value::Bool(false),
    );
    if let Some(path) = request
        .routing
        .download_directory
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            (!config.download_directory.is_empty()).then_some(config.download_directory.clone())
        })
    {
        options.insert(
            "download_location".to_string(),
            serde_json::Value::String(path),
        );
    }
    if !config.completed_directory.is_empty() {
        options.insert(
            "move_completed_path".to_string(),
            serde_json::Value::String(config.completed_directory.clone()),
        );
        options.insert("move_completed".to_string(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(options)
}

/// The label a grab should carry: whatever the core routed for this download, else the
/// configured category. Deluge lower-cases label ids, so this is what actually gets
/// stored.
fn add_label(config: &DelugeConfig, request: &PluginDownloadClientAddRequest) -> Option<String> {
    isolation_label(&request.routing.isolation)
        .or_else(|| normalize_label(request.routing.isolation_value.as_deref()))
        .or_else(|| normalize_label(Some(config.category.as_str())))
}

/// The label a finished import should carry: the configured post-import category,
/// under Sonarr's guard that it differs from the label the torrent was grabbed with
/// (Deluge.cs:42-43).
///
/// The core fills `post_import_isolation` with the *download's own* category
/// replicated across modes (`download_client_adapter.rs::build_isolation_entries`
/// on `request.category`) — it is the grab label this one replaces, never a new
/// label to apply — so it only feeds the "differs" guard, with the configured
/// category as the fallback when the core sent nothing.
fn post_import_label(config: &DelugeConfig, routed: &[PluginDownloadIsolation]) -> Option<String> {
    let imported = normalize_label(Some(config.imported_category.as_str()))?;
    let grabbed = isolation_label(routed)
        .or_else(|| normalize_label(Some(config.category.as_str())))
        .unwrap_or_default();
    (imported != grabbed).then_some(imported)
}

/// Deluge's single label answers to every naming the core might route it under.
fn isolation_label(isolation: &[PluginDownloadIsolation]) -> Option<String> {
    isolation
        .iter()
        .find(|entry| {
            matches!(
                entry.mode,
                DownloadIsolationMode::Tag
                    | DownloadIsolationMode::Label
                    | DownloadIsolationMode::Category
            )
        })
        .and_then(|entry| normalize_label(Some(entry.value.as_str())))
}

fn normalize_label(value: Option<&str>) -> Option<String> {
    value
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn should_move_to_top(config: &DelugeConfig, request: &PluginDownloadClientAddRequest) -> bool {
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

fn authenticate(config: &DelugeConfig, force: bool) -> DelugeResult<String> {
    if !force
        && let Some(cookie) = var::get(COOKIE_VAR_KEY)
            .ok()
            .flatten()
            .map(|value: String| value.trim().to_string())
            .filter(|value| !value.is_empty())
    {
        return Ok(cookie);
    }
    let response = raw_call(
        config,
        "auth.login",
        serde_json::json!([config.password]),
        None,
    )?;
    let parsed = parse_rpc(&response.body_text)?;
    if let Some(error) = parsed.error {
        return Err(DelugeFailure::for_field(
            PluginErrorCode::AuthFailed,
            "password",
            "Deluge rejected the configured password",
        )
        .with_detail(format!(
            "auth.login error {}: {}",
            error.code, error.message
        )));
    }
    if !parsed.result.as_bool().unwrap_or(false) {
        return Err(DelugeFailure::for_field(
            PluginErrorCode::AuthFailed,
            "password",
            "Deluge rejected the configured password",
        ));
    }
    let cookie = response.cookie.ok_or_else(|| {
        DelugeFailure::new(
            PluginErrorCode::UpstreamUnavailable,
            "Deluge accepted the password but returned no session cookie",
        )
    })?;
    let _ = var::set(COOKIE_VAR_KEY, cookie.clone());
    connect_daemon(config, &cookie)?;
    Ok(cookie)
}

fn connect_daemon(config: &DelugeConfig, cookie: &str) -> DelugeResult<()> {
    let connected = call_value_with_cookie(config, "web.connected", serde_json::json!([]), cookie)?;
    if connected.as_bool().unwrap_or(false) {
        return Ok(());
    }
    let hosts = call_value_with_cookie(config, "web.get_hosts", serde_json::json!([]), cookie)?;
    if let Some(hosts) = hosts.as_array() {
        // The returned list carries id, ip, port and status per connection; Sonarr
        // takes the 127.0.0.1 one (DelugeProxy.cs:355-365).
        for host in hosts {
            let Some(values) = host.as_array() else {
                continue;
            };
            if values.get(1).and_then(|value| value.as_str()) == Some("127.0.0.1")
                && let Some(id) = values.first()
            {
                call_value_with_cookie(config, "web.connect", serde_json::json!([id]), cookie)?;
                return Ok(());
            }
        }
    }
    Err(DelugeFailure::new(
        PluginErrorCode::UpstreamUnavailable,
        "Deluge's web UI is not connected to a daemon and offers no 127.0.0.1 host to connect to",
    ))
}

/// `ReconnectToDaemon` (DelugeProxy.cs:208-212).
fn reconnect_daemon(config: &DelugeConfig) -> DelugeResult<()> {
    let cookie = authenticate(config, false)?;
    let _ = call_value_with_cookie(config, "web.disconnect", serde_json::json!([]), &cookie);
    connect_daemon(config, &cookie)
}

fn call_value(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
) -> DelugeResult<serde_json::Value> {
    let cookie = authenticate(config, false)?;
    let response = raw_call(config, method, params.clone(), Some(cookie.clone()))?;
    let parsed = parse_rpc(&response.body_text)?;
    let Some(error) = parsed.error else {
        return Ok(parsed.result);
    };
    // Codes 1 and 2 are Deluge's "not authenticated" / "session expired"; Sonarr
    // re-authenticates once and retries before it gives up (DelugeProxy.cs:235-255).
    if !is_reauth_error(error.code) {
        return Err(rpc_failure(method, error.code, &error.message));
    }
    let cookie = authenticate(config, true)?;
    let response = raw_call(config, method, params, Some(cookie))?;
    let parsed = parse_rpc(&response.body_text)?;
    match parsed.error {
        None => Ok(parsed.result),
        Some(error) => Err(DelugeFailure::for_field(
            PluginErrorCode::AuthFailed,
            "password",
            "Deluge rejected the session even after re-authenticating",
        )
        .with_detail(format!("{method} error {}: {}", error.code, error.message))),
    }
}

fn call_value_with_cookie(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
    cookie: &str,
) -> DelugeResult<serde_json::Value> {
    let response = raw_call(config, method, params, Some(cookie.to_string()))?;
    let parsed = parse_rpc(&response.body_text)?;
    match parsed.error {
        None => Ok(parsed.result),
        Some(error) => Err(rpc_failure(method, error.code, &error.message)),
    }
}

fn is_reauth_error(code: i64) -> bool {
    matches!(code, 1 | 2)
}

fn rpc_failure(method: &str, code: i64, message: &str) -> DelugeFailure {
    DelugeFailure::new(
        PluginErrorCode::Temporary,
        format!("Deluge refused {method}: {message}"),
    )
    .with_detail(format!("json-rpc error code {code}"))
}

struct RawResponse {
    body_text: String,
    cookie: Option<String>,
}

fn raw_call(
    config: &DelugeConfig,
    method: &str,
    params: serde_json::Value,
    cookie: Option<String>,
) -> DelugeResult<RawResponse> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        // Deluge only echoes the id back, but every other client in the fleet — and
        // Sonarr's `JsonRpcRequestBuilder` — uses an incrementing integer.
        "id": next_request_id(),
    });
    let mut request = HttpRequest::new(&config.json_url)
        .with_method("POST")
        .with_header("Content-Type", "application/json")
        .with_header("User-Agent", user_agent());
    if let Some(cookie) = cookie {
        request = request.with_header("Cookie", cookie);
    }
    let payload = serde_json::to_vec(&body).map_err(|error| {
        DelugeFailure::new(
            PluginErrorCode::Permanent,
            "Deluge request could not be encoded",
        )
        .with_detail(error.to_string())
    })?;
    let response = http::request::<Vec<u8>>(&request, Some(payload))
        .map_err(|error| transport_failure(&error.to_string()))?;
    let status = response.status_code();
    let body_text = String::from_utf8_lossy(&response.body()).to_string();

    // Deluge answers 200 with `error` set for almost everything, so the body is read
    // before the status is judged; only a body that is not a JSON-RPC envelope falls
    // through to status classification.
    if status < 400 || parse_rpc(&body_text).is_ok() {
        return Ok(RawResponse {
            body_text,
            cookie: extract_cookie(&response),
        });
    }
    Err(http_status_failure(status, &body_text))
}

fn http_status_failure(status: u16, body: &str) -> DelugeFailure {
    let detail = format!("HTTP {status}: {}", truncate(body, 512));
    match status {
        401 | 403 => DelugeFailure::for_field(
            PluginErrorCode::AuthFailed,
            "password",
            "Deluge rejected the configured password",
        ),
        // Sonarr reads a 408 as "the web UI lost its daemon" and drives the same
        // reconnect the code-2 path does (DelugeProxy.cs:272-280).
        408 => DelugeFailure::new(
            PluginErrorCode::Temporary,
            "Deluge timed out; its web UI may have lost the daemon connection",
        ),
        // A login page instead of the JSON-RPC endpoint means url_base points at the
        // wrong resource.
        301 | 302 | 303 | 307 | 308 => DelugeFailure::for_field(
            PluginErrorCode::InvalidConfig,
            "url_base",
            "Deluge redirected the JSON-RPC endpoint; check the host, port and URL base",
        ),
        404 => DelugeFailure::for_field(
            PluginErrorCode::InvalidConfig,
            "url_base",
            "Deluge has no JSON-RPC endpoint at the configured URL",
        ),
        429 => DelugeFailure::new(PluginErrorCode::RateLimited, "Deluge rate-limited Scryer"),
        500..=599 => DelugeFailure::new(
            PluginErrorCode::Temporary,
            "Deluge's web UI returned a server error; its daemon may be down",
        ),
        _ => DelugeFailure::new(
            PluginErrorCode::Permanent,
            format!("Deluge rejected the request with HTTP {status}"),
        ),
    }
    .with_detail(detail)
}

/// Sonarr splits `WebException.Status` into Host / UseSsl validation failures
/// (Deluge.cs:289-311); the PDK gives us only a message, so the SSL hint is keyed off
/// the transport's own wording.
fn transport_failure(error: &str) -> DelugeFailure {
    let lower = error.to_ascii_lowercase();
    if lower.contains("ssl")
        || lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("handshake")
    {
        return DelugeFailure::for_field(
            PluginErrorCode::UpstreamUnavailable,
            "use_ssl",
            "Could not establish a secure connection to Deluge; verify the SSL setting and its certificate",
        )
        .with_detail(error.to_string());
    }
    DelugeFailure::for_field(
        PluginErrorCode::UpstreamUnavailable,
        "host",
        "Unable to connect to Deluge",
    )
    .with_detail(error.to_string())
}

fn parse_rpc(body: &str) -> DelugeResult<RpcResponse> {
    serde_json::from_str(body).map_err(|error| {
        DelugeFailure::new(
            PluginErrorCode::Temporary,
            "Deluge returned a response Scryer could not parse",
        )
        .with_detail(format!("{error}; body: {}", truncate(body, 512)))
    })
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

fn list_torrents(config: &DelugeConfig) -> DelugeResult<Vec<DelugeTorrent>> {
    let mut filter = serde_json::Map::new();
    if let Some(label) = normalize_label(Some(config.category.as_str())) {
        filter.insert("label".to_string(), serde_json::Value::String(label));
    }
    let response = call_value(
        config,
        "web.update_ui",
        serde_json::json!([REQUIRED_PROPERTIES, filter]),
    )?;
    let update: UpdateUiResult = serde_json::from_value(response).map_err(|error| {
        DelugeFailure::new(
            PluginErrorCode::Temporary,
            "Deluge returned a torrent listing Scryer could not parse",
        )
        .with_detail(error.to_string())
    })?;
    Ok(update.torrents.into_values().collect())
}

/// Sonarr skips hashless/nameless torrents and, once per instance, reconnects the web
/// UI to its daemon before warning about them (Deluge.cs:130-138, :198-212).
///
/// Sonarr's own guard reads `ignoredCount > 0 && _hasAttemptedReconnecting`, which
/// makes its reconnect branch unreachable; this implements the intent its comment
/// states — reconnect first, warn on a second occurrence.
fn note_invalid_torrents(config: &DelugeConfig, ignored: usize) {
    if ignored == 0 {
        let _ = var::remove(RECONNECT_VAR_KEY);
        let _ = var::remove(INVALID_TORRENTS_VAR_KEY);
        return;
    }
    let _ = var::set(INVALID_TORRENTS_VAR_KEY, ignored as u64);
    let attempted = var::get::<bool>(RECONNECT_VAR_KEY)
        .ok()
        .flatten()
        .unwrap_or(false);
    if attempted {
        warn(format!(
            "{ignored} torrent(s) were ignored because they had no hash or title. Deluge may have disconnected from its daemon; if this persists, check Deluge for invalid torrents."
        ));
        return;
    }
    let _ = var::set(RECONNECT_VAR_KEY, true);
    debug(format!(
        "{ignored} torrent(s) had no hash or title; reconnecting Deluge's web UI to its daemon."
    ));
    if let Err(failure) = reconnect_daemon(config) {
        warn(format!(
            "Deluge daemon reconnect failed: {}",
            failure.public_message()
        ));
    }
}

fn status_warnings() -> Vec<String> {
    var::get(INVALID_TORRENTS_VAR_KEY)
        .ok()
        .flatten()
        .filter(|ignored: &u64| *ignored > 0)
        .map(|ignored| {
            vec![format!(
                "{ignored} Deluge torrent(s) have no hash or title and are being skipped."
            )]
        })
        .unwrap_or_default()
}

fn get_methods(config: &DelugeConfig) -> DelugeResult<Vec<String>> {
    let methods = call_value(config, "system.listMethods", serde_json::json!([]))?;
    Ok(methods
        .as_array()
        .map(|methods| {
            methods
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

fn label_plugin_available(methods: &[String]) -> bool {
    methods.iter().any(|method| method.starts_with("label."))
}

fn version_from_methods(config: &DelugeConfig, methods: &[String]) -> DelugeResult<String> {
    // Deluge 1.3 has no `daemon.get_version`; Sonarr keeps the `daemon.info` fallback
    // (DelugeProxy.cs:52-62) and so do we.
    let method = if methods.iter().any(|method| method == "daemon.get_version") {
        "daemon.get_version"
    } else {
        "daemon.info"
    };
    call_value(config, method, serde_json::json!([]))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            DelugeFailure::new(
                PluginErrorCode::Temporary,
                "Deluge did not report a version string",
            )
        })
}

fn available_labels(config: &DelugeConfig) -> DelugeResult<Vec<String>> {
    let labels = call_value(config, "label.get_labels", serde_json::json!([]))?;
    Ok(labels
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default())
}

fn create_label(config: &DelugeConfig, label: &str) -> DelugeResult<()> {
    call_value(config, "label.add", serde_json::json!([label]))?;
    Ok(())
}

/// Create the label if this instance has not already proven it exists.
///
/// Sonarr only does this at `Test` time; here the add and post-import paths do it too,
/// because a Scryer operator can change the category without re-testing and an
/// unlabelled torrent is invisible to the label-filtered poll.
fn ensure_label(config: &DelugeConfig, label: &str) -> DelugeResult<()> {
    let mut known: Vec<String> = var::get(KNOWN_LABELS_VAR_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    if known.iter().any(|value| value == label) {
        return Ok(());
    }
    let existing = available_labels(config)?;
    if !existing.iter().any(|value| value == label) {
        create_label(config, label)?;
    }
    known = existing;
    if !known.iter().any(|value| value == label) {
        known.push(label.to_string());
    }
    let _ = var::set(KNOWN_LABELS_VAR_KEY, known);
    Ok(())
}

fn set_label(config: &DelugeConfig, hash: &str, label: &str) -> DelugeResult<()> {
    call_value(
        config,
        "label.set_torrent",
        serde_json::json!([hash, label]),
    )?;
    Ok(())
}

fn apply_seed_limits(
    config: &DelugeConfig,
    hash: &str,
    request: &PluginDownloadClientAddRequest,
) -> DelugeResult<()> {
    // Deluge enforces a ratio goal and nothing else; `supports_seed_time_limit: false`
    // is the descriptor saying so, and a seed-time goal stays a Scryer-side policy.
    let Some(ratio) = request
        .torrent
        .as_ref()
        .and_then(|torrent| torrent.seed_goal_ratio)
        .or(request.release.seed_goal_ratio)
    else {
        return Ok(());
    };
    call_value(
        config,
        "core.set_torrent_options",
        serde_json::json!([[hash], { "stop_ratio": ratio, "stop_at_ratio": 1 }]),
    )?;
    Ok(())
}

/// Sonarr's `GetStatus` chain (Deluge.cs:222-262): the configured directories first,
/// then the label's move-completed path, then the daemon's own configuration.
///
/// Sonarr reports exactly one root because its `RemotePathMappingService` maps one
/// path; Scryer's `remote_output_roots` is a set, so when both directories are
/// configured both are reported — the first entry is still Sonarr's pick.
fn output_roots(config: &DelugeConfig, methods: &[String]) -> DelugeResult<Vec<String>> {
    let mut roots = Vec::new();
    for candidate in [&config.completed_directory, &config.download_directory] {
        let candidate = candidate.trim();
        if !candidate.is_empty() && !roots.iter().any(|root| root == candidate) {
            roots.push(candidate.to_string());
        }
    }
    if !roots.is_empty() {
        return Ok(roots);
    }

    // Neither directory is configured — the common case, and the one where the old
    // port reported no root at all and left the core with nothing to path-map.
    let label = label_options(config, methods);
    let core_config: HashMap<String, serde_json::Value> = serde_json::from_value(call_value(
        config,
        "core.get_config",
        serde_json::json!([]),
    )?)
    .unwrap_or_default();
    Ok(resolve_output_root(label.as_ref(), &core_config)
        .into_iter()
        .collect())
}

fn label_options(config: &DelugeConfig, methods: &[String]) -> Option<DelugeLabel> {
    let label = normalize_label(Some(config.category.as_str()))?;
    if !label_plugin_available(methods) {
        return None;
    }
    // Best effort: a label that has never been given options answers with an error,
    // and that just means "fall through to the daemon's configuration".
    let value = call_value(config, "label.get_options", serde_json::json!([label])).ok()?;
    serde_json::from_value(value).ok()
}

fn resolve_output_root(
    label: Option<&DelugeLabel>,
    core_config: &HashMap<String, serde_json::Value>,
) -> Option<String> {
    if let Some(label) = label
        && label.apply_move_completed
        && label.move_completed
    {
        let path = label.move_completed_path.trim();
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    let key = if config_flag(core_config.get("move_completed")) {
        "move_completed_path"
    } else {
        "download_location"
    };
    core_config
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Deluge answers booleans as JSON booleans, but Sonarr compares
/// `config.GetValueOrDefault("move_completed", false).ToString() == "True"`, which also
/// accepts the stringified form some builds emit.
fn config_flag(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(text)) => text.eq_ignore_ascii_case("true"),
        Some(serde_json::Value::Number(number)) => number.as_i64().is_some_and(|value| value != 0),
        _ => false,
    }
}

fn torrent_to_item(config: &DelugeConfig, torrent: DelugeTorrent) -> PluginDownloadItem {
    let hash = normalize_hash(&torrent.hash);
    let remaining = (torrent.size - torrent.bytes_downloaded).max(0);
    let path = output_path(&torrent);
    let state = map_state(&torrent);
    let can_remove = derive_can_remove(&torrent, state);
    let label = reported_label(config, &torrent);
    let message = state_message(&torrent);
    let completed_at = completed_at(&torrent);
    PluginDownloadItem {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash.clone()),
        title: torrent.name.clone(),
        state,
        message: message.clone(),
        category: label.clone(),
        remote_output_path: Some(path.clone()),
        torrent: Some(PluginTorrentItem {
            info_hash_v1: Some(hash),
            labels: label.into_iter().collect(),
            save_path: Some(torrent.download_path.clone()),
            content_paths: vec![path],
            downloaded_bytes: Some(torrent.bytes_downloaded),
            uploaded_bytes: torrent.bytes_uploaded,
            download_rate_bytes_per_second: rate_to_bytes(torrent.download_rate),
            upload_rate_bytes_per_second: rate_to_bytes(torrent.upload_rate),
            seed_ratio: Some(torrent.ratio),
            seed_time_seconds: torrent.seeding_time,
            is_private: torrent.private,
            raw_status: Some(torrent.state.clone()),
            status_reason: message,
            ..PluginTorrentItem::default()
        }),
        total_size_bytes: Some(torrent.size),
        remaining_size_bytes: Some(remaining),
        eta_seconds: (torrent.eta >= 0.0).then_some(torrent.eta as i64),
        progress_percent: Some(torrent.progress.round().clamp(0.0, 100.0) as u8),
        // Data completeness only; whether a move is safe while seeding is decided Scryer-side.
        can_move_files: Some(state == DownloadItemState::Completed),
        can_remove,
        removed: Some(false),
        raw_state: Some(torrent.state),
        completed_at,
    }
}

fn torrent_to_completed(config: &DelugeConfig, torrent: DelugeTorrent) -> PluginCompletedDownload {
    let hash = normalize_hash(&torrent.hash);
    let path = output_path(&torrent);
    let completed_at = completed_at(&torrent);
    let category = reported_label(config, &torrent);
    let output_kind = output_kind(&torrent, &path);
    PluginCompletedDownload {
        client_item_id: hash.clone(),
        download_id: None,
        info_hash: Some(hash),
        name: torrent.name,
        dest_dir: path.clone(),
        category,
        output_kind: Some(output_kind),
        content_paths: vec![path],
        size_bytes: Some(torrent.size),
        completed_at,
        parameters: Vec::new(),
        release_name: None,
    }
}

/// The label Deluge itself reports, in Deluge's casing, falling back to the configured
/// category when the Label plugin is off (so the field is not silently empty).
fn reported_label(config: &DelugeConfig, torrent: &DelugeTorrent) -> Option<String> {
    torrent
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            let configured = config.category.trim();
            (!configured.is_empty()).then(|| configured.to_string())
        })
}

fn rate_to_bytes(rate: Option<f64>) -> Option<i64> {
    rate.filter(|rate| rate.is_finite() && *rate >= 0.0)
        .map(|rate| rate as i64)
}

fn completed_at(torrent: &DelugeTorrent) -> Option<String> {
    torrent
        .completed_time
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| unix_to_rfc3339(value as i64))
}

fn output_path(torrent: &DelugeTorrent) -> String {
    let save_path = torrent.download_path.trim_end_matches(['/', '\\']);
    // Deluge on Windows reports `C:\Downloads\...`; joining with `/` there would give
    // the core a path it cannot map.
    let separator = if save_path.contains('\\') && !save_path.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!("{save_path}{separator}{}", torrent.name)
}

/// Honest `can_remove` for Deluge.
///
/// Deluge only enforces a ratio goal, and only for auto-managed torrents with `stop_at_ratio`
/// set. When that is off there is no client-side seeding obligation to observe, so the plugin
/// reports `None` and Scryer-side goal evaluation decides.
///
/// Sonarr collapses the same four facts into a single `bool` and reuses it for
/// `CanMoveFiles` (Deluge.cs:188-193); Scryer separates the two — `can_move_files` is
/// about the data, `can_remove` about the seeding obligation — so this stays tri-state.
fn derive_can_remove(torrent: &DelugeTorrent, state: DownloadItemState) -> Option<bool> {
    if state != DownloadItemState::Completed {
        return Some(false);
    }
    if !torrent.is_auto_managed || !torrent.stop_at_ratio {
        return None;
    }
    if torrent.ratio < torrent.stop_ratio {
        return Some(false);
    }
    // Deluge pauses the torrent itself once `stop_ratio` is reached; until it has, the goal
    // is met on paper but the client is still seeding.
    if torrent.state == "Paused" {
        Some(true)
    } else {
        None
    }
}

/// Deluge's status table (`Deluge.cs:164-184`), plus the two Deluge 2 states Sonarr's
/// table predates and the richer Scryer states that are strictly more informative:
/// `Checking` is `Verifying`, not "Downloading", and `Moving` is `ImportPending`, so
/// the core does not read files out from under a relocation.
fn map_state(torrent: &DelugeTorrent) -> DownloadItemState {
    match torrent.state.trim() {
        "Error" => DownloadItemState::Warning,
        // Ordered before the finished check, exactly as Sonarr's
        // `IsFinished && State != Checking` guard is.
        "Checking" => DownloadItemState::Verifying,
        "Moving" => DownloadItemState::ImportPending,
        _ if torrent.is_finished => DownloadItemState::Completed,
        "Queued" => DownloadItemState::Queued,
        "Paused" => DownloadItemState::Paused,
        // `Downloading`, `Seeding` and `Allocating` all mean "keep polling", and so
        // does any state a newer Deluge adds: an unrecognised state is never evidence
        // of a fault (`7471134`).
        _ => DownloadItemState::Downloading,
    }
}

/// Sonarr always attaches a message to an errored torrent
/// (`DownloadClientDelugeTorrentStateError`); Deluge's own `message` is preferred when
/// it says something.
fn state_message(torrent: &DelugeTorrent) -> Option<String> {
    let message = torrent.message.trim();
    if !message.is_empty() {
        return Some(message.to_string());
    }
    match torrent.state.trim() {
        "Error" => Some("Deluge reports an error for this torrent".to_string()),
        "Moving" => Some("Deluge is moving this torrent's files".to_string()),
        _ => None,
    }
}

fn is_valid_torrent(torrent: &DelugeTorrent) -> bool {
    !torrent.hash.trim().is_empty() && !torrent.name.trim().is_empty()
}

fn magnet_source(request: &PluginDownloadClientAddRequest) -> Option<String> {
    request
        .source
        .magnet_uri
        .clone()
        .or_else(|| request.source.download_url.clone())
        .filter(|value| value.trim_start().starts_with("magnet:"))
}

fn torrent_file_url(request: &PluginDownloadClientAddRequest) -> Option<String> {
    if matches!(
        request.source.kind,
        DownloadInputKind::Nzb | DownloadInputKind::NzbUrl
    ) {
        return None;
    }

    request
        .source
        .torrent_url
        .clone()
        .or_else(|| request.source.download_url.clone())
        .filter(|value| !value.trim_start().starts_with("magnet:"))
}

fn torrent_file_name(request: &PluginDownloadClientAddRequest) -> String {
    request
        .source
        .torrent_file_name
        .clone()
        .or_else(|| request.source.source_title.clone())
        .unwrap_or_else(|| format!("{}.torrent", request.title.title_name))
}

fn get_external_bytes(url: &str) -> DelugeResult<Vec<u8>> {
    let request = HttpRequest::new(url)
        .with_method("GET")
        .with_header("User-Agent", user_agent());
    let response = http::request::<Vec<u8>>(&request, None).map_err(|error| {
        DelugeFailure::new(
            PluginErrorCode::UpstreamUnavailable,
            "Could not fetch the .torrent file",
        )
        .with_detail(error.to_string())
    })?;
    let status = response.status_code();
    let body = response.body();
    if status >= 400 {
        return Err(DelugeFailure::new(
            PluginErrorCode::Temporary,
            format!("The .torrent URL returned HTTP {status}"),
        )
        .with_detail(truncate(&String::from_utf8_lossy(&body), 512)));
    }
    if body.is_empty() {
        return Err(DelugeFailure::new(
            PluginErrorCode::Temporary,
            "The .torrent URL returned an empty response body",
        ));
    }
    Ok(body)
}

fn normalize_hash(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether `save_path/name` names a file or the directory Deluge created for the
/// torrent.
///
/// `num_files` settles it exactly — a single-file torrent's `name` *is* the file — and
/// the old extension guess is kept only for a daemon that does not report it, where it
/// would otherwise call a dotted season-pack folder (`Some.Show.S01`) a file.
fn output_kind(torrent: &DelugeTorrent, path: &str) -> PluginDownloadOutputKind {
    match torrent.num_files {
        Some(1) => PluginDownloadOutputKind::File,
        Some(_) => PluginDownloadOutputKind::Directory,
        None if path_looks_like_file(path) => PluginDownloadOutputKind::File,
        None => PluginDownloadOutputKind::Directory,
    }
}

fn path_looks_like_file(path: &str) -> bool {
    let Some(last) = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
    else {
        return false;
    };
    let Some(ext) = last.rsplit('.').next() else {
        return false;
    };
    ext != last
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "…"
}

fn user_agent() -> String {
    format!("scryer-deluge-plugin/{}", env!("CARGO_PKG_VERSION"))
}

/// Deluge's `json_api` only echoes the id back, but an incrementing integer is what
/// every other JSON-RPC client in the fleet — and Sonarr's `JsonRpcRequestBuilder` —
/// sends, and it makes a captured exchange readable.
fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn unix_to_rfc3339(timestamp: i64) -> String {
    // Kept dependency-free, like the other first-party download clients.
    let days = timestamp.div_euclid(86_400);
    let seconds_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year, m, d)
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

    fn finished_torrent(state: &str) -> DelugeTorrent {
        DelugeTorrent {
            hash: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            name: "Movie".to_string(),
            state: state.to_string(),
            is_finished: true,
            progress: 100.0,
            ..DelugeTorrent::default()
        }
    }

    #[test]
    fn can_remove_is_false_while_downloading() {
        let torrent = DelugeTorrent {
            hash: "a1".to_string(),
            name: "Movie".to_string(),
            state: "Downloading".to_string(),
            is_auto_managed: true,
            stop_at_ratio: true,
            stop_ratio: 1.0,
            ..DelugeTorrent::default()
        };
        let state = map_state(&torrent);
        assert_eq!(derive_can_remove(&torrent, state), Some(false));
    }

    #[test]
    fn can_remove_is_false_while_seeding_towards_an_unmet_stop_ratio() {
        let torrent = DelugeTorrent {
            is_auto_managed: true,
            stop_at_ratio: true,
            stop_ratio: 2.0,
            ratio: 0.4,
            ..finished_torrent("Seeding")
        };
        assert_eq!(
            derive_can_remove(&torrent, DownloadItemState::Completed),
            Some(false)
        );
    }

    #[test]
    fn can_remove_is_true_once_deluge_paused_a_torrent_at_its_stop_ratio() {
        let torrent = DelugeTorrent {
            is_auto_managed: true,
            stop_at_ratio: true,
            stop_ratio: 1.0,
            ratio: 1.2,
            ..finished_torrent("Paused")
        };
        assert_eq!(
            derive_can_remove(&torrent, DownloadItemState::Completed),
            Some(true)
        );
    }

    #[test]
    fn can_remove_is_unknown_without_a_client_side_ratio_goal() {
        let not_auto_managed = DelugeTorrent {
            is_auto_managed: false,
            stop_at_ratio: true,
            stop_ratio: 1.0,
            ratio: 5.0,
            ..finished_torrent("Paused")
        };
        let no_stop_at_ratio = DelugeTorrent {
            is_auto_managed: true,
            stop_at_ratio: false,
            ratio: 5.0,
            ..finished_torrent("Seeding")
        };
        assert_eq!(
            derive_can_remove(&not_auto_managed, DownloadItemState::Completed),
            None
        );
        assert_eq!(
            derive_can_remove(&no_stop_at_ratio, DownloadItemState::Completed),
            None
        );
    }

    #[test]
    fn met_ratio_that_deluge_has_not_paused_yet_is_unknown() {
        let torrent = DelugeTorrent {
            is_auto_managed: true,
            stop_at_ratio: true,
            stop_ratio: 1.0,
            ratio: 3.0,
            ..finished_torrent("Seeding")
        };
        assert_eq!(
            derive_can_remove(&torrent, DownloadItemState::Completed),
            None
        );
    }

    /// `DelugeFixture.GetItems_should_check_share_ratio_for_moveFiles_and_remove`
    /// (DelugeFixture.cs:275-292), split into Scryer's two independent answers.
    #[test]
    fn share_ratio_gates_removal_but_never_data_completeness() {
        for (ratio, expected_can_remove) in [(0.5, Some(false)), (1.01, Some(true))] {
            let torrent = DelugeTorrent {
                is_auto_managed: true,
                stop_at_ratio: true,
                stop_ratio: 1.0,
                ratio,
                ..finished_torrent("Paused")
            };
            let item = torrent_to_item(&test_config(), torrent);
            assert_eq!(item.state, DownloadItemState::Completed);
            assert_eq!(item.can_remove, expected_can_remove);
            assert_eq!(item.can_move_files, Some(true));
        }
    }

    #[test]
    fn can_move_files_tracks_data_completeness_not_seeding() {
        let config = test_config();
        let torrent = DelugeTorrent {
            is_auto_managed: true,
            stop_at_ratio: true,
            stop_ratio: 9.0,
            ratio: 0.1,
            ..finished_torrent("Seeding")
        };
        let item = torrent_to_item(&config, torrent);
        assert_eq!(item.can_move_files, Some(true));
        assert_eq!(item.can_remove, Some(false));
    }

    #[test]
    fn seed_time_comes_from_seeding_time_not_active_time() {
        let torrent: DelugeTorrent = serde_json::from_str(
            r#"{"hash":"a1","name":"n","state":"Seeding","is_finished":true,"active_time":100000,"seeding_time":7200,"ratio":1.0}"#,
        )
        .unwrap();
        let item = torrent_to_item(&test_config(), torrent);
        assert_eq!(item.torrent.unwrap().seed_time_seconds, Some(7_200));
    }

    #[test]
    fn is_private_maps_present_true_present_false_and_absent() {
        let map = |raw: &str| {
            let torrent: DelugeTorrent = serde_json::from_str(raw).unwrap();
            torrent_to_item(&test_config(), torrent)
                .torrent
                .unwrap()
                .is_private
        };
        assert_eq!(
            map(r#"{"hash":"a1","name":"n","state":"Seeding","private":true}"#),
            Some(true)
        );
        assert_eq!(
            map(r#"{"hash":"a1","name":"n","state":"Seeding","private":false}"#),
            Some(false)
        );
        assert_eq!(map(r#"{"hash":"a1","name":"n","state":"Seeding"}"#), None);
    }

    /// `DelugeFixture.GetItems_should_return_{queued,downloading}_item_as_downloadItemStatus`
    /// (DelugeFixture.cs:228-258).
    #[test]
    fn unfinished_status_table_matches_sonarr() {
        let cases = [
            ("Paused", DownloadItemState::Paused),
            ("Queued", DownloadItemState::Queued),
            ("Downloading", DownloadItemState::Downloading),
            ("Seeding", DownloadItemState::Downloading),
            ("Allocating", DownloadItemState::Downloading),
            ("Error", DownloadItemState::Warning),
        ];
        for (state, expected) in cases {
            let torrent = DelugeTorrent {
                hash: "a1".to_string(),
                name: "n".to_string(),
                state: state.to_string(),
                ..DelugeTorrent::default()
            };
            assert_eq!(map_state(&torrent), expected, "state {state}");
        }
    }

    /// `DelugeFixture.GetItems_should_return_completed_item_as_downloadItemStatus`
    /// (DelugeFixture.cs:260-273): a finished torrent is Completed in every state
    /// except `Checking`, where Sonarr reports Downloading and Scryer reports the
    /// strictly more informative `Verifying`.
    #[test]
    fn finished_status_table_matches_sonarr() {
        let cases = [
            ("Paused", DownloadItemState::Completed),
            ("Queued", DownloadItemState::Completed),
            ("Seeding", DownloadItemState::Completed),
            ("Checking", DownloadItemState::Verifying),
            ("Moving", DownloadItemState::ImportPending),
            ("Error", DownloadItemState::Warning),
        ];
        for (state, expected) in cases {
            assert_eq!(
                map_state(&finished_torrent(state)),
                expected,
                "state {state}"
            );
        }
    }

    /// Common rule 2: a state Deluge adds later keeps the item polling.
    #[test]
    fn unknown_states_keep_polling() {
        let torrent = DelugeTorrent {
            hash: "a1".to_string(),
            name: "n".to_string(),
            state: "SomeFutureDelugeState".to_string(),
            ..DelugeTorrent::default()
        };
        assert_eq!(map_state(&torrent), DownloadItemState::Downloading);
    }

    #[test]
    fn a_moving_torrent_is_not_yet_safe_to_read() {
        let item = torrent_to_item(&test_config(), finished_torrent("Moving"));
        assert_eq!(item.state, DownloadItemState::ImportPending);
        assert_eq!(item.can_move_files, Some(false));
        assert_eq!(item.can_remove, Some(false));
        assert_eq!(
            item.message.as_deref(),
            Some("Deluge is moving this torrent's files")
        );
    }

    #[test]
    fn an_errored_torrent_always_carries_a_message() {
        let with_message = DelugeTorrent {
            message: "tracker rejected".to_string(),
            ..finished_torrent("Error")
        };
        assert_eq!(
            torrent_to_item(&test_config(), with_message)
                .message
                .as_deref(),
            Some("tracker rejected")
        );
        assert_eq!(
            torrent_to_item(&test_config(), finished_torrent("Error"))
                .message
                .as_deref(),
            Some("Deluge reports an error for this torrent")
        );
    }

    /// `DelugeFixture.completed_download_should_have_required_properties`
    /// (DelugeFixture.cs:192-201) and `queued_item_should_have_required_properties`.
    #[test]
    fn item_carries_the_fixture_properties() {
        let torrent = DelugeTorrent {
            hash: "CBC2F069FE8BB2F544EAE707D75BCD3DE9DCF951".to_string(),
            name: "Some.Release.S01E01".to_string(),
            state: "Queued".to_string(),
            size: 1000,
            bytes_downloaded: 100,
            download_path: "somepath".to_string(),
            eta: 60.0,
            progress: 10.0,
            ..DelugeTorrent::default()
        };
        let item = torrent_to_item(&test_config(), torrent);
        assert_eq!(item.state, DownloadItemState::Queued);
        assert_eq!(item.title, "Some.Release.S01E01");
        assert_eq!(item.total_size_bytes, Some(1000));
        assert_eq!(item.remaining_size_bytes, Some(900));
        assert_eq!(item.eta_seconds, Some(60));
        assert_eq!(item.progress_percent, Some(10));
        assert_eq!(
            item.remote_output_path.as_deref(),
            Some("somepath/Some.Release.S01E01")
        );
        // Sonarr hands the rest of its pipeline `Hash.ToUpper()` and lower-cases again
        // on the way back in (Deluge.cs:141, :219). Scryer normalises once, at the
        // edge, so `client_item_id` round-trips through `core.remove_torrent` and
        // `label.set_torrent` without a casing dance.
        assert_eq!(
            item.client_item_id,
            "cbc2f069fe8bb2f544eae707d75bcd3de9dcf951"
        );
        assert_eq!(
            item.info_hash.as_deref(),
            Some(item.client_item_id.as_str())
        );
    }

    #[test]
    fn windows_save_paths_keep_their_separator() {
        let torrent = DelugeTorrent {
            download_path: r"C:\Downloads\deluge".to_string(),
            ..finished_torrent("Paused")
        };
        assert_eq!(output_path(&torrent), r"C:\Downloads\deluge\Movie");
    }

    #[test]
    fn completed_time_becomes_an_rfc3339_completed_at() {
        let finished: DelugeTorrent = serde_json::from_str(
            r#"{"hash":"a1","name":"n","state":"Paused","is_finished":true,"completed_time":1700000000}"#,
        )
        .unwrap();
        assert_eq!(
            torrent_to_item(&test_config(), finished)
                .completed_at
                .as_deref(),
            Some("2023-11-14T22:13:20Z")
        );

        // Deluge 1.3 omits the key entirely, and Deluge 2 reports 0 for a torrent that
        // never finished.
        let never_finished: DelugeTorrent =
            serde_json::from_str(r#"{"hash":"a1","name":"n","state":"Downloading"}"#).unwrap();
        assert_eq!(
            torrent_to_item(&test_config(), never_finished).completed_at,
            None
        );
        let zero: DelugeTorrent = serde_json::from_str(
            r#"{"hash":"a1","name":"n","state":"Downloading","completed_time":0}"#,
        )
        .unwrap();
        assert_eq!(torrent_to_item(&test_config(), zero).completed_at, None);
    }

    #[test]
    fn rates_and_uploaded_bytes_are_reported_when_deluge_sends_them() {
        let torrent: DelugeTorrent = serde_json::from_str(
            r#"{"hash":"a1","name":"n","state":"Seeding","total_uploaded":2048,"download_payload_rate":1024.0,"upload_payload_rate":512.5}"#,
        )
        .unwrap();
        let torrent_item = torrent_to_item(&test_config(), torrent).torrent.unwrap();
        assert_eq!(torrent_item.uploaded_bytes, Some(2048));
        assert_eq!(torrent_item.download_rate_bytes_per_second, Some(1024));
        assert_eq!(torrent_item.upload_rate_bytes_per_second, Some(512));
    }

    /// `memory: download-category-case-sensitivity` — report the client's own casing.
    #[test]
    fn category_comes_from_the_label_deluge_reports() {
        let config = DelugeConfig {
            category: "scryer-tv".to_string(),
            ..test_config()
        };
        let labelled: DelugeTorrent = serde_json::from_str(
            r#"{"hash":"a1","name":"n","state":"Seeding","label":"Scryer-TV"}"#,
        )
        .unwrap();
        assert_eq!(
            torrent_to_item(&config, labelled).category.as_deref(),
            Some("Scryer-TV")
        );

        // Label plugin off: fall back to the configured value rather than reporting
        // an empty category.
        let unlabelled: DelugeTorrent =
            serde_json::from_str(r#"{"hash":"a1","name":"n","state":"Seeding"}"#).unwrap();
        assert_eq!(
            torrent_to_item(&config, unlabelled).category.as_deref(),
            Some("scryer-tv")
        );
    }

    #[test]
    fn add_options_match_sonarrs_proxy() {
        let config = DelugeConfig {
            add_paused: true,
            download_directory: "/downloads".to_string(),
            completed_directory: "/done".to_string(),
            ..test_config()
        };
        let options = add_options(&config, &add_request("magnet:?xt=urn:btih:abc"));
        assert_eq!(options["add_paused"], serde_json::json!(true));
        assert_eq!(options["remove_at_ratio"], serde_json::json!(false));
        assert_eq!(
            options["download_location"],
            serde_json::json!("/downloads")
        );
        assert_eq!(options["move_completed_path"], serde_json::json!("/done"));
        assert_eq!(options["move_completed"], serde_json::json!(true));
    }

    #[test]
    fn a_routed_download_directory_wins_over_the_configured_one() {
        let config = DelugeConfig {
            download_directory: "/downloads".to_string(),
            ..test_config()
        };
        let mut request = add_request("magnet:?xt=urn:btih:abc");
        request.routing.download_directory = Some("/downloads/season-pack".to_string());
        let options = add_options(&config, &request);
        assert_eq!(
            options["download_location"],
            serde_json::json!("/downloads/season-pack")
        );
    }

    #[test]
    fn the_grab_label_prefers_the_cores_routing_and_is_lowercased() {
        let config = DelugeConfig {
            category: "scryer-tv".to_string(),
            ..test_config()
        };
        let mut request = add_request("magnet:?xt=urn:btih:abc");
        assert_eq!(add_label(&config, &request).as_deref(), Some("scryer-tv"));

        request.routing.isolation_value = Some("Routed-Category".to_string());
        assert_eq!(
            add_label(&config, &request).as_deref(),
            Some("routed-category")
        );

        request.routing.isolation = vec![PluginDownloadIsolation {
            mode: DownloadIsolationMode::Tag,
            value: "Scryer-Anime".to_string(),
        }];
        assert_eq!(
            add_label(&config, &request).as_deref(),
            Some("scryer-anime")
        );

        let unlabelled = DelugeConfig {
            category: String::new(),
            ..test_config()
        };
        assert_eq!(
            add_label(&unlabelled, &add_request("magnet:?xt=urn:btih:abc")),
            None
        );
    }

    /// Sonarr's `MarkItemAsImported` guard (Deluge.cs:42-43), plus the core's routed
    /// `post_import_isolation` taking precedence.
    #[test]
    fn post_import_label_follows_sonarrs_rule_and_the_cores_routing() {
        let config = DelugeConfig {
            category: "scryer-tv".to_string(),
            imported_category: "scryer-tv-imported".to_string(),
            ..test_config()
        };
        assert_eq!(
            post_import_label(&config, &[]).as_deref(),
            Some("scryer-tv-imported")
        );
        // The core routes the *grab* label (the download's own category) in
        // `post_import_isolation`; the configured post-import label still wins.
        assert_eq!(
            post_import_label(
                &config,
                &[PluginDownloadIsolation {
                    mode: DownloadIsolationMode::Category,
                    value: "Scryer-TV".to_string(),
                }]
            )
            .as_deref(),
            Some("scryer-tv-imported")
        );
        // A routed grab label equal to the post-import label means nothing to do,
        // even when the configured category differs.
        assert_eq!(
            post_import_label(
                &config,
                &[PluginDownloadIsolation {
                    mode: DownloadIsolationMode::Category,
                    value: "scryer-tv-imported".to_string(),
                }]
            ),
            None
        );

        // Not configured, or identical to the grab category: nothing to do.
        let unset = DelugeConfig {
            category: "scryer-tv".to_string(),
            ..test_config()
        };
        assert_eq!(post_import_label(&unset, &[]), None);
        let same = DelugeConfig {
            category: "scryer-tv".to_string(),
            imported_category: "scryer-tv".to_string(),
            ..test_config()
        };
        assert_eq!(post_import_label(&same, &[]), None);

        // An isolation entry the client cannot express is ignored rather than
        // mistaken for a label.
        assert_eq!(
            post_import_label(
                &unset,
                &[PluginDownloadIsolation {
                    mode: DownloadIsolationMode::Directory,
                    value: "/imported".to_string(),
                }]
            ),
            None
        );
    }

    /// Common rule 3: the destructive options are gone, and no legacy value can
    /// resurrect them — nothing in this client reads `post_import_action` any more, so
    /// a config that still carries `remove` or `remove_with_data` simply relabels.
    #[test]
    fn post_import_configuration_is_non_destructive_and_migrates_legacy_values() {
        let fields = config_fields();
        assert!(fields.iter().all(|field| field.key != "post_import_action"));
        assert!(
            fields
                .iter()
                .any(|field| field.key == "post_import_category")
        );
        // The descriptor tells the host the same thing.
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        assert_eq!(
            descriptor["provider"]["capabilities"]["mark_imported_non_destructive"],
            serde_json::json!(true)
        );
        assert_eq!(
            descriptor["provider"]["capabilities"]["remove"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn magnet_is_preferred_over_core_supplied_bytes() {
        let descriptor: serde_json::Value =
            serde_json::from_str(&scryer_describe(String::new()).unwrap()).unwrap();
        let preferred = descriptor["provider"]["capabilities"]["torrent"]["preferred_sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            preferred,
            vec!["magnet_uri", "torrent_bytes", "torrent_file", "torrent_url"]
        );
    }

    /// `DelugeSettingsValidator` (DelugeSettings.cs:14-15).
    #[test]
    fn label_names_are_validated_the_way_deluge_validates_them() {
        for good in ["", "scryer-tv", "tv-sonarr", "abc123", "---"] {
            assert!(validate_label_name("category", good).is_ok(), "{good}");
        }
        for bad in ["Scryer-TV", "scryer tv", "scryer_tv", "scryer.tv", "tv!"] {
            let failure = validate_label_name("category", bad).unwrap_err();
            assert_eq!(failure.code, PluginErrorCode::InvalidConfig);
            assert_eq!(failure.field, Some("category"));
            assert!(failure.public_message().starts_with("category: Allowed"));
        }
    }

    #[test]
    fn label_plugin_presence_is_detected_from_the_method_list() {
        assert!(label_plugin_available(&[
            "daemon.info".to_string(),
            "label.get_labels".to_string()
        ]));
        assert!(!label_plugin_available(&[
            "daemon.info".to_string(),
            "core.get_config".to_string()
        ]));
    }

    /// `Deluge.cs:237-249`, the branch the old port had no answer for.
    #[test]
    fn output_root_falls_back_through_the_label_and_daemon_configuration() {
        let core_config = |raw: &str| -> HashMap<String, serde_json::Value> {
            serde_json::from_str(raw).unwrap()
        };

        // Label move-completed path wins.
        let label = DelugeLabel {
            apply_move_completed: true,
            move_completed: true,
            move_completed_path: "/label/done".to_string(),
        };
        assert_eq!(
            resolve_output_root(
                Some(&label),
                &core_config(r#"{"move_completed":true,"move_completed_path":"/core/done"}"#)
            )
            .as_deref(),
            Some("/label/done")
        );

        // A label that does not apply its own move-completed path falls through.
        let inactive = DelugeLabel {
            apply_move_completed: false,
            move_completed: true,
            move_completed_path: "/label/done".to_string(),
        };
        assert_eq!(
            resolve_output_root(
                Some(&inactive),
                &core_config(
                    r#"{"move_completed":true,"move_completed_path":"/core/done","download_location":"/core/incomplete"}"#
                )
            )
            .as_deref(),
            Some("/core/done")
        );

        // No move-completed anywhere: the daemon's download location.
        assert_eq!(
            resolve_output_root(
                None,
                &core_config(
                    r#"{"move_completed":false,"move_completed_path":"/core/done","download_location":"/core/incomplete"}"#
                )
            )
            .as_deref(),
            Some("/core/incomplete")
        );

        // Sonarr compares the stringified bool; accept both shapes.
        assert_eq!(
            resolve_output_root(
                None,
                &core_config(r#"{"move_completed":"True","move_completed_path":"/core/done"}"#)
            )
            .as_deref(),
            Some("/core/done")
        );

        assert_eq!(resolve_output_root(None, &core_config("{}")), None);
    }

    /// `DelugeFixture.should_return_status_with_outputdirs_for_directories_in_settings`
    /// (DelugeFixture.cs:333-344): the configured directories short-circuit the chain,
    /// completed first.
    #[test]
    fn configured_directories_are_reported_completed_first() {
        let config = DelugeConfig {
            download_directory: "/downloads".to_string(),
            completed_directory: "/finished".to_string(),
            ..test_config()
        };
        assert_eq!(
            output_roots(&config, &[]).unwrap(),
            vec!["/finished".to_string(), "/downloads".to_string()]
        );

        let same = DelugeConfig {
            download_directory: "/downloads".to_string(),
            completed_directory: "/downloads".to_string(),
            ..test_config()
        };
        assert_eq!(
            output_roots(&same, &[]).unwrap(),
            vec!["/downloads".to_string()]
        );
    }

    #[test]
    fn http_statuses_map_to_honest_error_codes() {
        let cases = [
            (401, PluginErrorCode::AuthFailed),
            (403, PluginErrorCode::AuthFailed),
            (302, PluginErrorCode::InvalidConfig),
            (404, PluginErrorCode::InvalidConfig),
            (408, PluginErrorCode::Temporary),
            (429, PluginErrorCode::RateLimited),
            (500, PluginErrorCode::Temporary),
            (503, PluginErrorCode::Temporary),
            (418, PluginErrorCode::Permanent),
        ];
        for (status, expected) in cases {
            assert_eq!(
                http_status_failure(status, "body").code,
                expected,
                "status {status}"
            );
        }
    }

    #[test]
    fn transport_failures_separate_the_ssl_hint_from_a_dead_host() {
        let ssl = transport_failure("tls handshake failure: certificate verify failed");
        assert_eq!(ssl.code, PluginErrorCode::UpstreamUnavailable);
        assert_eq!(ssl.field, Some("use_ssl"));

        let host = transport_failure("connection refused");
        assert_eq!(host.code, PluginErrorCode::UpstreamUnavailable);
        assert_eq!(host.field, Some("host"));
        assert_eq!(host.public_message(), "host: Unable to connect to Deluge");
    }

    /// `DelugeProxy.cs:235-255`: only codes 1 and 2 mean "re-login and retry".
    #[test]
    fn only_codes_one_and_two_trigger_reauthentication() {
        assert!(is_reauth_error(1));
        assert!(is_reauth_error(2));
        for other in [-1, 0, 3, 4, 5] {
            assert!(!is_reauth_error(other), "code {other}");
        }
    }

    #[test]
    fn rpc_errors_keep_deluges_own_message_and_stay_retryable() {
        let failure = rpc_failure("label.set_torrent", 4, "Unknown Label");
        assert_eq!(failure.code, PluginErrorCode::Temporary);
        assert!(failure.public_message().contains("Unknown Label"));
        assert_eq!(failure.detail.as_deref(), Some("json-rpc error code 4"));
    }

    /// The invalid-torrent skip is what triggers Sonarr's daemon reconnect
    /// (Deluge.cs:130-138) and it must not drop valid rows.
    #[test]
    fn hashless_and_nameless_torrents_are_skipped() {
        let hashless = DelugeTorrent {
            name: "n".to_string(),
            ..DelugeTorrent::default()
        };
        let nameless = DelugeTorrent {
            hash: "a1".to_string(),
            ..DelugeTorrent::default()
        };
        let valid = DelugeTorrent {
            hash: "a1".to_string(),
            name: "n".to_string(),
            ..DelugeTorrent::default()
        };
        assert!(!is_valid_torrent(&hashless));
        assert!(!is_valid_torrent(&nameless));
        assert!(is_valid_torrent(&valid));
    }

    #[test]
    fn list_history_is_empty_because_deluge_has_no_failed_history() {
        let raw = scryer_download_list_history(String::new()).unwrap();
        let parsed: PluginResult<Vec<PluginDownloadItem>> = serde_json::from_str(&raw).unwrap();
        match parsed {
            PluginResult::Ok(items) => assert!(items.is_empty()),
            PluginResult::Err(error) => panic!("unexpected error: {}", error.public_message),
        }
    }

    #[test]
    fn request_ids_are_integers_and_increment() {
        let first = next_request_id();
        let second = next_request_id();
        assert!(second > first);
    }

    #[test]
    fn the_user_agent_carries_the_crate_version() {
        assert_eq!(
            user_agent(),
            format!("scryer-deluge-plugin/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn output_kind_distinguishes_a_single_file_from_a_directory() {
        let with_count = |name: &str, num_files: Option<i64>| DelugeTorrent {
            name: name.to_string(),
            download_path: "/downloads".to_string(),
            num_files,
            ..finished_torrent("Paused")
        };

        // `num_files` is exact, including for a dotted season-pack folder the
        // extension guess would have called a file.
        assert_eq!(
            torrent_to_completed(
                &test_config(),
                with_count("Some.Release.S01E01.mkv", Some(1))
            )
            .output_kind,
            Some(PluginDownloadOutputKind::File)
        );
        assert_eq!(
            torrent_to_completed(&test_config(), with_count("Some.Release.S01", Some(10)))
                .output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );

        // Daemon does not report it: fall back to the extension guess.
        assert_eq!(
            torrent_to_completed(&test_config(), with_count("Some.Release.S01E01.mkv", None))
                .output_kind,
            Some(PluginDownloadOutputKind::File)
        );
        assert_eq!(
            torrent_to_completed(&test_config(), with_count("Some Release S01", None)).output_kind,
            Some(PluginDownloadOutputKind::Directory)
        );
    }

    fn add_request(download_url: &str) -> PluginDownloadClientAddRequest {
        serde_json::from_value(serde_json::json!({
            "source": { "kind": "magnet_uri", "download_url": download_url },
            "release": {},
            "title": { "title_name": "Some Release", "media_facet": "tv" },
            "routing": {}
        }))
        .expect("add request fixture")
    }

    fn test_config() -> DelugeConfig {
        DelugeConfig {
            json_url: "http://localhost:8112/json".to_string(),
            password: "deluge".to_string(),
            category: String::new(),
            imported_category: String::new(),
            recent_priority: PluginTorrentQueuePlacement::Default,
            older_priority: PluginTorrentQueuePlacement::Default,
            add_paused: false,
            download_directory: String::new(),
            completed_directory: String::new(),
        }
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
